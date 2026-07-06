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
                            "error": format!("Failed to open file: {}", e)
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
                            "error": format!("Failed to get metadata: {}", e)
                        }),
                    ));
                }
            };
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
        Ok(None) => serve_raw_blob_inmemory(oid, storage, config, start).await,
        Err(StorageError::NotFound(_)) => RawBlobResult::Missing,
        Err(e) => {
            error!("Failed to get path for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            RawBlobResult::Error(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
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

/// Fallback: serve a raw blob by loading it entirely into memory.
/// Performs integrity verification first when configured.
async fn serve_raw_blob_inmemory(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    start: std::time::Instant,
) -> RawBlobResult {
    let object_key = format!("lfs/objects/{}", oid);
    let object_data = match storage.get(&object_key).await {
        Ok(data) => {
            GLOBAL_METRICS.record_storage_operation();
            data
        }
        Err(StorageError::NotFound(_)) => return RawBlobResult::Missing,
        Err(e) => {
            error!("Failed to fetch object: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                serde_json::json!({
                    "error": format!("Storage error: {}", e)
                }),
            ));
        }
    };

    if config.storage.verify_download_integrity {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&object_data);
        let computed_hash = format!("{:x}", hasher.finalize());

        if computed_hash != oid {
            error!(
                "Integrity check FAILED for {}: computed {} != expected {} ({} bytes)",
                oid,
                computed_hash,
                oid,
                object_data.len()
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                serde_json::json!({
                    "error": "Integrity verification failed: stored content does not match OID"
                }),
            ));
        }

        info!(
            "Integrity check passed for {} ({} bytes)",
            oid,
            object_data.len()
        );
    }

    info!(
        "Downloaded LFS object {} ({} bytes)",
        oid,
        object_data.len()
    );

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(object_data.len() as u64);
    GLOBAL_METRICS.record_latency(start);

    RawBlobResult::Served(
        HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(object_data),
    )
}
