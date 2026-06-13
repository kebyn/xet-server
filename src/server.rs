//! HTTP server implementation

use actix_web::{web, App, HttpServer, HttpResponse, middleware::{Logger, from_fn}};
use actix_governor::{Governor, GovernorConfigBuilder, GlobalKeyExtractor};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::api::auth::AuthVerifier;
use crate::config::ServerConfig;
use crate::conversion::ConvertingOids;
use crate::gc::{GarbageCollector, GcStats, start_gc_background_task};
use crate::storage::{create_storage, StorageBackend};
use crate::middleware::metrics_middleware;

pub async fn start_server(config: ServerConfig) -> std::io::Result<()> {
    // Load auth keys once at startup (avoid per-request file I/O)
    let auth_verifier = Arc::new(
        AuthVerifier::from_config(&config.auth)
            .expect("Failed to load auth public key")
    );

    // Check public key file permissions for security
    if let Some(warning) = crate::config::check_public_key_permissions(&config.auth.public_key_path) {
        tracing::warn!("{}", warning);
    }

    // Validate storage backend
    match config.storage.backend.as_str() {
        "local" | "s3" => {},
        invalid => {
            return Err(std::io::Error::other(format!(
                "Invalid XET_STORAGE_BACKEND '{}'. Must be 'local' or 's3'.",
                invalid
            )));
        }
    }

    let storage: Arc<Box<dyn StorageBackend>> = Arc::new(create_storage(&config.storage).await
        .expect("Failed to create storage backend"));

    let index = Arc::new(crate::index::MetadataIndex::new());

    // Rebuild MetadataIndex from stored shards (stateless server)
    match index.rebuild_from_storage(&**storage).await {
        Ok(count) => tracing::info!("Rebuilt metadata index: {} shards loaded", count),
        Err(e) => tracing::warn!("Failed to rebuild index: {}", e),
    }

    // Concurrent conversion tracker (in-memory, resets on restart)
    let converting = Arc::new(ConvertingOids::new());

    // GC: Garbage collector for orphaned blobs
    // Validate GC configuration
    for warning in crate::config::validate_gc_config(&config) {
        tracing::warn!("{}", warning);
    }
    let gc = Arc::new(GarbageCollector::new(storage.clone(), config.gc.clone()));
    let last_gc_stats = Arc::new(RwLock::new(None::<GcStats>));

    // Start background GC task (if enabled)
    start_gc_background_task(gc.clone(), last_gc_stats.clone()).await;

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);

    tracing::info!("Starting Xet Storage server on {}", bind_addr);

    // Warn if CAS is bound to localhost only — common gotcha for distributed deployments
    if config.server.host == "127.0.0.1" || config.server.host == "localhost" {
        tracing::warn!(
            "CAS server bound to {} only. Remote clients (including Hub on another host) cannot connect. \
            Set XET_HOST=0.0.0.0 for remote access.",
            config.server.host
        );
    }

    tracing::info!("Storage backend: {}", config.storage.backend);
    tracing::info!("Max upload size: {} MB", config.server.max_body_size_mb);
    tracing::info!("Conversion: {}", if config.conversion.enabled { "enabled" } else { "disabled" });
    tracing::info!("GC: {} (interval={}s, dry_run={})",
        if config.gc.enabled { "enabled" } else { "disabled" },
        config.gc.interval_seconds,
        config.gc.dry_run
    );

    let gc_for_app = gc.clone();
    let stats_for_app = last_gc_stats.clone();

    // Configure rate limiting for public endpoints only.
    // Internal endpoints (/internal/*) bypass rate limiting to avoid
    // disrupting Hub-to-CAS communication.
    // Allow `rate_limit_rpm` requests per minute per IP address.
    let rpm = config.server.rate_limit_rpm;
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(60)  // 60 seconds window
        .burst_size(rpm)   // rpm requests per window
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("Failed to configure rate limiter");

    tracing::info!("Rate limiting: {} requests/minute per IP for public endpoints (internal endpoints excluded)", rpm);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(from_fn(metrics_middleware))
            // PayloadConfig bounds non-upload routes (web::Bytes, web::Json).
            // Upload handlers use web::Payload which bypasses this limit and
            // enforce max_body_size_bytes manually via streaming byte counting.
            .app_data(web::PayloadConfig::new(10 * 1024 * 1024))
            .app_data(web::Data::from(auth_verifier.clone()))
            .app_data(web::Data::from(storage.clone()))
            .app_data(web::Data::from(index.clone()))
            .app_data(web::Data::new(converting.clone()))
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::new(config.conversion.clone()))
            .app_data(web::Data::new(gc_for_app.clone()))
            .app_data(web::Data::new(stats_for_app.clone()))
            // =============================================================
            // Internal endpoints (Hub-to-CAS communication) - NO rate limiting
            // These are registered at App level, BEFORE the public scope,
            // so they match first and bypass the Governor middleware.
            // =============================================================
            .route("/internal/state/{oid}", web::get().to(crate::api::internal::get_blob_state))
            .route("/internal/blob/{oid}", web::head().to(crate::api::internal::head_blob))
            // GC endpoints (CAS internal) - no rate limiting
            .route("/internal/gc/run", web::post().to(crate::api::gc::trigger_gc))
            .route("/internal/gc/status", web::get().to(crate::api::gc::gc_status))
            // Health and metrics endpoints - no rate limiting
            .route("/health", web::get().to(health_check))
            .route("/metrics", web::get().to(metrics_endpoint))
            // =============================================================
            // Public API routes - rate limited via Governor middleware.
            // The scope wraps all public routes with rate limiting.
            // =============================================================
            .service(
                web::scope("")
                    .wrap(Governor::new(&governor_conf))
                    .route("/v1/xorbs/{prefix}/{hash}", web::post().to(crate::api::xorb::upload_xorb))
                    .route("/v1/xorbs/{prefix}/{hash}", web::put().to(crate::api::xorb::upload_xorb))
                    .route("/v1/xorbs/{prefix}/{hash}/download", web::get().to(crate::api::xorb::download_xorb))
                    .route("/lfs/objects/{oid}", web::put().to(crate::api::lfs::upload_lfs_object))
                    .route("/lfs/objects/{oid}", web::get().to(crate::api::lfs::download_lfs_object))
                    .route("/v1/shards", web::post().to(crate::api::shard::upload_shard))
                    .route("/v1/reconstructions/{file_id}", web::get().to(crate::api::reconstruction::get_reconstruction_v1))
                    .route("/v2/reconstructions/{file_id}", web::get().to(crate::api::reconstruction::get_reconstruction))
                    .route("/v1/chunks/{prefix}/{hash}", web::get().to(crate::api::global_dedup::query_chunk_dedup))
                    .route("/objects/batch", web::post().to(crate::api::batch::batch_operation))
                    .route("/lfs/objects/batch", web::post().to(crate::api::batch::batch_operation))
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
}

pub async fn health_check() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok"
    }))
}

/// Prometheus metrics endpoint
///
/// # Security Note
/// This endpoint requires authentication (internal scope).
/// In production environments, use tokens with "internal" scope for monitoring systems.
pub async fn metrics_endpoint(
    auth: web::Data<AuthVerifier>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    // Extract and validate auth token
    let token = match crate::api::auth::extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            crate::metrics::GLOBAL_METRICS.record_request(401);
            crate::metrics::GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing or invalid authorization token"
            }));
        }
    };

    let claims = match auth.verify_token(&token) {
        Ok(c) => c,
        Err(_) => {
            crate::metrics::GLOBAL_METRICS.record_request(401);
            crate::metrics::GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token"
            }));
        }
    };

    // Check for "internal" scope (monitoring systems should use internal tokens)
    if !crate::api::auth::check_scope(&claims, "internal") {
        crate::metrics::GLOBAL_METRICS.record_request(403);
        crate::metrics::GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope: requires 'internal'"
        }));
    }

    let metrics = crate::metrics::GLOBAL_METRICS.export_metrics();
    crate::metrics::GLOBAL_METRICS.record_request(200);
    crate::metrics::GLOBAL_METRICS.record_latency(start);
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(metrics)
}
