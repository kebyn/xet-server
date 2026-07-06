//! Simple LFS Object Storage API
//!
//! PUT /lfs/objects/{oid} - Upload LFS objects (raw files) — streaming
//! GET /lfs/objects/{oid} - Download LFS objects
//!
//! This provides simple raw file storage compatible with Git LFS,
//! bypassing the Xorb format requirement for basic testing.
//!
//! Uploads stream data from the HTTP payload to a temp file with incremental
//! BLAKE3 hashing, bounding memory to O(chunk_size) regardless of file size.

use actix_web::{HttpResponse, web};
use futures_util::StreamExt;
use std::sync::Arc;
use tracing::{error, info};

use crate::api::auth::AuthVerifier;
use crate::api::guard::{AuthNeed, LfsOperation, require_auth};
use crate::config::{ConversionConfig, ServerConfig};
use crate::conversion::ConvertingOids;
#[cfg(test)]
use crate::format::compression::decompress;
#[cfg(test)]
use crate::format::xorb::XorbChunkHeader;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;
#[cfg(test)]
use crate::types::MerkleHash;
use crate::util::{DualHasher, TempFile};
#[cfg(test)]
use crate::xorb_reader::extract_chunk_verified_from_file;

mod raw;
mod reconstruction;

use raw::{RawBlobResult, serve_raw_blob};
use reconstruction::serve_verified_xet_reconstruction;

/// 从 xorb 原始字节中按偏移定位单个 chunk,校验其完整性后解压。
///
/// C-DATA-2: chunk_hash 覆盖 header(8B)+压缩数据。在解压前对该区域重算 hash
/// 并与 shard 记录值比对,检出磁盘 bit-rot / 存储损坏 / 串改,并先于解压挡住污染数据。
#[cfg(test)]
fn extract_chunk_verified(
    xorb_data: &[u8],
    chunk_offset_bytes: usize,
    expected_hash: &MerkleHash,
) -> std::result::Result<bytes::Bytes, String> {
    if chunk_offset_bytes + XorbChunkHeader::SIZE > xorb_data.len() {
        return Err("Chunk offset out of bounds".to_string());
    }
    let mut chunk_cursor = std::io::Cursor::new(&xorb_data[chunk_offset_bytes..]);
    let chunk_header = XorbChunkHeader::deserialize(&mut chunk_cursor)
        .map_err(|e| format!("Failed to parse chunk header: {}", e))?;

    let data_start = chunk_offset_bytes + XorbChunkHeader::SIZE;
    let data_end = data_start + chunk_header.compressed_length as usize;
    if data_end > xorb_data.len() {
        return Err("Chunk data out of bounds".to_string());
    }

    // 解压前校验 header+压缩数据 区域的完整性。
    let chunk_region = &xorb_data[chunk_offset_bytes..data_end];
    let actual_hash = crate::hash::compute_data_hash(chunk_region);
    if actual_hash != *expected_hash {
        return Err(format!(
            "Chunk hash mismatch at offset {}: stored data is corrupted",
            chunk_offset_bytes
        ));
    }

    let compressed_data = &xorb_data[data_start..data_end];
    let decompressed = decompress(
        chunk_header.compression_scheme,
        compressed_data,
        chunk_header.uncompressed_length as usize,
    )
    .map_err(|e| format!("Failed to decompress chunk: {}", e))?;
    Ok(bytes::Bytes::from(decompressed))
}

/// Upload an LFS object (raw file) via streaming.
///
/// Data is streamed from the HTTP payload to a temp file with incremental
/// BLAKE3 hashing. After the stream completes, the hash is verified against
/// the OID and the temp file is moved to final storage via rename.
/// After successful upload, the blob is registered as raw_only in the state manager.
pub async fn upload_lfs_object(
    path: web::Path<String>,
    mut payload: web::Payload,
    storage: web::Data<Box<dyn StorageBackend>>,
    auth: web::Data<AuthVerifier>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // Validate oid format (should be a hex hash)
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid oid format, expected 64-character hex string"
        }));
    }

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::LfsObject {
            operation: LfsOperation::Upload,
            oid: oid.clone(),
            message: "Insufficient scope or invalid LFS upload token",
        },
    ) {
        return rej.respond(start);
    }

    // M7 fix: Use a more reasonable pre-check threshold (see xorb.rs for rationale).
    let temp_dir = config.storage.resolve_upload_temp_dir();
    let check_bytes = std::cmp::min(
        config.server.max_body_size_bytes() as u64,
        100 * 1024 * 1024,
    );
    if let Err(e) = check_disk_space(&temp_dir, check_bytes) {
        error!("Insufficient disk space: {}", e);
        GLOBAL_METRICS.record_request(507);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InsufficientStorage().json(serde_json::json!({
            "error": format!("Insufficient disk space: {}", e)
        }));
    }

    // Stream payload to temp file with incremental BLAKE3 hashing.
    // Memory usage is bounded to O(chunk_size) regardless of file size.
    let mut temp_file = match TempFile::create(&temp_dir).await {
        Ok(tf) => tf,
        Err(e) => {
            error!("Failed to create temp file: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal storage error"
            }));
        }
    };

    let mut hasher = DualHasher::new();
    let max_bytes = config.server.max_body_size_bytes() as u64;
    let mut total_bytes: u64 = 0;

    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                error!("Payload stream error: {}", e);
                GLOBAL_METRICS.record_request(400);
                GLOBAL_METRICS.record_latency(start);
                // temp_file auto-cleaned by Drop
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

    // Ensure all data is on disk before hashing/storage
    if let Err(e) = temp_file.sync_all().await {
        error!("Failed to sync temp file: {}", e);
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Failed to sync upload data: {}", e)
        }));
    }

    // Content integrity verification:
    // Git LFS clients send SHA-256 OIDs, xet-native clients use BLAKE3 keyed hashes.
    // Verify the uploaded content matches the claimed OID using whichever algorithm applies.
    // This prevents storing arbitrary content under a known hash (defense against buggy/malicious clients).
    let (blake3_hash, sha256_hash) = hasher.finalize();
    if blake3_hash == oid {
        info!("Upload verified: OID matches BLAKE3 keyed hash (xet-native client)");
    } else if sha256_hash == oid {
        info!("Upload verified: OID matches SHA-256 hash (Git LFS client)");
    } else {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!(
                "Hash mismatch: OID {} does not match BLAKE3 ({}) or SHA-256 ({})",
                oid, blake3_hash, sha256_hash
            )
        }));
    }

    let object_key = format!("lfs/objects/{}", oid);

    // Check if object already exists
    let already_exists = match storage.exists(&object_key).await {
        Ok(exists) => exists,
        Err(e) => {
            error!("Failed to check object existence: {}", e);
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
        // temp_file auto-cleaned by Drop (object already in storage)
        return HttpResponse::Ok().json(serde_json::json!({
            "message": "Object already exists"
        }));
    }

    // Move temp file to final storage location (zero-copy rename for local storage)
    let temp_path = temp_file.into_path();
    if let Err(e) = storage.put_from_path(&object_key, &temp_path).await {
        error!("Failed to store object: {}", e);
        // M1 fix: Use async I/O to avoid blocking the async runtime on file cleanup.
        // Previously used std::fs::remove_file which blocks the tokio worker thread.
        let _ = tokio::fs::remove_file(&temp_path).await;
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Storage error: {}", e)
        }));
    }

    info!("Uploaded LFS object {} ({} bytes)", oid, total_bytes);

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(total_bytes);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(serde_json::json!({
        "message": "Object uploaded successfully"
    }))
}

/// Download an LFS object.
///
/// Stateless download logic:
/// - Check raw blob first → serve it and trigger lazy conversion in background
/// - Otherwise check MetadataIndex for verified xet data → reconstruct from xorbs/shards
// All arguments are actix-web extractors (web::Data / web::Path / HttpRequest).
// Refactoring into a shared AppState struct would require reworking all route configs;
// the allow attribute is the pragmatic choice here.
#[allow(clippy::too_many_arguments)]
pub async fn download_lfs_object(
    path: web::Path<String>,
    storage: web::Data<Box<dyn StorageBackend>>,
    auth: web::Data<AuthVerifier>,
    index: web::Data<MetadataIndex>,
    converting: web::Data<Arc<ConvertingOids>>,
    conversion_config: web::Data<ConversionConfig>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // Validate oid format
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid oid format, expected 64-character hex string"
        }));
    }

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::LfsObject {
            operation: LfsOperation::Download,
            oid: oid.clone(),
            message: "Insufficient scope or invalid LFS download token",
        },
    ) {
        return rej.respond(start);
    }

    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            // Raw blob exists — serve it and trigger lazy conversion in background
            match serve_raw_blob(&oid, storage.clone(), config.clone(), start).await {
                RawBlobResult::Served(response) => {
                    if conversion_config.enabled && converting.try_acquire(&oid) {
                        let pipeline = crate::conversion::ConversionPipeline::new(
                            storage.clone().into_inner(),
                            index.clone().into_inner(),
                            conversion_config.get_ref().clone(),
                        );
                        let converting_clone = converting.clone();
                        let oid_clone = oid.clone();
                        tokio::spawn(async move {
                            // I4 fix: Use scope guard to ensure OID lock is always released,
                            // even if convert() panics. Previously, a panic would skip the
                            // release() call, permanently locking the OID until server restart.
                            struct OidGuard {
                                converting: Arc<ConvertingOids>,
                                oid: String,
                            }
                            impl Drop for OidGuard {
                                fn drop(&mut self) {
                                    self.converting.release(&self.oid);
                                }
                            }
                            let _guard = OidGuard {
                                converting: converting_clone.get_ref().clone(),
                                oid: oid_clone.clone(),
                            };

                            match pipeline.convert(&oid_clone).await {
                                Ok(result) => {
                                    tracing::info!(
                                        "Lazy converted {}: {} chunks, {} deduped, {} → {} bytes",
                                        oid_clone,
                                        result.num_chunks,
                                        result.num_deduped_chunks,
                                        result.raw_size,
                                        result.xorb_size
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Lazy conversion failed for {}: {} (raw blob preserved)",
                                        oid_clone,
                                        e
                                    );
                                }
                            }
                            // _guard.drop() releases the OID lock, even on panic
                        });
                    }

                    return response;
                }
                RawBlobResult::Missing => {}
                RawBlobResult::Error(response) => return response,
            }
        }
        Ok(false) => {}
        Err(e) => {
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }));
        }
    }

    if let Some(file_refs) = index.get_file_refs(&oid) {
        let temp_dir = config.storage.resolve_reconstruction_temp_dir();
        return serve_verified_xet_reconstruction(&oid, file_refs, storage, temp_dir, start).await;
    }

    GLOBAL_METRICS.record_request(404);
    GLOBAL_METRICS.record_latency(start);
    HttpResponse::NotFound().json(serde_json::json!({
        "error": format!("Object not found: {}", oid)
    }))
}

/// Check if there's enough disk space for an upload.
/// Delegates to the shared utility in crate::util::disk.
fn check_disk_space(path: &std::path::Path, required_bytes: u64) -> Result<(), String> {
    crate::util::disk::check_disk_space(path, required_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
    use crate::config::{AuthConfig, ConversionConfig};
    use crate::conversion::ConvertingOids;
    use crate::format::shard::MDBShardFile;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::format::xorb_builder::XorbBuilder;
    use crate::hash::compute_data_hash;
    use crate::shard_validation::validate_shard_for_index;
    use crate::storage::local::LocalStorage;
    use actix_web::{App, test as actix_test, web};
    use bytes::Bytes;
    use sha2::{Digest, Sha256};
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
        let config = ServerConfig {
            auth: auth_config,
            ..Default::default()
        };

        (kp, auth_verifier, config)
    }

    fn create_test_token(kp: &KeyPair, scope: &str) -> String {
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
            kid: kp.kid(),
            token_type: "user".to_string(),
            oid: None,
            operation: None,
        };

        sign_xet_token(&claims, kp).unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    fn sha256_merkle_hash(data: &[u8]) -> MerkleHash {
        MerkleHash::from_hex(&sha256_hex(data)).unwrap()
    }

    fn build_mismatched_file_hash_xorb_and_shard(
        attacker_bytes: &[u8],
        victim_bytes: &[u8],
    ) -> (Vec<u8>, Vec<u8>, String) {
        let mut xorb_builder =
            XorbBuilder::new(crate::format::compression::CompressionScheme::None);
        let (serialized_chunk_hash, compressed_len) =
            xorb_builder.add_chunk(attacker_bytes).unwrap();
        let xorb = xorb_builder.build().unwrap();
        let raw_chunk_hash = compute_data_hash(attacker_bytes);
        let victim_hash = sha256_merkle_hash(victim_bytes);

        let mut shard_builder = ShardBuilder::new();
        let xorb_index = shard_builder
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                vec![XorbChunkBuildEntry {
                    chunk_hash: serialized_chunk_hash,
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: attacker_bytes.len() as u32,
                }],
                vec![raw_chunk_hash],
            )
            .unwrap();
        assert_eq!(compressed_len as usize, attacker_bytes.len());
        shard_builder.add_file(
            victim_hash,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: attacker_bytes.len() as u32,
            }],
        );

        (
            xorb.data,
            shard_builder.build().unwrap(),
            victim_hash.to_hex(),
        )
    }

    #[test]
    fn test_extract_chunk_verified_detects_corruption() {
        use crate::format::compression::{CompressionScheme, compress};
        use crate::hash::compute_data_hash;

        let raw = b"some chunk payload data for verification".to_vec();
        let compressed = compress(CompressionScheme::LZ4, &raw).unwrap();
        let header = XorbChunkHeader {
            version: 1,
            compressed_length: compressed.len() as u32,
            compression_scheme: CompressionScheme::LZ4,
            uncompressed_length: raw.len() as u32,
        };
        let mut chunk_bytes = Vec::new();
        header.serialize(&mut chunk_bytes).unwrap();
        chunk_bytes.extend_from_slice(&compressed);
        let hash = compute_data_hash(&chunk_bytes);

        // 正常路径:解压结果等于原文
        let ok = extract_chunk_verified(&chunk_bytes, 0, &hash).unwrap();
        assert_eq!(&ok[..], &raw[..]);

        // 翻转压缩区最后一字节 → hash 不匹配 → 报错
        let mut corrupted = chunk_bytes.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        assert!(extract_chunk_verified(&corrupted, 0, &hash).is_err());
    }

    #[tokio::test]
    async fn test_extract_chunk_verified_from_file_reads_only_target_chunk() {
        use crate::format::compression::{CompressionScheme, compress};
        use crate::hash::compute_data_hash;

        fn build_chunk(raw: &[u8]) -> (Vec<u8>, MerkleHash) {
            let compressed = compress(CompressionScheme::LZ4, raw).unwrap();
            let header = XorbChunkHeader {
                version: 1,
                compressed_length: compressed.len() as u32,
                compression_scheme: CompressionScheme::LZ4,
                uncompressed_length: raw.len() as u32,
            };
            let mut chunk_bytes = Vec::new();
            header.serialize(&mut chunk_bytes).unwrap();
            chunk_bytes.extend_from_slice(&compressed);
            let hash = compute_data_hash(&chunk_bytes);
            (chunk_bytes, hash)
        }

        let prefix = b"not-a-chunk-prefix";
        let first_raw = b"first chunk data";
        let second_raw = b"second chunk payload";
        let (first_chunk, first_hash) = build_chunk(first_raw);
        let (second_chunk, second_hash) = build_chunk(second_raw);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xorb.tmp");
        let mut file_bytes = Vec::new();
        file_bytes.extend_from_slice(prefix);
        let first_offset = file_bytes.len() as u64;
        file_bytes.extend_from_slice(&first_chunk);
        let second_offset = file_bytes.len() as u64;
        file_bytes.extend_from_slice(&second_chunk);
        tokio::fs::write(&path, &file_bytes).await.unwrap();

        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let first = extract_chunk_verified_from_file(
            &mut file,
            first_offset,
            first_raw.len() as u32,
            &first_hash,
        )
        .await
        .unwrap();
        assert_eq!(&first[..], &first_raw[..]);

        let second = extract_chunk_verified_from_file(
            &mut file,
            second_offset,
            second_raw.len() as u32,
            &second_hash,
        )
        .await
        .unwrap();
        assert_eq!(&second[..], &second_raw[..]);

        let mut file = tokio::fs::File::open(&path).await.unwrap();
        assert!(
            extract_chunk_verified_from_file(
                &mut file,
                second_offset,
                second_raw.len() as u32,
                &first_hash,
            )
            .await
            .is_err()
        );

        let mut file = tokio::fs::File::open(&path).await.unwrap();
        assert!(
            extract_chunk_verified_from_file(
                &mut file,
                first_offset,
                second_raw.len() as u32,
                &first_hash
            )
            .await
            .is_err()
        );
    }

    #[actix_web::test]
    async fn test_lfs_download_rejects_shard_poisoning_without_verified_index_entry() {
        let storage_dir = tempdir().unwrap();
        let upload_temp_dir = tempdir().unwrap();
        let reconstruction_temp_dir = tempdir().unwrap();

        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(
            LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
        ));
        let index = MetadataIndex::new();
        let index_for_assert = index.clone();
        let (kp, auth, mut config) = create_test_config();
        config.storage.upload_temp_dir = Some(upload_temp_dir.path().to_str().unwrap().to_string());
        config.storage.reconstruction_temp_dir =
            Some(reconstruction_temp_dir.path().to_str().unwrap().to_string());
        let token = create_test_token(&kp, "read");

        let attacker_bytes = b"attacker controlled bytes";
        let victim_bytes = b"victim bytes with different sha256";
        let (xorb_data, shard_data, victim_oid) =
            build_mismatched_file_hash_xorb_and_shard(attacker_bytes, victim_bytes);
        assert_ne!(victim_oid, sha256_hex(attacker_bytes));

        let shard_id = compute_data_hash(&shard_data).to_hex();
        let parsed_shard = MDBShardFile::parse(&shard_data).unwrap();
        let xorb_hash = parsed_shard.xorb_entries[0].xorb_hash.to_hex();

        storage
            .put(&format!("xorbs/{}", xorb_hash), Bytes::from(xorb_data))
            .await
            .unwrap();
        storage
            .put(
                &format!("shards/{}", shard_id),
                Bytes::from(shard_data.clone()),
            )
            .await
            .unwrap();

        let validate_result = validate_shard_for_index(
            &shard_id,
            &parsed_shard,
            storage.as_ref().as_ref(),
            reconstruction_temp_dir.path(),
        )
        .await;
        let validation_error = validate_result.unwrap_err();
        assert!(
            validation_error.contains("File hash mismatch"),
            "{}",
            validation_error
        );
        assert!(index_for_assert.get_file_refs(&victim_oid).is_none());

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::from(storage))
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(Arc::new(ConvertingOids::new())))
                .app_data(web::Data::new(ConversionConfig::default()))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route("/lfs/objects/{oid}", web::get().to(download_lfs_object)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri(&format!("/lfs/objects/{}", victim_oid))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }
}
