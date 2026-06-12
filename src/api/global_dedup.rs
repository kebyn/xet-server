//! Global Deduplication API
//!
//! GET /v1/chunks/{prefix}/{hash} - Query chunk deduplication information

use actix_web::{web, HttpResponse};
use serde::{Serialize, Deserialize};

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;

#[derive(Serialize, Deserialize)]
struct ChunkDedupResponse {
    hash: String,
    found: bool,
    xorb_hash: Option<String>,
    chunk_index: Option<u32>,
}

/// Query chunk deduplication information
pub async fn query_chunk_dedup(
    path: web::Path<(String, String)>,
    index: web::Data<MetadataIndex>,
    _storage: web::Data<Box<dyn StorageBackend>>,
    auth: web::Data<AuthVerifier>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    // Extract and validate auth token
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing or invalid authorization token"
            }));
        }
    };

    let claims = match auth.verify_token(&token) {
        Ok(c) => c,
        Err(_) => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token"
            }));
        }
    };

    if !check_scope(&claims, "read") {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope, 'read' required"
        }));
    }

    let (prefix, hash) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Validate hash format (should be a hex hash)
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid hash format, expected 64-character hex string"
        }));
    }

    // Look up chunk in metadata index
    let response = match index.get_xorb_for_chunk(&hash) {
        Some((xorb_hash, chunk_index)) => {
            ChunkDedupResponse {
                hash,
                found: true,
                xorb_hash: Some(xorb_hash),
                chunk_index: Some(chunk_index),
            }
        }
        None => {
            ChunkDedupResponse {
                hash,
                found: false,
                xorb_hash: None,
                chunk_index: None,
            }
        }
    };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};
    use crate::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
    use crate::config::{AuthConfig, ServerConfig};
    use crate::storage::local::LocalStorage;
    use tempfile::tempdir;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_test_config() -> (KeyPair, AuthVerifier, ServerConfig) {
        let kp = KeyPair::generate();
        let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

        let temp_dir = tempdir().unwrap();
        let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
        std::fs::write(&temp_path, &public_key_pem).unwrap();

        let temp_path_str = temp_path.to_str().unwrap().to_string();
        std::mem::forget(temp_dir);

        let auth_config = AuthConfig {
            public_key_path: temp_path_str,
            trusted_kids: vec![kp.kid()],
        };

        let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();
        let config = ServerConfig::default();

        (kp, auth_verifier, config)
    }

    fn create_test_token(kp: &KeyPair, scope: &str) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims = XetClaims {
            sub: "test-user".to_string(),
            scope: scope.to_string(),
            repo_id: "test/repo".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: now + 3600,
            iat: now,
            kid: kp.kid(),
        };

        sign_xet_token(&claims, kp).unwrap()
    }

    #[actix_web::test]
    async fn test_chunk_dedup_not_found() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let (kp, auth, _config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .route("/v1/chunks/{prefix}/{hash}", web::get().to(query_chunk_dedup))
        ).await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/chunks/default/{}", hash))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: ChunkDedupResponse = test::read_body_json(resp).await;
        assert!(!body.found);
        assert_eq!(body.hash, hash);
    }

    #[actix_web::test]
    async fn test_chunk_dedup_invalid_prefix() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let (kp, auth, _config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .route("/v1/chunks/{prefix}/{hash}", web::get().to(query_chunk_dedup))
        ).await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/chunks/invalid/{}", hash))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_chunk_dedup_invalid_hash() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let (kp, auth, _config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .route("/v1/chunks/{prefix}/{hash}", web::get().to(query_chunk_dedup))
        ).await;

        let req = test::TestRequest::get()
            .uri("/v1/chunks/default/invalid_hash")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }
}
