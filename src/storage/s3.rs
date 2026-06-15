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
use aws_sdk_s3::{Client, Config};
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Files smaller than this use simple put_object (no multipart overhead).
/// S3 requires minimum 5MB per part (except the last), so this is the threshold.
const MULTIPART_THRESHOLD: u64 = 5 * 1024 * 1024;

/// Size of each multipart upload part.
/// 8MB balances upload parallelism potential with API call overhead.
const PART_SIZE: usize = 8 * 1024 * 1024;

pub struct S3Storage {
    client: Client,
    bucket: String,
    /// Tracks in-flight multipart uploads for shutdown-time cleanup.
    /// Maps object key → upload_id. When the storage backend is dropped,
    /// any remaining entries are aborted to prevent orphaned parts from
    /// accumulating storage costs.
    /// I3 fix: Uses Mutex<HashMap> instead of external state to ensure
    /// abort-on-drop semantics even if the server is killed abruptly.
    active_uploads: std::sync::Mutex<std::collections::HashMap<String, String>>,
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
            StorageError::Internal("AWS_ACCESS_KEY_ID environment variable must be set for S3 storage backend".to_string())
        })?;

        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
            StorageError::Internal("AWS_SECRET_ACCESS_KEY environment variable must be set for S3 storage backend".to_string())
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
            active_uploads: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Upload a file using S3 multipart upload API.
    ///
    /// Memory usage: O(PART_SIZE) = 8MB, regardless of file size.
    /// The file is read in PART_SIZE chunks and each chunk is uploaded as a part.
    ///
    /// On any error, the in-progress multipart upload is aborted to avoid
    /// leaving orphaned parts that incur storage costs.
    ///
    /// I3 fix: The upload_id is registered in `active_uploads` on initiation and
    /// removed on completion (success or abort). If the S3Storage is dropped while
    /// uploads are still in flight (e.g., server shutdown), the Drop impl aborts
    /// them to prevent orphaned parts from accumulating costs.
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

        // Register the upload so Drop can abort it on shutdown
        if let Ok(mut guard) = self.active_uploads.lock() {
            guard.insert(key.to_string(), upload_id.clone());
        }

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
                // Unregister regardless of abort success
                if let Ok(mut guard) = self.active_uploads.lock() {
                    guard.remove(key);
                }
                return Err(e);
            }
        };

        // 3. Complete multipart upload
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();

        let complete_result = self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await;

        match complete_result {
            Ok(_) => {
                // Unregister after successful completion
                if let Ok(mut guard) = self.active_uploads.lock() {
                    guard.remove(key);
                }
                Ok(())
            }
            Err(e) => {
                // Best-effort abort on complete failure.
                // Note: tokio::spawn may not execute if the runtime is shutting down,
                // potentially leaving orphaned parts. Production S3 deployments should
                // configure a lifecycle rule with AbortIncompleteMultipartUpload to
                // automatically clean up stale multipart uploads after a timeout.
                // The Drop impl on S3Storage provides a second line of defense.
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                let key_clone = key.to_string();
                let uid = upload_id.clone();
                tokio::spawn(async move {
                    let _ = client
                        .abort_multipart_upload()
                        .bucket(&bucket)
                        .key(&key_clone)
                        .upload_id(&uid)
                        .send()
                        .await;
                });
                // Unregister — abort is in flight
                if let Ok(mut guard) = self.active_uploads.lock() {
                    guard.remove(key);
                }
                Err(StorageError::Internal(format!("S3 complete_multipart_upload failed: {}", e)))
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
        let uploads: Vec<(String, String)> = {
            let mut guard = match self.active_uploads.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.drain().collect()
        };

        for (key, upload_id) in uploads {
            tracing::info!(
                key = %key,
                upload_id = %upload_id,
                "Aborting in-flight multipart upload during shutdown"
            );
            let _ = self.client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(&upload_id)
                .send()
                .await;
        }
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

/// I3 fix: On drop, abort any in-flight multipart uploads to prevent orphaned parts
/// from accumulating storage costs.
///
/// This is a safety net for cases where the server is shut down abruptly (e.g., SIGKILL)
/// while multipart uploads are in progress. Under normal graceful shutdown, callers
/// should invoke `abort_all_active_uploads()` explicitly before dropping the backend.
///
/// Note: Drop is synchronous, so we spawn a blocking task that attempts to run the
/// abort asynchronously. If the tokio runtime is already shut down, the aborts may
/// not execute — in that case, the S3 lifecycle rule (AbortIncompleteMultipartUpload)
/// is the final line of defense.
impl Drop for S3Storage {
    fn drop(&mut self) {
        let uploads: Vec<(String, String)> = {
            let mut guard = match self.active_uploads.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if guard.is_empty() {
                return;
            }
            tracing::warn!(
                count = guard.len(),
                "S3Storage dropped with in-flight multipart uploads; attempting cleanup"
            );
            guard.drain().collect()
        };

        let client = self.client.clone();
        let bucket = self.bucket.clone();

        // Try to spawn the cleanup on the current tokio runtime
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for (key, upload_id) in uploads {
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
                count = uploads.len(),
                "Cannot abort in-flight multipart uploads: tokio runtime is shut down. \
                 Configure S3 lifecycle rule AbortIncompleteMultipartUpload to clean up."
            );
        }
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

    /// I2 fix: Download an S3 object directly to a file on disk using streaming.
    ///
    /// This implementation uses ByteStream's streaming capabilities to write
    /// the object directly to disk without loading the entire object into memory.
    /// Memory usage is bounded to the internal buffer size of the ByteStream.
    ///
    /// For large files (>500MB), this prevents memory pressure that would occur
    /// with the default implementation (get() + write()).
    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
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

        // Open the destination file
        let mut file = File::create(dest).await.map_err(|e| {
            StorageError::Internal(format!("Failed to create file {}: {}", dest.display(), e))
        })?;

        // Stream the body directly to file without collecting into memory
        let mut body = result.body;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| {
                StorageError::Internal(format!("Failed to read S3 stream: {}", e))
            })?;
            file.write_all(&chunk).await.map_err(|e| {
                StorageError::Internal(format!("Failed to write to {}: {}", dest.display(), e))
            })?;
        }

        // Flush to ensure all data is written
        file.flush().await.map_err(|e| {
            StorageError::Internal(format!("Failed to flush {}: {}", dest.display(), e))
        })?;

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

    /// S3-native paged listing using list_objects_v2 with max-keys and start-after.
    ///
    /// This is efficient for incremental GC: the scanner can resume from a checkpoint
    /// cursor without listing all objects. Server-side pagination avoids loading
    /// the entire key space into memory.
    async fn list_objects_paged(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        page_size: usize,
    ) -> StorageResult<(Vec<String>, Option<String>, bool)> {
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .max_keys(page_size as i32);

        if let Some(cursor) = start_after {
            req = req.start_after(cursor);
        }

        let resp = req.send().await.map_err(|e| {
            StorageError::Internal(format!("S3 list_objects_v2 paged failed: {}", e))
        })?;

        let keys: Vec<String> = resp.contents().iter()
            .filter_map(|obj| obj.key().map(|k| k.to_string()))
            .collect();

        let has_more = resp.is_truncated().unwrap_or(false);
        let next_cursor = if has_more {
            keys.last().cloned()
        } else {
            None
        };

        Ok((keys, next_cursor, has_more))
    }

    /// S3 conditional PUT using If-None-Match / If-Match headers.
    ///
    /// - expected_etag = None: If-None-Match: * (write only if key absent)
    /// - expected_etag = Some(etag): If-Match: etag (write only if etag matches)
    ///
    /// Returns the new ETag on success.
    /// Returns StorageError::ConditionFailed if the condition is not met.
    async fn put_if_absent_or_expired(
        &self,
        key: &str,
        data: Bytes,
        expected_etag: Option<&str>,
    ) -> StorageResult<String> {
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.to_vec().into());

        match expected_etag {
            None => {
                // Write only if absent: If-None-Match: *
                req = req.if_none_match("*");
            }
            Some(etag) => {
                // Write only if existing etag matches (lease expired check)
                req = req.if_match(etag);
            }
        }

        let result = req.send().await;

        match result {
            Ok(output) => {
                let new_etag = output.e_tag().unwrap_or("\"unknown\"").to_string();
                Ok(new_etag)
            }
            Err(e) => {
                let err_str = e.to_string();
                // S3 returns PreconditionFailed for If-None-Match / If-Match failures
                if err_str.contains("PreconditionFailed") || err_str.contains("precondition") {
                    Err(StorageError::ConditionFailed)
                } else {
                    Err(StorageError::Internal(format!("S3 conditional put failed: {}", e)))
                }
            }
        }
    }

    /// Get the S3 ETag for an object (used for lease management).
    async fn get_etag(&self, key: &str) -> StorageResult<Option<String>> {
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

        Ok(result.e_tag().map(|s| s.to_string()))
    }
}
