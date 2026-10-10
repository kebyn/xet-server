//! Shard Upload API
//!
//! POST /v1/shards - Upload metadata shards (streaming)

use actix_web::{HttpResponse, web};
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::AuthVerifier;
use crate::api::guard::{AuthNeed, require_auth};
use crate::config::ServerConfig;
use crate::format::shard::MDBShardFile;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::shard_io::parse_shard_from_storage;
use crate::shard_validation::validate_shard_for_index;
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
    temp_quota: Option<web::Data<xet_common::TempQuotaLedger>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(&req, &auth, AuthNeed::Scope("write")) {
        return rej.respond();
    }

    // Stream payload to temp file with incremental BLAKE3 hashing
    let temp_dir = config.storage.resolve_upload_temp_dir();
    // use the same pre-check threshold as the xorb/LFS upload paths
    // (min(max_body_size, 100MB)) so all upload entry points behave alike.
    let check_bytes = std::cmp::min(
        config.server.max_body_size_bytes() as u64,
        100 * 1024 * 1024,
    );
    if let Err(e) = crate::util::disk::ensure_dir_and_check_space(&temp_dir, check_bytes).await {
        error!("Insufficient disk space: {}", e);
        return HttpResponse::ServiceUnavailable()
            .insert_header((actix_web::http::header::RETRY_AFTER, "5"))
            .json(serde_json::json!({"error": "Temporary storage unavailable"}));
    }
    let temp_file_result = match temp_quota {
        Some(ledger) => TempFile::create_with_ledger(&temp_dir, ledger.get_ref().clone()).await,
        None => {
            TempFile::create_with_quota(
                &temp_dir,
                config.storage.temp_quota_bytes,
                config.storage.temp_min_free_bytes,
            )
            .await
        }
    };
    let mut temp_file = match temp_file_result {
        Ok(tf) => tf,
        Err(e) => {
            error!("Failed to create temp file: {}", e);
            if matches!(e, crate::storage::StorageError::Quota) {
                return HttpResponse::ServiceUnavailable()
                    .insert_header((actix_web::http::header::RETRY_AFTER, "5"))
                    .json(serde_json::json!({"error": "Temporary storage unavailable"}));
            }
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    };

    let mut hasher = StreamingHasher::new();
    let max_bytes = config.server.max_body_size_bytes() as u64;
    let total_bytes = match crate::util::payload_stream::stream_payload_to_temp(
        &mut payload,
        &mut temp_file,
        max_bytes,
        |chunk| hasher.update(chunk),
    )
    .await
    {
        Ok(n) => n,
        Err(e) => return e.error_response(),
    };

    // Fully validate the shard from disk before put_from_path transfers ownership.
    // Parse off the async runtime — shard parsing is synchronous file I/O
    // (same pattern as shard_io::parse_path).
    let parse_path = temp_file.path().to_path_buf();
    let shard =
        match tokio::task::spawn_blocking(move || MDBShardFile::parse_from_file(&parse_path)).await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                error!("Failed to parse shard for indexing: {}", e);
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Invalid shard format"
                }));
            }
            Err(join_err) => {
                error!("Shard parse task failed: {}", join_err);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": crate::api::INTERNAL_ERROR_MESSAGE
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
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    };

    if already_exists {
        let validation_temp_dir = config.storage.resolve_reconstruction_temp_dir();
        let stored_shard = match parse_shard_from_storage(
            storage.get_ref().as_ref(),
            &shard_key,
            &validation_temp_dir,
        )
        .await
        {
            Ok(shard) => shard,
            Err(e) => {
                error!("Failed to read or parse existing shard {}: {}", shard_id, e);
                if matches!(&e, crate::shard_io::ShardIoError::Parse { source, .. }
                    if !matches!(source, crate::error::XetError::IoError(_)))
                {
                    return HttpResponse::BadRequest().json(serde_json::json!({
                        "error": "Invalid existing shard format"
                    }));
                }

                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": crate::api::INTERNAL_ERROR_MESSAGE
                }));
            }
        };
        let stored_shard_id = stored_shard.compute_hash();
        if stored_shard_id != shard_id {
            error!(
                "Existing shard {} has mismatched stored content hash {}",
                shard_id, stored_shard_id
            );
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Existing shard content does not match requested shard id"
            }));
        }
        let registration = match validate_shard_for_index(
            &shard_id,
            &stored_shard,
            storage.get_ref().as_ref(),
            &validation_temp_dir,
        )
        .await
        {
            Ok(registration) => registration,
            Err(e) => {
                error!(
                    "Shard validation failed for existing shard {}: {}",
                    shard_id, e
                );
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Shard validation failed"
                }));
            }
        };
        if let Err(error) = index.register_verified_shard(registration) {
            error!(
                "Metadata index rejected existing shard {}: {}",
                shard_id, error
            );
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": format!("Metadata index conflict: {}", error)
            }));
        }

        GLOBAL_METRICS.record_storage_operation();
        return HttpResponse::Ok().json(ShardUploadResponse {
            was_inserted: false,
            shard_id,
        });
    }

    let validation_temp_dir = config.storage.resolve_reconstruction_temp_dir();
    let registration = match validate_shard_for_index(
        &shard_id,
        &shard,
        storage.get_ref().as_ref(),
        &validation_temp_dir,
    )
    .await
    {
        Ok(registration) => registration,
        Err(e) => {
            error!("Shard validation failed for shard {}: {}", shard_id, e);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Shard validation failed"
            }));
        }
    };

    // Store only after validation succeeds. TempFile cleans remote-upload
    // sources and error paths; local storage may consume the path by rename.
    if let Err(e) = temp_file
        .store(storage.get_ref().as_ref(), &shard_key)
        .await
    {
        error!("Failed to store shard: {}", e);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": crate::api::INTERNAL_ERROR_MESSAGE
        }));
    }

    let file_count = registration.files.len();
    let chunk_count = registration.chunks.len();
    if let Err(error) = index.register_verified_shard(registration) {
        error!("Metadata index rejected shard {}: {}", shard_id, error);
        return HttpResponse::Conflict().json(serde_json::json!({
            "error": format!("Metadata index conflict: {}", error)
        }));
    }

    info!(
        "Uploaded shard {} with {} files and {} chunks",
        shard_id, file_count, chunk_count
    );

    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(total_bytes);

    HttpResponse::Ok().json(ShardUploadResponse {
        was_inserted: true,
        shard_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
    use crate::config::AuthConfig;
    use crate::format::compression::CompressionScheme;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::format::xorb_builder::XorbBuilder;
    use crate::hash::compute_data_hash;
    use crate::storage::local::LocalStorage;
    use crate::types::MerkleHash;
    use actix_web::{App, test, web};
    use bytes::Bytes;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

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
            public_keys: Vec::new(),
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

    fn sha256_merkle_hash(data: &[u8]) -> crate::types::MerkleHash {
        let digest = Sha256::digest(data);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        crate::types::MerkleHash::from(bytes)
    }

    fn build_one_chunk_shard_data(raw_chunk: &[u8]) -> Vec<u8> {
        let mut xorb_builder = XorbBuilder::new(CompressionScheme::None);
        let (serialized_chunk_hash, compressed_len) = xorb_builder.add_chunk(raw_chunk).unwrap();
        let xorb = xorb_builder.build().unwrap();
        let raw_chunk_hash = compute_data_hash(raw_chunk);

        let mut shard_builder = ShardBuilder::new();
        let xorb_index = shard_builder
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                vec![XorbChunkBuildEntry {
                    chunk_hash: serialized_chunk_hash,
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: raw_chunk.len() as u32,
                }],
                vec![raw_chunk_hash],
            )
            .unwrap();

        assert_eq!(compressed_len as usize, raw_chunk.len());
        shard_builder.add_file(
            sha256_merkle_hash(raw_chunk),
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: raw_chunk.len() as u32,
            }],
        );

        shard_builder.build().unwrap()
    }

    fn build_one_chunk_xorb_and_shard_data(raw_chunk: &[u8]) -> (Vec<u8>, Vec<u8>, MerkleHash) {
        let mut xorb_builder = XorbBuilder::new(CompressionScheme::None);
        let (serialized_chunk_hash, compressed_len) = xorb_builder.add_chunk(raw_chunk).unwrap();
        let xorb = xorb_builder.build().unwrap();
        let raw_chunk_hash = compute_data_hash(raw_chunk);
        let file_hash = sha256_merkle_hash(raw_chunk);

        let mut shard_builder = ShardBuilder::new();
        let xorb_index = shard_builder
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                vec![XorbChunkBuildEntry {
                    chunk_hash: serialized_chunk_hash,
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: raw_chunk.len() as u32,
                }],
                vec![raw_chunk_hash],
            )
            .unwrap();

        assert_eq!(compressed_len as usize, raw_chunk.len());
        shard_builder.add_file(
            file_hash,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: raw_chunk.len() as u32,
            }],
        );

        (xorb.data, shard_builder.build().unwrap(), file_hash)
    }

    #[actix_web::test]
    async fn test_upload_shard_unauthorized() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let storage_arc: Arc<Box<dyn StorageBackend>> = Arc::new(storage);

        let (_, auth, config) = create_test_config();

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(storage_arc))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route("/v1/shards", web::post().to(upload_shard)),
        )
        .await;

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
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
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
                .route("/v1/shards", web::post().to(upload_shard)),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/v1/shards")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(vec![0u8; 100]) // Invalid shard data
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_upload_shard_validation_failure_does_not_persist_new_shard() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let storage_arc: Arc<Box<dyn StorageBackend>> = Arc::new(storage);

        let (kp, auth, mut config) = create_test_config();
        config.storage.local_path = Some(dir.path().to_str().unwrap().to_string());
        let token = create_test_token(&kp, "read write");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(storage_arc.clone()))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route("/v1/shards", web::post().to(upload_shard)),
        )
        .await;

        let shard_data = build_one_chunk_shard_data(b"valid shard but missing referenced xorb");
        let shard_id = compute_data_hash(&shard_data).to_hex();
        let shard_key = format!("shards/{}", shard_id);

        let req = test::TestRequest::post()
            .uri("/v1/shards")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(shard_data)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
        assert!(!storage_arc.exists(&shard_key).await.unwrap());
    }

    #[actix_web::test]
    async fn test_upload_existing_shard_validates_durable_storage_before_indexing() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let storage_arc: Arc<Box<dyn StorageBackend>> = Arc::new(storage);

        let (kp, auth, mut config) = create_test_config();
        config.storage.local_path = Some(dir.path().to_str().unwrap().to_string());
        let token = create_test_token(&kp, "read write");

        let index = MetadataIndex::new();
        let index_for_assert = index.clone();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(storage_arc.clone()))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route("/v1/shards", web::post().to(upload_shard)),
        )
        .await;

        let raw_chunk = b"valid shard payload with present xorb";
        let (xorb_data, shard_data, file_hash) = build_one_chunk_xorb_and_shard_data(raw_chunk);
        let shard_id = compute_data_hash(&shard_data).to_hex();
        let shard_key = format!("shards/{}", shard_id);
        let parsed_shard = MDBShardFile::parse(&shard_data).unwrap();
        let xorb_hash = parsed_shard.xorb_entries[0].xorb_hash.to_hex();

        storage_arc
            .put(&format!("xorbs/{}", xorb_hash), Bytes::from(xorb_data))
            .await
            .unwrap();
        storage_arc
            .put(&shard_key, Bytes::from_static(b"not a valid durable shard"))
            .await
            .unwrap();

        let req = test::TestRequest::post()
            .uri("/v1/shards")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(shard_data)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
        assert!(index_for_assert.get_shards_for_file(&file_hash).is_none());
    }
}
