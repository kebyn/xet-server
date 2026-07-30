use actix_web::{HttpResponse, web};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use crate::config::ServerConfig;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::{StorageBackend, StorageError};

/// Result of attempting to serve a raw LFS blob.
pub(super) enum RawBlobResult {
    Served(HttpResponse),
    Missing,
    Error(HttpResponse),
}

/// Serve a raw blob from storage with optional streaming integrity verification.
/// Uses streaming file I/O when the backend supports it to avoid loading large
/// files entirely into memory.
pub(super) async fn serve_raw_blob(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    start: std::time::Instant,
) -> RawBlobResult {
    let object_key = format!("lfs/objects/{}", oid);
    let verify_integrity = config.storage.verify_download_integrity;

    match storage.get_path(&object_key).await {
        Ok(Some(path)) => {
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return RawBlobResult::Missing;
                }
                Err(e) => {
                    error!(
                        "Failed to open file for streaming {}: {}",
                        path.display(),
                        e
                    );
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                        serde_json::json!({
                            "error": crate::api::INTERNAL_ERROR_MESSAGE
                        }),
                    ));
                }
            };
            let metadata = match file.metadata().await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return RawBlobResult::Missing;
                }
                Err(e) => {
                    error!("Failed to get file metadata: {}", e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                        serde_json::json!({
                            "error": crate::api::INTERNAL_ERROR_MESSAGE
                        }),
                    ));
                }
            };
            if !metadata.is_file() {
                error!("LFS storage path is not a file: {}", path.display());
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                    serde_json::json!({
                        "error": crate::api::INTERNAL_ERROR_MESSAGE
                    }),
                ));
            }
            let file_size = metadata.len();

            let base_stream = ReaderStream::new(file);
            let oid_owned = oid.to_string();
            if verify_integrity {
                let hashing_stream = IntegrityVerifyingStream::new(base_stream, oid_owned);
                let body = actix_web::body::SizedStream::new(file_size, hashing_stream);

                info!(
                    "Streaming LFS object {} ({} bytes) with integrity verification",
                    oid, file_size
                );
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_storage_operation();
                GLOBAL_METRICS.record_download_bytes(file_size);
                GLOBAL_METRICS.record_latency(start);

                RawBlobResult::Served(
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(body),
                )
            } else {
                let body = actix_web::body::SizedStream::new(file_size, base_stream);

                info!("Streaming LFS object {} ({} bytes)", oid, file_size);
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_storage_operation();
                GLOBAL_METRICS.record_download_bytes(file_size);
                GLOBAL_METRICS.record_latency(start);

                RawBlobResult::Served(
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(body),
                )
            }
        }
        Ok(None) => {
            let temp_dir = config.storage.resolve_reconstruction_temp_dir();
            let (file_size, base_stream) = match crate::util::download_to_temp_stream(
                storage.get_ref().as_ref(),
                &object_key,
                &temp_dir,
                "lfs-download",
            )
            .await
            {
                Ok(download) => download,
                Err(StorageError::NotFound(_)) => return RawBlobResult::Missing,
                Err(e) => {
                    error!("Failed to stream remote LFS object {}: {}", oid, e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                        serde_json::json!({
                            "error": crate::api::INTERNAL_ERROR_MESSAGE
                        }),
                    ));
                }
            };

            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_storage_operation();
            GLOBAL_METRICS.record_download_bytes(file_size);
            GLOBAL_METRICS.record_latency(start);

            if verify_integrity {
                let stream = IntegrityVerifyingStream::new(base_stream, oid.to_string());
                let body = actix_web::body::SizedStream::new(file_size, stream);
                RawBlobResult::Served(
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(body),
                )
            } else {
                let body = actix_web::body::SizedStream::new(file_size, base_stream);
                RawBlobResult::Served(
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(body),
                )
            }
        }
        Err(StorageError::NotFound(_)) => RawBlobResult::Missing,
        Err(e) => {
            error!("Failed to get path for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            RawBlobResult::Error(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            })))
        }
    }
}

/// Stream wrapper that computes SHA-256 incrementally and verifies on completion.
///
/// Integrity failures surface as stream errors after the final chunk. Git LFS
/// clients are expected to verify the downloaded OID as well, so this is a
/// server-side defense-in-depth check without preloading large files.
struct IntegrityVerifyingStream<S> {
    inner: S,
    hasher: Option<sha2::Sha256>,
    expected_oid: String,
    bytes_hashed: u64,
}

impl<S> IntegrityVerifyingStream<S> {
    fn new(inner: S, expected_oid: String) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: Some(sha2::Sha256::new()),
            expected_oid,
            bytes_hashed: 0,
        }
    }
}

impl<S> Stream for IntegrityVerifyingStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use sha2::Digest;
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(hasher) = &mut self.hasher {
                    hasher.update(&chunk);
                }
                self.bytes_hashed += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                if let Some(hasher) = self.hasher.take() {
                    let computed_hash = format!("{:x}", hasher.finalize());
                    if computed_hash != self.expected_oid {
                        error!(
                            "Integrity check FAILED for {}: computed {} != expected {} ({} bytes streamed)",
                            self.expected_oid, computed_hash, self.expected_oid, self.bytes_hashed
                        );
                        GLOBAL_METRICS.record_error();
                        return Poll::Ready(Some(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Integrity verification failed: content hash {} does not match expected OID {}",
                                computed_hash, self.expected_oid
                            ),
                        ))));
                    }
                    info!(
                        "Integrity check passed for {} ({} bytes streamed)",
                        self.expected_oid, self.bytes_hashed
                    );
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::storage::StorageResult;

    struct StreamingOnlyStorage {
        data: Bytes,
        download_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl StorageBackend for StreamingOnlyStorage {
        async fn put(&self, _key: &str, _data: Bytes) -> StorageResult<()> {
            Err(StorageError::Internal("unexpected put".to_string()))
        }

        async fn get(&self, _key: &str) -> StorageResult<Bytes> {
            Err(StorageError::Internal(
                "unbounded get must not be used for downloads".to_string(),
            ))
        }

        async fn exists(&self, _key: &str) -> StorageResult<bool> {
            Ok(true)
        }

        async fn delete(&self, _key: &str) -> StorageResult<()> {
            Ok(())
        }

        async fn get_size(&self, _key: &str) -> StorageResult<u64> {
            Ok(self.data.len() as u64)
        }

        async fn download_to_path(&self, _key: &str, dest: &Path) -> StorageResult<()> {
            self.download_called.store(true, Ordering::SeqCst);
            tokio::fs::write(dest, &self.data)
                .await
                .map_err(|error| StorageError::Internal(error.to_string()))
        }
    }

    #[actix_web::test]
    async fn remote_lfs_download_uses_bounded_streaming_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let data = Bytes::from_static(b"remote LFS bytes");
        let download_called = Arc::new(AtomicBool::new(false));
        let storage: Box<dyn StorageBackend> = Box::new(StreamingOnlyStorage {
            data: data.clone(),
            download_called: download_called.clone(),
        });
        let mut config = ServerConfig::default();
        config.storage.reconstruction_temp_dir =
            Some(temp_dir.path().to_str().unwrap().to_string());

        let result = serve_raw_blob(
            &"a".repeat(64),
            web::Data::new(storage),
            web::Data::new(config),
            std::time::Instant::now(),
        )
        .await;

        let RawBlobResult::Served(response) = result else {
            panic!("remote object should be served");
        };
        assert_eq!(response.status(), 200);
        assert_eq!(
            actix_web::body::to_bytes(response.into_body())
                .await
                .unwrap(),
            data
        );
        assert!(download_called.load(Ordering::SeqCst));
    }
}
