//! HTTP server implementation

use actix_web::{web, App, HttpServer, HttpResponse, middleware::Logger};
use std::sync::Arc;

use crate::config::ServerConfig;
use crate::storage::create_storage;
use crate::api::auth::{extract_bearer_token, validate_jwt, check_scope};

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
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb))
            .route("/v1/shards", web::post().to(crate::api::shard::upload_shard))
            .route("/v2/reconstructions/{file_id}", web::get().to(crate::api::reconstruction::get_reconstruction))
            .route("/v1/chunks/{prefix}/{hash}", web::get().to(crate::api::global_dedup::query_chunk_dedup))
            .route("/health", web::get().to(health_check))
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

pub async fn upload_xorb(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    storage: web::Data<Box<dyn crate::storage::StorageBackend>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let (prefix, hash) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Validate hash format (64 hex chars)
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid hash format"
        }));
    }

    // Check auth
    let auth_header = match req.headers().get("Authorization") {
        Some(h) => match h.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid authorization header"
            })),
        },
        None => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Missing authorization token"
        })),
    };

    let token = match extract_bearer_token(&auth_header) {
        Some(t) => t,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid token format"
        })),
    };

    let config = req.app_data::<web::Data<crate::config::ServerConfig>>().unwrap();
    let claims = match validate_jwt(&token, &config.auth.jwt_secret) {
        Ok(c) => c,
        Err(_) => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid token"
        })),
    };

    if !check_scope(&claims, "write") {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope"
        }));
    }

    // Check if already exists
    let key = format!("xorbs/{}/{}", prefix, hash);
    let already_exists = match storage.exists(&key).await {
        Ok(exists) => exists,
        Err(_) => false,
    };

    if already_exists {
        return HttpResponse::Ok().json(serde_json::json!({
            "was_inserted": false
        }));
    }

    // TODO: Verify xorb hash matches body

    // Store xorb
    if let Err(e) = storage.put(&key, bytes::Bytes::from(body.to_vec())).await {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Storage error: {}", e)
        }));
    }

    HttpResponse::Ok().json(serde_json::json!({
        "was_inserted": true
    }))
}
