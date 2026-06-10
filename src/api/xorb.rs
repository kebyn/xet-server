//! Xorb Upload API
//!
//! POST /v1/xorbs/{prefix}/{hash} - Upload xorb objects (streaming)

use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::{check_scope, extract_token_from_request, verify_token};
use crate::config::ServerConfig;
use crate::storage::{StorageBackend, StorageError};
use crate::types::MerkleHash;
use crate::metrics::GLOBAL_METRICS;
use crate::util::{StreamingHasher, TempFile};

#[derive(Serialize)]
struct XorbUploadResponse {
    was_inserted: bool,
}

/// Upload a xorb object via streaming.
///
/// Data is streamed to a temp file with incremental BLAKE3 hashing,
/// then verified from disk and moved to final storage via rename.
pub async fn upload_xorb(
    path: web::Path<(String, String)>,
    mut payload: web::Payload,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let (prefix, hash_str) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Parse hash
    let expected_hash = match MerkleHash::from_hex(&hash_str) {
        Ok(h) => h,
        Err(e) => {
            GLOBAL_METRICS.record_request(400);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid hash format: {}", e)
            }));
        }
    };

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

    let claims = match verify_token(&token, &config.auth) {
        Ok(c) => c,
        Err(_) => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token"
            }))
        }
    };

    if !check_scope(&claims, "write") {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope"
        }));
    }

    // Stream payload to temp file with incremental BLAKE3 hashing
    let temp_dir = config.storage.resolve_upload_temp_dir();
    let mut temp_file = match TempFile::create(&temp_dir).await {
        Ok(tf) => tf,
        Err(e) => {
            error!("Failed to create temp file: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to create temp file: {}", e)
            }));
        }
    };

    let mut hasher = StreamingHasher::new();
    let max_bytes = config.server.max_body_size_bytes() as u64;
    let mut total_bytes: u64 = 0;

    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                error!("Payload stream error: {}", e);
                GLOBAL_METRICS.record_request(400);
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": format!("Upload stream error: {}", e)
                }));
            }
        };

        total_bytes += chunk.len() as u64;
        if total_bytes > max_bytes {
            GLOBAL_METRICS.record_request(413);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::PayloadTooLarge().json(serde_json::json!({
                "error": format!("Upload exceeds maximum size of {} MB", config.server.max_body_size_mb)
            }));
        }

        hasher.update(&chunk);
        if let Err(e) = temp_file.write_all(&chunk).await {
            error!("Failed to write to temp file: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to write upload data: {}", e)
            }));
        }
    }

    if let Err(e) = temp_file.sync_all().await {
        error!("Failed to sync temp file: {}", e);
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Failed to sync upload data: {}", e)
        }));
    }

    // Verify xorb hash
    let actual_hash = hasher.finalize();
    if actual_hash != expected_hash {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Hash mismatch: expected {}, got {}", expected_hash.to_hex(), actual_hash.to_hex())
        }));
    }

    // Verify xorb structure and chunk hashes from temp file on disk
    let temp_path = temp_file.path().to_path_buf();
    if let Err(e) = crate::format::xorb::verify_xorb_from_file(&temp_path) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Xorb verification failed: {}", e)
        }));
    }

    // Check if xorb already exists.
    // Note: There is a TOCTOU race between exists() and put_from_path() below.
    // For content-addressed storage this is acceptable because:
    // 1. Same hash = same content, so concurrent uploads are idempotent
    // 2. The was_inserted field is best-effort under concurrency — it may not
    //    reflect which concurrent writer actually won the race, but this only
    //    affects metrics/dedup accounting, not data integrity.
    // For strict dedup accounting, storage backends should implement put_if_absent.
    let xorb_key = format!("xorbs/{}/{}", prefix, hash_str);
    let already_exists = match storage.exists(&xorb_key).await {
        Ok(exists) => exists,
        Err(e) => {
            error!("Failed to check xorb existence: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }));
        }
    };

    if already_exists {
        GLOBAL_METRICS.record_request(200);
        GLOBAL_METRICS.record_storage_operation();
        GLOBAL_METRICS.record_latency(start);
        // temp_file auto-cleaned by Drop
        return HttpResponse::Ok().json(XorbUploadResponse {
            was_inserted: false,
        });
    }

    // Move temp file to final storage (zero-copy rename for local storage)
    let temp_path = temp_file.into_path();
    if let Err(e) = storage.put_from_path(&xorb_key, &temp_path).await {
        error!("Failed to store xorb: {}", e);
        let _ = std::fs::remove_file(&temp_path);
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Storage error: {}", e)
        }));
    }

    info!("Uploaded xorb {} ({} bytes)", hash_str, total_bytes);

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(total_bytes);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(XorbUploadResponse {
        was_inserted: true,
    })
}

/// Download a xorb object
pub async fn download_xorb(
    path: web::Path<(String, String)>,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let (prefix, hash_str) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Validate hash format
    if hash_str.len() != 64 || !hash_str.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid hash format, expected 64-character hex string"
        }));
    }

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

    let claims = match verify_token(&token, &config.auth) {
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
            "error": "Insufficient scope"
        }));
    }

    // Fetch xorb from storage
    let xorb_key = format!("xorbs/{}/{}", prefix, hash_str);
    let xorb_data = match storage.get(&xorb_key).await {
        Ok(data) => {
            GLOBAL_METRICS.record_storage_operation();
            data
        }
        Err(StorageError::NotFound(_)) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": format!("Xorb not found: {}", hash_str)
            }));
        }
        Err(e) => {
            error!("Failed to fetch xorb: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }));
        }
    };

    info!("Downloaded xorb {} ({} bytes)", hash_str, xorb_data.len());

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(xorb_data.len() as u64);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(xorb_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{KeyPair, XetClaims, sign_xet_token};
    use crate::config::{AuthConfig, StateConfig};
    use crate::storage::local::LocalStorage;
    use actix_web::{test, web, App};
    use tempfile::tempdir;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_test_config() -> (KeyPair, ServerConfig) {
        let kp = KeyPair::generate();
        let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

        // Use a temp file path that persists for the test
        let temp_path = format!("/tmp/xet-test-pubkey-{}.pem", kp.kid());
        std::fs::write(&temp_path, &public_key_pem).unwrap();

        let config = ServerConfig {
            auth: AuthConfig {
                public_key_path: temp_path,
                trusted_kids: vec![kp.kid()],
                token_prefix: "xet_".to_string(),
            },
            state: StateConfig {
                sqlite_path: "/tmp/xet-test-state.db".to_string(),
            },
            ..Default::default()
        };
        (kp, config)
    }

    fn create_test_token(kp: &KeyPair, scope: &str) -> String {
        let kid = kp.kid();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize;
        let claims = XetClaims {
            sub: "test".to_string(),
            scope: scope.to_string(),
            repo_id: "test/repo".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: now + 3600,
            iat: now,
            kid,
        };
        sign_xet_token(&claims, kp).unwrap()
    }

    #[actix_web::test]
    async fn test_upload_xorb_unauthorized() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let (_, config) = create_test_config();

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

        let (kp, config) = create_test_config();
        let token = create_test_token(&kp, "read write");

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
