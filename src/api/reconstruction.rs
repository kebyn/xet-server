//! Reconstruction API
//!
//! GET /v1/reconstructions/{file_id} - Get file reconstruction information (V1 format)
//! GET /v2/reconstructions/{file_id} - Get file reconstruction information (V2 format)

use actix_web::{HttpResponse, web};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tracing::error;

use crate::api::auth::AuthVerifier;
use crate::api::guard::{AuthNeed, require_auth};
use crate::config::ServerConfig;
use crate::format::shard::MDBShardFile;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::reconstruction_plan::build_file_chunk_plan;
use crate::storage::StorageBackend;
use crate::types::MerkleHash;

// V1 Response structures
#[derive(Serialize)]
struct ReconstructionResponseV1 {
    file_id: String,
    xorbs: Vec<XorbInfoV1>,
}

#[derive(Serialize)]
struct XorbInfoV1 {
    xorb_hash: String,
    size: u64,
    chunks: Vec<ChunkInfoV1>,
}

#[derive(Serialize)]
struct ChunkInfoV1 {
    chunk_hash: String,
    offset: u64,
    length: u64,
}

// V2 Response structures (with fetch_info)
#[derive(Serialize)]
struct ReconstructionResponseV2 {
    file_id: String,
    xorbs: Vec<XorbInfoV2>,
    fetch_info: HashMap<String, XorbFetchInfo>,
}

#[derive(Serialize)]
struct XorbInfoV2 {
    xorb_hash: String,
    size: u64,
}

#[derive(Serialize)]
struct XorbFetchInfo {
    storage_path: String,
    size: u64,
}

/// Fetch and parse a shard from storage.
///
/// This is a shared helper used by both `get_reconstruction_v1` and
/// `reconstruct_from_xet` to reduce code duplication.
///
/// Returns the parsed shard or an error message. The caller is responsible
/// for recording metrics and converting errors to HTTP responses.
pub async fn fetch_and_parse_shard(
    shard_id: &str,
    storage: &dyn StorageBackend,
) -> Result<MDBShardFile, String> {
    let shard_key = format!("shards/{}", shard_id);
    let shard_data = storage
        .get(&shard_key)
        .await
        .map_err(|e| format!("Failed to fetch shard {}: {}", shard_id, e))?;

    GLOBAL_METRICS.record_storage_operation();

    MDBShardFile::parse(&shard_data)
        .map_err(|e| format!("Failed to parse shard {}: {}", shard_id, e))
}

/// Get file reconstruction information (V1 format)
/// Returns detailed chunk-level information for backward compatibility
pub async fn get_reconstruction_v1(
    path: web::Path<String>,
    index: web::Data<MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    _config: web::Data<ServerConfig>,
    auth: web::Data<AuthVerifier>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::ScopeMsg("read", "Insufficient scope, 'read' required"),
    ) {
        return rej.respond(start);
    }

    let file_id = path.into_inner();

    // Validate file_id format (should be a hex hash)
    if file_id.len() != 64 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid file_id format, expected 64-character hex string"
        }));
    }

    let target_hash = match MerkleHash::from_hex(&file_id) {
        Ok(hash) => hash,
        Err(e) => {
            error!("Invalid file_id {}: {}", file_id, e);
            GLOBAL_METRICS.record_request(400);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid file_id format, expected 64-character hex string"
            }));
        }
    };

    // Look up verified shard references for this file. Each ref is a full-file
    // candidate, so use one complete plan rather than merging candidate metadata.
    let file_refs = match index.get_file_refs(&file_id) {
        Some(refs) if !refs.is_empty() => refs,
        None => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
        Some(_) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
    };

    let mut selected_xorbs = None;
    let mut first_candidate_error: Option<String> = None;
    'candidate: for file_ref in file_refs {
        // Fetch and parse shard using shared helper
        let shard = match fetch_and_parse_shard(&file_ref.shard_id, &***storage).await {
            Ok(s) => s,
            Err(e) => {
                if first_candidate_error.is_none() {
                    first_candidate_error = Some(format!(
                        "failed to fetch/parse shard {}: {}",
                        file_ref.shard_id, e
                    ));
                }
                continue;
            }
        };

        let plan = match build_file_chunk_plan(&shard, &target_hash, Some(file_ref.file_index)) {
            Ok(plan) => plan,
            Err(e) => {
                if first_candidate_error.is_none() {
                    first_candidate_error = Some(format!(
                        "failed to build reconstruction plan for shard {} file index {}: {}",
                        file_ref.shard_id, file_ref.file_index, e
                    ));
                }
                continue;
            }
        };

        let xorb_sizes: HashMap<String, u64> = shard
            .xorb_entries
            .iter()
            .map(|entry| (entry.xorb_hash.to_hex(), entry.num_bytes_in_xorb as u64))
            .collect();
        let mut xorbs = Vec::new();
        let mut seen_xorbs = HashSet::new();
        let mut xorb_positions = HashMap::new();

        for planned in &plan.chunks {
            let xorb_hash = planned.xorb_hash.to_hex();
            if seen_xorbs.insert(xorb_hash.clone()) {
                let Some(xorb_size) = xorb_sizes.get(&xorb_hash).copied() else {
                    if first_candidate_error.is_none() {
                        first_candidate_error = Some(format!(
                            "planned xorb {} for file {} missing from shard {}",
                            xorb_hash, file_id, file_ref.shard_id
                        ));
                    }
                    continue 'candidate;
                };

                xorb_positions.insert(xorb_hash.clone(), xorbs.len());
                xorbs.push(XorbInfoV1 {
                    xorb_hash: xorb_hash.clone(),
                    size: xorb_size,
                    chunks: Vec::new(),
                });
            }

            let xorb_position = xorb_positions[&xorb_hash];
            xorbs[xorb_position].chunks.push(ChunkInfoV1 {
                chunk_hash: planned.raw_chunk_hash.to_hex(),
                offset: planned.chunk_byte_range_start as u64,
                length: planned.unpacked_segment_bytes as u64,
            });
        }

        selected_xorbs = Some(xorbs);
        break;
    }

    let xorbs = match selected_xorbs {
        Some(xorbs) => xorbs,
        None => {
            error!(
                "Failed to build reconstruction metadata for {}: {}",
                file_id,
                first_candidate_error
                    .as_deref()
                    .unwrap_or("no verified shard reference could be planned")
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to build reconstruction plan"
            }));
        }
    };

    // Calculate total download bytes (sum of all xorb sizes)
    let total_download_bytes: u64 = xorbs.iter().map(|x| x.size).sum();

    let response = ReconstructionResponseV1 { file_id, xorbs };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(total_download_bytes);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}

/// Get file reconstruction information (V2 format)
/// Returns xorb-level information with fetch_info for efficient retrieval
pub async fn get_reconstruction(
    path: web::Path<String>,
    index: web::Data<MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    _config: web::Data<ServerConfig>,
    auth: web::Data<AuthVerifier>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::ScopeMsg("read", "Insufficient scope, 'read' required"),
    ) {
        return rej.respond(start);
    }

    let file_id = path.into_inner();

    // Validate file_id format (should be a hex hash)
    if file_id.len() != 64 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid file_id format, expected 64-character hex string"
        }));
    }

    let target_hash = match MerkleHash::from_hex(&file_id) {
        Ok(hash) => hash,
        Err(e) => {
            error!("Invalid file_id {}: {}", file_id, e);
            GLOBAL_METRICS.record_request(400);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid file_id format, expected 64-character hex string"
            }));
        }
    };

    // Look up verified shard references for this file. Each ref is a full-file
    // candidate, so use one complete plan rather than merging candidate metadata.
    let file_refs = match index.get_file_refs(&file_id) {
        Some(refs) if !refs.is_empty() => refs,
        None => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
        Some(_) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
    };

    let mut selected = None;
    let mut first_candidate_error: Option<String> = None;
    for file_ref in file_refs {
        // Fetch and parse shard using shared helper
        let shard = match fetch_and_parse_shard(&file_ref.shard_id, &***storage).await {
            Ok(s) => s,
            Err(e) => {
                if first_candidate_error.is_none() {
                    first_candidate_error = Some(format!(
                        "failed to fetch/parse shard {}: {}",
                        file_ref.shard_id, e
                    ));
                }
                continue;
            }
        };

        let plan = match build_file_chunk_plan(&shard, &target_hash, Some(file_ref.file_index)) {
            Ok(plan) => plan,
            Err(e) => {
                if first_candidate_error.is_none() {
                    first_candidate_error = Some(format!(
                        "failed to build reconstruction plan for shard {} file index {}: {}",
                        file_ref.shard_id, file_ref.file_index, e
                    ));
                }
                continue;
            }
        };

        let planned_xorbs: HashSet<String> = plan
            .chunks
            .iter()
            .map(|chunk| chunk.xorb_hash.to_hex())
            .collect();

        // Extract planned xorb information (deduplicated)
        let mut xorbs = Vec::new();
        let mut fetch_info = HashMap::new();
        let mut seen_xorbs = HashSet::new();
        for xorb_entry in &shard.xorb_entries {
            let xorb_hash = xorb_entry.xorb_hash.to_hex();
            if !planned_xorbs.contains(&xorb_hash) {
                continue;
            }

            let xorb_size = xorb_entry.num_bytes_in_xorb as u64;
            // C1 fix: Use xorbs/{hash} format to match conversion pipeline and LFS download.
            let storage_path = format!("xorbs/{}", xorb_hash);

            // Only add to xorbs vec if not seen before
            if seen_xorbs.insert(xorb_hash.clone()) {
                xorbs.push(XorbInfoV2 {
                    xorb_hash: xorb_hash.clone(),
                    size: xorb_size,
                });

                fetch_info.insert(
                    xorb_hash,
                    XorbFetchInfo {
                        storage_path,
                        size: xorb_size,
                    },
                );
            }
        }

        selected = Some((xorbs, fetch_info));
        break;
    }

    let (xorbs, fetch_info) = match selected {
        Some(selected) => selected,
        None => {
            error!(
                "Failed to build reconstruction metadata for {}: {}",
                file_id,
                first_candidate_error
                    .as_deref()
                    .unwrap_or("no verified shard reference could be planned")
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to build reconstruction plan"
            }));
        }
    };

    // Calculate total download bytes (sum of all xorb sizes)
    let total_download_bytes: u64 = xorbs.iter().map(|x| x.size).sum();

    let response = ReconstructionResponseV2 {
        file_id,
        xorbs,
        fetch_info,
    };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(total_download_bytes);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use actix_web::{App, test, web};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

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
            private_key_path: None,
            signing_kid: None,
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
            token_type: "user".to_string(),
            oid: None,
            operation: None,
        };

        sign_xet_token(&claims, kp).unwrap()
    }

    #[actix_web::test]
    async fn test_reconstruction_not_found() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(auth))
                .route(
                    "/v2/reconstructions/{file_id}",
                    web::get().to(get_reconstruction),
                ),
        )
        .await;

        let file_id = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v2/reconstructions/{}", file_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn test_reconstruction_invalid_file_id() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(auth))
                .route(
                    "/v2/reconstructions/{file_id}",
                    web::get().to(get_reconstruction),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v2/reconstructions/invalid")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_reconstruction_v1_not_found() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(auth))
                .route(
                    "/v1/reconstructions/{file_id}",
                    web::get().to(get_reconstruction_v1),
                ),
        )
        .await;

        let file_id = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/reconstructions/{}", file_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn test_reconstruction_v1_invalid_file_id() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(auth))
                .route(
                    "/v1/reconstructions/{file_id}",
                    web::get().to(get_reconstruction_v1),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/reconstructions/invalid")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_reconstruction_v1_returns_only_requested_file_chunks() {
        use crate::format::compression::CompressionScheme;
        use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
        use crate::format::xorb_builder::XorbBuilder;
        use crate::hash::compute_data_hash;
        use crate::index::{VerifiedChunkMapping, VerifiedFileMapping, VerifiedShardRegistration};
        use crate::types::MerkleHash;

        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read");
        let index = MetadataIndex::new();

        let raw_a = b"aaa";
        let raw_b = b"bbb";
        let mut xb = XorbBuilder::new(CompressionScheme::None);
        let mut xorb_chunks = Vec::new();
        let mut raw_hashes = Vec::new();
        let mut offset = 0u32;
        for raw in [raw_a.as_slice(), raw_b.as_slice()] {
            raw_hashes.push(compute_data_hash(raw));
            let (serialized_hash, compressed_len) = xb.add_chunk(raw).unwrap();
            xorb_chunks.push(XorbChunkBuildEntry {
                chunk_hash: serialized_hash,
                chunk_byte_range_start: offset,
                unpacked_segment_bytes: raw.len() as u32,
            });
            offset += 8 + compressed_len;
        }
        let xorb = xb.build().unwrap();
        let file_a = MerkleHash::from([1u8; 32]);
        let file_b = MerkleHash::from([2u8; 32]);
        let mut sb = ShardBuilder::new();
        let xorb_index = sb
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                xorb_chunks,
                raw_hashes.clone(),
            )
            .unwrap();
        sb.add_file(
            file_a,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: 3,
            }],
        );
        sb.add_file(
            file_b,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 1,
                chunk_index_end: 2,
                unpacked_segment_bytes: 3,
            }],
        );
        let shard_data = sb.build().unwrap();
        let shard_id = compute_data_hash(&shard_data).to_hex();
        storage
            .put(
                &format!("shards/{}", shard_id),
                bytes::Bytes::from(shard_data),
            )
            .await
            .unwrap();
        index.register_verified_shard(VerifiedShardRegistration {
            shard_id,
            files: vec![
                VerifiedFileMapping {
                    file_hash: file_a.to_hex(),
                    file_index: 0,
                },
                VerifiedFileMapping {
                    file_hash: file_b.to_hex(),
                    file_index: 1,
                },
            ],
            chunks: vec![
                VerifiedChunkMapping {
                    chunk_hash: raw_hashes[0].to_hex(),
                    xorb_hash: xorb.xorb_hash.to_hex(),
                    chunk_index: 0,
                },
                VerifiedChunkMapping {
                    chunk_hash: raw_hashes[1].to_hex(),
                    xorb_hash: xorb.xorb_hash.to_hex(),
                    chunk_index: 1,
                },
            ],
        });

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(auth))
                .route(
                    "/v1/reconstructions/{file_id}",
                    web::get().to(get_reconstruction_v1),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri(&format!("/v1/reconstructions/{}", file_b.to_hex()))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        let chunks = body["xorbs"][0]["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["chunk_hash"], raw_hashes[1].to_hex());
    }
}
