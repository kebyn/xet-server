//! Local filesystem storage backend

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;

/// 跨文件系统安全拷贝:先 copy 到临时文件,再原子 rename 到最终路径。
/// 避免中断时在最终 key 留下截断文件。
async fn copy_then_rename(source: &Path, dest: &Path) -> StorageResult<()> {
    let temp_dest = dest.with_extension("tmp");
    fs::copy(source, &temp_dest).await.map_err(|e| {
        StorageError::Internal(format!(
            "Failed to copy {} → {}: {}", source.display(), temp_dest.display(), e
        ))
    })?;
    fs::rename(&temp_dest, dest).await.map_err(|e| {
        let _ = std::fs::remove_file(&temp_dest);
        StorageError::Internal(format!(
            "Failed to rename {} → {}: {}", temp_dest.display(), dest.display(), e
        ))
    })?;
    Ok(())
}

pub struct LocalStorage {
    base_path: PathBuf,
}

impl LocalStorage {
    pub fn new(base_path: &str) -> StorageResult<Self> {
        let path = PathBuf::from(base_path);
        Ok(Self { base_path: path })
    }

    /// C2 fix: Compute etag from file metadata using nanosecond-precision mtime.
    /// Nanosecond precision dramatically reduces the race window for lease coordination
    /// compared to second-level precision. Most Linux filesystems (ext4, xfs, btrfs)
    /// support nanosecond timestamps.
    fn file_etag_from_metadata(meta: &Option<std::fs::Metadata>) -> String {
        let nanos = meta.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("\"{}\"", nanos)
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
        // 经由原子写(temp + rename),避免崩溃时留下截断文件。
        self.put_atomic(key, data).await
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

    /// Paged listing for local storage: sort all keys, skip after cursor, take page_size.
    ///
    /// For local filesystem, all keys are read upfront (walk_dir), then sliced.
    /// This is less efficient than server-side pagination but correct for checkpoint resumption.
    async fn list_objects_paged(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        page_size: usize,
    ) -> StorageResult<(Vec<String>, Option<String>, bool)> {
        let all_keys = self.list_objects(prefix).await?;

        // Sort for deterministic pagination
        let mut sorted_keys = all_keys;
        sorted_keys.sort();

        // Filter keys after the cursor
        let filtered: Vec<String> = if let Some(cursor) = start_after {
            sorted_keys.into_iter().filter(|k| k.as_str() > cursor).collect()
        } else {
            sorted_keys
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

    /// Conditional PUT for local storage using filesystem atomic operations.
    ///
    /// - expected_etag = None: write only if key absent (create-exclusive)
    /// - expected_etag = Some(mtime_str): write only if current mtime matches
    ///
    /// Uses a lock file for atomicity on the local filesystem.
    /// The "etag" for local storage is the mtime in nanoseconds as a quoted string.
    ///
    /// NOTE: Local storage lease coordination is only safe for single-node deployments.
    /// For multi-node GC, use S3 storage which provides true conditional operations via ETags.
    async fn put_if_absent_or_expired(
        &self,
        key: &str,
        data: Bytes,
        expected_etag: Option<&str>,
    ) -> StorageResult<String> {
        let path = self.object_path(key)?;

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        match expected_etag {
            None => {
                // Write only if absent
                // Use OpenOptions with create_new(true) for atomic create-exclusive
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .await;

                match file {
                    Ok(mut f) => {
                        use tokio::io::AsyncWriteExt;
                        f.write_all(&data).await
                            .map_err(|e| StorageError::Internal(format!("Failed to write: {}", e)))?;
                        let etag = Self::file_etag_from_metadata(&f.metadata().await.ok());
                        Ok(etag)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Err(StorageError::ConditionFailed)
                    }
                    Err(e) => {
                        Err(StorageError::Internal(format!("Failed to create file: {}", e)))
                    }
                }
            }
            Some(expected) => {
                // Write only if current mtime matches expected etag
                let current_etag = match fs::metadata(&path).await {
                    Ok(meta) => Self::file_etag_from_metadata(&Some(meta)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(StorageError::NotFound(key.to_string()));
                    }
                    Err(e) => {
                        return Err(StorageError::Internal(format!("Failed to get metadata: {}", e)));
                    }
                };

                if current_etag != expected {
                    return Err(StorageError::ConditionFailed);
                }

                // Etag matches — overwrite
                fs::write(&path, &data).await
                    .map_err(|e| StorageError::Internal(format!("Failed to write: {}", e)))?;

                // Get the new etag
                let new_etag = match fs::metadata(&path).await {
                    Ok(meta) => Self::file_etag_from_metadata(&Some(meta)),
                    _ => "\"0\"".to_string(),
                };
                Ok(new_etag)
            }
        }
    }

    /// ETag for local storage: mtime in nanoseconds as a quoted string.
    /// C2 fix: Uses nanosecond precision to minimize race window in lease coordination.
    async fn get_etag(&self, key: &str) -> StorageResult<Option<String>> {
        let path = self.object_path(key)?;

        match fs::metadata(&path).await {
            Ok(meta) => {
                Ok(Some(Self::file_etag_from_metadata(&Some(meta))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Internal(format!("Failed to get metadata: {}", e))),
        }
    }

    /// M4 fix: Atomic write using write-to-temp + rename pattern.
    ///
    /// Writes data to `{key}.tmp` first, then atomically renames to `{key}`.
    /// This prevents partial writes from corrupting files during crashes.
    /// The rename operation is atomic on POSIX filesystems.
    async fn put_atomic(&self, key: &str, data: Bytes) -> StorageResult<()> {
        let path = self.object_path(key)?;
        let temp_path = path.with_extension("tmp");

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| StorageError::Internal(format!("Failed to create dirs: {}", e)))?;
        }

        // Write to temp file
        fs::write(&temp_path, &data).await
            .map_err(|e| StorageError::Internal(format!("Failed to write temp file: {}", e)))?;

        // Atomic rename
        fs::rename(&temp_path, &path).await
            .map_err(|e| {
                // Best-effort cleanup of temp file
                let _ = std::fs::remove_file(&temp_path);
                StorageError::Internal(format!("Failed to rename temp to final: {}", e))
            })?;

        Ok(())
    }

    /// I1 fix: Conditional delete using etag (mtime) check.
    ///
    /// Only deletes the file if its current mtime matches the expected etag.
    /// Uses a two-step check-then-delete pattern (not truly atomic on local fs,
    /// but minimizes the race window compared to unconditional delete).
    async fn delete_if_match(&self, key: &str, expected_etag: &str) -> StorageResult<()> {
        let path = self.object_path(key)?;

        // Get current mtime
        let current_etag = match self.get_etag(key).await? {
            Some(etag) => etag,
            None => return Ok(()), // File already gone
        };

        if current_etag != expected_etag {
            return Err(StorageError::ConditionFailed);
        }

        // Delete (small race window between check and delete)
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // Idempotent
            Err(e) => Err(StorageError::Internal(format!("Failed to delete: {}", e))),
        }
    }
}

impl LocalStorage {
    /// M1: Recursively walk a directory, collecting entries with optional mtime.
    /// Single implementation to reduce code duplication.
    async fn walk_dir_impl(
        base_path: &Path,
        dir: &Path,
        include_mtime: bool,
    ) -> StorageResult<Vec<(String, Option<u64>)>> {
        let mut results = Vec::new();
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
                let sub_results = Box::pin(Self::walk_dir_impl(base_path, &path, include_mtime)).await?;
                results.extend(sub_results);
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

                let mtime = if include_mtime {
                    match fs::metadata(&path).await {
                        Ok(meta) => meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs()),
                        // M6 fix: Use current time instead of 0 to prevent GC from incorrectly
                        // deleting files with unreadable metadata. If we use 0, the grace period
                        // check (now - mtime > grace) would think the file is extremely old.
                        Err(_) => Some(std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)),
                    }
                } else {
                    None
                };

                results.push((key, mtime));
            }
        }

        Ok(results)
    }

    /// Recursively walk a directory, collecting keys relative to base_path.
    async fn walk_dir(
        base_path: &Path,
        dir: &Path,
        keys: &mut Vec<String>,
    ) -> StorageResult<()> {
        let entries = Self::walk_dir_impl(base_path, dir, false).await?;
        keys.extend(entries.into_iter().map(|(key, _)| key));
        Ok(())
    }

    /// Recursively walk a directory, collecting (key, mtime_unix_seconds) pairs.
    async fn walk_dir_with_mtime(
        base_path: &Path,
        dir: &Path,
        entries_with_mtime: &mut Vec<(String, u64)>,
    ) -> StorageResult<()> {
        let entries = Self::walk_dir_impl(base_path, dir, true).await?;
        // M6 fix: Use current time instead of 0 for missing mtime to prevent GC from
        // incorrectly deleting files. If mtime is 0, grace period check would think
        // the file is extremely old and delete it.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries_with_mtime.extend(entries.into_iter().map(|(key, mtime)| (key, mtime.unwrap_or(now))));
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
        tokio::fs::create_dir_all(dest.parent().unwrap()).await.unwrap();
        tokio::fs::write(&src, b"payload").await.unwrap();

        copy_then_rename(&src, &dest).await.unwrap();

        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"payload");
        // 不留下 .tmp 中间文件
        assert!(!dest.with_extension("tmp").exists());
    }

    #[tokio::test]
    async fn test_put_is_atomic_no_temp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        store.put("xorbs/abc", Bytes::from_static(b"data")).await.unwrap();
        assert_eq!(store.get("xorbs/abc").await.unwrap(), Bytes::from_static(b"data"));
        // 原子写不应残留 .tmp
        assert!(!dir.path().join("xorbs/abc.tmp").exists());
    }
}
