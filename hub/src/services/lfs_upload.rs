use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
pub(crate) struct StoredLfsUpload {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LfsUploadStoreError {
    CreateTempDir(String),
    CreateTempFile(String),
    PrepareTempFile(String),
    OpenTempFile(String),
    ReadPayload(String),
    PayloadTooLarge { actual: u64, max: u64 },
    WriteTempFile(String),
    FlushTempFile(String),
}

pub(crate) async fn write_payload_to_temp_file<S, E>(
    mut payload: S,
    temp_dir: &Path,
    max_upload_size: u64,
) -> Result<StoredLfsUpload, LfsUploadStoreError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    tokio::fs::create_dir_all(temp_dir)
        .await
        .map_err(|err| LfsUploadStoreError::CreateTempDir(err.to_string()))?;

    let temp_file = tempfile::Builder::new()
        .prefix("upload-")
        .tempfile_in(temp_dir)
        .map_err(|err| LfsUploadStoreError::CreateTempFile(err.to_string()))?;
    let (temp_file_handle, temp_path) = temp_file
        .keep()
        .map_err(|err| LfsUploadStoreError::PrepareTempFile(err.to_string()))?;
    drop(temp_file_handle);

    let mut hasher = Sha256::new();
    let mut file_writer = match tokio::fs::File::create(&temp_path).await {
        Ok(file) => tokio::io::BufWriter::new(file),
        Err(err) => {
            remove_temp_file(&temp_path).await;
            return Err(LfsUploadStoreError::OpenTempFile(err.to_string()));
        }
    };

    let mut total_bytes: u64 = 0;
    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(chunk) => chunk,
            Err(err) => {
                remove_open_temp_file(file_writer, &temp_path).await;
                return Err(LfsUploadStoreError::ReadPayload(err.to_string()));
            }
        };

        total_bytes += chunk.len() as u64;
        if total_bytes > max_upload_size {
            remove_open_temp_file(file_writer, &temp_path).await;
            return Err(LfsUploadStoreError::PayloadTooLarge {
                actual: total_bytes,
                max: max_upload_size,
            });
        }

        hasher.update(&chunk);
        if let Err(err) = file_writer.write_all(&chunk).await {
            remove_open_temp_file(file_writer, &temp_path).await;
            return Err(LfsUploadStoreError::WriteTempFile(err.to_string()));
        }
    }

    if let Err(err) = file_writer.flush().await {
        remove_open_temp_file(file_writer, &temp_path).await;
        return Err(LfsUploadStoreError::FlushTempFile(err.to_string()));
    }
    drop(file_writer);

    Ok(StoredLfsUpload {
        path: temp_path,
        size: total_bytes,
        sha256: hex::encode(hasher.finalize()),
    })
}

async fn remove_temp_file(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

async fn remove_open_temp_file(file_writer: tokio::io::BufWriter<tokio::fs::File>, path: &Path) {
    drop(file_writer);
    remove_temp_file(path).await;
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::stream;
    use sha2::{Digest, Sha256};

    #[tokio::test]
    async fn writes_payload_to_temp_file_with_size_and_sha256() {
        let temp_dir = tempfile::tempdir().unwrap();
        let payload = stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"hello ")),
            Ok::<_, std::io::Error>(Bytes::from_static(b"world")),
        ]);

        let stored = super::write_payload_to_temp_file(payload, temp_dir.path(), 1024)
            .await
            .unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello world");
        let expected_hash = hex::encode(hasher.finalize());

        assert_eq!(stored.size, 11);
        assert_eq!(stored.sha256, expected_hash);
        assert_eq!(tokio::fs::read(&stored.path).await.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn payload_over_limit_returns_size_error_and_removes_temp_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let payload = stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc")),
            Ok::<_, std::io::Error>(Bytes::from_static(b"def")),
        ]);

        let err = super::write_payload_to_temp_file(payload, temp_dir.path(), 5)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            super::LfsUploadStoreError::PayloadTooLarge { actual: 6, max: 5 }
        );

        let mut entries = tokio::fs::read_dir(temp_dir.path()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }
}
