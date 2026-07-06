use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::chunking::ChunkConfig;
use crate::format::compression::{CompressionScheme, decompress};
use crate::format::xorb::XorbChunkHeader;
use crate::types::MerkleHash;
use crate::util::StreamingHasher;

pub fn max_compressed_len_for_chunk(
    scheme: CompressionScheme,
    expected_uncompressed_len: usize,
) -> usize {
    match scheme {
        CompressionScheme::None => expected_uncompressed_len,
        // lz4_flex::compress_prepend_size uses the LZ4 block format plus a 4-byte
        // original-size prefix. The LZ4 worst-case bound is n + n/255 + 16.
        CompressionScheme::LZ4 | CompressionScheme::ByteGrouping4LZ4 => {
            expected_uncompressed_len + expected_uncompressed_len / 255 + 20
        }
    }
}

#[derive(Debug)]
pub struct TempPathGuard {
    path: Option<std::path::PathBuf>,
}

impl TempPathGuard {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn try_path(&self) -> std::result::Result<&std::path::Path, String> {
        self.path
            .as_deref()
            .ok_or_else(|| "temp path already cleaned".to_string())
    }

    pub fn path(&self) -> &std::path::Path {
        self.path
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(""))
    }

    pub async fn cleanup(mut self) {
        if let Some(path) = self.path.take() {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _ = std::fs::remove_file(path);
            });
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Read one chunk from a xorb file at `chunk_offset_bytes`, verify header+compressed
/// bytes against `expected_hash`, then decompress it.
///
/// This keeps reconstruction memory bounded to one compressed chunk plus one
/// decompressed chunk, instead of loading the entire xorb into memory.
pub async fn extract_chunk_verified_from_file(
    xorb_file: &mut tokio::fs::File,
    chunk_offset_bytes: u64,
    expected_uncompressed_len: u32,
    expected_hash: &MerkleHash,
) -> std::result::Result<bytes::Bytes, String> {
    xorb_file
        .seek(std::io::SeekFrom::Start(chunk_offset_bytes))
        .await
        .map_err(|e| {
            format!(
                "Failed to seek to chunk offset {}: {}",
                chunk_offset_bytes, e
            )
        })?;

    let mut header_bytes = [0u8; XorbChunkHeader::SIZE];
    xorb_file.read_exact(&mut header_bytes).await.map_err(|e| {
        format!(
            "Failed to read chunk header at offset {}: {}",
            chunk_offset_bytes, e
        )
    })?;

    let mut chunk_cursor = std::io::Cursor::new(&header_bytes);
    let chunk_header = XorbChunkHeader::deserialize(&mut chunk_cursor)
        .map_err(|e| format!("Failed to parse chunk header: {}", e))?;

    let max_uncompressed_len = ChunkConfig::default().max_chunk_size();
    if expected_uncompressed_len as usize > max_uncompressed_len {
        return Err(format!(
            "Shard chunk length {} exceeds maximum {} at offset {}",
            expected_uncompressed_len, max_uncompressed_len, chunk_offset_bytes
        ));
    }

    if chunk_header.uncompressed_length != expected_uncompressed_len {
        return Err(format!(
            "Chunk length mismatch at offset {}: header declares {} bytes, shard expects {} bytes",
            chunk_offset_bytes, chunk_header.uncompressed_length, expected_uncompressed_len
        ));
    }

    let max_compressed_len = max_compressed_len_for_chunk(
        chunk_header.compression_scheme,
        expected_uncompressed_len as usize,
    );
    if chunk_header.compressed_length as usize > max_compressed_len {
        return Err(format!(
            "Chunk compressed length {} exceeds maximum {} at offset {}",
            chunk_header.compressed_length, max_compressed_len, chunk_offset_bytes
        ));
    }

    let mut compressed_data = vec![0u8; chunk_header.compressed_length as usize];
    xorb_file
        .read_exact(&mut compressed_data)
        .await
        .map_err(|e| {
            format!(
                "Failed to read chunk data at offset {} ({} bytes): {}",
                chunk_offset_bytes, chunk_header.compressed_length, e
            )
        })?;

    // Verify the exact same byte region as xet-core: header + compressed payload.
    let mut hasher = StreamingHasher::new();
    hasher.update(&header_bytes);
    hasher.update(&compressed_data);
    let actual_hash = hasher.finalize();
    if actual_hash != *expected_hash {
        return Err(format!(
            "Chunk hash mismatch at offset {}: stored data is corrupted",
            chunk_offset_bytes
        ));
    }

    let decompressed = decompress(
        chunk_header.compression_scheme,
        &compressed_data,
        chunk_header.uncompressed_length as usize,
    )
    .map_err(|e| format!("Failed to decompress chunk: {}", e))?;
    Ok(bytes::Bytes::from(decompressed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temp_path_guard_try_path_reports_cleaned_guard() {
        let mut guard = TempPathGuard::new(std::path::PathBuf::from("xorb.tmp"));
        guard.path = None;

        let err = guard
            .try_path()
            .expect_err("cleaned guard should return an error");

        assert!(err.contains("cleaned"));
    }
}
