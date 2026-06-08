use xet_server::chunking::{Chunker, ChunkConfig};
use xet_server::hash::{compute_data_hash, file_hash, xorb_hash};
use xet_server::types::MerkleHash;

#[test]
fn test_full_upload_flow() {
    // Simulate a complete upload flow:
    // 1. Read file data
    // 2. CDC chunking
    // 3. Compute chunk hashes
    // 4. Compute xorb hash
    // 5. Compute file hash

    let file_data = b"This is a test file with some content.".repeat(1000);

    // Step 1-2: CDC chunking
    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);
    let chunks = chunker.chunk_data(&file_data);

    assert!(!chunks.is_empty(), "Should produce chunks");

    // Step 3: Compute chunk hashes
    let chunk_hashes: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|chunk| {
            let chunk_data = &file_data[chunk.offset..chunk.offset + chunk.size];
            let hash = compute_data_hash(chunk_data);
            (hash, chunk.size as u64)
        })
        .collect();

    // Step 4: Compute xorb hash
    let xorb = xorb_hash(&chunk_hashes);
    assert_ne!(xorb, MerkleHash::default());

    // Step 5: Compute file hash
    let file = file_hash(&chunk_hashes);
    assert_ne!(file, MerkleHash::default());
    assert_ne!(file, xorb); // file_hash uses HMAC salt
}

#[test]
fn test_deduplication_scenario() {
    // Two files share a common prefix (realistic dedup scenario)
    // When files have identical prefixes, CDC should find matching chunks
    // Use large enough data to trigger multiple chunks (need > 128KB max chunk size)
    let common_part = b"This is common content that is shared across multiple files for deduplication testing. ".repeat(2000);
    let suffix1 = b"Unique suffix for file 1 with extra data.";
    let suffix2 = b"Different suffix for file 2 with different data.";

    let file1 = [common_part.as_slice(), suffix1].concat();
    let file2 = [common_part.as_slice(), suffix2].concat();

    let config = ChunkConfig::default();
    let mut chunker1 = Chunker::new(config.clone());
    let mut chunker2 = Chunker::new(config);

    let chunks1 = chunker1.chunk_data(&file1);
    let chunks2 = chunker2.chunk_data(&file2);

    // Files should produce multiple chunks (file is ~180KB, max chunk is 128KB)
    assert!(chunks1.len() > 1, "Should produce multiple chunks, got {} chunks for {} bytes", chunks1.len(), file1.len());

    // Compute chunk hashes
    let hashes1: Vec<MerkleHash> = chunks1
        .iter()
        .map(|c| compute_data_hash(&file1[c.offset..c.offset + c.size]))
        .collect();

    let hashes2: Vec<MerkleHash> = chunks2
        .iter()
        .map(|c| compute_data_hash(&file2[c.offset..c.offset + c.size]))
        .collect();

    // Files should share some chunk hashes (dedup opportunity)
    // Since they share a common prefix, all chunks before the last one should match
    let common_count: usize = hashes1
        .iter()
        .filter(|h| hashes2.contains(h))
        .count();

    assert!(common_count > 0, "Files should share some chunks for dedup");
}

#[test]
fn test_large_file() {
    // Simulate a large file (70MB) that needs multiple xorbs conceptually
    let file_data = vec![0xABu8; 70 * 1024 * 1024]; // 70MB

    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);
    let chunks = chunker.chunk_data(&file_data);

    // Compute all chunk hashes
    let chunk_hashes: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|c| {
            let data = &file_data[c.offset..c.offset + c.size];
            (compute_data_hash(data), c.size as u64)
        })
        .collect();

    // Compute file hash
    let file = file_hash(&chunk_hashes);
    assert_ne!(file, MerkleHash::default());

    // Verify total size
    let total: usize = chunks.iter().map(|c| c.size).sum();
    assert_eq!(total, file_data.len());
}

#[test]
fn test_hash_consistency() {
    // Same data always produces same hash
    let data = b"Consistent data for testing".repeat(100);

    let hash1 = compute_data_hash(&data);
    let hash2 = compute_data_hash(&data);
    assert_eq!(hash1, hash2);

    let config = ChunkConfig::default();
    let mut chunker1 = Chunker::new(config.clone());
    let mut chunker2 = Chunker::new(config);

    let chunks1 = chunker1.chunk_data(&data);
    let chunks2 = chunker2.chunk_data(&data);

    assert_eq!(chunks1.len(), chunks2.len());
    for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
        assert_eq!(c1.size, c2.size);
        assert_eq!(c1.offset, c2.offset);
    }
}