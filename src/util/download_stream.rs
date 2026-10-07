use futures_util::Stream;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_util::io::ReaderStream;

use crate::storage::{StorageBackend, StorageError, StorageResult};

use super::{TempPathGuard, check_disk_space};

/// Download a backend object to a guarded temporary file and expose it as a stream.
///
/// This is the bounded-memory fallback for backends that cannot expose a local
/// object path. The temporary file remains owned until the HTTP body stream is
/// completed or dropped.
pub async fn download_to_temp_stream(
    storage: &dyn StorageBackend,
    key: &str,
    temp_dir: &Path,
    file_prefix: &str,
) -> StorageResult<(u64, GuardedFileStream<ReaderStream<tokio::fs::File>>)> {
    tokio::fs::create_dir_all(temp_dir).await.map_err(|error| {
        StorageError::internal_with_source(
            format!(
                "Failed to create download temp directory {}",
                temp_dir.display()
            ),
            error,
        )
    })?;

    let expected_size = storage.get_size(key).await?;
    check_disk_space(temp_dir, expected_size).map_err(StorageError::internal)?;

    let guard =
        TempPathGuard::new(temp_dir.join(format!("{}-{}.tmp", file_prefix, uuid::Uuid::new_v4())));
    storage.download_to_path(key, guard.path()).await?;

    let file = tokio::fs::File::open(guard.path()).await.map_err(|error| {
        StorageError::internal_with_source(
            format!(
                "Failed to open downloaded object {}",
                guard.path().display()
            ),
            error,
        )
    })?;
    let metadata = file.metadata().await.map_err(|error| {
        StorageError::internal_with_source(
            format!(
                "Failed to stat downloaded object {}",
                guard.path().display()
            ),
            error,
        )
    })?;
    if !metadata.is_file() {
        return Err(StorageError::internal(
            "Downloaded storage object is not a regular file",
        ));
    }
    let actual_size = metadata.len();
    if actual_size != expected_size {
        return Err(StorageError::internal(format!(
            "Downloaded object size mismatch: backend declared {}, received {}",
            expected_size, actual_size
        )));
    }

    Ok((
        actual_size,
        GuardedFileStream::new(ReaderStream::new(file), guard),
    ))
}

/// Keeps a temporary file alive for exactly as long as its reader stream.
pub struct GuardedFileStream<S> {
    inner: S,
    _guard: TempPathGuard,
}

impl<S> GuardedFileStream<S> {
    pub fn new(inner: S, guard: TempPathGuard) -> Self {
        Self {
            inner,
            _guard: guard,
        }
    }
}

impl<S> Stream for GuardedFileStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;

    struct FileDownloadStorage {
        data: Bytes,
        declared_size: u64,
    }

    #[async_trait]
    impl StorageBackend for FileDownloadStorage {
        async fn put(&self, _key: &str, _data: Bytes) -> StorageResult<()> {
            unreachable!("download tests never upload")
        }

        async fn get(&self, _key: &str) -> StorageResult<Bytes> {
            Err(StorageError::internal("bounded download must not call get"))
        }

        async fn exists(&self, _key: &str) -> StorageResult<bool> {
            Ok(true)
        }

        async fn delete(&self, _key: &str) -> StorageResult<()> {
            Ok(())
        }

        async fn get_size(&self, _key: &str) -> StorageResult<u64> {
            Ok(self.declared_size)
        }

        async fn download_to_path(&self, _key: &str, dest: &Path) -> StorageResult<()> {
            tokio::fs::write(dest, &self.data).await.map_err(|error| {
                StorageError::internal_with_source("failed to write download", error)
            })
        }
    }

    async fn assert_temp_dir_eventually_empty(path: &Path) {
        for _ in 0..20 {
            if std::fs::read_dir(path).unwrap().next().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("temporary download file was not removed");
    }

    #[tokio::test]
    async fn dropping_response_stream_removes_downloaded_temp_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = FileDownloadStorage {
            data: Bytes::from_static(b"downloaded bytes"),
            declared_size: 16,
        };

        let (_, stream) = download_to_temp_stream(&storage, "object", temp_dir.path(), "test")
            .await
            .unwrap();
        assert_eq!(std::fs::read_dir(temp_dir.path()).unwrap().count(), 1);

        drop(stream);
        assert_temp_dir_eventually_empty(temp_dir.path()).await;
    }

    #[tokio::test]
    async fn size_mismatch_rejects_and_removes_downloaded_temp_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = FileDownloadStorage {
            data: Bytes::from_static(b"short"),
            declared_size: 6,
        };

        let error = match download_to_temp_stream(&storage, "object", temp_dir.path(), "test").await
        {
            Ok(_) => panic!("declared and downloaded sizes must match"),
            Err(error) => error,
        };
        assert!(matches!(error, StorageError::Internal { .. }));
        assert_temp_dir_eventually_empty(temp_dir.path()).await;
    }
}
