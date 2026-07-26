//! S3/MinIO storage backend with streaming multipart upload support.
//!
//! # I7: S3 Lifecycle Rule Recommendation
//!
//! When using S3 storage backend, configure a lifecycle rule to automatically clean up
//! incomplete multipart uploads. This is critical because:
//!
//! - Multipart uploads that fail mid-way (network error, server crash) leave orphaned parts
//! - Orphaned parts continue to incur storage costs indefinitely
//! - The `abort_multipart_upload` in this code is best-effort and may not execute during shutdown
//!
//! **Recommended S3 lifecycle rule:**
//! ```json
//! {
//!   "Rules": [
//!     {
//!       "ID": "AbortIncompleteMultipartUploads",
//!       "Status": "Enabled",
//!       "Filter": { "Prefix": "" },
//!       "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 7 }
//!     }
//!   ]
//! }
//! ```
//!
//! This will automatically abort any multipart upload that hasn't completed within 7 days,
//! preventing orphaned parts from accumulating and incurring unnecessary costs.

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::{Client, Config};
use bytes::Bytes;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Files smaller than this use simple put_object (no multipart overhead).
/// S3 requires minimum 5MB per part (except the last), so this is the threshold.
const MULTIPART_THRESHOLD: u64 = 5 * 1024 * 1024;

/// Size of each multipart upload part.
/// 8MB balances upload parallelism potential with API call overhead.
const PART_SIZE: u64 = 8 * 1024 * 1024;
const MAX_MULTIPART_PARTS: i32 = 10_000;
const MAX_S3_OBJECT_SIZE: u64 = 5 * 1024 * 1024 * 1024 * 1024;

#[derive(Default)]
struct ActiveMultipartUploads {
    /// Maps upload ID to object key. Upload IDs are unique even when multiple
    /// clients concurrently write the same content-addressed key.
    uploads: Mutex<HashMap<String, String>>,
}

impl ActiveMultipartUploads {
    fn register(&self, upload_id: String, key: String) {
        self.lock().insert(upload_id, key);
    }

    fn remove(&self, upload_id: &str) -> Option<String> {
        self.lock().remove(upload_id)
    }

    fn drain(&self) -> Vec<(String, String)> {
        self.lock().drain().collect()
    }

    fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.uploads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct ActiveUploadGuard {
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    active_uploads: Arc<ActiveMultipartUploads>,
    armed: bool,
}

impl ActiveUploadGuard {
    fn new(
        client: Client,
        bucket: String,
        key: String,
        upload_id: String,
        active_uploads: Arc<ActiveMultipartUploads>,
    ) -> Self {
        active_uploads.register(upload_id.clone(), key.clone());
        Self {
            client,
            bucket,
            key,
            upload_id,
            active_uploads,
            armed: true,
        }
    }

    async fn abort(&mut self) {
        if !self.armed {
            return;
        }
        let abort_result = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .send()
            .await;
        if let Err(error) = abort_result {
            tracing::warn!(
                key = %self.key,
                upload_id = %self.upload_id,
                error = %error,
                "Failed to abort multipart upload; retaining it for cleanup retry"
            );
        } else {
            self.disarm();
        }
    }

    fn disarm(&mut self) {
        if self.armed {
            self.active_uploads.remove(&self.upload_id);
            self.armed = false;
        }
    }
}

impl Drop for ActiveUploadGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Keep the registry entry so S3Storage::drop or a graceful
            // abort_all_active_uploads call can still attempt cleanup.
            return;
        };
        let Some(key) = self.active_uploads.remove(&self.upload_id) else {
            return;
        };

        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let upload_id = self.upload_id.clone();
        let active_uploads = self.active_uploads.clone();
        handle.spawn(async move {
            if let Err(error) = client
                .abort_multipart_upload()
                .bucket(&bucket)
                .key(&key)
                .upload_id(&upload_id)
                .send()
                .await
            {
                tracing::warn!(
                    key = %key,
                    upload_id = %upload_id,
                    error = %error,
                    "Failed to abort cancelled multipart upload"
                );
                active_uploads.register(upload_id, key);
            }
        });
    }
}

pub struct S3Storage {
    client: Client,
    bucket: String,
    /// Tracks in-flight multipart uploads for shutdown-time cleanup.
    /// Maps upload_id → object key. When the storage backend is dropped,
    /// any remaining entries are aborted to prevent orphaned parts from
    /// accumulating storage costs.
    /// Uses a process-local registry so graceful shutdown and future
    /// cancellation can attempt to abort every unique upload.
    active_uploads: Arc<ActiveMultipartUploads>,
}

impl S3Storage {
    pub async fn new(
        bucket: &str,
        region: Option<&str>,
        endpoint: Option<&str>,
    ) -> StorageResult<Self> {
        // M-1: Region defaults to us-east-1 (AWS default) as it's a safe fallback.
        // Unlike credentials (which must be explicit for security), region is not
        // security-sensitive and us-east-1 is the most common default region.
        let region = region.unwrap_or("us-east-1");

        // Gracefully handle missing credentials instead of panicking
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
            StorageError::Internal(
                "AWS_ACCESS_KEY_ID environment variable must be set for S3 storage backend"
                    .to_string(),
            )
        })?;

        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
            StorageError::Internal(
                "AWS_SECRET_ACCESS_KEY environment variable must be set for S3 storage backend"
                    .to_string(),
            )
        })?;

        let mut config_builder = Config::builder()
            .region(aws_sdk_s3::config::Region::new(region.to_string()))
            .credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "static",
            ));

        if let Some(endpoint) = endpoint {
            config_builder = config_builder.endpoint_url(endpoint);
            config_builder = config_builder.force_path_style(true);
        }

        let client = Client::from_conf(config_builder.build());

        Ok(Self {
            client,
            bucket: bucket.to_string(),
            active_uploads: Arc::new(ActiveMultipartUploads::default()),
        })
    }

    /// Upload a file using S3 multipart upload API.
    ///
    /// Memory usage is one multipart part. Parts start at 8 MiB and scale only
    /// when necessary to stay within S3's 10,000-part limit.
    ///
    /// On any error, the in-progress multipart upload is aborted to avoid
    /// leaving orphaned parts that incur storage costs.
    ///
    /// I3 fix: The upload_id is registered in `active_uploads` on initiation and
    /// removed on completion (success or abort). If the S3Storage is dropped while
    /// uploads are still in flight (e.g., server shutdown), the Drop impl aborts
    /// them to prevent orphaned parts from accumulating costs.
    async fn multipart_upload(&self, key: &str, path: &Path, file_size: u64) -> StorageResult<()> {
        let part_size = multipart_part_size(file_size)?;

        // 1. Initiate multipart upload
        let create_output = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                StorageError::Internal(format!("S3 create_multipart_upload failed: {}", e))
            })?;

        let upload_id = create_output
            .upload_id()
            .ok_or_else(|| {
                StorageError::Internal(
                    "S3 create_multipart_upload returned no upload_id".to_string(),
                )
            })?
            .to_string();

        // Register by unique upload ID. The per-upload guard also aborts if
        // this future is cancelled before completion.
        let mut upload_guard = ActiveUploadGuard::new(
            self.client.clone(),
            self.bucket.clone(),
            key.to_string(),
            upload_id.clone(),
            self.active_uploads.clone(),
        );

        // 2. Upload parts — if any part fails, abort the entire upload
        let upload_result = self
            .upload_parts(key, &upload_id, path, file_size, part_size)
            .await;

        let parts = match upload_result {
            Ok(parts) => parts,
            Err(e) => {
                upload_guard.abort().await;
                return Err(e);
            }
        };

        // 3. Complete multipart upload
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();

        let complete_result = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await;

        match complete_result {
            Ok(_) => {
                upload_guard.disarm();
                Ok(())
            }
            Err(e) => {
                upload_guard.abort().await;
                Err(StorageError::Internal(format!(
                    "S3 complete_multipart_upload failed: {}",
                    e
                )))
            }
        }
    }

    /// Abort all in-flight multipart uploads.
    ///
    /// Called automatically on Drop. Can also be called explicitly during
    /// graceful shutdown to ensure cleanup before the runtime exits.
    ///
    /// I3 fix: Provides shutdown-time cleanup for multipart uploads that would
    /// otherwise leave orphaned parts accumulating storage costs.
    pub async fn abort_all_active_uploads(&self) {
        let uploads = self.active_uploads.drain();

        for (upload_id, key) in uploads {
            tracing::info!(
                key = %key,
                upload_id = %upload_id,
                "Aborting in-flight multipart upload during shutdown"
            );
            if let Err(error) = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(&upload_id)
                .send()
                .await
            {
                tracing::warn!(
                    key = %key,
                    upload_id = %upload_id,
                    error = %error,
                    "Failed to abort multipart upload during shutdown; retaining for retry"
                );
                self.active_uploads.register(upload_id, key);
            }
        }
    }

    /// Read file in PART_SIZE chunks and upload each as an S3 part.
    /// Returns the list of completed parts (part_number + e_tag) for the
    /// complete_multipart_upload call.
    ///
    /// Peak memory is one calculated part. Each iteration allocates exactly one
    /// buffer which is moved into the ByteStream for upload.
    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        path: &Path,
        file_size: u64,
        part_size: u64,
    ) -> StorageResult<Vec<CompletedPart>> {
        let mut file = File::open(path).await.map_err(|e| {
            StorageError::Internal(format!("Failed to open file for multipart upload: {}", e))
        })?;

        let mut parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut offset: u64 = 0;

        while offset < file_size {
            if part_number > MAX_MULTIPART_PARTS {
                return Err(StorageError::InvalidArgument(format!(
                    "Multipart upload requires more than {} parts",
                    MAX_MULTIPART_PARTS
                )));
            }
            let remaining = file_size.checked_sub(offset).ok_or_else(|| {
                StorageError::Internal("Multipart upload offset exceeded file size".to_string())
            })?;
            let to_read = usize::try_from(remaining.min(part_size)).map_err(|_| {
                StorageError::InvalidArgument(
                    "Multipart read size does not fit in usize".to_string(),
                )
            })?;

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
                    let read_offset = offset
                        .checked_add(u64::try_from(read_total).map_err(|_| {
                            StorageError::Internal(
                                "Multipart read offset does not fit in u64".to_string(),
                            )
                        })?)
                        .ok_or_else(|| {
                            StorageError::Internal(
                                "Multipart read offset overflowed u64".to_string(),
                            )
                        })?;
                    return Err(StorageError::Internal(format!(
                        "Unexpected EOF at offset {} (expected {} more bytes)",
                        read_offset,
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
                    StorageError::Internal(format!("S3 upload_part {} failed: {}", part_number, e))
                })?;

            let completed_part = CompletedPart::builder()
                .part_number(part_number)
                .e_tag(part_output.e_tag().unwrap_or_default())
                .build();

            parts.push(completed_part);
            part_number = part_number.checked_add(1).ok_or_else(|| {
                StorageError::Internal("Multipart part number overflowed i32".to_string())
            })?;
            offset = offset
                .checked_add(u64::try_from(to_read).map_err(|_| {
                    StorageError::Internal("Multipart read size does not fit in u64".to_string())
                })?)
                .ok_or_else(|| {
                    StorageError::Internal("Multipart upload offset overflowed u64".to_string())
                })?;
        }

        if parts.is_empty() {
            return Err(StorageError::Internal(
                "Multipart upload produced no parts".to_string(),
            ));
        }

        Ok(parts)
    }
}

fn multipart_part_size(file_size: u64) -> StorageResult<u64> {
    if file_size > MAX_S3_OBJECT_SIZE {
        return Err(StorageError::InvalidArgument(format!(
            "File size {} exceeds the S3 object limit of {} bytes",
            file_size, MAX_S3_OBJECT_SIZE
        )));
    }

    let part_limit = u64::try_from(MAX_MULTIPART_PARTS).map_err(|_| {
        StorageError::Internal("Multipart part limit does not fit in u64".to_string())
    })?;
    let required_size = file_size / part_limit + u64::from(file_size % part_limit != 0);
    Ok(PART_SIZE.max(required_size))
}

/// I3 fix: On drop, abort any in-flight multipart uploads to prevent orphaned parts
/// from accumulating storage costs.
///
/// This is a best-effort safety net for runtime-driven shutdown while multipart
/// uploads are in progress. Under normal graceful shutdown, callers
/// should invoke `abort_all_active_uploads()` explicitly before dropping the backend.
///
/// Note: Drop is synchronous, so it spawns an asynchronous abort task. If the
/// tokio runtime is already shut down, the aborts may
/// not execute — in that case, the S3 lifecycle rule (AbortIncompleteMultipartUpload)
/// is the final line of defense.
impl Drop for S3Storage {
    fn drop(&mut self) {
        let count = self.active_uploads.len();
        if count == 0 {
            return;
        }
        tracing::warn!(
            count,
            "S3Storage dropped with in-flight multipart uploads; attempting cleanup"
        );
        let uploads = self.active_uploads.drain();

        let client = self.client.clone();
        let bucket = self.bucket.clone();

        // Try to spawn the cleanup on the current tokio runtime
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for (upload_id, key) in uploads {
                    tracing::info!(
                        key = %key,
                        upload_id = %upload_id,
                        "Aborting in-flight multipart upload on S3Storage drop"
                    );
                    let _ = client
                        .abort_multipart_upload()
                        .bucket(&bucket)
                        .key(&key)
                        .upload_id(&upload_id)
                        .send()
                        .await;
                }
            });
        } else {
            // Runtime is gone — log a warning. S3 lifecycle rule is the last resort.
            tracing::error!(
                count,
                "Cannot abort in-flight multipart uploads: tokio runtime is shut down. \
                 Configure S3 lifecycle rule AbortIncompleteMultipartUpload to clean up."
            );
        }
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn health_check(&self) -> StorageResult<()> {
        self.client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 head_bucket failed: {}", e)))?;
        Ok(())
    }

    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.into())
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 put failed: {}", e)))?;

        Ok(())
    }

    /// Store an object from a file on disk.
    ///
    /// For files < 5MB: uses simple put_object.
    /// For files >= 5MB: uses multipart upload with a calculated part size, so
    /// peak memory is one part and the request stays within S3's 10,000-part limit.
    async fn put_from_path(&self, key: &str, path: &Path) -> StorageResult<()> {
        let file_size = tokio::fs::metadata(path)
            .await
            .map_err(|e| StorageError::Internal(format!("Failed to read file metadata: {}", e)))?
            .len();

        if file_size < MULTIPART_THRESHOLD {
            // Small file: simple put_object
            let data = tokio::fs::read(path)
                .await
                .map_err(|e| StorageError::Internal(format!("Failed to read file: {}", e)))?;
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
                if e.code() == Some("NoSuchKey") || e.code() == Some("NotFound") {
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

    /// I2 fix: Download an S3 object directly to a file on disk using streaming.
    ///
    /// This implementation uses ByteStream's streaming capabilities to write
    /// the object directly to disk without loading the entire object into memory.
    /// Memory usage is bounded to the internal buffer size of the ByteStream.
    ///
    /// C1 fix: Writes to a temp file first, then renames to dest on success.
    /// If download fails mid-stream, the partial temp file is cleaned up and
    /// dest is never left in a corrupted state.
    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.code() == Some("NoSuchKey") || e.code() == Some("NotFound") {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Internal(format!("S3 get failed: {}", e))
                }
            })?;

        // C1 fix: Write to a unique temp file in the same directory, then rename on success.
        // This ensures dest is never left as a partial/corrupted file and avoids
        // collisions between concurrent downloads to the same destination.
        let temp_dest = {
            let file_name = dest
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("download");
            dest.with_file_name(format!("{}.{}.part", file_name, uuid::Uuid::new_v4()))
        };

        let mut file = File::create(&temp_dest).await.map_err(|e| {
            StorageError::Internal(format!(
                "Failed to create file {}: {}",
                temp_dest.display(),
                e
            ))
        })?;

        // Stream the body directly to file without collecting into memory
        let mut body = result.body;
        let download_result: Result<(), StorageError> = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| {
                    StorageError::Internal(format!("Failed to read S3 stream: {}", e))
                })?;
                file.write_all(&chunk).await.map_err(|e| {
                    StorageError::Internal(format!(
                        "Failed to write to {}: {}",
                        temp_dest.display(),
                        e
                    ))
                })?;
            }

            // Flush to ensure all data is written
            file.flush().await.map_err(|e| {
                StorageError::Internal(format!("Failed to flush {}: {}", temp_dest.display(), e))
            })?;

            Ok(())
        }
        .await;

        // If download failed, clean up the partial temp file
        if let Err(e) = download_result {
            let _ = tokio::fs::remove_file(&temp_dest).await;
            return Err(e);
        }

        // Atomic rename from temp to final destination
        if let Err(error) = tokio::fs::rename(&temp_dest, dest).await {
            let _ = tokio::fs::remove_file(&temp_dest).await;
            return Err(StorageError::Internal(format!(
                "Failed to rename temp file to {}: {}",
                dest.display(),
                error
            )));
        }

        Ok(())
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
            Err(e) if e.code() == Some("NotFound") || e.code() == Some("NoSuchKey") => Ok(false),
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

            let resp = req
                .send()
                .await
                .map_err(|e| StorageError::Internal(format!("S3 list_objects_v2 failed: {}", e)))?;

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

    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.code() == Some("NotFound") || e.code() == Some("NoSuchKey") {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Internal(format!("S3 head_object failed: {}", e))
                }
            })?;

        let content_length = result.content_length().ok_or_else(|| {
            StorageError::Internal("S3 HEAD response omitted Content-Length".to_string())
        })?;
        u64::try_from(content_length).map_err(|_| {
            StorageError::Internal(format!(
                "S3 HEAD returned a negative Content-Length: {}",
                content_length
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_uploads_keep_concurrent_uploads_for_the_same_key_distinct() {
        let uploads = ActiveMultipartUploads::default();
        uploads.register("upload-a".to_string(), "same-key".to_string());
        uploads.register("upload-b".to_string(), "same-key".to_string());

        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads.remove("upload-a").as_deref(), Some("same-key"));
        assert_eq!(uploads.len(), 1);

        assert_eq!(
            uploads.drain(),
            vec![("upload-b".to_string(), "same-key".to_string())]
        );
    }

    #[test]
    fn multipart_part_size_scales_and_enforces_s3_object_limit() {
        assert_eq!(multipart_part_size(PART_SIZE).unwrap(), PART_SIZE);
        let large_file = PART_SIZE * u64::try_from(MAX_MULTIPART_PARTS).unwrap() + 1;
        assert!(multipart_part_size(large_file).unwrap() > PART_SIZE);

        let error = multipart_part_size(MAX_S3_OBJECT_SIZE + 1).unwrap_err();
        assert!(matches!(error, StorageError::InvalidArgument(_)));
    }
}
