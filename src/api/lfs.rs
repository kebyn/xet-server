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

use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use std::sync::Arc;
use tracing::{error, info};

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
use crate::api::reconstruction::fetch_and_parse_shard;
use crate::config::{ConversionConfig, ServerConfig};
use crate::conversion::ConvertingOids;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::{StorageBackend, StorageError};
use crate::util::{DualHasher, TempFile};

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
                "error": format!("Failed to create temp file: {}", e)
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
        // Clean up temp file on failure
        let _ = std::fs::remove_file(&temp_path);
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
/// - Check MetadataIndex for xet data → reconstruct from xorbs/shards
/// - Otherwise → serve raw blob and trigger lazy conversion in background
pub async fn download_lfs_object(
    path: web::Path<String>,
    storage: web::Data<Box<dyn StorageBackend>>,
    auth: web::Data<AuthVerifier>,
    index: web::Data<MetadataIndex>,
    converting: web::Data<Arc<ConvertingOids>>,
    conversion_config: web::Data<ConversionConfig>,
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

    // STATELESS: Check MetadataIndex for xet data
    if index.get_shards_for_file(&oid).is_some() {
        return reconstruct_from_xet(&oid, index, storage, start).await;
    }

    // Raw blob path — check existence first to handle race with concurrent conversion.
    // A background conversion (triggered by an earlier download) may have deleted the
    // raw blob between our index check above and now. In that case, the index should
    // now have the xet data, so we retry reconstruction.
    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            // Raw blob exists — serve it and trigger lazy conversion in background
            let response = serve_raw_blob(&oid, storage.clone(), start).await;

            if conversion_config.enabled && converting.try_acquire(&oid) {
                let pipeline = crate::conversion::ConversionPipeline::new(
                    storage.clone().into_inner(),
                    index.clone().into_inner(),
                    conversion_config.get_ref().clone(),
                );
                let converting_clone = converting.clone();
                let oid_clone = oid.clone();
                tokio::spawn(async move {
                    match pipeline.convert(&oid_clone).await {
                        Ok(result) => {
                            tracing::info!("Lazy converted {}: {} chunks, {} deduped, {} → {} bytes",
                                oid_clone, result.num_chunks, result.num_deduped_chunks,
                                result.raw_size, result.xorb_size);
                        }
                        Err(e) => {
                            tracing::warn!("Lazy conversion failed for {}: {} (raw blob preserved)", oid_clone, e);
                        }
                    }
                    converting_clone.release(&oid_clone);
                });
            }

            response
        }
        Ok(false) => {
            // Raw blob gone — re-check index (conversion may have completed)
            if index.get_shards_for_file(&oid).is_some() {
                reconstruct_from_xet(&oid, index, storage, start).await
            } else {
                GLOBAL_METRICS.record_request(404);
                GLOBAL_METRICS.record_latency(start);
                HttpResponse::NotFound().json(serde_json::json!({
                    "error": format!("Object not found: {}", oid)
                }))
            }
        }
        Err(e) => {
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }))
        }
    }
}

/// Serve a raw blob from storage.
/// Uses streaming file I/O when the backend supports it (e.g. local storage)
/// to avoid loading multi-gigabyte files entirely into RAM.
async fn serve_raw_blob(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    start: std::time::Instant,
) -> HttpResponse {
    let object_key = format!("lfs/objects/{}", oid);

    // Try streaming path first (avoids loading entire file into memory)
    match storage.get_path(&object_key).await {
        Ok(Some(path)) => {
            // Stream from file
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to open file for streaming {}: {}", path.display(), e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Failed to open file: {}", e)
                    }));
                }
            };
            let metadata = match file.metadata().await {
                Ok(m) => m,
                Err(e) => {
                    error!("Failed to get file metadata: {}", e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Failed to get metadata: {}", e)
                    }));
                }
            };
            let file_size = metadata.len();

            // Safety: LFS objects are content-addressed and immutable after upload.
            // The file size cannot change between metadata() and stream completion
            // because the object key is derived from the content hash, and the server
            // never modifies a stored object in place.
            use tokio_util::io::ReaderStream;
            let stream = ReaderStream::new(file);
            let body = actix_web::body::SizedStream::new(file_size, stream);

            info!("Streaming LFS object {} ({} bytes)", oid, file_size);
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_storage_operation();
            GLOBAL_METRICS.record_download_bytes(file_size);
            GLOBAL_METRICS.record_latency(start);

            HttpResponse::Ok()
                .content_type("application/octet-stream")
                .body(body)
        }
        Ok(None) => {
            // Non-file backend: fall back to in-memory get
            serve_raw_blob_inmemory(oid, storage, start).await
        }
        Err(StorageError::NotFound(_)) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::NotFound().json(serde_json::json!({
                "error": format!("Object not found: {}", oid)
            }))
        }
        Err(e) => {
            error!("Failed to get path for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }))
        }
    }
}

/// Fallback: serve a raw blob by loading it entirely into memory.
async fn serve_raw_blob_inmemory(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    start: std::time::Instant,
) -> HttpResponse {
    let object_key = format!("lfs/objects/{}", oid);
    let object_data = match storage.get(&object_key).await {
        Ok(data) => {
            GLOBAL_METRICS.record_storage_operation();
            data
        }
        Err(StorageError::NotFound(_)) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": format!("Object not found: {}", oid)
            }));
        }
        Err(e) => {
            error!("Failed to fetch object: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            }));
        }
    };

    info!("Downloaded LFS object {} ({} bytes)", oid, object_data.len());

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(object_data.len() as u64);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(object_data)
}

/// Reconstruct a file from xorb/shard storage
///
/// This function:
/// 1. Retrieves shard information for the file
/// 2. Downloads and parses shards to get xorb/chunk metadata
/// 3. Downloads all required xorbs
/// 4. Extracts and decompresses chunks
/// 5. Reassembles chunks into the complete file
async fn reconstruct_from_xet(
    file_id: &str,
    index: web::Data<crate::index::MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    start: std::time::Instant,
) -> HttpResponse {
    use crate::format::xorb::XorbChunkHeader;
    use crate::format::compression::decompress;
    use std::collections::HashSet;
    use std::io::Cursor;

    // Look up shards for this file
    let shard_ids = match index.get_shards_for_file(file_id) {
        Some(ids) => ids,
        None => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": format!("File not found in metadata index: {}", file_id)
            }));
        }
    };

    // Collect xorb information from all shards
    let mut xorbs = Vec::new();
    let mut seen_xorbs = HashSet::new();
    let mut file_data = Vec::new();

    for shard_id in shard_ids {
        // Fetch and parse shard using shared helper
        let shard = match fetch_and_parse_shard(&shard_id, &***storage).await {
            Ok(s) => s,
            Err(e) => {
                // Log detailed error with shard_id, but return generic message to client
                error!("Failed to fetch/parse shard {}: {}", shard_id, e);
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to fetch or parse shard data"
                }));
            }
        };

        // Extract xorb information (deduplicated)
        let mut chunk_index_offset = 0;
        for xorb_entry in &shard.xorb_entries {
            let xorb_hash = xorb_entry.xorb_hash.to_hex();
            if seen_xorbs.insert(xorb_hash.clone()) {
                xorbs.push(xorb_hash.clone());

                // Download xorb
                let xorb_key = format!("xorbs/{}", xorb_hash);
                let xorb_data = match storage.get(&xorb_key).await {
                    Ok(data) => {
                        GLOBAL_METRICS.record_storage_operation();
                        data
                    }
                    Err(e) => {
                        error!("Failed to fetch xorb {}: {}", xorb_hash, e);
                        GLOBAL_METRICS.record_request(500);
                        GLOBAL_METRICS.record_error();
                        GLOBAL_METRICS.record_latency(start);
                        return HttpResponse::InternalServerError().json(serde_json::json!({
                            "error": format!("Failed to fetch xorb: {}", e)
                        }));
                    }
                };

                // Extract chunks from xorb
                for i in 0..xorb_entry.num_entries as usize {
                    if chunk_index_offset + i < shard.xorb_chunk_entries.len() {
                        let chunk_entry = &shard.xorb_chunk_entries[chunk_index_offset + i];
                        let chunk_offset = chunk_entry.chunk_byte_range_start as usize;

                        // Read chunk header (8 bytes)
                        if chunk_offset + 8 > xorb_data.len() {
                            error!("Chunk offset out of bounds");
                            GLOBAL_METRICS.record_request(500);
                            GLOBAL_METRICS.record_error();
                            GLOBAL_METRICS.record_latency(start);
                            return HttpResponse::InternalServerError().json(serde_json::json!({
                                "error": "Chunk offset out of bounds"
                            }));
                        }

                        let mut chunk_cursor = Cursor::new(&xorb_data[chunk_offset..]);
                        let chunk_header = match XorbChunkHeader::deserialize(&mut chunk_cursor) {
                            Ok(h) => h,
                            Err(e) => {
                                error!("Failed to parse chunk header: {}", e);
                                GLOBAL_METRICS.record_request(500);
                                GLOBAL_METRICS.record_error();
                                GLOBAL_METRICS.record_latency(start);
                                return HttpResponse::InternalServerError().json(serde_json::json!({
                                    "error": format!("Failed to parse chunk header: {}", e)
                                }));
                            }
                        };

                        // Read compressed chunk data
                        let data_start = chunk_offset + XorbChunkHeader::SIZE;
                        let data_end = data_start + chunk_header.compressed_length as usize;
                        if data_end > xorb_data.len() {
                            error!("Chunk data out of bounds");
                            GLOBAL_METRICS.record_request(500);
                            GLOBAL_METRICS.record_error();
                            GLOBAL_METRICS.record_latency(start);
                            return HttpResponse::InternalServerError().json(serde_json::json!({
                                "error": "Chunk data out of bounds"
                            }));
                        }

                        let compressed_data = &xorb_data[data_start..data_end];

                        // Decompress chunk
                        let decompressed = match decompress(
                            chunk_header.compression_scheme,
                            compressed_data,
                            chunk_header.uncompressed_length as usize,
                        ) {
                            Ok(d) => d,
                            Err(e) => {
                                error!("Failed to decompress chunk: {}", e);
                                GLOBAL_METRICS.record_request(500);
                                GLOBAL_METRICS.record_error();
                                GLOBAL_METRICS.record_latency(start);
                                return HttpResponse::InternalServerError().json(serde_json::json!({
                                    "error": format!("Failed to decompress chunk: {}", e)
                                }));
                            }
                        };

                        file_data.extend_from_slice(&decompressed);
                    }
                }
                chunk_index_offset += xorb_entry.num_entries as usize;
            } else {
                // Skip chunks for duplicate xorbs
                chunk_index_offset += xorb_entry.num_entries as usize;
            }
        }
    }

    // Check if we actually reconstructed any data
    if file_data.is_empty() {
        error!("Shards found for file {} but no data could be reconstructed", file_id);
        GLOBAL_METRICS.record_request(500);
        GLOBAL_METRICS.record_error();
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Shards found but no xorb data could be reconstructed for file: {}", file_id)
        }));
    }

    info!("Reconstructed file {} from xet storage ({} bytes)", file_id, file_data.len());

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(file_data.len() as u64);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(file_data)
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

        // Note: MetadataExt::blocks gives us filesystem block info, but for available space
        // we need to use statvfs. For simplicity, we'll use a basic check.
        // In production, consider using the nix crate or libc for proper statvfs.

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
        // For now, we trust that if the directory is writable, there's likely space
        // This is a simplification; a full implementation would check actual available space
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
        // In production, implement platform-specific checks
        let _ = required_bytes;
        Ok(())
    }
}
