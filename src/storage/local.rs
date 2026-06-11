//! Local filesystem storage backend

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;

pub struct LocalStorage {
    base_path: PathBuf,
}

impl LocalStorage {
    pub fn new(base_path: &str) -> StorageResult<Self> {
        let path = PathBuf::from(base_path);
        Ok(Self { base_path: path })
    }

    /// Validate key and construct object path, preventing path traversal attacks.
    fn object_path(&self, key: &str) -> StorageResult<PathBuf> {
        // Reject absolute paths
        if key.starts_with('/') || key.starts_with('\\') {
            return Err(StorageError::InvalidArgument(
                format!("Invalid key: absolute path not allowed: {}", key)
            ));
        }

        // Check for null bytes
        if key.contains('\0') {
            return Err(StorageError::InvalidArgument(
                "Invalid key: contains null bytes".to_string()
            ));
        }

        // Check for empty key
        if key.is_empty() {
            return Err(StorageError::InvalidArgument(
                "Invalid key: empty key".to_string()
            ));
        }

        // Reject path traversal: check each path component for ".."
        // Split on both '/' and '\\' to handle Windows-style separators.
        // This is more precise than key.contains("..") which would also reject
        // legitimate filenames like "file..name" or "..hidden".
        for component in key.split(|c| c == '/' || c == '\\') {
            if component == ".." {
                return Err(StorageError::InvalidArgument(
                    format!("Invalid key: path traversal detected: {}", key)
                ));
            }
        }

        Ok(self.base_path.join(key))
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        let path = self.object_path(key)?;

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

    /// Store an object by moving a file from disk.
    /// Tries atomic rename first (zero-copy on same filesystem).
    /// Falls back to copy+delete on cross-filesystem.
    async fn put_from_path(&self, key: &str, source: &Path) -> StorageResult<()> {
        let dest = self.object_path(key)?;

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        // Try atomic rename first (same filesystem → zero-copy)
        match fs::rename(source, &dest).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // Cross-filesystem: copy then delete source
                fs::copy(source, &dest).await.map_err(|e| {
                    StorageError::Internal(format!(
                        "Failed to copy {} → {}: {}",
                        source.display(),
                        dest.display(),
                        e
                    ))
                })?;
                let _ = fs::remove_file(source).await;
                Ok(())
            }
        }
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        let path = self.object_path(key)?;

        // Directly attempt read; map NotFound errors (avoids TOCTOU race with exists())
        match fs::read(&path).await {
            Ok(data) => Ok(Bytes::from(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Internal(format!("Failed to read: {}", e))),
        }
    }

    async fn get_path(&self, key: &str) -> StorageResult<Option<PathBuf>> {
        let path = self.object_path(key)?;
        if path.exists() {
            Ok(Some(path))
        } else {
            Err(StorageError::NotFound(key.to_string()))
        }
    }

    async fn exists(&self, key: &str) -> StorageResult<bool> {
        let path = self.object_path(key)?;
        Ok(path.exists())
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        let path = self.object_path(key)?;

        // Directly attempt delete; ignore NotFound (avoids TOCTOU race with exists())
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Internal(format!("Failed to delete: {}", e))),
        }
    }
}
