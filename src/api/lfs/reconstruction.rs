use actix_web::{HttpResponse, web};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_util::io::ReaderStream;
use tracing::error;

use crate::metrics::GLOBAL_METRICS;
use crate::reconstruction_io::{ReconstructionError, reconstruct_verified_file_to_temp};
use crate::storage::StorageBackend;

pub(super) async fn serve_verified_xet_reconstruction(
    oid: &str,
    file_refs: Vec<crate::index::FileShardRef>,
    storage: web::Data<Box<dyn StorageBackend>>,
    temp_dir: std::path::PathBuf,
    start: std::time::Instant,
) -> HttpResponse {
    let reconstruction =
        match reconstruct_verified_file_to_temp(oid, file_refs, &***storage, &temp_dir).await {
            Ok(reconstruction) => reconstruction,
            Err(e) => {
                error!("Verified xet reconstruction failed for {}: {}", oid, e);
                if matches!(e, ReconstructionError::Stale(_)) {
                    GLOBAL_METRICS.record_request(404);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Object not found: {}", oid)
                    }));
                }
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to reconstruct verified object"
                }));
            }
        };

    let size = reconstruction.size();
    let path = reconstruction.path().to_path_buf();
    let guard = reconstruction.into_guard();
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            error!(
                "Failed to open verified reconstruction {}: {}",
                path.display(),
                e
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to open reconstructed object"
            }));
        }
    };

    let stream = GuardedFileStream {
        inner: ReaderStream::new(file),
        _guard: guard,
    };
    let body = actix_web::body::SizedStream::new(size, stream);

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_download_bytes(size);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(body)
}

struct GuardedFileStream<S> {
    inner: S,
    _guard: crate::xorb_reader::TempPathGuard,
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
