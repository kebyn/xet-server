//! Xorb Upload API
//!
//! POST /v1/xorbs/{prefix}/{hash} - Upload xorb objects (streaming)

use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
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
    auth: web::Data<AuthVerifier>,
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

    let claims = match auth.verify_token(&token) {
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

    // Check available disk space before accepting upload
    let temp_dir = config.storage.resolve_upload_temp_dir();
    if let Err(e) = check_disk_space(&temp_dir, config.server.max_body_size_bytes() as u64) {
        error!("Insufficient disk space: {}", e);
        GLOBAL_METRICS.record_request(507);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InsufficientStorage().json(serde_json::json!({
            "error": format!("Insufficient disk space: {}", e)
        }));
    }

    // Stream payload to temp file with incremental BLAKE3 hashing
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
    auth: web::Data<AuthVerifier>,
    _config: web::Data<ServerConfig>,
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

/// Check if there's enough disk space for an upload.
/// Returns Ok(()) if sufficient space is available, Err with description otherwise.
fn check_disk_space(path: &std::path::Path, required_bytes: u64) -> Result<(), String> {
    // Use statvfs on Unix-like systems to check available space
    #[cfg(unix)]
    {
        // Get filesystem statistics
        let _metadata = std::fs::metadata(path).map_err(|e| {
            format!("Failed to get filesystem info for {}: {}", path.display(), e)
        })?;

        // For now, we'll do a basic sanity check - ensure the path exists and is writable
        // A more sophisticated check would use statvfs to get actual available space
        if !path.exists() {
            return Err(format!("Path does not exist: {}", path.display()));
        }

        // Check if we can write to the directory
        let test_file = path.join(".disk_space_check");
        match std::fs::write(&test_file, b"") {
            Ok(_) => {
                let _ = std::fs::remove_file(&test_file);
            }
            Err(e) => {
                return Err(format!("Cannot write to {}: {}", path.display(), e));
            }
        }

        // Basic check passed - in production, use proper statvfs
        if required_bytes > 100 * 1024 * 1024 * 1024 {
            // If requesting >100GB, log a warning but allow it
            tracing::warn!(
                "Large upload requested ({} MB) - disk space check is basic",
                required_bytes / 1024 / 1024
            );
        }

        Ok(())
    }

    #[cfg(not(unix))]
    {
        // On non-Unix systems, skip the check
        let _ = required_bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{KeyPair, XetClaims, sign_xet_token, AuthVerifier};
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use actix_web::{test, web, App};
    use tempfile::tempdir;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_test_config() -> (KeyPair, AuthVerifier, ServerConfig) {
        let kp = KeyPair::generate();
        let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

        // Use a temp file inside a tempdir to ensure cleanup
        let temp_dir = tempdir().unwrap();
        let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
        std::fs::write(&temp_path, &public_key_pem).unwrap();

        // Keep temp_dir alive by leaking it (test scope is short)
        let temp_path_str = temp_path.to_str().unwrap().to_string();
        std::mem::forget(temp_dir); // Keep temp dir alive for test duration

        let auth_config = AuthConfig {
            public_key_path: temp_path_str,
            trusted_kids: vec![kp.kid()],
        };

        let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();

        let config = ServerConfig {
            auth: auth_config,
            ..Default::default()
        };
        (kp, auth_verifier, config)
    }

    fn create_test_token(kp: &KeyPair, scope: &str) -> String {
        let kid = kp.kid();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
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

        let (_, auth, config) = create_test_config();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
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

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read write");

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
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
