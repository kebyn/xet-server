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
use tracing::{error, info};

use crate::api::auth::{check_scope, extract_token_from_request, AuthVerifier};
use crate::api::reconstruction::fetch_and_parse_shard;
use crate::config::{ConversionConfig, ServerConfig};
use crate::conversion::ConvertingOids;
use crate::format::compression::decompress;
use crate::format::shard::MDBShardFile;
use crate::format::xorb::XorbChunkHeader;
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
///
/// I4: True streaming implementation using custom ReconstructionStream.
/// Processes one chunk at a time, avoiding temp files and minimizing memory usage.
async fn reconstruct_from_xet(
    file_id: &str,
    index: web::Data<crate::index::MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
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
    // web::Data is internally Arc-wrapped
    let storage_arc = storage.clone().into_inner();
    let stream = ReconstructionStream::new(storage_arc, shard_ids);

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

/// Custom Stream for streaming reconstructed file data chunk-by-chunk.
///
/// I4: True streaming implementation that processes one chunk at a time,
/// avoiding loading the entire file into memory or using temporary files.
///
/// State machine:
/// 1. Fetch next shard (if needed)
/// 2. Download next xorb (if needed)
/// 3. Extract and decompress next chunk
/// 4. Return chunk data
/// 5. Repeat until all chunks processed
// Type alias for the pinned boxed future that fetches and parses a shard.
type ShardFetchFuture = Pin<Box<dyn std::future::Future<Output = Result<MDBShardFile, String>> + Send>>;
// Type alias for the pinned boxed future that fetches xorb data.
type XorbFetchFuture = Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, StorageError>> + Send>>;

struct ReconstructionStream {
    /// Storage backend for fetching xorbs
    storage: Arc<Box<dyn StorageBackend>>,
    /// List of shard IDs to process
    shard_ids: Vec<String>,
    /// Current shard index
    current_shard_idx: usize,
    /// Parsed shard data (cached)
    current_shard: Option<MDBShardFile>,
    /// Current chunk index within the shard
    current_chunk_idx: usize,
    /// Xorb entries from current shard (deduplicated)
    xorb_entries: Vec<(String, usize, usize)>, // (xorb_hash, num_entries, chunk_offset)
    /// Current xorb index
    current_xorb_idx: usize,
    /// Currently loaded xorb data
    current_xorb_data: Option<Vec<u8>>,
    /// Future for fetching the next shard
    fetch_shard_future: Option<ShardFetchFuture>,
    /// Future for fetching the next xorb
    fetch_xorb_future: Option<XorbFetchFuture>,
    /// Total bytes yielded (for metrics)
    total_bytes: u64,
    /// Whether the stream has completed
    completed: bool,
}

impl ReconstructionStream {
    fn new(storage: Arc<Box<dyn StorageBackend>>, shard_ids: Vec<String>) -> Self {
        Self {
            storage,
            shard_ids,
            current_shard_idx: 0,
            current_shard: None,
            current_chunk_idx: 0,
            xorb_entries: Vec::new(),
            current_xorb_idx: 0,
            current_xorb_data: None,
            fetch_shard_future: None,
            fetch_xorb_future: None,
            total_bytes: 0,
            completed: false,
        }
    }
}

impl Stream for ReconstructionStream {
    type Item = Result<bytes::Bytes, String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.completed {
            return Poll::Ready(None);
        }

        loop {
            // If we're fetching a shard, poll that future first
            if let Some(future) = self.fetch_shard_future.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(shard)) => {
                        // Extract xorb entries from shard
                        let mut seen_xorbs = std::collections::HashSet::new();
                        self.xorb_entries.clear();
                        let mut chunk_offset = 0;

                        for xorb_entry in &shard.xorb_entries {
                            let xorb_hash = xorb_entry.xorb_hash.to_hex();
                            if seen_xorbs.insert(xorb_hash.clone()) {
                                self.xorb_entries.push((
                                    xorb_hash,
                                    xorb_entry.num_entries as usize,
                                    chunk_offset,
                                ));
                            }
                            chunk_offset += xorb_entry.num_entries as usize;
                        }

                        self.current_shard = Some(shard);
                        self.current_xorb_idx = 0;
                        self.current_chunk_idx = 0;
                        self.fetch_shard_future = None;
                        // Continue to process xorb
                    }
                    Poll::Ready(Err(e)) => {
                        self.completed = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // If we're fetching a xorb, poll that future
            if let Some(future) = self.fetch_xorb_future.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(data)) => {
                        self.current_xorb_data = Some(data);
                        self.fetch_xorb_future = None;
                        // Continue to extract chunks from the xorb
                    }
                    Poll::Ready(Err(e)) => {
                        self.completed = true;
                        return Poll::Ready(Some(Err(format!("Failed to fetch xorb: {}", e))));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // Check if we need to fetch the next shard
            if self.current_shard.is_none() {
                if self.current_shard_idx >= self.shard_ids.len() {
                    // All shards processed
                    self.completed = true;
                    return Poll::Ready(None);
                }

                // Create future for fetching shard asynchronously
                let shard_id = self.shard_ids[self.current_shard_idx].clone();
                let storage = self.storage.clone();
                let shard_id_clone = shard_id.clone();

                let future = Box::pin(async move {
                    fetch_and_parse_shard(&shard_id_clone, &**storage)
                        .await
                        .map_err(|e| format!("Failed to fetch shard {}: {}", shard_id, e))
                });
                self.fetch_shard_future = Some(future);
                self.current_shard_idx += 1;
                // Continue loop to poll the future
                continue;
            }

            // Check if we've processed all xorbs in current shard
            if self.current_xorb_idx >= self.xorb_entries.len() {
                // Move to next shard
                self.current_shard = None;
                self.current_xorb_data = None;
                continue;
            }

            // Check if we need to download the next xorb
            if self.current_xorb_data.is_none() {
                let (xorb_hash, _, _) = &self.xorb_entries[self.current_xorb_idx];
                let xorb_key = format!("xorbs/{}", xorb_hash);
                let storage = self.storage.clone();

                // Create future for fetching xorb
                let future = Box::pin(async move {
                    storage.get(&xorb_key).await.map(|b| b.to_vec())
                });
                self.fetch_xorb_future = Some(future);
                // Continue loop to poll the future
                continue;
            }

            // Extract next chunk from current xorb
            let shard = self.current_shard.as_ref().unwrap();
            let xorb_data = self.current_xorb_data.as_ref().unwrap();
            let (_, num_entries, chunk_offset) = self.xorb_entries[self.current_xorb_idx];

            // Check if we've processed all chunks in current xorb
            if self.current_chunk_idx >= num_entries {
                // Move to next xorb
                self.current_xorb_idx += 1;
                self.current_chunk_idx = 0;
                self.current_xorb_data = None;
                continue;
            }

            // Get chunk entry
            let global_chunk_idx = chunk_offset + self.current_chunk_idx;
            if global_chunk_idx >= shard.xorb_chunk_entries.len() {
                // Should not happen, but handle gracefully
                self.current_xorb_idx += 1;
                self.current_chunk_idx = 0;
                self.current_xorb_data = None;
                continue;
            }

            let chunk_entry = &shard.xorb_chunk_entries[global_chunk_idx];
            let chunk_offset_bytes = chunk_entry.chunk_byte_range_start as usize;

            // Read chunk header
            if chunk_offset_bytes + 8 > xorb_data.len() {
                self.completed = true;
                return Poll::Ready(Some(Err("Chunk offset out of bounds".to_string())));
            }

            let mut chunk_cursor = std::io::Cursor::new(&xorb_data[chunk_offset_bytes..]);
            let chunk_header = match XorbChunkHeader::deserialize(&mut chunk_cursor) {
                Ok(h) => h,
                Err(e) => {
                    self.completed = true;
                    return Poll::Ready(Some(Err(format!("Failed to parse chunk header: {}", e))));
                }
            };

            // Read compressed chunk data
            let data_start = chunk_offset_bytes + XorbChunkHeader::SIZE;
            let data_end = data_start + chunk_header.compressed_length as usize;
            if data_end > xorb_data.len() {
                self.completed = true;
                return Poll::Ready(Some(Err("Chunk data out of bounds".to_string())));
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
                    self.completed = true;
                    return Poll::Ready(Some(Err(format!("Failed to decompress chunk: {}", e))));
                }
            };

            self.total_bytes += decompressed.len() as u64;
            self.current_chunk_idx += 1;

            return Poll::Ready(Some(Ok(bytes::Bytes::from(decompressed))));
        }
    }
}

/// Check if there's enough disk space for an upload.
/// Delegates to the shared utility in crate::util::disk.
fn check_disk_space(path: &std::path::Path, required_bytes: u64) -> Result<(), String> {
    crate::util::disk::check_disk_space(path, required_bytes)
}
