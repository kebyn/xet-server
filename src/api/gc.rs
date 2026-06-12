//! GC API endpoints for manual trigger and status queries

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
use crate::gc::{GarbageCollector, GcStats};
use actix_web::{web, HttpRequest, HttpResponse};
use std::sync::Arc;
use tokio::sync::RwLock;

/// POST /internal/gc/run
///
/// Manually trigger a GC run. Returns immediately with 202 Accepted.
/// GC runs in background. Requires internal scope authentication.
pub async fn trigger_gc(
    req: HttpRequest,
    gc: web::Data<Arc<GarbageCollector>>,
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

    // I6: Use check_scope for consistent scope validation
    if !check_scope(&claims, "internal") {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Internal endpoint requires internal scope",
            "error_type": "AuthorizationError"
        }));
    }

    // Clone for background task
    let gc_clone = gc.get_ref().clone();

    // Spawn background task
    tokio::spawn(async move {
        match gc_clone.run().await {
            Ok(stats) => {
                tracing::info!("Manual GC completed: {:?}", stats);
            }
            Err(e) => {
                tracing::error!("Manual GC failed: {}", e);
            }
        }
    });

    HttpResponse::Accepted().json(serde_json::json!({
        "message": "GC triggered, running in background",
        "dry_run": gc.config().dry_run
    }))
}

/// GET /internal/gc/status
///
/// Get the status and statistics from the last GC run.
/// Requires internal scope authentication.
pub async fn gc_status(
    req: HttpRequest,
    auth: web::Data<Arc<AuthVerifier>>,
    last_stats: web::Data<Arc<RwLock<Option<GcStats>>>>,
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

    // I6: Use check_scope for consistent scope validation
    if !check_scope(&claims, "internal") {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Internal endpoint requires internal scope",
            "error_type": "AuthorizationError"
        }));
    }

    let stats = last_stats.read().await;

    match stats.as_ref() {
        Some(s) => HttpResponse::Ok().json(serde_json::json!({
            "status": "ok",
            "stats": {
                "total_lfs_blobs": s.total_lfs_blobs,
                "total_xorbs": s.total_xorbs,
                "total_shards": s.total_shards,
                "referenced_lfs_blobs": s.referenced_lfs_blobs,
                "referenced_xorbs": s.referenced_xorbs,
                "orphaned_lfs_blobs": s.orphaned_lfs_blobs,
                "orphaned_xorbs": s.orphaned_xorbs,
                "deleted_lfs_blobs": s.deleted_lfs_blobs,
                "deleted_xorbs": s.deleted_xorbs,
                "grace_period_skipped": s.grace_period_skipped,
                "errors": s.errors,
                "duration_seconds": s.duration_seconds,
                "dry_run": s.dry_run,
                "last_run": s.last_run,
            }
        })),
        None => HttpResponse::Ok().json(serde_json::json!({
            "status": "no_gc_run_yet",
            "message": "No GC run has been completed yet"
        })),
    }
}

