use std::path::{Path, PathBuf};

use crate::error::XetError;
use crate::format::shard::MDBShardFile;
use crate::storage::{StorageBackend, StorageError};
use crate::util::TempPathGuard;

#[derive(Debug)]
pub enum ShardIoError {
    Storage { key: String, source: StorageError },
    TempIo(String),
    Parse { key: String, source: XetError },
    ParseTask { key: String, message: String },
}

impl std::fmt::Display for ShardIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage { key, source } => {
                write!(formatter, "failed to fetch shard {}: {}", key, source)
            }
            Self::TempIo(message) => formatter.write_str(message),
            Self::Parse { key, source } => {
                write!(formatter, "failed to parse shard {}: {}", key, source)
            }
            Self::ParseTask { key, message } => {
                write!(
                    formatter,
                    "shard parse task failed for {}: {}",
                    key, message
                )
            }
        }
    }
}

impl std::error::Error for ShardIoError {}

/// Resolve a shard to a local path and parse it without loading the complete
/// object into memory. File-backed storage is parsed in place; other backends
/// stream into an automatically cleaned temporary file.
pub async fn parse_shard_from_storage(
    storage: &dyn StorageBackend,
    key: &str,
    temp_dir: &Path,
) -> Result<MDBShardFile, ShardIoError> {
    // Bounded retry for transient storage errors, so every caller (index
    // rebuild, request-path shard re-validation, reconstruction) survives a
    // transient network blip at a cost of at most ~600 ms. The inner
    // download_to_path deliberately has no retry of its own: this loop
    // already retries a transient download failure, and nesting would
    // multiply the attempt count.
    let mut attempt = 0;
    loop {
        attempt += 1;
        match parse_shard_once(storage, key, temp_dir).await {
            Ok(shard) => return Ok(shard),
            Err(error)
                if attempt <= crate::storage::TRANSIENT_STORAGE_RETRY_DELAYS.len()
                    && is_retryable_shard_fetch_error(&error) =>
            {
                let delay = crate::storage::TRANSIENT_STORAGE_RETRY_DELAYS[attempt - 1];
                tracing::warn!(
                    "Transient storage error fetching shard {} (attempt {}/{}): {}; retrying in {:?}",
                    key,
                    attempt,
                    crate::storage::TRANSIENT_STORAGE_RETRY_DELAYS.len() + 1,
                    error,
                    delay
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// A shard fetch is worth retrying when the storage layer failed for a
/// reason other than the object being missing.
fn is_retryable_shard_fetch_error(error: &ShardIoError) -> bool {
    matches!(
        error,
        ShardIoError::Storage { source, .. }
            if crate::storage::is_transient_storage_error(source)
    )
}

async fn parse_shard_once(
    storage: &dyn StorageBackend,
    key: &str,
    temp_dir: &Path,
) -> Result<MDBShardFile, ShardIoError> {
    let direct_path = storage
        .get_path(key)
        .await
        .map_err(|source| ShardIoError::Storage {
            key: key.to_string(),
            source,
        })?;

    let mut temp_guard = None;
    let path = match direct_path {
        Some(path) => path,
        None => {
            tokio::fs::create_dir_all(temp_dir).await.map_err(|error| {
                ShardIoError::TempIo(format!(
                    "failed to create shard temp directory {}: {}",
                    temp_dir.display(),
                    error
                ))
            })?;
            let guard =
                TempPathGuard::new(temp_dir.join(format!("shard-{}.tmp", uuid::Uuid::new_v4())));
            storage
                .download_to_path(key, guard.try_path().map_err(ShardIoError::TempIo)?)
                .await
                .map_err(|source| ShardIoError::Storage {
                    key: key.to_string(),
                    source,
                })?;
            let path = guard
                .try_path()
                .map_err(ShardIoError::TempIo)?
                .to_path_buf();
            temp_guard = Some(guard);
            path
        }
    };

    let parse_key = key.to_string();
    let parsed = parse_path(path, parse_key).await;
    drop(temp_guard);
    parsed
}

async fn parse_path(path: PathBuf, key: String) -> Result<MDBShardFile, ShardIoError> {
    let parse_key = key.clone();
    tokio::task::spawn_blocking(move || MDBShardFile::parse_from_file(&path))
        .await
        .map_err(|error| ShardIoError::ParseTask {
            key: key.clone(),
            message: error.to_string(),
        })?
        .map_err(|source| ShardIoError::Parse {
            key: parse_key,
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::tempdir;

    use crate::format::shard_builder::ShardBuilder;
    use crate::storage::StorageResult;

    struct StreamingOnlyStorage {
        data: Bytes,
        get_called: AtomicBool,
        download_called: AtomicBool,
    }

    #[async_trait]
    impl StorageBackend for StreamingOnlyStorage {
        async fn put(&self, _key: &str, _data: Bytes) -> StorageResult<()> {
            Ok(())
        }

        async fn get(&self, _key: &str) -> StorageResult<Bytes> {
            self.get_called.store(true, Ordering::SeqCst);
            Err(StorageError::internal(
                "get must not be used for shard parsing",
            ))
        }

        async fn exists(&self, _key: &str) -> StorageResult<bool> {
            Ok(true)
        }

        async fn delete(&self, _key: &str) -> StorageResult<()> {
            Ok(())
        }

        async fn download_to_path(&self, _key: &str, dest: &Path) -> StorageResult<()> {
            self.download_called.store(true, Ordering::SeqCst);
            tokio::fs::write(dest, &self.data).await.map_err(|error| {
                StorageError::internal_with_source("failed to write shard download", error)
            })
        }
    }

    #[tokio::test]
    async fn remote_storage_uses_streaming_download_and_cleans_temp_file() {
        let data = Bytes::from(ShardBuilder::new().build().unwrap());
        let storage = StreamingOnlyStorage {
            data: data.clone(),
            get_called: AtomicBool::new(false),
            download_called: AtomicBool::new(false),
        };
        let dir = tempdir().unwrap();

        let shard = parse_shard_from_storage(&storage, "shards/test", dir.path())
            .await
            .unwrap();

        assert_eq!(
            shard.compute_hash(),
            crate::hash::compute_data_hash(&data).to_hex()
        );
        assert!(storage.download_called.load(Ordering::SeqCst));
        assert!(!storage.get_called.load(Ordering::SeqCst));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn parse_retries_transient_storage_error_then_succeeds() {
        use crate::storage::flaky::{FlakyStorage, InjectedFailure};
        use crate::storage::local::LocalStorage;

        let dir = tempdir().unwrap();
        let inner = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        let data = ShardBuilder::new().build().unwrap();
        inner.put("shards/s1", Bytes::from(data)).await.unwrap();

        let (flaky, attempts) = FlakyStorage::new(
            inner,
            "shards/s1".to_string(),
            InjectedFailure::Transient,
            1,
        );
        let temp_dir = tempdir().unwrap();
        let shard = parse_shard_from_storage(&flaky, "shards/s1", temp_dir.path())
            .await
            .expect("the retried fetch must parse the shard");
        drop(shard);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "exactly one retry must happen"
        );
    }

    #[tokio::test]
    async fn parse_does_not_retry_missing_shard() {
        use crate::storage::flaky::{FlakyStorage, InjectedFailure};
        use crate::storage::local::LocalStorage;

        let dir = tempdir().unwrap();
        let inner = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        let data = ShardBuilder::new().build().unwrap();
        inner.put("shards/s1", Bytes::from(data)).await.unwrap();

        let (flaky, attempts) = FlakyStorage::new(
            inner,
            "shards/missing".to_string(),
            InjectedFailure::Missing,
            usize::MAX,
        );
        let temp_dir = tempdir().unwrap();
        let result = parse_shard_from_storage(&flaky, "shards/missing", temp_dir.path()).await;
        assert!(matches!(
            result,
            Err(ShardIoError::Storage {
                source: StorageError::NotFound(_),
                ..
            })
        ));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "missing objects must not consume the retry budget"
        );
    }
}
