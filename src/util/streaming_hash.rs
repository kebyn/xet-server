//! Incremental BLAKE3 hasher for streaming uploads.
//!
//! Uses the same DATA_KEY as `compute_data_hash` so that incremental
//! hashing produces identical results to one-shot hashing.

use crate::hash::DATA_KEY;
use crate::types::MerkleHash;

/// Incremental BLAKE3 hasher for streaming data.
///
/// Produces the same hash as `compute_data_hash` when given the same total input.
/// Data can be fed in arbitrary chunk sizes via `update()`.
pub struct StreamingHasher {
    hasher: blake3::Hasher,
    bytes_processed: u64,
}

impl StreamingHasher {
    /// Create a new incremental hasher using the standard DATA_KEY.
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new_keyed(&DATA_KEY),
            bytes_processed: 0,
        }
    }

    /// Feed data into the hasher.
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
        self.bytes_processed += data.len() as u64;
    }

    /// Finalize and return the hash. Consumes the hasher.
    pub fn finalize(self) -> MerkleHash {
        MerkleHash::from(*self.hasher.finalize().as_bytes())
    }

    /// Number of bytes processed so far.
    pub fn bytes_processed(&self) -> u64 {
        self.bytes_processed
    }
}

impl Default for StreamingHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::compute_data_hash;

    #[test]
    fn test_streaming_matches_oneshot_empty() {
        let hasher = StreamingHasher::new();
        let hash = hasher.finalize();
        assert_eq!(hash, compute_data_hash(&[]));
    }

    #[test]
    fn test_streaming_matches_oneshot_small() {
        let data = b"hello world";
        let mut hasher = StreamingHasher::new();
        hasher.update(data);
        let hash = hasher.finalize();
        assert_eq!(hash, compute_data_hash(data));
    }

    #[test]
    fn test_streaming_matches_oneshot_byte_by_byte() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let mut hasher = StreamingHasher::new();
        for byte in data.iter() {
            hasher.update(&[*byte]);
        }
        let hash = hasher.finalize();
        assert_eq!(hash, compute_data_hash(data));
    }

    #[test]
    fn test_streaming_matches_oneshot_varied_chunks() {
        let data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let mut hasher = StreamingHasher::new();
        // Feed in varied chunk sizes: 1, 3, 7, 13, 1, 3, 7, 13, ...
        let chunk_sizes = [1, 3, 7, 13];
        let mut offset = 0;
        let mut i = 0;
        while offset < data.len() {
            let size = chunk_sizes[i % chunk_sizes.len()];
            let end = std::cmp::min(offset + size, data.len());
            hasher.update(&data[offset..end]);
            offset = end;
            i += 1;
        }
        let hash = hasher.finalize();
        assert_eq!(hash, compute_data_hash(&data));
    }

    #[test]
    fn test_bytes_processed() {
        let mut hasher = StreamingHasher::new();
        assert_eq!(hasher.bytes_processed(), 0);
        hasher.update(b"hello");
        assert_eq!(hasher.bytes_processed(), 5);
        hasher.update(b" world");
        assert_eq!(hasher.bytes_processed(), 11);
        hasher.finalize();
    }
}
