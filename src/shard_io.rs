use std::path::{Path, PathBuf};

use crate::error::XetError;
use crate::format::shard::MDBShardFile;
use crate::storage::{StorageBackend, StorageError};
use crate::xorb_reader::TempPathGuard;

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
            Err(StorageError::Internal(
                "get must not be used for shard parsing".to_string(),
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
            tokio::fs::write(dest, &self.data)
                .await
                .map_err(|error| StorageError::Internal(error.to_string()))
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
}
