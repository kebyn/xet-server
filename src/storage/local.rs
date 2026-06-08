//! Local filesystem storage backend

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::PathBuf;
use tokio::fs;

pub struct LocalStorage {
    base_path: PathBuf,
}

impl LocalStorage {
    pub fn new(base_path: &str) -> StorageResult<Self> {
        let path = PathBuf::from(base_path);
        Ok(Self { base_path: path })
    }

    fn object_path(&self, key: &str) -> PathBuf {
        self.base_path.join(key)
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        let path = self.object_path(key);

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        // Write file
        fs::write(&path, &data).await
            .map_err(|e| StorageError::Internal(format!("Failed to write: {}", e)))?;

        Ok(())
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        let path = self.object_path(key);

        if !path.exists() {
            return Err(StorageError::NotFound(key.to_string()));
        }

        let data = fs::read(&path).await
            .map_err(|e| StorageError::Internal(format!("Failed to read: {}", e)))?;

        Ok(Bytes::from(data))
    }

    async fn exists(&self, key: &str) -> StorageResult<bool> {
        let path = self.object_path(key);
        Ok(path.exists())
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        let path = self.object_path(key);

        if path.exists() {
            fs::remove_file(&path).await
                .map_err(|e| StorageError::Internal(format!("Failed to delete: {}", e)))?;
        }

        Ok(())
    }
}
