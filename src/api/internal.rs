//! Internal API endpoints for Hub-to-CAS communication.
//!
//! These endpoints are used by HuggingFace Hub to query blob storage state
//! and check blob accessibility. They require the "internal" scope.

use actix_web::{web, HttpResponse};
use serde::Serialize;
use std::sync::Arc;
use tracing::{info, warn};

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
use crate::metrics::GLOBAL_METRICS;
use crate::state::{StorageState, StorageStateManager};
use crate::storage::StorageBackend;

/// Response for GET /internal/state/{oid}
#[derive(Serialize)]
struct StateResponse {
    state: String,
    xet_file_id: Option<String>,
    size: u64,
    sha256: String,
    converted_at: Option<u64>,
}

/// Error response for internal endpoints
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Get storage state for a blob by OID.
///
/// Returns JSON with state, xet_file_id, size, sha256, and converted_at.
/// Returns 404 if no state record exists.
///
/// Requires "internal" scope.
pub async fn get_blob_state(
    path: web::Path<String>,
    auth: web::Data<AuthVerifier>,
    state_mgr: web::Data<Arc<dyn StorageStateManager>>,
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

    // Query state from state manager
    let file_state = match state_mgr.get_state(&oid).await {
        Ok(Some(state)) => state,
        Ok(None) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(ErrorResponse {
                error: format!("No state found for oid: {}", oid),
            });
        }
        Err(e) => {
            warn!("Failed to get state for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(ErrorResponse {
                error: format!("State manager error: {}", e),
            });
        }
    };

    // Build response
    let state_str = match file_state.state {
        StorageState::RawOnly => "raw_only",
        StorageState::XetOnly => "xet_only",
    };

    let response = StateResponse {
        state: state_str.to_string(),
        xet_file_id: file_state.xet_file_id,
        size: file_state.size,
        sha256: file_state.sha256,
        converted_at: file_state.converted_at,
    };

    info!("Internal state query for {}: {}", oid, response.state);

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}

/// Check if blob is accessible via HEAD request.
///
/// Returns:
/// - 200 with X-Storage-State: xet_only and X-File-Id header if XetOnly
/// - 200 with X-Storage-State: raw_only if RawOnly
/// - 200 with X-Storage-State: raw_only if raw blob exists in storage (no state record)
/// - 404 if blob not found anywhere
///
/// Requires "internal" scope.
pub async fn head_blob(
    path: web::Path<String>,
    auth: web::Data<AuthVerifier>,
    state_mgr: web::Data<Arc<dyn StorageStateManager>>,
    storage: web::Data<Box<dyn StorageBackend>>,
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

    // Query state from state manager
    let file_state = match state_mgr.get_state(&oid).await {
        Ok(state) => state,
        Err(e) => {
            warn!("Failed to get state for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(ErrorResponse {
                error: format!("State manager error: {}", e),
            });
        }
    };

    // Build response based on state
    if let Some(state) = file_state {
        match state.state {
            StorageState::XetOnly => {
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_latency(start);
                HttpResponse::Ok()
                    .insert_header(("X-Storage-State", "xet_only"))
                    .insert_header(("X-File-Id", state.xet_file_id.unwrap_or_default()))
                    .finish()
            }
            StorageState::RawOnly => {
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_latency(start);
                HttpResponse::Ok()
                    .insert_header(("X-Storage-State", "raw_only"))
                    .finish()
            }
        }
    } else {
        // No state record - check if raw blob exists in storage
        let object_key = format!("lfs/objects/{}", oid);
        let exists = match storage.exists(&object_key).await {
            Ok(exists) => exists,
            Err(e) => {
                warn!("Failed to check storage for {}: {}", oid, e);
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(ErrorResponse {
                    error: format!("Storage error: {}", e),
                });
            }
        };

        if exists {
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::Ok()
                .insert_header(("X-Storage-State", "raw_only"))
                .finish()
        } else {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::NotFound().finish()
        }
    }
}