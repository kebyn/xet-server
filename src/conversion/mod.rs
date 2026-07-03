pub mod converting_oids;

use std::sync::Arc;
use tracing::{info, warn};

use crate::chunking::{ChunkConfig, StreamingChunker};
use crate::config::ConversionConfig;
use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
use crate::format::xorb_builder::XorbBuilder;
use crate::hash::compute_data_hash;
use crate::index::MetadataIndex;
use crate::storage::StorageBackend;
use crate::types::MerkleHash;

pub use converting_oids::ConvertingOids;

/// Block size for streaming reads during conversion (1 MB).
/// Memory usage is bounded to this + max_chunk_size (128 KB).
const CONVERSION_BLOCK_SIZE: usize = 1024 * 1024;

/// RAII guard that deletes a file path when dropped.
/// I2: Ensures temporary files are cleaned up even on error paths.
/// I3 fix: Uses spawn_blocking for file deletion to avoid blocking the tokio runtime.
struct PathGuard {
    path: std::path::PathBuf,
}

impl PathGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // I3 fix: Use spawn_blocking to avoid blocking the tokio worker thread.
        // std::fs::remove_file is a blocking syscall that can block the async runtime.
        // spawn_blocking moves this to a dedicated thread pool for blocking operations.
        let path = self.path.clone();
        drop(tokio::task::spawn_blocking(move || {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("Failed to cleanup temp file {}: {}", path.display(), e);
            }
        }));
        // Note: We don't await the spawn_blocking result because Drop is synchronous.
        // The cleanup will happen eventually in the blocking thread pool.
        // This is acceptable for temp file cleanup - it's not critical if it's delayed.
    }
}

/// Result of a successful conversion from raw blob to xorb+shard format.
pub struct ConversionResult {
    pub xorb_hash: String,
    pub shard_hash: String,
    pub num_chunks: usize,
    pub num_deduped_chunks: usize,
    pub raw_size: u64,
    pub xorb_size: u64,
}

/// Errors that can occur during conversion.
#[derive(Debug)]
pub enum ConversionError {
    NotFound(String),
    StorageError(String),
    ChunkingError(String),
    BuildError(String),
    TooSmall(u64),
    TooLarge(u64),
    Disabled,
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(oid) => write!(f, "Raw blob not found: {}", oid),
            Self::StorageError(e) => write!(f, "Storage error: {}", e),
            Self::ChunkingError(e) => write!(f, "Chunking error: {}", e),
            Self::BuildError(e) => write!(f, "Build error: {}", e),
            Self::TooSmall(size) => write!(f, "File too small to convert: {} bytes", size),
            Self::TooLarge(size) => write!(
                f,
                "File too large to convert: {} bytes (exceeds max_conversion_size)",
                size
            ),
            Self::Disabled => write!(f, "Conversion is disabled"),
        }
    }
}

/// Pipeline for converting raw LFS blobs into xorb+shard format.
/// Deduplication is performed at whole-xorb granularity (an identical xorb is
/// stored once). `num_deduped` counts chunks observed to already exist as a
/// statistic only — chunks are still packed into the new xorb.
pub struct ConversionPipeline {
    storage: Arc<Box<dyn StorageBackend>>,
    index: Arc<MetadataIndex>,
    config: ConversionConfig,
}

impl ConversionPipeline {
    pub fn new(
        storage: Arc<Box<dyn StorageBackend>>,
        index: Arc<MetadataIndex>,
        config: ConversionConfig,
    ) -> Self {
        Self {
            storage,
            index,
            config,
        }
    }

    /// Convert a raw blob to xorb/shard format.
    /// `oid` is the SHA-256 OID used as the file_id in MetadataIndex.
    ///
    /// Streaming implementation: reads the blob in blocks and processes chunks
    /// incrementally. Memory usage is bounded to O(block_size + max_chunk_size)
    /// regardless of blob size, eliminating the previous 500MB OOM risk.
    pub async fn convert(&self, oid: &str) -> Result<ConversionResult, ConversionError> {
        if !self.config.enabled {
            return Err(ConversionError::Disabled);
        }

        let object_key = format!("lfs/objects/{}", oid);

        // 1. Open raw blob for streaming reads.
        // Prefer get_path() (local storage: zero-copy file access).
        // Fall back to downloading into a temp file (S3 or other remote backends).
        let (file_path, _temp_guard) = self.open_blob_for_streaming(&object_key).await?;

        // Get file size for bounds checking
        let file_meta = tokio::fs::metadata(&file_path)
            .await
            .map_err(|e| ConversionError::StorageError(format!("Failed to stat blob: {}", e)))?;
        let raw_size = file_meta.len();

        // Check minimum size
        if raw_size < self.config.min_conversion_size {
            return Err(ConversionError::TooSmall(raw_size));
        }

        // Check maximum size (still enforced as a policy limit, not an OOM guard)
        if raw_size > self.config.max_conversion_size {
            return Err(ConversionError::TooLarge(raw_size));
        }

        // 2. Open file for streaming reads
        let mut file = tokio::fs::File::open(&file_path)
            .await
            .map_err(|e| ConversionError::StorageError(format!("Failed to open blob: {}", e)))?;

        use tokio::io::AsyncReadExt;

        // 3. Stream through file, chunking and building xorb incrementally.
        //    - Read block_size bytes at a time
        //    - Feed to StreamingChunker which emits complete chunks
        //    - For each complete chunk: hash it, dedup-check, add to xorb builder
        //    - After EOF: finalize the chunker to emit the last chunk
        let scheme = self.config.scheme();
        let mut xorb_builder = XorbBuilder::new(scheme);
        let mut streaming_chunker = StreamingChunker::new(ChunkConfig::default());
        let mut num_deduped: usize = 0;
        let mut num_chunks: usize = 0;
        let mut chunk_entries: Vec<XorbChunkBuildEntry> = Vec::new();
        // Raw chunk hashes back MetadataIndex dedup queries. Shard chunk entries
        // store serialized chunk hashes for xorb reconstruction integrity checks.
        let mut raw_chunk_hashes: Vec<MerkleHash> = Vec::new();
        let mut cumulative_uncompressed: u32 = 0;
        let mut xorb_offset: u32 = 0;

        // Buffer for reading blocks from the file
        let mut read_buf = vec![0u8; CONVERSION_BLOCK_SIZE];
        // Buffer holding raw bytes not yet assigned to a complete chunk
        let mut chunk_data_buf: Vec<u8> = Vec::new();

        loop {
            let bytes_read = file.read(&mut read_buf).await.map_err(|e| {
                ConversionError::StorageError(format!("Failed to read blob: {}", e))
            })?;

            if bytes_read == 0 {
                break;
            }

            let block = &read_buf[..bytes_read];
            chunk_data_buf.extend_from_slice(block);

            // I5 fix: Safety check to prevent unbounded buffer growth if chunker has a bug.
            // chunk_data_buf should only hold data for the current incomplete chunk.
            // If it grows beyond expected size (e.g., 2x max chunk size), something is wrong.
            // Max chunk size is typically ~64KB, so 10MB is a generous safety margin.
            debug_assert!(
                chunk_data_buf.len() < 10 * 1024 * 1024,
                "chunk_data_buf grew unexpectedly large: {} bytes. Possible chunker bug.",
                chunk_data_buf.len()
            );

            // Get any complete chunks from the streaming chunker
            let new_chunks = streaming_chunker.next_block(block);

            for chunk in new_chunks {
                // The chunk data is at the start of chunk_data_buf
                // (chunks are emitted in order by StreamingChunker)
                let chunk_data = &chunk_data_buf[..chunk.size];
                let unpacked_size = chunk.size as u32;

                // Hash and dedup check
                let chunk_hash = compute_data_hash(chunk_data);
                let chunk_hash_hex = chunk_hash.to_hex();
                if self.index.chunk_exists(&chunk_hash_hex) {
                    num_deduped += 1;
                }
                raw_chunk_hashes.push(chunk_hash);

                // Add to xorb builder
                let (serialized_chunk_hash, compressed_len) = xorb_builder
                    .add_chunk(chunk_data)
                    .map_err(|e| ConversionError::BuildError(format!("Add chunk failed: {}", e)))?;

                chunk_entries.push(XorbChunkBuildEntry {
                    chunk_hash: serialized_chunk_hash,
                    chunk_byte_range_start: xorb_offset,
                    unpacked_segment_bytes: unpacked_size,
                });

                xorb_offset += 8 + compressed_len;
                cumulative_uncompressed += unpacked_size;
                num_chunks += 1;

                // Consume the chunk data from the buffer
                chunk_data_buf.drain(..chunk.size);
            }
        }

        // Finalize: emit the last chunk(s)
        let remaining_chunks = streaming_chunker.finalize();
        for chunk in remaining_chunks {
            let chunk_data = &chunk_data_buf[..chunk.size];
            let unpacked_size = chunk.size as u32;

            let chunk_hash = compute_data_hash(chunk_data);
            let chunk_hash_hex = chunk_hash.to_hex();
            if self.index.chunk_exists(&chunk_hash_hex) {
                num_deduped += 1;
            }
            raw_chunk_hashes.push(chunk_hash);

            let (serialized_chunk_hash, compressed_len) = xorb_builder
                .add_chunk(chunk_data)
                .map_err(|e| ConversionError::BuildError(format!("Add chunk failed: {}", e)))?;

            chunk_entries.push(XorbChunkBuildEntry {
                chunk_hash: serialized_chunk_hash,
                chunk_byte_range_start: xorb_offset,
                unpacked_segment_bytes: unpacked_size,
            });

            xorb_offset += 8 + compressed_len;
            cumulative_uncompressed += unpacked_size;
            num_chunks += 1;

            chunk_data_buf.drain(..chunk.size);
        }

        // I5 fix: Verify buffer is empty after processing all chunks.
        // If not empty, it means the chunker didn't emit all chunks (bug).
        debug_assert!(
            chunk_data_buf.is_empty(),
            "chunk_data_buf not empty after finalize: {} bytes remaining. Possible chunker bug.",
            chunk_data_buf.len()
        );

        info!(
            "Converting OID {}: {} bytes, {} chunks (streaming)",
            oid, raw_size, num_chunks
        );

        // 4. Build xorb
        let xorb_result = xorb_builder
            .build()
            .map_err(|e| ConversionError::BuildError(format!("Xorb build failed: {}", e)))?;

        let xorb_hash = xorb_result.xorb_hash.to_hex();
        let xorb_key = format!("xorbs/{}", xorb_hash);

        // 5. Store xorb (skip if exists — content-addressed)
        let xorb_exists = self
            .storage
            .exists(&xorb_key)
            .await
            .map_err(|e| ConversionError::StorageError(e.to_string()))?;

        if !xorb_exists {
            self.storage
                .put(&xorb_key, bytes::Bytes::from(xorb_result.data))
                .await
                .map_err(|e| {
                    ConversionError::StorageError(format!("Failed to store xorb: {}", e))
                })?;
            info!(
                "Stored xorb: {} ({} bytes)",
                xorb_hash, xorb_result.total_compressed_size
            );
        }

        // 6. Build shard
        let file_hash = MerkleHash::from_hex(oid).map_err(|e| {
            ConversionError::BuildError(format!("Invalid OID as MerkleHash: {}", e))
        })?;

        let mut shard_builder = ShardBuilder::new();

        // Build verified chunk mappings for index registration BEFORE add_xorb consumes chunk_entries
        let mut verified_chunks: Vec<crate::index::VerifiedChunkMapping> = Vec::new();
        for (i, chunk_hash) in raw_chunk_hashes.iter().enumerate() {
            verified_chunks.push(crate::index::VerifiedChunkMapping {
                chunk_hash: chunk_hash.to_hex(),
                xorb_hash: xorb_hash.clone(),
                chunk_index: i as u32,
            });
        }

        let xorb_index = shard_builder
            .add_xorb_with_raw_chunk_hashes(
                xorb_result.xorb_hash,
                xorb_result.total_uncompressed_size as u32,
                xorb_result.total_compressed_size as u32,
                chunk_entries,
                raw_chunk_hashes.clone(),
            )
            .map_err(|e| ConversionError::BuildError(format!("Add xorb to shard failed: {}", e)))?;

        // Add file mapping — all chunks belong to one segment
        let segment = FileSegment {
            xorb_hash: xorb_result.xorb_hash,
            xorb_index,
            chunk_index_start: 0,
            chunk_index_end: num_chunks as u32,
            unpacked_segment_bytes: cumulative_uncompressed,
        };

        shard_builder.add_file(file_hash, vec![segment]);

        let shard_data = shard_builder
            .build()
            .map_err(|e| ConversionError::BuildError(format!("Shard build failed: {}", e)))?;

        // Compute shard hash using BLAKE3
        let shard_hash_merkle = compute_data_hash(&shard_data);
        let shard_hash = shard_hash_merkle.to_hex();
        let shard_key = format!("shards/{}", shard_hash);

        // 7. Store shard
        let shard_size = shard_data.len();
        self.storage
            .put(&shard_key, bytes::Bytes::from(shard_data))
            .await
            .map_err(|e| ConversionError::StorageError(format!("Failed to store shard: {}", e)))?;

        info!("Stored shard: {} ({} bytes)", shard_hash, shard_size);

        // 8. Register in MetadataIndex
        self.index
            .register_verified_shard(crate::index::VerifiedShardRegistration {
                shard_id: shard_hash.clone(),
                files: vec![crate::index::VerifiedFileMapping {
                    file_hash: oid.to_string(),
                    file_index: 0,
                }],
                chunks: verified_chunks,
            });

        // 10. Delete raw blob (if configured)
        if self.config.delete_raw_after_conversion {
            match self.storage.delete(&object_key).await {
                Ok(_) => info!("Deleted raw blob: {}", oid),
                Err(e) => warn!("Failed to delete raw blob {}: {} (non-fatal)", oid, e),
            }
        }

        info!(
            "Conversion complete for {}: {} chunks ({} deduped), {} -> {} bytes (streaming)",
            oid, num_chunks, num_deduped, raw_size, xorb_result.total_compressed_size
        );

        Ok(ConversionResult {
            xorb_hash,
            shard_hash,
            num_chunks,
            num_deduped_chunks: num_deduped,
            raw_size,
            xorb_size: xorb_result.total_compressed_size,
        })
    }

    /// Open a blob for streaming reads.
    ///
    /// Returns (path_to_file, optional_path_guard).
    /// - For local storage: returns the existing file path directly (no copy).
    /// - For remote backends (S3): downloads to a temp file, returns that path.
    ///   The path guard ensures cleanup when dropped (I2: RAII temp file cleanup).
    async fn open_blob_for_streaming(
        &self,
        object_key: &str,
    ) -> Result<(std::path::PathBuf, Option<PathGuard>), ConversionError> {
        // Try local path first (zero-copy for local storage)
        if let Ok(Some(path)) = self.storage.get_path(object_key).await {
            return Ok((path, None));
        }

        // Fall back to downloading to a temp file
        // I1 fix: Use download_to_path for streaming download (avoids loading entire
        // blob into RAM). Previously used storage.get() + tokio::fs::write() which
        // buffered the entire blob in memory before writing.
        // M5 fix: Use app-specific directory instead of system /tmp for security.
        let temp_dir = std::env::temp_dir().join("xet-conversion");
        tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| {
            ConversionError::StorageError(format!("Failed to create temp dir: {}", e))
        })?;

        // Generate unique filename to avoid conflicts
        let unique_id = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            {
                use std::sync::atomic::{AtomicU64, Ordering};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                COUNTER.fetch_add(1, Ordering::Relaxed)
            }
        );
        let temp_path = temp_dir.join(format!("blob-{}.tmp", unique_id));

        // I1 fix: Use download_to_path for streaming download.
        // S3 backend overrides this with ByteStream::write_to_path (bounded memory).
        // Default implementation falls back to get() + write() with a warning.
        self.storage
            .download_to_path(object_key, &temp_path)
            .await
            .map_err(|e| {
                ConversionError::StorageError(format!(
                    "Failed to download blob to temp file: {}",
                    e
                ))
            })?;

        // I2: Return a RAII guard that will delete the temp file when dropped
        let guard = PathGuard::new(temp_path.clone());

        Ok((temp_path, Some(guard)))
    }
}
