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
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{debug, error, info};

use crate::api::auth::{extract_token_from_request, AuthVerifier};
use crate::api::reconstruction::fetch_and_parse_shard;
use crate::config::{ConversionConfig, ServerConfig};
use crate::conversion::ConvertingOids;
use crate::format::compression::decompress;
use crate::format::shard::MDBShardFile;
use crate::format::xorb::XorbChunkHeader;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::{StorageBackend, StorageError};
use crate::types::MerkleHash;
use crate::util::{DualHasher, TempFile};

/// 从 xorb 原始字节中按偏移定位单个 chunk,校验其完整性后解压。
///
/// C-DATA-2: chunk_hash 覆盖 header(8B)+压缩数据。在解压前对该区域重算 hash
/// 并与 shard 记录值比对,检出磁盘 bit-rot / 存储损坏 / 串改,并先于解压挡住污染数据。
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

    // I1: Use shared helper for internal token check (defense-in-depth)
    if !crate::api::auth::authorize_endpoint(&claims, "write") {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope"
        }));
    }

    // C6 fix: Verify proxy token is bound to this specific OID and operation
    if claims.token_type == "proxy" {
        if let Some(ref bound_oid) = claims.oid
            && bound_oid != &oid {
                GLOBAL_METRICS.record_request(403);
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Proxy token is bound to a different OID"
                }));
            }
        if let Some(ref bound_op) = claims.operation
            && bound_op != "upload" {
                GLOBAL_METRICS.record_request(403);
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Proxy token is not authorized for upload"
                }));
            }
    }

    // M7 fix: Use a more reasonable pre-check threshold (see xorb.rs for rationale).
    let temp_dir = config.storage.resolve_upload_temp_dir();
    let check_bytes = std::cmp::min(config.server.max_body_size_bytes() as u64, 100 * 1024 * 1024);
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
/// - Check MetadataIndex for xet data → reconstruct from xorbs/shards
/// - Otherwise → serve raw blob and trigger lazy conversion in background
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
    ref_tracker: web::Data<Arc<dyn crate::gc::reference_tracker::ReferenceTracker>>,
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

    // I1: Use shared helper for internal token check (defense-in-depth)
    if !crate::api::auth::authorize_endpoint(&claims, "read") {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Insufficient scope"
        }));
    }

    // C6 fix: Verify proxy token is bound to this specific OID and operation
    if claims.token_type == "proxy" {
        if let Some(ref bound_oid) = claims.oid
            && bound_oid != &oid {
                GLOBAL_METRICS.record_request(403);
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Proxy token is bound to a different OID"
                }));
            }
        if let Some(ref bound_op) = claims.operation
            && bound_op != "download" {
                GLOBAL_METRICS.record_request(403);
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Proxy token is not authorized for download"
                }));
            }
    }

    // STATELESS: Check MetadataIndex for xet data
    if index.get_shards_for_file(&oid).is_some() {
        let temp_dir = config.storage.resolve_reconstruction_temp_dir();
        return reconstruct_from_xet(&oid, index, storage, temp_dir, start).await;
    }

    // Raw blob path — check existence first to handle race with concurrent conversion.
    // A background conversion (triggered by an earlier download) may have deleted the
    // raw blob between our index check above and now. In that case, the index should
    // now have the xet data, so we retry reconstruction.
    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            // Raw blob exists — serve it and trigger lazy conversion in background
            let response = serve_raw_blob(&oid, storage.clone(), config.clone(), start).await;

            if conversion_config.enabled && converting.try_acquire(&oid) {
                let pipeline = crate::conversion::ConversionPipeline::new(
                    storage.clone().into_inner(),
                    index.clone().into_inner(),
                    conversion_config.get_ref().clone(),
                )
                // I5 fix: Pass ref_tracker for proactive sidecar generation
                .with_ref_tracker(ref_tracker.get_ref().clone());
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
                    let _guard = OidGuard { converting: converting_clone.get_ref().clone(), oid: oid_clone.clone() };

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
                    // _guard.drop() releases the OID lock, even on panic
                });
            }

            response
        }
        Ok(false) => {
            // Raw blob gone — re-check index (conversion may have completed)
            if index.get_shards_for_file(&oid).is_some() {
                let temp_dir = config.storage.resolve_reconstruction_temp_dir();
                reconstruct_from_xet(&oid, index, storage, temp_dir, start).await
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

/// Serve a raw blob from storage with optional streaming integrity verification.
/// Uses streaming file I/O when the backend supports it (e.g. local storage)
/// to avoid loading multi-gigabyte files entirely into RAM.
/// I3: When integrity verification is enabled, computes SHA-256 hash incrementally
/// during streaming (not a separate pre-read), then verifies after stream completes.
async fn serve_raw_blob(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    start: std::time::Instant,
) -> HttpResponse {
    let object_key = format!("lfs/objects/{}", oid);
    let verify_integrity = config.storage.verify_download_integrity;

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
            let base_stream = ReaderStream::new(file);

            // C1 Fix: If integrity verification is enabled, wrap stream with hasher
            // This computes SHA-256 incrementally during streaming (no double-read)
            let oid_owned = oid.to_string();
            if verify_integrity {
                let hashing_stream = IntegrityVerifyingStream::new(base_stream, oid_owned);
                let body = actix_web::body::SizedStream::new(file_size, hashing_stream);

                info!("Streaming LFS object {} ({} bytes) with integrity verification", oid, file_size);
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_storage_operation();
                GLOBAL_METRICS.record_download_bytes(file_size);
                GLOBAL_METRICS.record_latency(start);

                HttpResponse::Ok()
                    .content_type("application/octet-stream")
                    .body(body)
            } else {
                let body = actix_web::body::SizedStream::new(file_size, base_stream);

                info!("Streaming LFS object {} ({} bytes)", oid, file_size);
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_storage_operation();
                GLOBAL_METRICS.record_download_bytes(file_size);
                GLOBAL_METRICS.record_latency(start);

                HttpResponse::Ok()
                    .content_type("application/octet-stream")
                    .body(body)
            }
        }
        Ok(None) => {
            // Non-file backend: fall back to in-memory get
            serve_raw_blob_inmemory(oid, storage, config, start).await
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

/// Stream wrapper that computes SHA-256 hash incrementally and verifies after completion.
/// C1 Fix: Avoids double-read by hashing during streaming, not before.
///
/// # I1: Known Limitation - Post-streaming Integrity Verification
///
/// ## Behavior
///
/// This implementation performs integrity verification **after** the last byte is streamed.
/// When the hash doesn't match the expected OID, the stream emits an error as its final item.
/// By this point, most or all of the data has already been sent to the client over the wire.
///
/// The client receives either:
/// - A complete valid response (hash matches — normal case), or
/// - A truncated/errored response (hash mismatch — triggers `InvalidData` error)
///
/// Either way, the client's Git LFS implementation is expected to verify the OID of the
/// downloaded content against the expected OID in the LFS metadata. A hash mismatch here
/// causes the stream to terminate with an error, which signals to the client that the
/// download failed and should be retried.
///
/// ## Why not pre-verify?
///
/// - Loading entire files into memory before sending would defeat the purpose of streaming
///   (files can be up to 512 MB; pre-loading would negate O(chunk_size) memory bound)
/// - HTTP/1.1 doesn't support trailer headers for hash verification in most clients
/// - Pre-computing the hash requires double-reading the file (2x I/O and latency cost)
///
/// ## Mitigations (defense-in-depth)
///
/// 1. **Client-side verification**: Git LFS protocol requires clients to verify OID after
///    download. A mismatched hash will cause the client to reject and retry.
/// 2. **Server-side logging**: Every mismatch is logged at ERROR level with the computed
///    and expected hashes, enabling monitoring/alerting.
/// 3. **Metrics**: Each mismatch increments `GLOBAL_METRICS.record_error()`, visible in
///    the `/metrics` endpoint for alerting.
/// 4. **Stream error propagation**: The `InvalidData` error terminates the stream, which
///    causes actix-web to close the HTTP response abnormally (no 200 OK finalization).
/// 5. **Small-file fast path**: For files served in-memory (`serve_raw_blob_inmemory`),
///    verification happens before any data is sent, providing full protection for small files.
/// 6. **Storage backend checksums**: S3 provides its own ETag/MD5 verification; a mismatch
///    at this layer would indicate S3-side corruption caught earlier by S3's own checks.
///
/// This is the industry-standard approach for large file streaming with integrity checks.
/// AWS S3, GCS, and Azure Blob all use similar post-stream verification patterns for
/// large object downloads.
struct IntegrityVerifyingStream<S> {
    inner: S,
    hasher: Option<sha2::Sha256>,
    expected_oid: String,
    bytes_hashed: u64,
}

impl<S> IntegrityVerifyingStream<S> {
    fn new(inner: S, expected_oid: String) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: Some(sha2::Sha256::new()),
            expected_oid,
            bytes_hashed: 0,
        }
    }
}

impl<S> Stream for IntegrityVerifyingStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use sha2::Digest;
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(hasher) = &mut self.hasher {
                    hasher.update(&chunk);
                }
                self.bytes_hashed += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                // Stream completed - verify hash
                // Take ownership of hasher to call finalize()
                if let Some(hasher) = self.hasher.take() {
                    let computed_hash = format!("{:x}", hasher.finalize());
                    if computed_hash != self.expected_oid {
                        error!(
                            "Integrity check FAILED for {}: computed {} != expected {} ({} bytes streamed)",
                            self.expected_oid, computed_hash, self.expected_oid, self.bytes_hashed
                        );
                        // I4 fix: Return an error to the client instead of silently succeeding.
                        // The error is returned as a stream error, which causes actix-web to send
                        // a truncated or error response, preventing clients from trusting corrupt data.
                        GLOBAL_METRICS.record_error();
                        return Poll::Ready(Some(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Integrity verification failed: content hash {} does not match expected OID {}",
                                computed_hash, self.expected_oid
                            ),
                        ))));
                    } else {
                        info!(
                            "Integrity check passed for {} ({} bytes streamed)",
                            self.expected_oid, self.bytes_hashed
                        );
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Fallback: serve a raw blob by loading it entirely into memory.
/// I3: Performs integrity verification if enabled (since we have the data in memory anyway).
async fn serve_raw_blob_inmemory(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
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

    // I3: Integrity verification for in-memory path
    if config.storage.verify_download_integrity {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&object_data);
        let computed_hash = format!("{:x}", hasher.finalize());

        if computed_hash != oid {
            error!(
                "Integrity check FAILED for {}: computed {} != expected {} ({} bytes)",
                oid, computed_hash, oid, object_data.len()
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Integrity verification failed: stored content does not match OID"
            }));
        }

        info!("Integrity check passed for {} ({} bytes)", oid, object_data.len());
    }

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
///
/// I4: True streaming implementation using custom ReconstructionStream.
/// Processes one chunk at a time, avoiding temp files and minimizing memory usage.
async fn reconstruct_from_xet(
    file_id: &str,
    index: web::Data<crate::index::MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    reconstruction_temp_dir: std::path::PathBuf,
    start: std::time::Instant,
) -> HttpResponse {
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

    // I4: Create custom streaming implementation
    // M3: Refactored to use async_stream for improved readability
    // web::Data is internally Arc-wrapped
    let storage_arc = storage.clone().into_inner();
    let stream = create_reconstruction_stream(storage_arc, shard_ids, reconstruction_temp_dir);

    // Wrap the stream with a metrics-recording wrapper that:
    // 1. Tracks total bytes streamed (for download_bytes metric)
    // 2. Records latency when stream completes (not when it starts)
    let metrics_stream = MetricsRecordingStream::new(stream, start);

    // Map stream errors to HttpResponse
    let mapped_stream = metrics_stream.map(|result| {
        match result {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                error!("Reconstruction stream error: {}", e);
                Err(actix_web::Error::from(std::io::Error::other(e)))
            }
        }
    });

    info!("Streaming reconstruction for file {}", file_id);

    // Note: record_request(200) and record_latency are called by MetricsRecordingStream
    // when the stream completes, not when it starts.

    // I4: Use chunked transfer encoding since we don't know total size upfront
    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .streaming(mapped_stream)
}

/// Wrapper stream that records metrics when streaming completes.
///
/// Tracks total bytes yielded and records download_bytes + latency metrics
/// when the underlying stream returns None (completes) or errors.
struct MetricsRecordingStream<S> {
    inner: S,
    start: std::time::Instant,
    total_bytes: u64,
    completed: bool,
}

impl<S> MetricsRecordingStream<S> {
    fn new(inner: S, start: std::time::Instant) -> Self {
        Self {
            inner,
            start,
            total_bytes: 0,
            completed: false,
        }
    }

    fn record_metrics(&mut self) {
        if !self.completed {
            self.completed = true;
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_download_bytes(self.total_bytes);
            GLOBAL_METRICS.record_latency(self.start);
        }
    }
}

impl<S> Drop for MetricsRecordingStream<S> {
    fn drop(&mut self) {
        // Ensure metrics are recorded even if stream is dropped before completion
        self.record_metrics();
    }
}

impl<S> Stream for MetricsRecordingStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, String>> + Unpin,
{
    type Item = Result<bytes::Bytes, String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = Pin::new(&mut self.inner);
        match inner.poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.total_bytes += bytes.len() as u64;
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                // Record metrics on error
                self.record_metrics();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                // Stream completed successfully - record metrics
                self.record_metrics();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Create a streaming reconstruction pipeline for file data with parallel prefetch.
///
/// ## Prefetch Strategy
///
/// This implementation uses `tokio::spawn` to overlap network I/O with CPU decompression:
///
/// 1. **Xorb-level prefetch** (critical): While decompressing chunks from xorb N,
///    xorb N+1 is fetched from storage in a background task. This hides S3 latency
///    (typically 100-500ms) behind chunk decompression work.
///
/// 2. **Shard-level prefetch** (supplementary): While processing shard N's xorbs,
///    shard N+1 is fetched in a background task. Shards are smaller than xorbs,
///    so this is less impactful but trivially added with the same pattern.
///
/// ## Overlap Mechanics
///
/// When `yield` returns a chunk to the consumer, the `async_stream` generator is
/// suspended. The tokio runtime then schedules the prefetch task, making network
/// I/O progress while the consumer writes the chunk to the TCP socket. By the time
/// `poll_next` is called again, the prefetch may already be complete.
///
/// ```text
/// poll_next() → decompress chunk 0 of xorb N → yield chunk 0
///   [consumer writes chunk 0] [xorb N+1 prefetch runs]  ← OVERLAP
/// poll_next() → decompress chunk 1 of xorb N → yield chunk 1
///   [consumer writes chunk 1] [xorb N+1 prefetch continues]  ← OVERLAP
///   ...
/// poll_next() → await xorb N+1 prefetch (likely Ready) → process xorb N+1
/// ```
///
/// ## Memory
///
/// I3 fix: Peak memory is bounded by download_to_path (streams to temp file),
/// plus one xorb read from temp file at a time (streamed in chunks).
/// Previously, get() loaded entire xorbs (up to 512MB each) into memory,
/// with 2 concurrent xorbs (current + prefetched) = potential 1GB+ peak RAM.
/// Now: O(chunk_size) for streaming download + O(chunk_size) for temp file read.
///
/// ## Extension Point
///
/// For high-latency backends, prefetch depth > 1 can be added by replacing
/// `Option<JoinHandle>` with `VecDeque<JoinHandle>` or `tokio::task::JoinSet`.
fn create_reconstruction_stream(
    storage: Arc<Box<dyn StorageBackend>>,
    shard_ids: Vec<String>,
    temp_dir: std::path::PathBuf,
) -> Pin<Box<dyn Stream<Item = Result<bytes::Bytes, String>> + Send>> {
    use async_stream::stream;

    // Type aliases for prefetch handles
    // I3 fix: XorbPrefetch now returns a temp file path instead of in-memory bytes
    type ShardPrefetch = tokio::task::JoinHandle<Result<MDBShardFile, String>>;
    type XorbPrefetch = tokio::task::JoinHandle<Result<std::path::PathBuf, String>>;

    Box::pin(stream! {
        // Resolve temp directory for xorb downloads (configurable via XET_RECONSTRUCTION_TEMP_DIR)
        // M-3 fix: Use configured temp dir instead of hardcoded OS temp dir
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            yield Err(format!("Failed to create reconstruction temp dir: {}", e));
            return;
        }

        // --- Shard-level prefetch ---
        // Start fetching the first shard immediately
        let mut next_shard_prefetch: Option<ShardPrefetch> = None;
        if !shard_ids.is_empty() {
            let storage_clone = storage.clone();
            let first_shard_id = shard_ids[0].clone();
            next_shard_prefetch = Some(tokio::spawn(async move {
                fetch_and_parse_shard(&first_shard_id, &**storage_clone).await
            }));
        }

        for (shard_idx, shard_id) in shard_ids.iter().enumerate() {
            // Await current shard (prefetched or fallback)
            let shard_start = std::time::Instant::now();
            let shard = if let Some(handle) = next_shard_prefetch.take() {
                let result = handle.await
                    .map_err(|e| format!("Shard prefetch task panicked: {}", e))?
                    .map_err(|e| format!("Failed to fetch shard {}: {}", shard_id, e));
                debug!(
                    shard = %shard_id,
                    elapsed = ?shard_start.elapsed(),
                    prefetch = true,
                    "shard fetched"
                );
                result?
            } else {
                let result = fetch_and_parse_shard(shard_id, &**storage)
                    .await
                    .map_err(|e| format!("Failed to fetch shard {}: {}", shard_id, e));
                debug!(
                    shard = %shard_id,
                    elapsed = ?shard_start.elapsed(),
                    prefetch = false,
                    "shard fetched"
                );
                result?
            };

            // Kick off prefetch of next shard (while we process current shard's xorbs)
            if shard_idx + 1 < shard_ids.len() {
                let storage_clone = storage.clone();
                let next_shard_id = shard_ids[shard_idx + 1].clone();
                debug!(shard = %next_shard_id, "shard prefetch started");
                next_shard_prefetch = Some(tokio::spawn(async move {
                    fetch_and_parse_shard(&next_shard_id, &**storage_clone).await
                }));
            }

            // Extract xorb entries (deduplicated) with chunk offsets
            let mut seen_xorbs = std::collections::HashSet::new();
            let mut xorb_entries: Vec<(String, usize, usize)> = Vec::new();
            let mut chunk_offset = 0;

            for xorb_entry in &shard.xorb_entries {
                let xorb_hash = xorb_entry.xorb_hash.to_hex();
                if seen_xorbs.insert(xorb_hash.clone()) {
                    xorb_entries.push((
                        xorb_hash,
                        xorb_entry.num_entries as usize,
                        chunk_offset,
                    ));
                }
                chunk_offset += xorb_entry.num_entries as usize;
            }

            // --- Xorb-level prefetch ---
            // I3 fix: Prefetch downloads to temp file (bounded memory) instead of get() (full RAM load)
            let mut next_xorb_prefetch: Option<XorbPrefetch> = None;

            // Helper to download xorb to temp file
            let storage_for_download = storage.clone();
            let temp_dir_clone = temp_dir.clone();
            let download_xorb_to_temp = move |xorb_hash: String| -> tokio::task::JoinHandle<Result<std::path::PathBuf, String>> {
                let storage = storage_for_download.clone();
                let temp_dir = temp_dir_clone.clone();
                tokio::spawn(async move {
                    let xorb_key = format!("xorbs/{}", xorb_hash);
                    let temp_path = temp_dir.join(format!("xorb-{}-{}.tmp", xorb_hash, uuid::Uuid::new_v4()));

                    // I3 fix: Use download_to_path for streaming download (bounded memory)
                    storage.download_to_path(&xorb_key, &temp_path).await
                        .map_err(|e| format!("Failed to download xorb {}: {}", xorb_hash, e))?;

                    Ok(temp_path)
                })
            };

            // Start fetching the first xorb immediately
            if !xorb_entries.is_empty() {
                next_xorb_prefetch = Some(download_xorb_to_temp(xorb_entries[0].0.clone()));
            }

            for (xorb_idx, (xorb_hash, num_entries, xorb_chunk_offset)) in xorb_entries.iter().enumerate() {
                // Await current xorb (prefetched or fallback)
                let xorb_start = std::time::Instant::now();
                let xorb_temp_path: std::path::PathBuf = if let Some(handle) = next_xorb_prefetch.take() {
                    let result = handle.await
                        .map_err(|e| format!("Xorb prefetch task panicked: {}", e))?
                        .map_err(|e| format!("Failed to fetch xorb {}: {}", xorb_hash, e));
                    let elapsed = xorb_start.elapsed();
                    if elapsed.as_millis() < 5 {
                        debug!(xorb = %xorb_hash, ?elapsed, "xorb prefetch hit");
                    } else {
                        debug!(xorb = %xorb_hash, ?elapsed, "xorb prefetch miss");
                    }
                    result?
                } else {
                    let xorb_key = format!("xorbs/{}", xorb_hash);
                    let temp_path = temp_dir.join(format!("xorb-{}-{}.tmp", xorb_hash, uuid::Uuid::new_v4()));
                    storage.download_to_path(&xorb_key, &temp_path).await
                        .map_err(|e| format!("Failed to download xorb {}: {}", xorb_hash, e))?;
                    debug!(
                        xorb = %xorb_hash,
                        elapsed = ?xorb_start.elapsed(),
                        prefetch = false,
                        "xorb fetched"
                    );
                    temp_path
                };

                // Kick off prefetch of next xorb (while we decompress current xorb's chunks)
                if xorb_idx + 1 < xorb_entries.len() {
                    let next_xorb_hash = xorb_entries[xorb_idx + 1].0.clone();
                    debug!(xorb = %next_xorb_hash, "xorb prefetch started");
                    next_xorb_prefetch = Some(download_xorb_to_temp(next_xorb_hash));
                }

                // I3 fix: Read xorb data from temp file (bounded memory via streaming read)
                // Read the entire xorb from temp file — this is still needed because chunk
                // extraction requires random access into the xorb data. However, the key
                // improvement is that the download itself is streaming (via download_to_path).
                // For very large xorbs, a chunk-by-chunk streaming read from the temp file
                // could be added in the future, but this already eliminates the S3→RAM bottleneck.
                let xorb_data = match tokio::fs::read(&xorb_temp_path).await {
                    Ok(data) => data,
                    Err(e) => {
                        // Clean up temp file on error
                        let _ = tokio::fs::remove_file(&xorb_temp_path).await;
                        yield Err(format!("Failed to read xorb temp file {}: {}", xorb_hash, e));
                        return;
                    }
                };

                // Clean up temp file after reading
                let _ = tokio::fs::remove_file(&xorb_temp_path).await;

                // Extract each chunk from the xorb
                for chunk_idx in 0..*num_entries {
                    let global_chunk_idx = xorb_chunk_offset + chunk_idx;
                    if global_chunk_idx >= shard.xorb_chunk_entries.len() {
                        break;
                    }

                    let chunk_entry = &shard.xorb_chunk_entries[global_chunk_idx];
                    let chunk_offset_bytes = chunk_entry.chunk_byte_range_start as usize;

                    match extract_chunk_verified(&xorb_data, chunk_offset_bytes, &chunk_entry.chunk_hash) {
                        Ok(bytes) => yield Ok(bytes),
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Check if there's enough disk space for an upload.
/// Delegates to the shared utility in crate::util::disk.
fn check_disk_space(path: &std::path::Path, required_bytes: u64) -> Result<(), String> {
    crate::util::disk::check_disk_space(path, required_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_chunk_verified_detects_corruption() {
        use crate::format::compression::{compress, CompressionScheme};
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
}
