use xet_server::chunking::{Chunker, ChunkConfig};

#[test]
fn test_chunker_basic() {
    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);

    // 200KB of data (should produce at least 2 chunks)
    let data = vec![0xABu8; 200 * 1024];
    let chunks = chunker.chunk_data(&data);

    assert!(chunks.len() >= 2, "Should produce at least 2 chunks");

    // Verify each chunk size is within range
    let config = chunker.config();
    for chunk in &chunks {
        assert!(chunk.size >= config.min_chunk_size());
        assert!(chunk.size <= config.max_chunk_size());
    }
}

#[test]
fn test_chunker_deterministic() {
    let config = ChunkConfig::default();
    let data = b"test data for chunking".repeat(10000);

    let mut chunker1 = Chunker::new(config.clone());
    let chunks1 = chunker1.chunk_data(&data);

    let mut chunker2 = Chunker::new(config);
    let chunks2 = chunker2.chunk_data(&data);

    assert_eq!(chunks1.len(), chunks2.len());
    for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
        assert_eq!(c1.size, c2.size);
        assert_eq!(c1.offset, c2.offset);
    }
}

#[test]
fn test_chunker_content_defined() {
    let config = ChunkConfig::default();

    // Two files with identical content should produce identical chunks
    let data1 = b"common content".repeat(5000);
    let data2 = b"common content".repeat(5000);

    let mut chunker1 = Chunker::new(config.clone());
    let chunks1 = chunker1.chunk_data(&data1);

    let mut chunker2 = Chunker::new(config);
    let chunks2 = chunker2.chunk_data(&data2);

    assert_eq!(chunks1.len(), chunks2.len());
}

#[test]
fn test_chunker_small_data() {
    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);

    // Data smaller than min chunk should produce one chunk
    let small_data = vec![0u8; 1024]; // 1KB
    let chunks = chunker.chunk_data(&small_data);

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].size, 1024);
}

#[test]
fn test_chunker_empty_data() {
    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);

    let chunks = chunker.chunk_data(b"");
    assert_eq!(chunks.len(), 0);
}

#[test]
fn test_chunker_covers_all_data() {
    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);

    let data = vec![0xCDu8; 500 * 1024]; // 500KB
    let chunks = chunker.chunk_data(&data);

    // Verify chunks cover the entire input
    let mut offset = 0;
    for chunk in &chunks {
        assert_eq!(chunk.offset, offset);
        offset += chunk.size;
    }
    assert_eq!(offset, data.len());
}

#[test]
fn test_chunker_large_data() {
    let config = ChunkConfig::default();
    let mut chunker = Chunker::new(config);

    // 2MB of data
    let data = vec![0xEFu8; 2 * 1024 * 1024];
    let chunks = chunker.chunk_data(&data);

    assert!(chunks.len() >= 16, "2MB should produce many chunks");

    // Verify total size
    let total: usize = chunks.iter().map(|c| c.size).sum();
    assert_eq!(total, data.len());
}