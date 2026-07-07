use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::cas_client::{CasClient, CasUploadError};

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

#[async_trait]
pub(crate) trait LfsUploadCasClient: Send + Sync {
    async fn proxy_lfs_upload_from_path(
        &self,
        oid: &str,
        file_path: &Path,
        file_size: u64,
        token: &str,
    ) -> Result<(), CasUploadError>;
}

#[async_trait]
impl LfsUploadCasClient for CasClient {
    async fn proxy_lfs_upload_from_path(
        &self,
        oid: &str,
        file_path: &Path,
        file_size: u64,
        token: &str,
    ) -> Result<(), CasUploadError> {
        CasClient::proxy_lfs_upload_from_path(self, oid, file_path, file_size, token).await
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LfsUploadServiceError {
    Store(LfsUploadStoreError),
    HashMismatch { computed: String, size: u64 },
    Cas { status: u16, message: String },
}

pub(crate) struct LfsUploadService {
    cas_client: Arc<dyn LfsUploadCasClient>,
}

impl LfsUploadService {
    pub(crate) fn new(cas_client: Arc<dyn LfsUploadCasClient>) -> Self {
        Self { cas_client }
    }

    pub(crate) async fn upload<S, E>(
        &self,
        oid: &str,
        token: &str,
        payload: S,
        temp_dir: &Path,
        max_upload_size: u64,
    ) -> Result<(), LfsUploadServiceError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let stored_upload = write_payload_to_temp_file(payload, temp_dir, max_upload_size)
            .await
            .map_err(LfsUploadServiceError::Store)?;

        if stored_upload.sha256 != oid {
            remove_temp_file(&stored_upload.path).await;
            return Err(LfsUploadServiceError::HashMismatch {
                computed: stored_upload.sha256,
                size: stored_upload.size,
            });
        }

        let result = self
            .cas_client
            .proxy_lfs_upload_from_path(oid, &stored_upload.path, stored_upload.size, token)
            .await
            .map_err(|err| LfsUploadServiceError::Cas {
                status: err.status,
                message: err.message,
            });

        remove_temp_file(&stored_upload.path).await;
        result
    }
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
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::stream;
    use sha2::{Digest, Sha256};

    use crate::cas_client::CasUploadError;

    use super::{LfsUploadCasClient, LfsUploadService, LfsUploadServiceError};

    #[derive(Debug)]
    struct UploadCall {
        oid: String,
        token: String,
        file_size: u64,
        bytes: Vec<u8>,
    }

    struct MockUploadCasClient {
        calls: Arc<Mutex<Vec<UploadCall>>>,
        error: Option<CasUploadError>,
    }

    #[async_trait]
    impl LfsUploadCasClient for MockUploadCasClient {
        async fn proxy_lfs_upload_from_path(
            &self,
            oid: &str,
            file_path: &Path,
            file_size: u64,
            token: &str,
        ) -> Result<(), CasUploadError> {
            let bytes = tokio::fs::read(file_path).await.unwrap();
            self.calls.lock().unwrap().push(UploadCall {
                oid: oid.to_string(),
                token: token.to_string(),
                file_size,
                bytes,
            });

            if let Some(error) = &self.error {
                Err(CasUploadError {
                    status: error.status,
                    message: error.message.clone(),
                })
            } else {
                Ok(())
            }
        }
    }

    fn service(error: Option<CasUploadError>) -> (LfsUploadService, Arc<Mutex<Vec<UploadCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = LfsUploadService::new(Arc::new(MockUploadCasClient {
            calls: calls.clone(),
            error,
        }));
        (service, calls)
    }

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

    #[tokio::test]
    async fn upload_service_forwards_verified_temp_file_to_cas_and_cleans_up() {
        let temp_dir = tempfile::tempdir().unwrap();
        let content = Bytes::from_static(b"hello world");
        let oid = hex::encode(Sha256::digest(&content));
        let payload = stream::iter(vec![Ok::<_, std::io::Error>(content.clone())]);
        let (service, calls) = service(None);

        service
            .upload(&oid, "proxy_token", payload, temp_dir.path(), 1024)
            .await
            .unwrap();

        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].oid, oid);
            assert_eq!(calls[0].token, "proxy_token");
            assert_eq!(calls[0].file_size, content.len() as u64);
            assert_eq!(calls[0].bytes, content.as_ref());
        }

        let mut entries = tokio::fs::read_dir(temp_dir.path()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upload_service_hash_mismatch_skips_cas_and_cleans_up() {
        let temp_dir = tempfile::tempdir().unwrap();
        let content = Bytes::from_static(b"wrong content");
        let payload = stream::iter(vec![Ok::<_, std::io::Error>(content.clone())]);
        let (service, calls) = service(None);

        let err = service
            .upload(
                &"a".repeat(64),
                "proxy_token",
                payload,
                temp_dir.path(),
                1024,
            )
            .await
            .unwrap_err();

        assert_eq!(
            err,
            LfsUploadServiceError::HashMismatch {
                computed: hex::encode(Sha256::digest(&content)),
                size: content.len() as u64,
            }
        );
        assert!(calls.lock().unwrap().is_empty());

        let mut entries = tokio::fs::read_dir(temp_dir.path()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upload_service_cas_error_preserves_status_and_cleans_up() {
        let temp_dir = tempfile::tempdir().unwrap();
        let content = Bytes::from_static(b"hello world");
        let oid = hex::encode(Sha256::digest(&content));
        let payload = stream::iter(vec![Ok::<_, std::io::Error>(content.clone())]);
        let (service, calls) = service(Some(CasUploadError {
            status: 413,
            message: "payload too large".to_string(),
        }));

        let err = service
            .upload(&oid, "proxy_token", payload, temp_dir.path(), 1024)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            LfsUploadServiceError::Cas {
                status: 413,
                message: "payload too large".to_string(),
            }
        );
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].oid, oid);
            assert_eq!(calls[0].file_size, content.len() as u64);
            assert_eq!(calls[0].bytes, content.as_ref());
        }

        let mut entries = tokio::fs::read_dir(temp_dir.path()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }
}
