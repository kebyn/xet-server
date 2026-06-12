//! Internal API endpoints for Hub-to-CAS communication.
//!
//! These endpoints are used by HuggingFace Hub to query blob storage state
//! and check blob accessibility. They require the "internal" scope.

use actix_web::{web, HttpResponse};
use serde::Serialize;
use tracing::{info, warn};

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;

/// Error response for internal endpoints
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Get storage state for a blob by OID.
///
/// Stateless logic:
/// - Check MetadataIndex for xet data → return xet_only
/// - Check raw blob in storage → return raw_only
/// - Not found → 404
///
/// Requires "internal" scope.
pub async fn get_blob_state(
    path: web::Path<String>,
    auth: web::Data<AuthVerifier>,
    storage: web::Data<Box<dyn StorageBackend>>,
    index: web::Data<MetadataIndex>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // Validate oid format (should be a hex hash)
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid oid format, expected 64-character hex string".to_string(),
        });
    }

    // Extract and validate auth token
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(ErrorResponse {
                error: "Missing or invalid authorization token".to_string(),
            });
        }
    };

    let claims = match auth.verify_token(&token) {
        Ok(c) => c,
        Err(_) => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(ErrorResponse {
                error: "Invalid token".to_string(),
            });
        }
    };

    // Check for "internal" scope
    if !check_scope(&claims, "internal") {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(ErrorResponse {
            error: "Insufficient scope: requires 'internal'".to_string(),
        });
    }

    // Check MetadataIndex first
    if index.get_shards_for_file(&oid).is_some() {
        info!("Internal state query for {}: xet_only", oid);
        GLOBAL_METRICS.record_request(200);
        GLOBAL_METRICS.record_latency(start);
        // Get actual blob size from storage
        let size = storage.get_size(&format!("lfs/objects/{}", oid)).await.unwrap_or(0);
        return HttpResponse::Ok().json(serde_json::json!({
            "state": "xet_only",
            "xet_file_id": oid,
            "size": size,
            "sha256": oid,
            "converted_at": null
        }));
    }

    // Check raw blob
    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            info!("Internal state query for {}: raw_only", oid);
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_latency(start);
            // Get actual blob size from storage
            let size = storage.get_size(&object_key).await.unwrap_or(0);
            HttpResponse::Ok().json(serde_json::json!({
                "state": "raw_only",
                "xet_file_id": null,
                "size": size,
                "sha256": oid,
                "converted_at": null
            }))
        }
        Ok(false) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::NotFound().json(ErrorResponse {
                error: format!("Blob not found: {}", oid),
            })
        }
        Err(e) => {
            warn!("Failed to check storage for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::InternalServerError().json(ErrorResponse {
                error: format!("Storage error: {}", e),
            })
        }
    }
}

/// Check if blob is accessible via HEAD request.
///
/// Stateless logic:
/// - Check MetadataIndex for xet data → X-Storage-State: xet_only
/// - Check raw blob in storage → X-Storage-State: raw_only
/// - Not found → 404
///
/// Requires "internal" scope.
pub async fn head_blob(
    path: web::Path<String>,
    auth: web::Data<AuthVerifier>,
    storage: web::Data<Box<dyn StorageBackend>>,
    index: web::Data<MetadataIndex>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // Validate oid format
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid oid format, expected 64-character hex string".to_string(),
        });
    }

    // Extract and validate auth token
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(ErrorResponse {
                error: "Missing or invalid authorization token".to_string(),
            });
        }
    };

    let claims = match auth.verify_token(&token) {
        Ok(c) => c,
        Err(_) => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(ErrorResponse {
                error: "Invalid token".to_string(),
            });
        }
    };

    // Check for "internal" scope
    if !check_scope(&claims, "internal") {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(ErrorResponse {
            error: "Insufficient scope: requires 'internal'".to_string(),
        });
    }

    // Check MetadataIndex first
    if index.get_shards_for_file(&oid).is_some() {
        GLOBAL_METRICS.record_request(200);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Ok()
            .insert_header(("X-Storage-State", "xet_only"))
            .insert_header(("X-File-Id", oid.as_str()))
            .finish();
    }

    // Check raw blob
    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::Ok()
                .insert_header(("X-Storage-State", "raw_only"))
                .finish()
        }
        Ok(false) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::NotFound().finish()
        }
        Err(_) => {
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::InternalServerError().finish()
        }
    }
}
