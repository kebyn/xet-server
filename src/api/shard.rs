//! Shard Upload API
//!
//! POST /v1/shards - Upload metadata shards (streaming)

use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::AuthVerifier;
use crate::api::guard::{require_auth, AuthNeed};
use crate::config::ServerConfig;
use crate::format::shard::MDBShardFile;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;
use crate::util::{StreamingHasher, TempFile};

#[derive(Serialize)]
struct ShardUploadResponse {
    was_inserted: bool,
    shard_id: String,
}

/// Upload a metadata shard via streaming.
///
/// Data is streamed to a temp file with incremental BLAKE3 hashing,
/// then parsed from disk and moved to final storage via rename.
pub async fn upload_shard(
    mut payload: web::Payload,
    storage: web::Data<Box<dyn StorageBackend>>,
    index: web::Data<MetadataIndex>,
    auth: web::Data<AuthVerifier>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(&req, &auth, AuthNeed::Scope("write")) {
        return rej.respond(start);
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

    // C1 fix: Read and fully parse the shard BEFORE storing (put_from_path moves the file).
    // The full parse validates format AND provides file_hashes/chunk_mappings for index registration.
    // (Previously used parse_header_footer_from_file which returned empty data sections,
    // causing register_shard to be a no-op — uploaded shards were unusable for reconstruction.)
    let temp_path_for_read = temp_file.path().to_path_buf();
    let shard_data = match std::fs::read(&temp_path_for_read) {
        Ok(data) => data,
        Err(e) => {
            error!("Failed to read shard for indexing: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to read shard data: {}", e)
            }));
        }
    };

    let shard = match MDBShardFile::parse(&shard_data) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to parse shard for indexing: {}", e);
            GLOBAL_METRICS.record_request(400);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid shard format (full parse failed): {}", e)
            }));
        }
    };

    // Use streaming-computed hash as shard ID
    let shard_id = hasher.finalize().to_hex();
    let shard_key = format!("shards/{}", shard_id);

    // Check if shard already exists
    let already_exists = match storage.exists(&shard_key).await {
        Ok(exists) => exists,
        Err(e) => {
            error!("Failed to check shard existence: {}", e);
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
        return HttpResponse::Ok().json(ShardUploadResponse {
            was_inserted: false,
            shard_id,
        });
    }

    // Move temp file to final storage (zero-copy rename for local storage)
    let temp_path = temp_file.into_path();
    if let Err(e) = storage.put_from_path(&shard_key, &temp_path).await {
        error!("Failed to store shard: {}", e);
        let _ = std::fs::remove_file(&temp_path);
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Storage error: {}", e)
        }));
    }

    // Update metadata index with actual file/chunk data from full parse
    let file_hashes: Vec<String> = shard.file_hashes().iter().map(|h| h.to_hex()).collect();
    let chunk_mappings: Vec<(String, String, u32)> = shard
        .chunk_mappings()
        .iter()
        .map(|(c, x, i)| (c.to_hex(), x.to_hex(), *i))
        .collect();

    index.register_shard(shard_id.clone(), file_hashes.clone(), chunk_mappings);

    info!("Uploaded shard {} with {} files and {} chunks",
        shard_id,
        file_hashes.len(),
        shard.chunk_mappings().len()
    );

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(total_bytes);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(ShardUploadResponse {
        was_inserted: true,
        shard_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{KeyPair, XetClaims, sign_xet_token, AuthVerifier};
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use actix_web::{test, web, App};
    use tempfile::tempdir;
    use std::sync::Arc;
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
            private_key_path: None,
            signing_kid: None,
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
            token_type: "user".to_string(),
            oid: None,
            operation: None,
        };
        sign_xet_token(&claims, kp).unwrap()
    }

    #[actix_web::test]
    async fn test_upload_shard_unauthorized() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );
        let storage_arc: Arc<Box<dyn StorageBackend>> = Arc::new(storage);

        let (_, auth, config) = create_test_config();

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(storage_arc))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(auth))
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
        let storage_arc: Arc<Box<dyn StorageBackend>> = Arc::new(storage);

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read write");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(storage_arc))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(auth))
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
