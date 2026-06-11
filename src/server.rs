//! HTTP server implementation

use actix_web::{web, App, HttpServer, HttpResponse, middleware::{Logger, from_fn}};
use std::sync::Arc;

use crate::api::auth::AuthVerifier;
use crate::config::ServerConfig;
use crate::storage::{create_storage, StorageBackend};
use crate::state::SqliteStateManager;
use crate::middleware::metrics_middleware;

pub async fn start_server(config: ServerConfig) -> std::io::Result<()> {
    // Load auth keys once at startup (avoid per-request file I/O)
    let auth_verifier = Arc::new(
        AuthVerifier::from_config(&config.auth)
            .expect("Failed to load auth public key")
    );

    let storage: Arc<Box<dyn StorageBackend>> = Arc::new(create_storage(&config.storage).await
        .expect("Failed to create storage backend"));

    let index = Arc::new(crate::index::MetadataIndex::new());

    // Create state manager for tracking blob storage state
    let state_mgr: Arc<dyn crate::state::StorageStateManager> = Arc::new(
        SqliteStateManager::new(&config.state.sqlite_path)
            .expect("Failed to create state manager")
    );

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);

    println!("Starting Xet Storage server on {}", bind_addr);
    println!("Storage backend: {}", config.storage.backend);
    println!("Max upload size: {} MB", config.server.max_body_size_mb);
    println!("State database: {}", config.state.sqlite_path);

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
            .app_data(web::Data::from(state_mgr.clone()))  // Data::from(Arc<T>) = Data<T>
            // Also register as Data<Arc<T>> for handlers that expect the Arc wrapper
            .app_data(web::Data::new(state_mgr.clone()))  // Data::new(T) = Data<T> wrapping Arc<T>
            .app_data(web::Data::new(config.clone()))
            // Internal endpoints (Hub-to-CAS communication)
            .route("/internal/state/{oid}", web::get().to(crate::api::internal::get_blob_state))
            .route("/internal/blob/{oid}", web::head().to(crate::api::internal::head_blob))
            // Public API routes
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
            .route("/health", web::get().to(health_check))
            .route("/metrics", web::get().to(metrics_endpoint))
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
/// This endpoint exposes operational metrics (request counts, latency, error rates)
/// without authentication. In production environments, consider:
/// - Restricting access via network policies/firewall rules
/// - Adding authentication if metrics contain sensitive information
/// - Using a dedicated metrics port that's not publicly accessible
pub async fn metrics_endpoint() -> HttpResponse {
    let metrics = crate::metrics::GLOBAL_METRICS.export_metrics();
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(metrics)
}
