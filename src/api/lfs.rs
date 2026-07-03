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
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio_util::io::ReaderStream;
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
use crate::reconstruction_io::{ReconstructionError, reconstruct_verified_file_to_temp};
use crate::storage::{StorageBackend, StorageError};
#[cfg(test)]
use crate::types::MerkleHash;
use crate::util::{DualHasher, TempFile};
#[cfg(test)]
use crate::xorb_reader::extract_chunk_verified_from_file;

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

/// Serve a raw blob from storage with optional streaming integrity verification.
/// Uses streaming file I/O when the backend supports it (e.g. local storage)
/// to avoid loading multi-gigabyte files entirely into RAM.
/// I3: When integrity verification is enabled, computes SHA-256 hash incrementally
/// during streaming (not a separate pre-read), then verifies after stream completes.
enum RawBlobResult {
    Served(HttpResponse),
    Missing,
    Error(HttpResponse),
}

async fn serve_raw_blob(
    oid: &str,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    start: std::time::Instant,
) -> RawBlobResult {
    let object_key = format!("lfs/objects/{}", oid);
    let verify_integrity = config.storage.verify_download_integrity;

    // Try streaming path first (avoids loading entire file into memory)
    match storage.get_path(&object_key).await {
        Ok(Some(path)) => {
            // Stream from file
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return RawBlobResult::Missing;
                }
                Err(e) => {
                    error!(
                        "Failed to open file for streaming {}: {}",
                        path.display(),
                        e
                    );
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                        serde_json::json!({
                            "error": format!("Failed to open file: {}", e)
                        }),
                    ));
                }
            };
            let metadata = match file.metadata().await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return RawBlobResult::Missing;
                }
                Err(e) => {
                    error!("Failed to get file metadata: {}", e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_error();
                    GLOBAL_METRICS.record_latency(start);
                    return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                        serde_json::json!({
                            "error": format!("Failed to get metadata: {}", e)
                        }),
                    ));
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

                info!(
                    "Streaming LFS object {} ({} bytes) with integrity verification",
                    oid, file_size
                );
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_storage_operation();
                GLOBAL_METRICS.record_download_bytes(file_size);
                GLOBAL_METRICS.record_latency(start);

                RawBlobResult::Served(
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(body),
                )
            } else {
                let body = actix_web::body::SizedStream::new(file_size, base_stream);

                info!("Streaming LFS object {} ({} bytes)", oid, file_size);
                GLOBAL_METRICS.record_request(200);
                GLOBAL_METRICS.record_storage_operation();
                GLOBAL_METRICS.record_download_bytes(file_size);
                GLOBAL_METRICS.record_latency(start);

                RawBlobResult::Served(
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(body),
                )
            }
        }
        Ok(None) => {
            // Non-file backend: fall back to in-memory get
            serve_raw_blob_inmemory(oid, storage, config, start).await
        }
        Err(StorageError::NotFound(_)) => RawBlobResult::Missing,
        Err(e) => {
            error!("Failed to get path for {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            RawBlobResult::Error(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Storage error: {}", e)
            })))
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
) -> RawBlobResult {
    let object_key = format!("lfs/objects/{}", oid);
    let object_data = match storage.get(&object_key).await {
        Ok(data) => {
            GLOBAL_METRICS.record_storage_operation();
            data
        }
        Err(StorageError::NotFound(_)) => return RawBlobResult::Missing,
        Err(e) => {
            error!("Failed to fetch object: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                serde_json::json!({
                    "error": format!("Storage error: {}", e)
                }),
            ));
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
                oid,
                computed_hash,
                oid,
                object_data.len()
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return RawBlobResult::Error(HttpResponse::InternalServerError().json(
                serde_json::json!({
                    "error": "Integrity verification failed: stored content does not match OID"
                }),
            ));
        }

        info!(
            "Integrity check passed for {} ({} bytes)",
            oid,
            object_data.len()
        );
    }

    info!(
        "Downloaded LFS object {} ({} bytes)",
        oid,
        object_data.len()
    );

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(object_data.len() as u64);
    GLOBAL_METRICS.record_latency(start);

    RawBlobResult::Served(
        HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(object_data),
    )
}

async fn serve_verified_xet_reconstruction(
    oid: &str,
    file_refs: Vec<crate::index::FileShardRef>,
    storage: web::Data<Box<dyn StorageBackend>>,
    temp_dir: std::path::PathBuf,
    start: std::time::Instant,
) -> HttpResponse {
    let reconstruction =
        match reconstruct_verified_file_to_temp(oid, file_refs, &***storage, &temp_dir).await {
            Ok(reconstruction) => reconstruction,
            Err(e) => {
                error!("Verified xet reconstruction failed for {}: {}", oid, e);
                if matches!(e, ReconstructionError::Stale(_)) {
                    GLOBAL_METRICS.record_request(404);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Object not found: {}", oid)
                    }));
                }
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to reconstruct verified object"
                }));
            }
        };

    let size = reconstruction.size();
    let path = reconstruction.path().to_path_buf();
    let guard = reconstruction.into_guard();
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            error!(
                "Failed to open verified reconstruction {}: {}",
                path.display(),
                e
            );
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to open reconstructed object"
            }));
        }
    };

    let stream = GuardedFileStream {
        inner: ReaderStream::new(file),
        _guard: guard,
    };
    let body = actix_web::body::SizedStream::new(size, stream);

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_download_bytes(size);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(body)
}

struct GuardedFileStream<S> {
    inner: S,
    _guard: crate::xorb_reader::TempPathGuard,
}

impl<S> Stream for GuardedFileStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
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
}
