//! GC API endpoints for manual trigger and status queries

use crate::api::auth::{extract_token_from_request, is_internal_token, AuthVerifier};
use crate::gc::{IncrementalGarbageCollector, IncrementalGcStats};
use actix_web::{web, HttpRequest, HttpResponse};
use std::sync::Arc;
use tokio::sync::RwLock;

/// POST /internal/gc/run
///
/// Manually trigger a GC run. Returns immediately with 202 Accepted.
/// GC runs in background. Requires internal scope authentication.
pub async fn trigger_gc(
    req: HttpRequest,
    gc: web::Data<Arc<IncrementalGarbageCollector>>,
    auth: web::Data<Arc<AuthVerifier>>,
) -> HttpResponse {
    // I6: Use standard auth extraction for consistency with other internal endpoints
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing or invalid authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let claims = match auth.verify_token(&token) {
        Ok(c) => c,
        Err(_) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token",
                "error_type": "AuthenticationError"
            }));
        }
    };

    // I2 fix: Use defense-in-depth check consistent with other internal endpoints.
    if !is_internal_token(&claims) {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Internal endpoint requires internal token type (sub=hub-service, scope=internal, token_type=internal)",
            "error_type": "AuthorizationError"
        }));
    }

    // Clone Arc for background task
    let gc_clone = gc.get_ref().clone();

    // Spawn background task
    tokio::spawn(async move {
        match gc_clone.run().await {
            Ok(stats) => {
                tracing::info!("Manual incremental GC completed: {:?}", stats);
            }
            Err(e) => {
                tracing::error!("Manual incremental GC failed: {}", e);
            }
        }
    });

    HttpResponse::Accepted().json(serde_json::json!({
        "message": "Incremental GC triggered, running in background",
        "dry_run": gc.config().dry_run,
        "gc_type": "incremental"
    }))
}

/// GET /internal/gc/status
///
/// Get the status and statistics from the last incremental GC run.
/// Requires internal scope authentication.
pub async fn gc_status(
    req: HttpRequest,
    auth: web::Data<Arc<AuthVerifier>>,
    last_stats: web::Data<Arc<RwLock<Option<IncrementalGcStats>>>>,
) -> HttpResponse {
    // I6: Use standard auth extraction for consistency with other internal endpoints
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing or invalid authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let claims = match auth.verify_token(&token) {
        Ok(c) => c,
        Err(_) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token",
                "error_type": "AuthenticationError"
            }));
        }
    };

    // I2 fix: Use defense-in-depth check consistent with other internal endpoints.
    if !is_internal_token(&claims) {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Internal endpoint requires internal token type (sub=hub-service, scope=internal, token_type=internal)",
            "error_type": "AuthorizationError"
        }));
    }

    let stats = last_stats.read().await;

    match stats.as_ref() {
        Some(s) => HttpResponse::Ok().json(serde_json::json!({
            "status": "ok",
            "gc_type": "incremental",
            "stats": {
                "lease_acquired": s.lease_acquired,
                "shards_scanned": s.shards_scanned,
                "refs_inserted": s.refs_inserted,
                "candidates": s.candidates,
                "deleted_lfs_blobs": s.deleted_lfs_blobs,
                "deleted_xorbs": s.deleted_xorbs,
                "deleted_shards": s.deleted_shards,
                "bloom_protected": s.bloom_protected,
                "grace_period_skipped": s.grace_period_skipped,
                "errors": s.errors,
                "sidecar_missing": s.sidecar_missing,
                "duration_seconds": s.duration_seconds,
                "dry_run": s.dry_run,
                "scan_completed": s.scan_completed,
                "bloom_items": s.bloom_items,
                "bloom_rebuild_count": s.bloom_rebuild_count,
                "last_run": s.last_run.map(|dt| dt.to_rfc3339()),
            }
        })),
        None => HttpResponse::Ok().json(serde_json::json!({
            "status": "no_gc_run_yet",
            "gc_type": "incremental",
            "message": "No incremental GC run has been completed yet"
        })),
    }
}
