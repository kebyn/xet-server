//! Xorb Upload API
//!
//! POST /v1/xorbs/{prefix}/{hash} - Upload xorb objects

use actix_web::{web, HttpResponse};
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::{check_scope, extract_bearer_token, validate_jwt};
use crate::config::ServerConfig;
use crate::storage::StorageBackend;
use crate::types::MerkleHash;
use crate::hash::compute_data_hash;
use crate::metrics::GLOBAL_METRICS;

#[derive(Serialize)]
struct XorbUploadResponse {
    was_inserted: bool,
}

/// Upload a xorb object
pub async fn upload_xorb(
    path: web::Path<(String, String)>,
    body: web::Bytes,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let (prefix, hash_str) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        GLOBAL_METRICS.record_request(400);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Parse hash
    let expected_hash = match MerkleHash::from_hex(&hash_str) {
        Ok(h) => h,
        Err(e) => {
            GLOBAL_METRICS.record_request(400);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid hash format: {}", e)
            }));
        }
    };

    // Extract and validate auth token
    let auth_header = match req.headers().get("Authorization") {
        Some(h) => match h.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                GLOBAL_METRICS.record_request(401);
                return HttpResponse::Unauthorized().json(serde_json::json!({
                    "error": "Invalid authorization header"
                }))
            }
        },
        None => {
            GLOBAL_METRICS.record_request(401);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization token"
            }))
        }
    };

    let token = match extract_bearer_token(&auth_header) {
        Some(t) => t,
        None => {
            GLOBAL_METRICS.record_request(401);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token format"
            }))
        }
    };

    let claims = match validate_jwt(&token, &config.auth.jwt_secret) {
        Ok(c) => c,
        Err(_) => {
            GLOBAL_METRICS.record_request(401);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token"
            }))
        }
    };

    if !check_scope(&claims, "write") {
        GLOBAL_METRICS.record_request(403);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope"
        }));
    }

    // Verify xorb hash
    let actual_hash = compute_data_hash(&body);
    if actual_hash != expected_hash {
        GLOBAL_METRICS.record_request(400);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Hash mismatch: expected {}, got {}", expected_hash.to_hex(), actual_hash.to_hex())
        }));
    }

    // Verify xorb structure and chunk hashes
    if let Err(e) = crate::format::xorb::verify_xorb(&body) {
        GLOBAL_METRICS.record_request(400);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Xorb verification failed: {}", e)
        }));
    }

    // Check if xorb already exists
    // Note: There is a TOCTOU race between exists() and put() below.
    // For content-addressed storage this is acceptable because:
    // 1. Same hash = same content, so concurrent uploads are idempotent
    // 2. The was_inserted field may be inaccurate under concurrency, but this
    //    only affects metrics/dedup accounting, not data integrity
    // For strict dedup accounting, storage backends should implement put_if_absent.
    let xorb_key = format!("xorbs/{}/{}", prefix, hash_str);
    let already_exists = match storage.exists(&xorb_key).await {
        Ok(exists) => exists,
        Err(e) => {
            error!("Failed to check xorb existence: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }));
        }
    };

    if already_exists {
        GLOBAL_METRICS.record_request(200);
        GLOBAL_METRICS.record_storage_operation();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Ok().json(XorbUploadResponse {
            was_inserted: false,
        });
    }

    // Store xorb
    if let Err(e) = storage.put(&xorb_key, body.clone()).await {
        error!("Failed to store xorb: {}", e);
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Storage error: {}", e)
        }));
    }

    info!("Uploaded xorb {} ({} bytes)", hash_str, body.len());

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(body.len() as u64);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(XorbUploadResponse {
        was_inserted: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{JwtClaims, create_jwt};
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use actix_web::{test, web, App};
    use tempfile::tempdir;

    #[actix_web::test]
    async fn test_upload_xorb_unauthorized() {
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

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb))
        ).await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::post()
            .uri(&format!("/v1/xorbs/default/{}", hash))
            .set_payload(vec![0u8; 100])
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_upload_xorb_invalid_prefix() {
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

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb))
        ).await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::post()
            .uri(&format!("/v1/xorbs/invalid/{}", hash))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(vec![0u8; 100])
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }
}
