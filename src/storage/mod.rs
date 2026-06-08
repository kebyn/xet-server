//! Storage abstraction layer for Xet Storage server

use async_trait::async_trait;
use bytes::Bytes;
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

    /// Retrieve an object
    async fn get(&self, key: &str) -> StorageResult<Bytes>;

    /// Check if object exists
    async fn exists(&self, key: &str) -> StorageResult<bool>;

    /// Delete an object
    async fn delete(&self, key: &str) -> StorageResult<()>;
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
