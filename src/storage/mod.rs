//! Storage abstraction layer for Xet Storage server

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod local;
pub mod s3;

#[cfg(test)]
pub(crate) mod flaky;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Storage error: {message}")]
    Internal {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
}

impl StorageError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            source: None,
        }
    }

    pub fn internal_with_source<E>(message: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        let message = message.into();
        // The source's Display text is embedded in `message` so that `StorageError`'s
        // own Display (via thiserror) is self-contained. It is also stored as the
        // `#[source]` field so that chain-aware reporters can walk it. This means
        // tools like `anyhow`'s `{:#}` will show the cause twice. That duplication
        // is intentional: simpler Display output is worth it for this codebase.
        Self::Internal {
            message: format!("{message}: {source}"),
            source: Some(Box::new(source)),
        }
    }
}

pub type StorageResult<T> = Result<T, StorageError>;
pub type ObjectKeyStream<'a> = futures_util::stream::BoxStream<'a, StorageResult<String>>;

#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Check whether the storage backend is reachable enough for the server to
    /// accept traffic. Implementations should keep this lightweight.
    async fn health_check(&self) -> StorageResult<()> {
        Ok(())
    }

    /// Store an object
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()>;

    /// Store an object from a file on disk.
    ///
    /// Implementations may move the source path or leave it intact. The caller
    /// retains cleanup ownership and must tolerate either behavior.
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
        let data = tokio::fs::read(path).await.map_err(|e| {
            StorageError::internal_with_source(format!("Failed to read file {}", path.display()), e)
        })?;
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

    /// Stream object keys matching a prefix without requiring callers to retain
    /// the complete listing. Production backends should override this method;
    /// the compatibility implementation delegates to `list_objects`.
    fn list_objects_stream<'a>(&'a self, prefix: &'a str) -> ObjectKeyStream<'a> {
        stream::once(async move { self.list_objects(prefix).await })
            .map_ok(|keys| stream::iter(keys.into_iter().map(Ok)))
            .try_flatten()
            .boxed()
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
    /// This enables bounded-memory downloads for the conversion pipeline and xorb downloads.
    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
        tracing::warn!(
            "download_to_path using default (non-streaming) implementation for key={}; \
             this reads the entire object into RAM. Override download_to_path in your \
             StorageBackend implementation for streaming support.",
            key
        );
        let data = self.get(key).await?;
        tokio::fs::write(dest, &data).await.map_err(|e| {
            StorageError::internal_with_source(format!("Failed to write to {}", dest.display()), e)
        })?;
        Ok(())
    }
}

/// Bounded retry delays for transient storage errors: 100 ms after the
/// first attempt, 500 ms after the second. Storage errors are ambiguous
/// between transient blips (network) and permanent failures; the small
/// bound makes a misclassification cost ~600 ms.
pub(crate) const TRANSIENT_STORAGE_RETRY_DELAYS: &[std::time::Duration] = &[
    std::time::Duration::from_millis(100),
    std::time::Duration::from_millis(500),
];

/// A storage failure is worth retrying unless the object is simply missing —
/// retrying an absent key only burns the delay budget.
pub(crate) fn is_transient_storage_error(error: &StorageError) -> bool {
    !matches!(error, StorageError::NotFound(_))
}

/// Download `key` to `dest` with bounded retries for transient storage
/// errors.
///
/// `dest` is reused across attempts: backends stage through a unique temp
/// file and rename (or overwrite the destination), so a retry never observes
/// a partial previous attempt.
pub(crate) async fn download_to_path_with_retries(
    storage: &dyn StorageBackend,
    key: &str,
    dest: &Path,
) -> StorageResult<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match storage.download_to_path(key, dest).await {
            Ok(()) => return Ok(()),
            Err(error)
                if attempt <= TRANSIENT_STORAGE_RETRY_DELAYS.len()
                    && is_transient_storage_error(&error) =>
            {
                let delay = TRANSIENT_STORAGE_RETRY_DELAYS[attempt - 1];
                tracing::warn!(
                    "Transient storage error downloading {} (attempt {}/{}): {}; retrying in {:?}",
                    key,
                    attempt,
                    TRANSIENT_STORAGE_RETRY_DELAYS.len() + 1,
                    error,
                    delay
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

pub async fn create_storage(
    config: &crate::config::StorageConfig,
) -> StorageResult<Box<dyn StorageBackend>> {
    match config.backend.as_str() {
        "local" => {
            let path = config
                .local_path
                .as_ref()
                .ok_or_else(|| StorageError::InvalidArgument("local_path required".to_string()))?;
            Ok(Box::new(local::LocalStorage::new(path)?))
        }
        "s3" => {
            let bucket = config
                .s3_bucket
                .as_ref()
                .ok_or_else(|| StorageError::InvalidArgument("s3_bucket required".to_string()))?;
            Ok(Box::new(
                s3::S3Storage::new(
                    bucket,
                    config.s3_region.as_deref(),
                    config.s3_endpoint.as_deref(),
                )
                .await?,
            ))
        }
        _ => Err(StorageError::InvalidArgument(format!(
            "Unknown backend: {}",
            config.backend
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::download_to_path_with_retries;
    use super::flaky::{FlakyStorage, InjectedFailure};
    use super::{StorageBackend, StorageError};
    use crate::storage::local::LocalStorage;
    use bytes::Bytes;
    use std::error::Error;
    use tempfile::tempdir;

    #[test]
    fn internal_error_has_no_source() {
        let error = StorageError::internal("quota exceeded");
        assert_eq!(error.to_string(), "Storage error: quota exceeded");
        assert!(error.source().is_none(), "internal() should have no source");
    }

    #[test]
    fn internal_error_preserves_source_chain() {
        let source = std::io::Error::other("disk failure");
        let error = StorageError::internal_with_source("write failed", source);

        assert_eq!(
            error.to_string(),
            "Storage error: write failed: disk failure"
        );
        assert_eq!(
            error
                .source()
                .expect("source should be preserved")
                .to_string(),
            "disk failure"
        );
    }

    #[tokio::test]
    async fn download_to_path_with_retries_recovers_after_transient_failure() {
        let dir = tempdir().unwrap();
        let inner = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        inner
            .put("obj", Bytes::from_static(b"payload"))
            .await
            .unwrap();

        let (flaky, attempts) =
            FlakyStorage::new(inner, "obj".to_string(), InjectedFailure::Transient, 1);
        let dest = dir.path().join("downloaded");
        download_to_path_with_retries(&flaky, "obj", &dest)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn download_to_path_with_retries_does_not_retry_missing() {
        let dir = tempdir().unwrap();
        let inner = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();

        let (flaky, attempts) = FlakyStorage::new(
            inner,
            "obj".to_string(),
            InjectedFailure::Missing,
            usize::MAX,
        );
        let dest = dir.path().join("never");
        assert!(matches!(
            download_to_path_with_retries(&flaky, "obj", &dest).await,
            Err(StorageError::NotFound(_))
        ));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
