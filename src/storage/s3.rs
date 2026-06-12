//! S3/MinIO storage backend with streaming multipart upload support.

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use aws_sdk_s3::{Client, Config};
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

/// Files smaller than this use simple put_object (no multipart overhead).
/// S3 requires minimum 5MB per part (except the last), so this is the threshold.
const MULTIPART_THRESHOLD: u64 = 5 * 1024 * 1024;

/// Size of each multipart upload part.
/// 8MB balances upload parallelism potential with API call overhead.
const PART_SIZE: usize = 8 * 1024 * 1024;

pub struct S3Storage {
    client: Client,
    bucket: String,
}

impl S3Storage {
    pub async fn new(
        bucket: &str,
        region: Option<&str>,
        endpoint: Option<&str>,
    ) -> StorageResult<Self> {
        let region = region.unwrap_or("us-east-1");

        let mut config_builder = Config::builder()
            .region(aws_sdk_s3::config::Region::new(region.to_string()))
            .credentials_provider(Credentials::new(
                std::env::var("AWS_ACCESS_KEY_ID")
                    .expect("AWS_ACCESS_KEY_ID must be set for S3 storage backend"),
                std::env::var("AWS_SECRET_ACCESS_KEY")
                    .expect("AWS_SECRET_ACCESS_KEY must be set for S3 storage backend"),
                None,
                None,
                "test",
            ));

        if let Some(endpoint) = endpoint {
            config_builder = config_builder.endpoint_url(endpoint);
            config_builder = config_builder.force_path_style(true);
        }

        let client = Client::from_conf(config_builder.build());

        Ok(Self {
            client,
            bucket: bucket.to_string(),
        })
    }

    /// Upload a file using S3 multipart upload API.
    ///
    /// Memory usage: O(PART_SIZE) = 8MB, regardless of file size.
    /// The file is read in PART_SIZE chunks and each chunk is uploaded as a part.
    ///
    /// On any error, the in-progress multipart upload is aborted to avoid
    /// leaving orphaned parts that incur storage costs.
    async fn multipart_upload(&self, key: &str, path: &Path, file_size: u64) -> StorageResult<()> {
        // 1. Initiate multipart upload
        let create_output = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 create_multipart_upload failed: {}", e)))?;

        let upload_id = create_output.upload_id().ok_or_else(|| {
            StorageError::Internal("S3 create_multipart_upload returned no upload_id".to_string())
        })?.to_string();

        // 2. Upload parts — if any part fails, abort the entire upload
        let upload_result = self
            .upload_parts(key, &upload_id, path, file_size)
            .await;

        let parts = match upload_result {
            Ok(parts) => parts,
            Err(e) => {
                // Abort the multipart upload to clean up orphaned parts.
                // Best-effort: ignore abort errors since we're already returning
                // the original upload error.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                return Err(e);
            }
        };

        // 3. Complete multipart upload
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| {
                // Best-effort abort on complete failure.
                // Note: tokio::spawn may not execute if the runtime is shutting down,
                // potentially leaving orphaned parts. Production S3 deployments should
                // configure a lifecycle rule with AbortIncompleteMultipartUpload to
                // automatically clean up stale multipart uploads after a timeout.
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                let key = key.to_string();
                let uid = upload_id.clone();
                tokio::spawn(async move {
                    let _ = client
                        .abort_multipart_upload()
                        .bucket(&bucket)
                        .key(&key)
                        .upload_id(&uid)
                        .send()
                        .await;
                });
                StorageError::Internal(format!("S3 complete_multipart_upload failed: {}", e))
            })?;

        Ok(())
    }

    /// Read file in PART_SIZE chunks and upload each as an S3 part.
    /// Returns the list of completed parts (part_number + e_tag) for the
    /// complete_multipart_upload call.
    ///
    /// Peak memory: O(PART_SIZE) = 8MB. Each iteration allocates exactly one
    /// part-sized buffer which is moved into the ByteStream for upload.
    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        path: &Path,
        file_size: u64,
    ) -> StorageResult<Vec<CompletedPart>> {
        let mut file = File::open(path).await.map_err(|e| {
            StorageError::Internal(format!("Failed to open file for multipart upload: {}", e))
        })?;

        let mut parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut offset: u64 = 0;

        while offset < file_size {
            let remaining = (file_size - offset) as usize;
            let to_read = std::cmp::min(PART_SIZE, remaining);

            // Allocate exactly one buffer per part — moved into ByteStream,
            // so no separate reusable buffer (which would double peak RAM).
            let mut part_buf = vec![0u8; to_read];

            // Read exactly to_read bytes
            let mut read_total = 0;
            while read_total < to_read {
                let n = file.read(&mut part_buf[read_total..]).await.map_err(|e| {
                    StorageError::Internal(format!("Failed to read upload file: {}", e))
                })?;
                if n == 0 {
                    return Err(StorageError::Internal(format!(
                        "Unexpected EOF at offset {} (expected {} more bytes)",
                        offset + read_total as u64,
                        to_read - read_total
                    )));
                }
                read_total += n;
            }

            let part_data = Bytes::from(part_buf);

            let part_output = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(part_data.into())
                .send()
                .await
                .map_err(|e| {
                    StorageError::Internal(format!(
                        "S3 upload_part {} failed: {}",
                        part_number, e
                    ))
                })?;

            let completed_part = CompletedPart::builder()
                .part_number(part_number)
                .e_tag(part_output.e_tag().unwrap_or_default())
                .build();

            parts.push(completed_part);
            part_number += 1;
            offset += to_read as u64;
        }

        if parts.is_empty() {
            return Err(StorageError::Internal(
                "Multipart upload produced no parts".to_string(),
            ));
        }

        Ok(parts)
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.to_vec().into())
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 put failed: {}", e)))?;

        Ok(())
    }

    /// Store an object from a file on disk.
    ///
    /// For files < 5MB: uses simple put_object.
    /// For files >= 5MB: uses multipart upload, reading the file in 8MB chunks
    /// so peak memory usage is O(PART_SIZE) regardless of file size.
    async fn put_from_path(&self, key: &str, path: &Path) -> StorageResult<()> {
        let file_size = tokio::fs::metadata(path)
            .await
            .map_err(|e| {
                StorageError::Internal(format!("Failed to read file metadata: {}", e))
            })?
            .len();

        if file_size < MULTIPART_THRESHOLD {
            // Small file: simple put_object
            let data = tokio::fs::read(path).await.map_err(|e| {
                StorageError::Internal(format!("Failed to read file: {}", e))
            })?;
            return self.put(key, Bytes::from(data)).await;
        }

        // Large file: multipart upload
        self.multipart_upload(key, path, file_size).await
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.to_string().contains("NoSuchKey") {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Internal(format!("S3 get failed: {}", e))
                }
            })?;

        let data = result
            .body
            .collect()
            .await
            .map_err(|e| StorageError::Internal(format!("Failed to read body: {}", e)))?
            .into_bytes();

        Ok(data)
    }

    async fn exists(&self, key: &str) -> StorageResult<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("NotFound") => Ok(false),
            Err(e) => Err(StorageError::Internal(format!("S3 head failed: {}", e))),
        }
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 delete failed: {}", e)))?;

        Ok(())
    }

    async fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req.send().await.map_err(|e| {
                StorageError::Internal(format!("S3 list_objects_v2 failed: {}", e))
            })?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    keys.push(key.to_string());
                }
            }

            match resp.next_continuation_token() {
                Some(token) => continuation_token = Some(token.to_string()),
                None => break,
            }
        }

        Ok(keys)
    }

    async fn list_objects_with_mtime(&self, prefix: &str) -> StorageResult<Vec<(String, u64)>> {
        let mut entries = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req.send().await.map_err(|e| {
                StorageError::Internal(format!("S3 list_objects_v2 failed: {}", e))
            })?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    // Convert S3 DateTime to Unix timestamp seconds
                    let mtime = obj.last_modified()
                        .map(|dt| dt.secs() as u64)
                        .unwrap_or(0);
                    entries.push((key.to_string(), mtime));
                }
            }

            match resp.next_continuation_token() {
                Some(token) => continuation_token = Some(token.to_string()),
                None => break,
            }
        }

        Ok(entries)
    }

    async fn get_mtime(&self, key: &str) -> StorageResult<u64> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.to_string().contains("NotFound") {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Internal(format!("S3 head_object failed: {}", e))
                }
            })?;

        // Convert S3 DateTime to Unix timestamp seconds
        let mtime = result
            .last_modified()
            .map(|dt| dt.secs() as u64)
            .unwrap_or(0);

        Ok(mtime)
    }

    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.to_string().contains("NotFound") {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Internal(format!("S3 head_object failed: {}", e))
                }
            })?;

        // Get content length from HEAD response
        let size = result
            .content_length()
            .unwrap_or(0) as u64;

        Ok(size)
    }
}
