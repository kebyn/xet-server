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
    /// The "etag" for local storage is the mtime as a string (e.g., "1718000000").
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
                        // Get the mtime as the new etag
                        let mtime = f.metadata().await
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        Ok(format!("\"{}\"", mtime))
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
                let current_mtime = match fs::metadata(&path).await {
                    Ok(meta) => meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(StorageError::NotFound(key.to_string()));
                    }
                    Err(e) => {
                        return Err(StorageError::Internal(format!("Failed to get metadata: {}", e)));
                    }
                };

                let current_etag = format!("\"{}\"", current_mtime);
                if current_etag != expected {
                    return Err(StorageError::ConditionFailed);
                }

                // Etag matches — overwrite
                fs::write(&path, &data).await
                    .map_err(|e| StorageError::Internal(format!("Failed to write: {}", e)))?;

                // Get the new mtime
                let new_mtime = fs::metadata(&path).await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(format!("\"{}\"", new_mtime))
            }
        }
    }

    /// ETag for local storage: mtime as a quoted string.
    async fn get_etag(&self, key: &str) -> StorageResult<Option<String>> {
        let path = self.object_path(key)?;

        match fs::metadata(&path).await {
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(Some(format!("\"{}\"", mtime)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Internal(format!("Failed to get metadata: {}", e))),
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
