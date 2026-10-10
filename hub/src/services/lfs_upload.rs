use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::cas_client::{CasClient, CasUploadError};

#[derive(Debug)]
pub(crate) struct StoredLfsUpload {
    pub(crate) path: tempfile::TempPath,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LfsUploadStoreError {
    CreateTempDir(String),
    CreateTempFile(String),
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
            return Err(LfsUploadServiceError::HashMismatch {
                computed: stored_upload.sha256,
                size: stored_upload.size,
            });
        }

        self.cas_client
            .proxy_lfs_upload_from_path(oid, &stored_upload.path, stored_upload.size, token)
            .await
            .map_err(|err| LfsUploadServiceError::Cas {
                status: err.status,
                message: err.message,
            })
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
    // Keep the path guard alive during reception and CAS forwarding, including
    // cancellation. Reuse the securely opened handle instead of reopening by path.
    let (temp_file_handle, temp_path) = temp_file.into_parts();
    let mut hasher = Sha256::new();
    let mut file_writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(temp_file_handle));

    let mut total_bytes: u64 = 0;
    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(chunk) => chunk,
            Err(err) => {
                return Err(LfsUploadStoreError::ReadPayload(err.to_string()));
            }
        };

        total_bytes += chunk.len() as u64;
        if total_bytes > max_upload_size {
            return Err(LfsUploadStoreError::PayloadTooLarge {
                actual: total_bytes,
                max: max_upload_size,
            });
        }

        hasher.update(&chunk);
        if let Err(err) = file_writer.write_all(&chunk).await {
            return Err(LfsUploadStoreError::WriteTempFile(err.to_string()));
        }
    }

    if let Err(err) = file_writer.flush().await {
        return Err(LfsUploadStoreError::FlushTempFile(err.to_string()));
    }
    drop(file_writer);

    Ok(StoredLfsUpload {
        path: temp_path,
        size: total_bytes,
        sha256: hex::encode(hasher.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::{Stream, stream};
    use sha2::{Digest, Sha256};

    use crate::cas_client::CasUploadError;

    use super::{LfsUploadCasClient, LfsUploadService, LfsUploadServiceError};

    struct BlockingCasClient {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl LfsUploadCasClient for BlockingCasClient {
        async fn proxy_lfs_upload_from_path(
            &self,
            _oid: &str,
            _file_path: &Path,
            _file_size: u64,
            _token: &str,
        ) -> Result<(), CasUploadError> {
            self.started.notify_one();
            std::future::pending().await
        }
    }

    struct BlockingPayload {
        sent: bool,
        waiting: Arc<tokio::sync::Notify>,
    }

    impl Stream for BlockingPayload {
        type Item = Result<Bytes, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            if !self.sent {
                self.sent = true;
                Poll::Ready(Some(Ok(Bytes::from_static(b"partial payload"))))
            } else {
                self.waiting.notify_one();
                Poll::Pending
            }
        }
    }

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

    #[tokio::test]
    async fn cancellation_while_waiting_for_cas_drops_temp_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let service = LfsUploadService::new(Arc::new(BlockingCasClient {
            started: started.clone(),
        }));
        let temp_path = temp_dir.path().to_path_buf();
        let content = Bytes::from_static(b"cancel during CAS forwarding");
        let oid = hex::encode(Sha256::digest(&content));
        let task = tokio::spawn(async move {
            service
                .upload(
                    &oid,
                    "proxy_token",
                    stream::iter(vec![Ok::<_, std::io::Error>(content)]),
                    &temp_path,
                    1024,
                )
                .await
        });
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut entries = tokio::fs::read_dir(temp_dir.path()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancellation_while_receiving_payload_drops_temp_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let waiting = Arc::new(tokio::sync::Notify::new());
        let service = LfsUploadService::new(Arc::new(MockUploadCasClient {
            calls: Arc::new(Mutex::new(Vec::new())),
            error: None,
        }));
        let temp_path = temp_dir.path().to_path_buf();
        let task = tokio::spawn({
            let waiting = waiting.clone();
            async move {
                service
                    .upload(
                        &"a".repeat(64),
                        "proxy_token",
                        BlockingPayload {
                            sent: false,
                            waiting,
                        },
                        &temp_path,
                        1024,
                    )
                    .await
            }
        });
        waiting.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut entries = tokio::fs::read_dir(temp_dir.path()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }
}
