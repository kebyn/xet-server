use actix_web::{HttpResponse, web};
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
) -> HttpResponse {
    let reconstruction =
        match reconstruct_verified_file_to_temp(oid, file_refs, &***storage, &temp_dir).await {
            Ok(reconstruction) => reconstruction,
            Err(e) => {
                error!("Verified xet reconstruction failed for {}: {}", oid, e);
                if matches!(e, ReconstructionError::Stale(_)) {
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Object not found: {}", oid)
                    }));
                }
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": crate::api::INTERNAL_ERROR_MESSAGE
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
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    };

    let stream = crate::util::GuardedFileStream::new(ReaderStream::new(file), guard);
    let body = actix_web::body::SizedStream::new(size, stream);

    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_download_bytes(size);

    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(body)
}
