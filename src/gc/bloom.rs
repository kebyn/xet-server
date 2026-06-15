//! Bloom Filter Protected Set for incremental GC.
//!
//! Provides O(1) probabilistic membership tests for chunk/xorb hashes.
//! A false positive rate of 0.001 with 10M expected items uses ~17MB.
//!
//! # Persistence Format
//!
//! ```text
//! [CRC32: 4 bytes LE]
//! [num_bits: 8 bytes LE]
//! [num_hashes: 4 bytes LE]
//! [sip_key_0_0: 8 bytes LE][sip_key_0_1: 8 bytes LE]
//! [sip_key_1_0: 8 bytes LE][sip_key_1_1: 8 bytes LE]
//! [bitmap_len: 8 bytes LE]
//! [bitmap: bitmap_len bytes]
//! ```

use bloomfilter::Bloom;
use crate::config::BloomConfig;
use crate::gc::errors::{GcError, GcResult};
use std::io::{Read, Write};

/// Statistics about the bloom filter's state.
#[derive(Debug, Clone, Default)]
pub struct BloomStats {
    pub items_inserted: u64,
    pub rebuild_count: u32,
}

/// Bloom filter protected set with double-buffered rebuild support.
///
/// When the filter's occupancy reaches `rebuild_threshold`, a background
/// rebuild can be triggered. During rebuild, the active filter continues
/// serving reads while a new filter is being populated.
pub struct BloomFilterProtectedSet {
    /// Current active bloom filter (serves reads and writes).
    active: Bloom<[u8]>,
    /// Filter being rebuilt (None if no rebuild in progress).
    refreshing: Option<Bloom<[u8]>>,
    /// Configuration.
    config: BloomConfig,
    /// Statistics.
    stats: BloomStats,
}

impl BloomFilterProtectedSet {
    /// Create a new empty bloom filter protected set.
    pub fn new(config: BloomConfig) -> Self {
        let active = Bloom::new_for_fp_rate(
            config.expected_items as usize,
            config.false_positive_rate,
        );

        Self {
            active,
            refreshing: None,
            config,
            stats: BloomStats::default(),
        }
    }

    /// Insert a hash (as bytes) into the bloom filter.
    ///
    /// If a rebuild is in progress, the item is also inserted into the
    /// refreshing filter to ensure no entries are lost during the swap.
    pub fn insert(&mut self, hash: &[u8]) {
        self.active.set(hash);
        self.stats.items_inserted += 1;

        // Also insert into refreshing filter if rebuild is in progress
        if let Some(ref mut refreshing) = self.refreshing {
            refreshing.set(hash);
        }
    }

    /// Insert multiple hashes.
    pub fn insert_all(&mut self, hashes: &[String]) {
        for hash in hashes {
            self.insert(hash.as_bytes());
        }
    }

    /// Check if a hash (as bytes) is probably in the set.
    ///
    /// Returns `true` if the hash is probably in the set (may be a false positive).
    /// Returns `false` if the hash is definitely NOT in the set.
    pub fn contains(&self, hash: &[u8]) -> bool {
        self.active.check(hash)
    }

    /// Check if the active filter should be rebuilt based on occupancy.
    ///
    /// Returns true when the estimated number of inserted items exceeds
    /// `rebuild_threshold * capacity`.
    pub fn should_rebuild(&self) -> bool {
        // Estimate capacity from number of bits and hash functions
        // Optimal capacity ≈ num_bits * ln(2) / num_hashes
        let num_bits = self.active.number_of_bits() as f64;
        let num_hashes = self.active.number_of_hash_functions() as f64;
        let capacity = num_bits * 0.693 / num_hashes; // ln(2) ≈ 0.693

        let usage_ratio = self.stats.items_inserted as f64 / capacity;
        usage_ratio >= self.config.rebuild_threshold
    }

    /// Start a background rebuild of the bloom filter.
    ///
    /// The new filter uses the configured `expected_items` and `false_positive_rate`.
    /// Existing items in the active filter are NOT copied — the caller must
    /// re-insert all known references during the next scan cycle.
    pub fn start_rebuild(&mut self) {
        if self.refreshing.is_some() {
            return; // Already rebuilding
        }

        let new_bloom = Bloom::new_for_fp_rate(
            self.config.expected_items as usize,
            self.config.false_positive_rate,
        );
        self.refreshing = Some(new_bloom);
        self.stats.rebuild_count += 1;
    }

    /// Complete the rebuild by swapping the refreshing filter into active.
    ///
    /// After the swap, the stats counter is reset.
    pub fn complete_rebuild(&mut self) {
        if let Some(new_bloom) = self.refreshing.take() {
            self.active = new_bloom;
            self.stats.items_inserted = 0;
        }
    }

    /// Cancel an in-progress rebuild, keeping the active filter unchanged.
    pub fn cancel_rebuild(&mut self) {
        self.refreshing = None;
    }

    /// Save the bloom filter to a writer with CRC32 integrity check.
    ///
    /// Format: [CRC32:4][num_bits:8][num_hashes:4][sip_keys:32][bitmap_len:8][bitmap:N]
    pub fn save<W: Write>(&self, writer: &mut W) -> GcResult<()> {
        let bitmap = self.active.bitmap();
        let num_bits = self.active.number_of_bits();
        let num_hashes = self.active.number_of_hash_functions();
        let sip_keys = self.active.sip_keys();

        // Build the data payload (everything after CRC32)
        let mut payload = Vec::with_capacity(8 + 4 + 32 + 8 + bitmap.len());
        payload.extend_from_slice(&num_bits.to_le_bytes());
        payload.extend_from_slice(&num_hashes.to_le_bytes());
        payload.extend_from_slice(&sip_keys[0].0.to_le_bytes());
        payload.extend_from_slice(&sip_keys[0].1.to_le_bytes());
        payload.extend_from_slice(&sip_keys[1].0.to_le_bytes());
        payload.extend_from_slice(&sip_keys[1].1.to_le_bytes());
        payload.extend_from_slice(&(bitmap.len() as u64).to_le_bytes());
        payload.extend_from_slice(&bitmap);

        // Compute CRC32
        let crc = crc32fast::hash(&payload);
        writer.write_all(&crc.to_le_bytes())
            .map_err(|e| GcError::Io(e))?;
        writer.write_all(&payload)
            .map_err(|e| GcError::Io(e))?;

        Ok(())
    }

    /// Load a bloom filter from a reader with CRC32 integrity verification.
    ///
    /// Returns `GcError::BloomFilterCorrupted` if the CRC32 doesn't match.
    pub fn load<R: Read>(reader: &mut R, config: BloomConfig) -> GcResult<Self> {
        // Read CRC32
        let mut crc_bytes = [0u8; 4];
        reader.read_exact(&mut crc_bytes)
            .map_err(|e| GcError::Io(e))?;
        let expected_crc = u32::from_le_bytes(crc_bytes);

        // Read the rest of the data
        let mut payload = Vec::new();
        reader.read_to_end(&mut payload)
            .map_err(|e| GcError::Io(e))?;

        // Verify CRC32
        let actual_crc = crc32fast::hash(&payload);
        if expected_crc != actual_crc {
            return Err(GcError::BloomFilterCorrupted {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        // Parse payload
        if payload.len() < 8 + 4 + 32 + 8 {
            return Err(GcError::BloomFilterCorrupted {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        let mut offset = 0;

        let num_bits = u64::from_le_bytes(payload[offset..offset+8].try_into().unwrap());
        offset += 8;

        let num_hashes = u32::from_le_bytes(payload[offset..offset+4].try_into().unwrap());
        offset += 4;

        let sip_k0_0 = u64::from_le_bytes(payload[offset..offset+8].try_into().unwrap());
        offset += 8;
        let sip_k0_1 = u64::from_le_bytes(payload[offset..offset+8].try_into().unwrap());
        offset += 8;
        let sip_k1_0 = u64::from_le_bytes(payload[offset..offset+8].try_into().unwrap());
        offset += 8;
        let sip_k1_1 = u64::from_le_bytes(payload[offset..offset+8].try_into().unwrap());
        offset += 8;

        let bitmap_len = u64::from_le_bytes(payload[offset..offset+8].try_into().unwrap()) as usize;
        offset += 8;

        if payload.len() < offset + bitmap_len {
            return Err(GcError::BloomFilterCorrupted {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        let bitmap = payload[offset..offset+bitmap_len].to_vec();
        let sip_keys = [(sip_k0_0, sip_k0_1), (sip_k1_0, sip_k1_1)];

        // Reconstruct the bloom filter from serialized data.
        // from_existing expects: (bytes, bitmap_bits, num_hashes, sip_keys)
        let active = Bloom::from_existing(
            &bitmap,
            num_bits,       // bitmap_bits: the actual number of bits in the filter
            num_hashes,     // number of hash functions
            sip_keys,       // SipHash keys for deterministic hashing
        );

        Ok(Self {
            active,
            refreshing: None,
            config,
            stats: BloomStats::default(),
        })
    }

    /// Get a reference to the current stats.
    pub fn stats(&self) -> &BloomStats {
        &self.stats
    }

    /// Get a reference to the config.
    pub fn config(&self) -> &BloomConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> BloomConfig {
        BloomConfig {
            expected_items: 1000,
            false_positive_rate: 0.01,
            rebuild_threshold: 0.8,
        }
    }

    #[test]
    fn test_insert_and_contains() {
        let mut bloom = BloomFilterProtectedSet::new(test_config());

        assert!(!bloom.contains(b"hash1"));
        assert!(!bloom.contains(b"hash2"));

        bloom.insert(b"hash1");
        assert!(bloom.contains(b"hash1"));
        // hash2 may or may not be found (false positive), but hash1 must be found

        bloom.insert(b"hash2");
        assert!(bloom.contains(b"hash2"));
    }

    #[test]
    fn test_insert_all() {
        let mut bloom = BloomFilterProtectedSet::new(test_config());
        let hashes = vec!["abc123".to_string(), "def456".to_string(), "789ghi".to_string()];

        bloom.insert_all(&hashes);

        for hash in &hashes {
            assert!(bloom.contains(hash.as_bytes()));
        }
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let config = test_config();
        let mut bloom = BloomFilterProtectedSet::new(config.clone());

        bloom.insert(b"hash_a");
        bloom.insert(b"hash_b");
        bloom.insert(b"hash_c");

        // Save
        let mut buf = Vec::new();
        bloom.save(&mut buf).expect("save failed");

        // Load
        let mut cursor = std::io::Cursor::new(buf);
        let loaded = BloomFilterProtectedSet::load(&mut cursor, config)
            .expect("load failed");

        // Verify all items are still present
        assert!(loaded.contains(b"hash_a"));
        assert!(loaded.contains(b"hash_b"));
        assert!(loaded.contains(b"hash_c"));
    }

    #[test]
    fn test_corrupted_crc_detected() {
        let config = test_config();
        let mut bloom = BloomFilterProtectedSet::new(config.clone());
        bloom.insert(b"test");

        let mut buf = Vec::new();
        bloom.save(&mut buf).expect("save failed");

        // Corrupt the CRC32 (first 4 bytes)
        buf[0] ^= 0xFF;

        let mut cursor = std::io::Cursor::new(buf);
        let result = BloomFilterProtectedSet::load(&mut cursor, config);

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            GcError::BloomFilterCorrupted { .. } => {},
            other => panic!("Expected BloomFilterCorrupted, got: {:?}", other),
        }
    }

    #[test]
    fn test_should_rebuild() {
        let config = BloomConfig {
            expected_items: 100,
            false_positive_rate: 0.01,
            rebuild_threshold: 0.5,
        };
        let mut bloom = BloomFilterProtectedSet::new(config);

        assert!(!bloom.should_rebuild());

        // Insert enough items to exceed threshold
        for i in 0..60 {
            bloom.insert(format!("item_{}", i).as_bytes());
        }

        assert!(bloom.should_rebuild());
    }

    #[test]
    fn test_rebuild_flow() {
        let config = test_config();
        let mut bloom = BloomFilterProtectedSet::new(config);

        bloom.insert(b"before_rebuild");
        bloom.start_rebuild();
        assert!(bloom.refreshing.is_some());

        // Items inserted during rebuild go to both filters
        bloom.insert(b"during_rebuild");
        assert!(bloom.contains(b"before_rebuild"));
        assert!(bloom.contains(b"during_rebuild"));

        bloom.complete_rebuild();
        assert!(bloom.refreshing.is_none());

        // After rebuild, only items inserted during/after rebuild are in the new filter
        assert!(bloom.contains(b"during_rebuild"));
        // before_rebuild is NOT in the new filter (it was not re-inserted)
    }
}
