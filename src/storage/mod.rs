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
