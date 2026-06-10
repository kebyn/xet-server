//! RAII temporary file for streaming uploads.
//!
//! Ensures cleanup on all error paths: when the `TempFile` is dropped without
//! being persisted, the underlying file is removed from disk.

use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::storage::{StorageError, StorageResult};

/// A temporary file that auto-cleans on drop.
///
/// Usage:
/// 1. `TempFile::create(dir)` — creates a temp file in `dir`
/// 2. `write_all()` — stream data into the file
/// 3. `sync_all()` — fsync before persist
/// 4. `persist(dest)` — atomic rename to final location (consumes self)
///
/// If dropped without `persist()`, the temp file is removed.
pub struct TempFile {
    path: PathBuf,
    file: Option<fs::File>,
}

impl TempFile {
    /// Create a new temp file in the given directory.
    /// Creates the directory if it doesn't exist.
    pub async fn create(temp_dir: &Path) -> StorageResult<Self> {
        fs::create_dir_all(temp_dir).await.map_err(|e| {
            StorageError::Internal(format!(
                "Failed to create temp dir {}: {}",
                temp_dir.display(),
                e
            ))
        })?;

        // Generate a unique filename using timestamp + random suffix
        let unique_id = format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            // Use a simple counter-based approach since we don't have a UUID dep
            {
                use std::sync::atomic::{AtomicU64, Ordering};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                COUNTER.fetch_add(1, Ordering::Relaxed)
            }
        );

        let path = temp_dir.join(format!("upload-{}.tmp", unique_id));
        let file = fs::File::create(&path).await.map_err(|e| {
            StorageError::Internal(format!(
                "Failed to create temp file {}: {}",
                path.display(),
                e
            ))
        })?;

        Ok(Self {
            path,
            file: Some(file),
        })
    }

    /// Get the path to the temp file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write data to the temp file.
    pub async fn write_all(&mut self, data: &[u8]) -> StorageResult<()> {
        let file = self.file.as_mut().ok_or_else(|| {
            StorageError::Internal("TempFile already persisted or consumed".to_string())
        })?;
        file.write_all(data).await.map_err(|e| {
            StorageError::Internal(format!("Failed to write to temp file: {}", e))
        })
    }

    /// Flush and fsync the temp file to disk.
    /// Must be called before `persist()` to ensure data durability.
    pub async fn sync_all(&mut self) -> StorageResult<()> {
        let file = self.file.as_mut().ok_or_else(|| {
            StorageError::Internal("TempFile already persisted or consumed".to_string())
        })?;
        file.flush().await.map_err(|e| {
            StorageError::Internal(format!("Failed to flush temp file: {}", e))
        })?;
        file.sync_all().await.map_err(|e| {
            StorageError::Internal(format!("Failed to fsync temp file: {}", e))
        })
    }

    /// Consume the TempFile and return its path, without cleanup.
    /// The caller takes responsibility for the file.
    pub fn into_path(mut self) -> PathBuf {
        // Close the file handle
        self.file.take();
        let path = self.path.clone();
        std::mem::forget(self);
        path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Close file handle if still open
        self.file.take();

        let path = self.path.clone();
        // Use spawn_blocking to avoid blocking the async runtime in Drop.
        // If the runtime is shutting down, the spawn may succeed but the
        // blocking pool may not drain before exit — the file may remain on
        // disk. This is acceptable for temp files; OS temp cleaners or the
        // next run will handle orphaned files. Fall back to sync remove if
        // no runtime is available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _ = std::fs::remove_file(&path);
            });
        } else {
            // No runtime available (e.g., outside async context) — sync cleanup
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_temp_file_create_and_cleanup() {
        let dir = tempdir().unwrap();
        let path;
        {
            let mut tf = TempFile::create(dir.path()).await.unwrap();
            tf.write_all(b"hello world").await.unwrap();
            path = tf.path().to_path_buf();
            assert!(path.exists());
            // Drop without persist — file should be cleaned up
        }
        // Give spawn_blocking a moment to execute
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!path.exists(), "Temp file should be cleaned up on drop");
    }

    #[tokio::test]
    async fn test_temp_file_into_path_persists() {
        let dir = tempdir().unwrap();
        let mut tf = TempFile::create(dir.path()).await.unwrap();
        tf.write_all(b"persist me").await.unwrap();
        let path = tf.into_path();
        assert!(path.exists(), "File should persist after into_path");
        // Clean up manually
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_temp_file_write_multiple() {
        let dir = tempdir().unwrap();
        let mut tf = TempFile::create(dir.path()).await.unwrap();
        tf.write_all(b"hello ").await.unwrap();
        tf.write_all(b"world").await.unwrap();
        tf.sync_all().await.unwrap();
        let path = tf.into_path();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "hello world");
        let _ = std::fs::remove_file(&path);
    }
}
