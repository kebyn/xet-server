use std::io::Write;

use crate::error::Result;
use crate::format::compression::{CompressionScheme, compress};
use crate::format::xorb::{XorbChunkHeader, XorbObjectInfoV1, verify_xorb};
use crate::hash::{compute_data_hash, xorb_hash};
use crate::types::MerkleHash;

/// Internal tracking for each chunk added to the builder.
struct ChunkData {
    chunk_hash: MerkleHash,
    /// M4 fix: Store the serialized chunk bytes (header + compressed data) to avoid
    /// re-serializing in build(). Previously, add_chunk() serialized to compute the hash,
    /// then build() re-created the header and serialized again — doubling CPU and allocations.
    serialized_chunk: Vec<u8>,
    compressed_len: u32,
    uncompressed_size: u32,
    /// Byte offset in the xorb binary where the NEXT chunk starts
    /// (i.e., end of this chunk including its header).
    boundary_offset: u32,
    /// Cumulative uncompressed size before this chunk.
    unpacked_offset: u32,
}

/// Result of building a xorb from accumulated chunks.
pub struct XorbBuildResult {
    pub data: Vec<u8>,
    pub xorb_hash: MerkleHash,
    pub chunk_hashes: Vec<MerkleHash>,
    pub total_uncompressed_size: u64,
    pub total_compressed_size: u64,
}

/// Assembles raw data chunks into a valid xorb binary format.
///
/// Usage:
/// ```ignore
/// let mut builder = XorbBuilder::new(CompressionScheme::LZ4);
/// builder.add_chunk(b"hello")?;
/// builder.add_chunk(b"world")?;
/// let result = builder.build()?;
/// ```
pub struct XorbBuilder {
    chunks: Vec<ChunkData>,
    compression_scheme: CompressionScheme,
    /// Running total of (8 + compressed_size) for all chunks added so far.
    next_boundary_offset: u32,
    /// Running total of uncompressed_size for all chunks added so far.
    next_unpacked_offset: u32,
}

impl XorbBuilder {
    /// Create a new builder that will compress chunks with the given scheme.
    pub fn new(scheme: CompressionScheme) -> Self {
        Self {
            chunks: Vec::new(),
            compression_scheme: scheme,
            next_boundary_offset: 0,
            next_unpacked_offset: 0,
        }
    }

    /// Add a chunk of raw (uncompressed) data.
    ///
    /// The data is compressed according to the builder's compression scheme,
    /// and the chunk hash is computed over the serialized chunk (header + compressed data)
    /// to match the verification logic in `verify_xorb`.
    ///
    /// Returns `(chunk_hash, compressed_data_len)` where `compressed_data_len` is
    /// the length of the compressed payload (excluding the 8-byte header).
    pub fn add_chunk(&mut self, data: &[u8]) -> Result<(MerkleHash, u32)> {
        let uncompressed_size = data.len() as u32;
        let compressed_data = compress(self.compression_scheme, data)?;

        // Build the chunk header to compute the serialized form.
        let header = XorbChunkHeader {
            version: 1,
            compressed_length: compressed_data.len() as u32,
            compression_scheme: self.compression_scheme,
            uncompressed_length: uncompressed_size,
        };

        let mut chunk_bytes = Vec::with_capacity(XorbChunkHeader::SIZE + compressed_data.len());
        header.serialize(&mut chunk_bytes)?;
        chunk_bytes.write_all(&compressed_data)?;

        // The chunk hash covers the entire serialized chunk (header + compressed data),
        // matching how verify_xorb hashes each chunk region.
        let chunk_hash = compute_data_hash(&chunk_bytes);

        // Use running totals instead of iterating all prior chunks (O(1) vs O(n)).
        let boundary_offset = self.next_boundary_offset + chunk_bytes.len() as u32;
        let unpacked_offset = self.next_unpacked_offset;

        let compressed_len = compressed_data.len() as u32;

        self.chunks.push(ChunkData {
            chunk_hash,
            serialized_chunk: chunk_bytes,
            compressed_len,
            uncompressed_size,
            boundary_offset,
            unpacked_offset,
        });

        // Update running totals for the next chunk.
        self.next_boundary_offset = boundary_offset;
        self.next_unpacked_offset += uncompressed_size;

        Ok((chunk_hash, compressed_len))
    }

    /// Consume the builder and produce the final xorb binary.
    ///
    /// The returned `XorbBuildResult` contains the complete xorb data
    /// (chunks + footer) along with metadata. The xorb is verified
    /// with `verify_xorb` before returning.
    pub fn build(self) -> Result<XorbBuildResult> {
        let mut buf = Vec::new();

        let chunk_hashes: Vec<MerkleHash> = self.chunks.iter().map(|c| c.chunk_hash).collect();
        let boundary_offsets: Vec<u32> = self.chunks.iter().map(|c| c.boundary_offset).collect();
        let unpacked_offsets: Vec<u32> = self.chunks.iter().map(|c| c.unpacked_offset).collect();

        // M4 fix: Write pre-serialized chunk bytes directly (no re-serialization).
        // Each chunk was fully serialized (header + compressed data) in add_chunk().
        for chunk in &self.chunks {
            buf.extend_from_slice(&chunk.serialized_chunk);
        }

        // Compute the xorb hash from (chunk_hash, serialized_chunk_size) pairs.
        // verify_xorb uses the on-disk byte length of each chunk region as the size.
        let chunk_info: Vec<(MerkleHash, u64)> = self
            .chunks
            .iter()
            .map(|c| {
                let serialized_size = c.serialized_chunk.len() as u64;
                (c.chunk_hash, serialized_size)
            })
            .collect();

        let xorb_hash_val = xorb_hash(&chunk_info);

        let total_uncompressed_size: u64 =
            self.chunks.iter().map(|c| c.uncompressed_size as u64).sum();
        let total_compressed_size: u64 = self.chunks.iter().map(|c| c.compressed_len as u64).sum();

        // Build and append the footer
        let footer = XorbObjectInfoV1 {
            xorb_hash: xorb_hash_val,
            chunk_hashes: chunk_hashes.clone(),
            chunk_boundary_offsets: boundary_offsets,
            unpacked_chunk_offsets: unpacked_offsets,
        };
        footer.serialize(&mut buf)?;

        // Verify integrity before returning
        verify_xorb(&buf)?;

        Ok(XorbBuildResult {
            data: buf,
            xorb_hash: xorb_hash_val,
            chunk_hashes,
            total_uncompressed_size,
            total_compressed_size,
        })
    }

    /// Compute the xorb hash from accumulated chunks without finalizing.
    pub fn xorb_hash(&self) -> MerkleHash {
        let chunk_info: Vec<(MerkleHash, u64)> = self
            .chunks
            .iter()
            .map(|c| {
                let serialized_size = c.serialized_chunk.len() as u64;
                (c.chunk_hash, serialized_size)
            })
            .collect();
        xorb_hash(&chunk_info)
    }

    /// Return (hash, uncompressed_size) pairs for all accumulated chunks.
    pub fn chunk_info(&self) -> Vec<(MerkleHash, u64)> {
        self.chunks
            .iter()
            .map(|c| (c.chunk_hash, c.uncompressed_size as u64))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::compression::CompressionScheme;
    use crate::format::xorb::verify_xorb;

    #[test]
    fn test_xorb_builder_single_chunk() {
        let mut builder = XorbBuilder::new(CompressionScheme::None);
        let (hash, _compressed_len) = builder.add_chunk(b"hello world").unwrap();

        let result = builder.build().unwrap();

        // The build already calls verify_xorb internally, but verify again explicitly
        verify_xorb(&result.data).unwrap();

        assert_eq!(result.chunk_hashes.len(), 1);
        assert_eq!(result.chunk_hashes[0], hash);
        assert_eq!(result.xorb_hash, result.chunk_hashes[0]);
        assert_eq!(result.total_uncompressed_size, 11);
    }

    #[test]
    fn test_xorb_builder_multiple_chunks() {
        let mut builder = XorbBuilder::new(CompressionScheme::LZ4);

        let chunks: Vec<Vec<u8>> = vec![
            b"first chunk data".to_vec(),
            b"second chunk with different content".to_vec(),
            b"third".to_vec(),
            vec![0xABu8; 4096],
            vec![0xCDu8; 128],
            b"final chunk".to_vec(),
        ];

        let mut expected_hashes = Vec::new();
        for chunk in &chunks {
            let (h, _) = builder.add_chunk(chunk).unwrap();
            expected_hashes.push(h);
        }

        let result = builder.build().unwrap();
        verify_xorb(&result.data).unwrap();

        assert_eq!(result.chunk_hashes.len(), 6);
        assert_eq!(result.chunk_hashes, expected_hashes);
        assert_eq!(
            result.total_uncompressed_size,
            chunks.iter().map(|c| c.len() as u64).sum::<u64>()
        );
    }

    #[test]
    fn test_xorb_builder_compression_schemes() {
        let schemes = [
            CompressionScheme::None,
            CompressionScheme::LZ4,
            CompressionScheme::ByteGrouping4LZ4,
        ];

        for scheme in &schemes {
            let mut builder = XorbBuilder::new(*scheme);
            let _ = builder.add_chunk(b"test data for compression").unwrap();
            let _ = builder
                .add_chunk(b"more test data to make it interesting")
                .unwrap();

            let result = builder.build().unwrap();
            verify_xorb(&result.data).unwrap();
            assert_eq!(result.chunk_hashes.len(), 2);
        }
    }

    #[test]
    fn test_xorb_builder_hash_consistency() {
        let build_xorb = || {
            let mut builder = XorbBuilder::new(CompressionScheme::LZ4);
            let _ = builder.add_chunk(b"chunk one").unwrap();
            let _ = builder.add_chunk(b"chunk two").unwrap();
            let _ = builder.add_chunk(b"chunk three").unwrap();
            builder.build().unwrap()
        };

        let result1 = build_xorb();
        let result2 = build_xorb();

        assert_eq!(result1.xorb_hash, result2.xorb_hash);
        assert_eq!(result1.chunk_hashes, result2.chunk_hashes);
        assert_eq!(result1.data, result2.data);
    }

    #[test]
    fn test_xorb_builder_empty() {
        let builder = XorbBuilder::new(CompressionScheme::None);
        let result = builder.build().unwrap();
        verify_xorb(&result.data).unwrap();

        assert_eq!(result.chunk_hashes.len(), 0);
        assert_eq!(result.total_uncompressed_size, 0);
        assert_eq!(result.total_compressed_size, 0);
    }
}
