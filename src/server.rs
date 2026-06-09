//! HTTP server implementation

use actix_web::{web, App, HttpServer, HttpResponse, middleware::{Logger, from_fn}};
use std::sync::Arc;

use crate::config::ServerConfig;
use crate::storage::create_storage;
use crate::middleware::metrics_middleware;

/// Maximum request body size: 64MB
/// This allows for large xorb uploads while preventing memory exhaustion
const MAX_BODY_SIZE: usize = 64 * 1024 * 1024;

pub async fn start_server(config: ServerConfig) -> std::io::Result<()> {
    let storage = Arc::new(create_storage(&config.storage).await
        .expect("Failed to create storage backend"));

    let index = Arc::new(crate::index::MetadataIndex::new());

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);

    println!("Starting Xet Storage server on {}", bind_addr);
    println!("Storage backend: {}", config.storage.backend);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(from_fn(metrics_middleware))
            // Configure payload size limit to prevent memory exhaustion
            .app_data(web::PayloadConfig::new(MAX_BODY_SIZE))
            .app_data(web::Data::from(storage.clone()))
            .app_data(web::Data::from(index.clone()))
            .app_data(web::Data::new(config.clone()))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(crate::api::xorb::upload_xorb))
            .route("/v1/shards", web::post().to(crate::api::shard::upload_shard))
            .route("/v1/reconstructions/{file_id}", web::get().to(crate::api::reconstruction::get_reconstruction_v1))
            .route("/v2/reconstructions/{file_id}", web::get().to(crate::api::reconstruction::get_reconstruction))
            .route("/v1/chunks/{prefix}/{hash}", web::get().to(crate::api::global_dedup::query_chunk_dedup))
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
