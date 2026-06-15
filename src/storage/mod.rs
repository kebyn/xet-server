//! Storage abstraction layer for Xet Storage server

use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod local;
pub mod s3;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Storage error: {0}")]
    Internal(String),

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Condition failed (optimistic locking)")]
    ConditionFailed,
}

pub type StorageResult<T> = Result<T, StorageError>;

#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Store an object
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()>;

    /// Store an object from a file on disk.
    /// Default implementation reads the entire file into RAM and delegates to `put()`.
    ///
    /// **Performance warning**: this default defeats the purpose of streaming uploads.
    /// Storage backends should override this method with a streaming implementation
    /// (e.g., LocalStorage uses rename for zero-copy, S3Storage uses multipart upload).
    /// A warning is logged when this default is exercised.
    async fn put_from_path(&self, key: &str, path: &Path) -> StorageResult<()> {
        tracing::warn!(
            "put_from_path using default (non-streaming) implementation for key={}; \
             this reads the entire file into RAM. Override put_from_path in your \
             StorageBackend implementation for streaming support.",
            key
        );
        let data = tokio::fs::read(path).await
            .map_err(|e| StorageError::Internal(format!("Failed to read file {}: {}", path.display(), e)))?;
        self.put(key, Bytes::from(data)).await
    }

    /// Retrieve an object
    async fn get(&self, key: &str) -> StorageResult<Bytes>;

    /// Get the filesystem path for a stored object, if the backend is file-based.
    /// Returns None for non-file backends (e.g. S3).
    /// This enables streaming downloads without loading the entire file into memory.
    async fn get_path(&self, _key: &str) -> StorageResult<Option<PathBuf>> {
        Ok(None)
    }

    /// Check if object exists
    async fn exists(&self, key: &str) -> StorageResult<bool>;

    /// Delete an object
    async fn delete(&self, key: &str) -> StorageResult<()>;

    /// List object keys matching a prefix.
    /// Returns full keys (e.g., "shards/abc123", "shards/def456").
    async fn list_objects(&self, _prefix: &str) -> StorageResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// List objects matching a prefix with their modification times.
    /// Returns (key, unix_timestamp_seconds) pairs.
    /// Used by GC to determine which blobs are older than the grace period.
    async fn list_objects_with_mtime(&self, prefix: &str) -> StorageResult<Vec<(String, u64)>> {
        // Default implementation: call list_objects and return mtime=0
        // Storage backends should override this for proper grace period support
        let keys = self.list_objects(prefix).await?;
        Ok(keys.into_iter().map(|k| (k, 0)).collect())
    }

    /// Get the modification time of a single object.
    /// Returns unix timestamp in seconds.
    /// Used by GC to re-check blob age before deletion, preventing race conditions
    /// where a blob is uploaded between GC's scan and delete phases.
    async fn get_mtime(&self, key: &str) -> StorageResult<u64> {
        // Default implementation: list with prefix and find the key
        // Storage backends should override this for efficiency
        let prefix = key.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let objects = self.list_objects_with_mtime(prefix).await?;
        objects
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, mtime)| mtime)
            .ok_or_else(|| StorageError::NotFound(key.to_string()))
    }

    /// Get the size of an object in bytes.
    /// Used by internal API to report blob size to Hub.
    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        // Default implementation: fetch the object and return its size
        // Storage backends should override this for efficiency (e.g., HEAD request)
        let data = self.get(key).await?;
        Ok(data.len() as u64)
    }

    /// Download an object directly to a file on disk, streaming the data
    /// to avoid loading the entire object into memory.
    ///
    /// Default implementation: uses get() and writes to file (loads entire object into RAM).
    /// Storage backends should override this with a streaming implementation.
    /// I1/I3: This enables bounded-memory downloads for the conversion pipeline and xorb downloads.
    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
        tracing::warn!(
            "download_to_path using default (non-streaming) implementation for key={}; \
             this reads the entire object into RAM. Override download_to_path in your \
             StorageBackend implementation for streaming support.",
            key
        );
        let data = self.get(key).await?;
        tokio::fs::write(dest, &data).await
            .map_err(|e| StorageError::Internal(
                format!("Failed to write to {}: {}", dest.display(), e)
            ))?;
        Ok(())
    }

    /// List objects matching a prefix in pages, supporting incremental scanning.
    ///
    /// Returns `(keys, next_cursor, has_more)` where:
    /// - `keys`: up to `page_size` object keys (full keys like "shards/abc123")
    /// - `next_cursor`: pass as `start_after` to get the next page; None if no more pages
    /// - `has_more`: true if there are more objects beyond this page
    ///
    /// Used by incremental GC to resume scanning from a checkpoint without
    /// listing all objects at once.
    ///
    /// Default implementation: calls list_objects() and slices by page.
    /// Storage backends should override for efficient server-side pagination.
    async fn list_objects_paged(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        page_size: usize,
    ) -> StorageResult<(Vec<String>, Option<String>, bool)> {
        let all_keys = self.list_objects(prefix).await?;

        // Filter keys after the cursor (if provided)
        let filtered: Vec<String> = if let Some(cursor) = start_after {
            all_keys.into_iter().filter(|k| k.as_str() > cursor).collect()
        } else {
            all_keys
        };

        let has_more = filtered.len() > page_size;
        let page: Vec<String> = filtered.into_iter().take(page_size).collect();
        let next_cursor = if has_more {
            page.last().cloned()
        } else {
            None
        };

        Ok((page, next_cursor, has_more))
    }

    /// Conditionally put an object only if it doesn't exist or the existing etag matches.
    ///
    /// This implements optimistic locking for concurrent operations:
    /// - `expected_etag = None`: write only if the key does NOT exist (If-None-Match: *)
    /// - `expected_etag = Some(etag)`: write only if existing object's etag matches (If-Match)
    ///
    /// Returns the new etag on success.
    /// Returns `StorageError::ConditionFailed` if the condition is not met.
    ///
    /// Used by GC coordinator for S3-based lease management.
    ///
    /// Default implementation: uses exists() + put() (NOT atomic — backends should override).
    async fn put_if_absent_or_expired(
        &self,
        key: &str,
        data: Bytes,
        expected_etag: Option<&str>,
    ) -> StorageResult<String> {
        // Default: non-atomic check-then-put. Backends MUST override for atomicity.
        tracing::warn!(
            "put_if_absent_or_expired using default (non-atomic) implementation for key={}; \
             this is a race condition. Override in your StorageBackend implementation.",
            key
        );

        let exists = self.exists(key).await?;

        match expected_etag {
            None => {
                // Write only if absent
                if exists {
                    return Err(StorageError::ConditionFailed);
                }
                self.put(key, data).await?;
                // No real etag in default impl, return placeholder
                Ok("\"default\"".to_string())
            }
            Some(_expected) => {
                // Write if expired (etag match) — default impl can't check etag
                // Just overwrite; backends should override for proper etag checking
                self.put(key, data).await?;
                Ok("\"default\"".to_string())
            }
        }
    }

    /// Get the etag (or equivalent identifier) of an object.
    /// Used for conditional operations (lease management).
    ///
    /// Default implementation: returns None (not supported).
    /// Storage backends should override for etag support.
    async fn get_etag(&self, _key: &str) -> StorageResult<Option<String>> {
        Ok(None)
    }
}

pub async fn create_storage(config: &crate::config::StorageConfig) -> StorageResult<Box<dyn StorageBackend>> {
    match config.backend.as_str() {
        "local" => {
            let path = config.local_path.as_ref()
                .ok_or_else(|| StorageError::InvalidArgument("local_path required".to_string()))?;
            Ok(Box::new(local::LocalStorage::new(path)?))
        }
        "s3" => {
            let bucket = config.s3_bucket.as_ref()
                .ok_or_else(|| StorageError::InvalidArgument("s3_bucket required".to_string()))?;
            Ok(Box::new(s3::S3Storage::new(
                bucket,
                config.s3_region.as_deref(),
                config.s3_endpoint.as_deref(),
            ).await?))
        }
        _ => Err(StorageError::InvalidArgument(format!("Unknown backend: {}", config.backend))),
    }
}
