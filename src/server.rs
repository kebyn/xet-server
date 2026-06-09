//! HTTP server implementation

use actix_web::{web, App, HttpServer, HttpResponse, middleware::Logger};
use std::sync::Arc;

use crate::config::ServerConfig;
use crate::storage::create_storage;

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

pub async fn metrics_endpoint() -> HttpResponse {
    let metrics = crate::metrics::GLOBAL_METRICS.export_metrics();
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(metrics)
}
