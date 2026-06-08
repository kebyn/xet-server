//! Shard Upload API
//!
//! POST /v1/shards - Upload metadata shards

use actix_web::{web, HttpResponse};
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::{check_scope, extract_bearer_token, validate_jwt};
use crate::config::ServerConfig;
use crate::format::shard::MDBShardFile;
use crate::index::MetadataIndex;
use crate::storage::StorageBackend;

#[derive(Serialize)]
struct ShardUploadResponse {
    was_inserted: bool,
    shard_id: String,
}

/// Upload a metadata shard
pub async fn upload_shard(
    body: web::Bytes,
    storage: web::Data<Box<dyn StorageBackend>>,
    index: web::Data<MetadataIndex>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    // Extract and validate auth token
    let auth_header = match req.headers().get("Authorization") {
        Some(h) => match h.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                return HttpResponse::Unauthorized().json(serde_json::json!({
                    "error": "Invalid authorization header"
                }))
            }
        },
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization token"
            }))
        }
    };

    let token = match extract_bearer_token(&auth_header) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token format"
            }))
        }
    };

    let claims = match validate_jwt(&token, &config.auth.jwt_secret) {
        Ok(c) => c,
        Err(_) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token"
            }))
        }
    };

    if !check_scope(&claims, "write") {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope"
        }));
    }

    // Parse shard binary format
    let shard = match MDBShardFile::parse(&body) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to parse shard: {}", e);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid shard format: {}", e)
            }));
        }
    };

    // Generate shard ID (using shard hash)
    let shard_id = shard.compute_hash();
    let shard_key = format!("shards/{}", shard_id);

    // Check if shard already exists
    let already_exists = match storage.exists(&shard_key).await {
        Ok(exists) => exists,
        Err(e) => {
            error!("Failed to check shard existence: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }));
        }
    };

    if already_exists {
        return HttpResponse::Ok().json(ShardUploadResponse {
            was_inserted: false,
            shard_id,
        });
    }

    // Store shard
    if let Err(e) = storage.put(&shard_key, body.to_vec().into()).await {
        error!("Failed to store shard: {}", e);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Storage error: {}", e)
        }));
    }

    // Update metadata index
    let file_hashes: Vec<String> = shard.file_hashes().iter().map(|h| h.to_string()).collect();
    let chunk_mappings: Vec<(String, String, u32)> = shard
        .chunk_mappings()
        .iter()
        .map(|(c, x, i)| (c.to_string(), x.to_string(), *i))
        .collect();

    index.register_shard(shard_id.clone(), file_hashes, chunk_mappings);

    info!("Uploaded shard {} with {} files and {} chunks",
        shard_id,
        shard.file_hashes().len(),
        shard.chunk_mappings().len()
    );

    HttpResponse::Ok().json(ShardUploadResponse {
        was_inserted: true,
        shard_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::JwtClaims;
    use crate::api::auth::create_jwt;
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use actix_web::{test, web, App};
    use tempfile::tempdir;

    #[actix_web::test]
    async fn test_upload_shard_unauthorized() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let config = ServerConfig {
            auth: AuthConfig {
                jwt_secret: "test-secret".to_string(),
            },
            ..Default::default()
        };

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(config))
                .route("/v1/shards", web::post().to(upload_shard))
        ).await;

        let req = test::TestRequest::post()
            .uri("/v1/shards")
            .set_payload(vec![0u8; 100])
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_upload_shard_invalid_format() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let config = ServerConfig {
            auth: AuthConfig {
                jwt_secret: "test-secret".to_string(),
            },
            ..Default::default()
        };

        let token = create_jwt(
            &JwtClaims {
                sub: "test".to_string(),
                scope: "read write".to_string(),
                exp: 9999999999,
            },
            &config.auth.jwt_secret,
        ).unwrap();

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(config))
                .route("/v1/shards", web::post().to(upload_shard))
        ).await;

        let req = test::TestRequest::post()
            .uri("/v1/shards")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(vec![0u8; 100])  // Invalid shard data
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }
}
