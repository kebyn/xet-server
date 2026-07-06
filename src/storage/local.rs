//! Local filesystem storage backend

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;

/// 跨文件系统安全拷贝:先 copy 到临时文件,再原子 rename 到最终路径。
/// 避免中断时在最终 key 留下截断文件。
async fn copy_then_rename(source: &Path, dest: &Path) -> StorageResult<()> {
    let temp_dest = unique_temp_path(dest);
    fs::copy(source, &temp_dest).await.map_err(|e| {
        StorageError::Internal(format!(
            "Failed to copy {} → {}: {}",
            source.display(),
            temp_dest.display(),
            e
        ))
    })?;
    fs::rename(&temp_dest, dest).await.map_err(|e| {
        let _ = std::fs::remove_file(&temp_dest);
        StorageError::Internal(format!(
            "Failed to rename {} → {}: {}",
            temp_dest.display(),
            dest.display(),
            e
        ))
    })?;
    Ok(())
}

fn unique_temp_path(dest: &Path) -> PathBuf {
    dest.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()))
}

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
            return Err(StorageError::InvalidArgument(format!(
                "Invalid key: absolute path not allowed: {}",
                key
            )));
        }

        // Check for null bytes
        if key.contains('\0') {
            return Err(StorageError::InvalidArgument(
                "Invalid key: contains null bytes".to_string(),
            ));
        }

        // Check for empty key
        if key.is_empty() {
            return Err(StorageError::InvalidArgument(
                "Invalid key: empty key".to_string(),
            ));
        }

        // Reject path traversal: check each path component for ".."
        // Split on both '/' and '\\' to handle Windows-style separators.
        // This is more precise than key.contains("..") which would also reject
        // legitimate filenames like "file..name" or "..hidden".
        for component in key.split(['/', '\\']) {
            if component == ".." {
                return Err(StorageError::InvalidArgument(format!(
                    "Invalid key: path traversal detected: {}",
                    key
                )));
            }
        }

        Ok(self.base_path.join(key))
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn health_check(&self) -> StorageResult<()> {
        match fs::metadata(&self.base_path).await {
            Ok(meta) if meta.is_dir() => Ok(()),
            Ok(_) => Err(StorageError::Internal(format!(
                "Local storage path is not a directory: {}",
                self.base_path.display()
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = self.base_path.parent().unwrap_or_else(|| Path::new("."));
                match fs::metadata(parent).await {
                    Ok(meta) if meta.is_dir() => Ok(()),
                    Ok(_) => Err(StorageError::Internal(format!(
                        "Local storage parent is not a directory: {}",
                        parent.display()
                    ))),
                    Err(e) => Err(StorageError::Internal(format!(
                        "Local storage path is not accessible: {}",
                        e
                    ))),
                }
            }
            Err(e) => Err(StorageError::Internal(format!(
                "Local storage path is not accessible: {}",
                e
            ))),
        }
    }

    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        // 原子写:先写入临时文件,再 rename 到最终路径,避免崩溃时留下截断文件。
        let path = self.object_path(key)?;
        let temp_path = unique_temp_path(&path);

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        // Write to temp file
        fs::write(&temp_path, &data)
            .await
            .map_err(|e| StorageError::Internal(format!("Failed to write temp file: {}", e)))?;

        // Atomic rename
        fs::rename(&temp_path, &path).await.map_err(|e| {
            // Best-effort cleanup of temp file
            let _ = std::fs::remove_file(&temp_path);
            StorageError::Internal(format!("Failed to rename temp to final: {}", e))
        })?;

        Ok(())
    }

    /// Store an object by moving a file from disk.
    /// Tries atomic rename first (zero-copy on same filesystem).
    /// Falls back to copy+delete on cross-filesystem.
    async fn put_from_path(&self, key: &str, source: &Path) -> StorageResult<()> {
        let dest = self.object_path(key)?;

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        // Try atomic rename first (same filesystem → zero-copy)
        match fs::rename(source, &dest).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // Cross-filesystem: copy to temp + rename (atomic), then delete source.
                copy_then_rename(source, &dest).await?;
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

    /// Download a local object to `dest` without routing through `get()`.
    ///
    /// This keeps shard validation bounded-memory for the default local backend:
    /// the object is copied by the filesystem into a temp file and then renamed
    /// into place, rather than being read fully into RAM.
    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
        let source = self.object_path(key)?;

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        match fs::metadata(&source).await {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                return Err(StorageError::Internal(format!(
                    "Object path is not a file: {}",
                    source.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(key.to_string()));
            }
            Err(e) => {
                return Err(StorageError::Internal(format!(
                    "Failed to stat object {}: {}",
                    source.display(),
                    e
                )));
            }
        }

        copy_then_rename(&source, dest).await
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

    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        let path = self.object_path(key)?;

        match fs::metadata(&path).await {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Internal(format!(
                "Failed to get metadata: {}",
                e
            ))),
        }
    }
}

impl LocalStorage {
    /// Recursively walk a directory, collecting keys relative to base_path.
    async fn walk_dir(base_path: &Path, dir: &Path, keys: &mut Vec<String>) -> StorageResult<()> {
        let mut entries = fs::read_dir(dir).await.map_err(|e| {
            StorageError::Internal(format!("Failed to read dir {}: {}", dir.display(), e))
        })?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StorageError::Internal(format!("Failed to read dir entry: {}", e)))?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| StorageError::Internal(format!("Failed to get file type: {}", e)))?;

            if file_type.is_dir() {
                Box::pin(Self::walk_dir(base_path, &path, keys)).await?;
            } else if file_type.is_file() {
                let key = path
                    .strip_prefix(base_path)
                    .map_err(|e| {
                        StorageError::Internal(format!("Failed to compute relative path: {}", e))
                    })?
                    .to_string_lossy()
                    .to_string();
                keys.push(key);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_copy_then_rename_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("sub/dest.bin");
        tokio::fs::create_dir_all(dest.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&src, b"payload").await.unwrap();

        copy_then_rename(&src, &dest).await.unwrap();

        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"payload");
        // 不留下 .tmp 中间文件
        assert_no_tmp_files(dest.parent().unwrap());
    }

    #[tokio::test]
    async fn test_put_is_atomic_no_temp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        store
            .put("xorbs/abc", Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert_eq!(
            store.get("xorbs/abc").await.unwrap(),
            Bytes::from_static(b"data")
        );
        // 原子写不应残留 .tmp
        assert_no_tmp_files(&dir.path().join("xorbs"));
    }

    #[tokio::test]
    async fn test_concurrent_put_same_key_uses_independent_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let key = "objects/shared.bin";
        let data = Bytes::from_static(b"same content");

        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let data = data.clone();
            handles.push(tokio::spawn(async move { store.put(key, data).await }));
        }

        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        assert_eq!(store.get(key).await.unwrap(), data);
        assert_no_tmp_files(&dir.path().join("objects"));
    }

    #[tokio::test]
    async fn test_download_to_path_creates_parent_and_copies_without_tmp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().join("store").to_str().unwrap()).unwrap();
        let data = Bytes::from_static(b"download without default get allocation");
        store.put("xorbs/object", data.clone()).await.unwrap();

        let dest = dir.path().join("downloads/nested/object.bin");
        store.download_to_path("xorbs/object", &dest).await.unwrap();

        assert_eq!(tokio::fs::read(&dest).await.unwrap(), data);
        assert_no_tmp_files(dest.parent().unwrap());
    }

    fn assert_no_tmp_files(dir: &Path) {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
