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
        for component in key.split(['/', '\\']) {
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
        // Return the path without checking existence. The caller handles
        // missing files via File::open error, avoiding a TOCTOU race between
        // the exists() check and the subsequent open().
        let path = self.object_path(key)?;
        Ok(Some(path))
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

    async fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        let dir = self.base_path.join(prefix);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut keys = Vec::new();
        Self::walk_dir(&self.base_path, &dir, &mut keys).await?;
        Ok(keys)
    }

    async fn list_objects_with_mtime(&self, prefix: &str) -> StorageResult<Vec<(String, u64)>> {
        let dir = self.base_path.join(prefix);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries_with_mtime = Vec::new();
        Self::walk_dir_with_mtime(&self.base_path, &dir, &mut entries_with_mtime).await?;
        Ok(entries_with_mtime)
    }

    async fn get_mtime(&self, key: &str) -> StorageResult<u64> {
        let path = self.object_path(key)?;

        match fs::metadata(&path).await {
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(mtime)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Internal(format!("Failed to get metadata: {}", e))),
        }
    }

    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        let path = self.object_path(key)?;

        match fs::metadata(&path).await {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Internal(format!("Failed to get metadata: {}", e))),
        }
    }
}

impl LocalStorage {
    /// Recursively walk a directory, collecting keys relative to base_path.
    async fn walk_dir(
        base_path: &Path,
        dir: &Path,
        keys: &mut Vec<String>,
    ) -> StorageResult<()> {
        let mut entries = fs::read_dir(dir).await.map_err(|e| {
            StorageError::Internal(format!("Failed to read dir {}: {}", dir.display(), e))
        })?;

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            StorageError::Internal(format!("Failed to read dir entry: {}", e))
        })? {
            let path = entry.path();
            let file_type = entry.file_type().await.map_err(|e| {
                StorageError::Internal(format!("Failed to get file type: {}", e))
            })?;

            if file_type.is_dir() {
                Box::pin(Self::walk_dir(base_path, &path, keys)).await?;
            } else if file_type.is_file() {
                let key = path
                    .strip_prefix(base_path)
                    .map_err(|e| {
                        StorageError::Internal(format!(
                            "Failed to compute relative path: {}",
                            e
                        ))
                    })?
                    .to_string_lossy()
                    .to_string();
                keys.push(key);
            }
        }

        Ok(())
    }

    /// Recursively walk a directory, collecting (key, mtime_unix_seconds) pairs.
    async fn walk_dir_with_mtime(
        base_path: &Path,
        dir: &Path,
        entries_with_mtime: &mut Vec<(String, u64)>,
    ) -> StorageResult<()> {
        let mut entries = fs::read_dir(dir).await.map_err(|e| {
            StorageError::Internal(format!("Failed to read dir {}: {}", dir.display(), e))
        })?;

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            StorageError::Internal(format!("Failed to read dir entry: {}", e))
        })? {
            let path = entry.path();
            let file_type = entry.file_type().await.map_err(|e| {
                StorageError::Internal(format!("Failed to get file type: {}", e))
            })?;

            if file_type.is_dir() {
                Box::pin(Self::walk_dir_with_mtime(base_path, &path, entries_with_mtime)).await?;
            } else if file_type.is_file() {
                let key = path
                    .strip_prefix(base_path)
                    .map_err(|e| {
                        StorageError::Internal(format!(
                            "Failed to compute relative path: {}",
                            e
                        ))
                    })?
                    .to_string_lossy()
                    .to_string();

                // Get modification time
                let mtime = match fs::metadata(&path).await {
                    Ok(meta) => {
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    }
                    Err(_) => 0,
                };

                entries_with_mtime.push((key, mtime));
            }
        }

        Ok(())
    }
}
