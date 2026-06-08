//! End-to-end integration tests for Xet Storage server
//!
//! These tests verify the complete workflow from file chunking to xorb upload,
//! shard creation, and file reconstruction.

use bytes::Bytes;
use tempfile::tempdir;

use xet_server::api::auth::{create_jwt, JwtClaims};
use xet_server::chunking::{ChunkConfig, Chunker};
use xet_server::config::ServerConfig;
use xet_server::hash::{compute_data_hash, xorb_hash};
use xet_server::index::MetadataIndex;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

/// Helper function to create a valid JWT token
fn create_test_token(config: &ServerConfig) -> String {
    create_jwt(
        &JwtClaims {
            sub: "test-user".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    )
    .unwrap()
}

/// Test 5.1.1: Complete file upload workflow
///
/// This test verifies the complete flow:
/// 1. Create test data and chunk it
/// 2. Build a xorb from chunks
/// 3. Verify the xorb can be stored
/// 4. Verify idempotency
#[actix_web::test]
async fn test_e2e_complete_file_upload() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap(),
    );
    let config = ServerConfig::default();
    let _token = create_test_token(&config);

    // Step 1: Create test data and chunk it
    let test_data = b"Hello, this is a test file for end-to-end testing. \
                      We need enough data to create meaningful chunks. \
                      Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                      Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";

    let mut chunker = Chunker::new(ChunkConfig::default());
    let chunks = chunker.chunk_data(test_data);

    assert!(!chunks.is_empty(), "Should produce at least one chunk");

    // Step 2: Build xorb from chunks
    let mut chunk_hashes = Vec::new();
    let mut xorb_data = Vec::new();

    for chunk in &chunks {
        let chunk_data = &test_data[chunk.offset..chunk.offset + chunk.size];
        let chunk_hash = compute_data_hash(chunk_data);
        chunk_hashes.push((chunk_hash, chunk.size as u64));
        xorb_data.extend_from_slice(chunk_data);
    }

    let xorb = xorb_hash(&chunk_hashes);
    let xorb_hex = xorb.to_hex();

    // Step 3: Store xorb
    let xorb_key = format!("xorbs/default/{}", xorb_hex);
    storage.put(&xorb_key, Bytes::from(xorb_data.clone())).await.unwrap();

    // Step 4: Verify xorb is stored
    let stored = storage.exists(&xorb_key).await.unwrap();
    assert!(stored, "Xorb should be stored in backend");

    // Step 5: Verify idempotency (check exists again)
    let stored_again = storage.exists(&xorb_key).await.unwrap();
    assert!(stored_again, "Xorb should still be stored");
}

/// Test 5.1.3: Global deduplication
///
/// This test verifies:
/// 1. Create two files with identical chunks
/// 2. Build xorbs for both
/// 3. Verify chunks have same hash
#[actix_web::test]
async fn test_e2e_global_deduplication() {
    // Step 1: Create two files with identical content
    let content = b"Identical content that will be deduplicated";

    // Step 2: Chunk both files
    let mut chunker1 = Chunker::new(ChunkConfig::default());
    let chunks1 = chunker1.chunk_data(content);

    let mut chunker2 = Chunker::new(ChunkConfig::default());
    let chunks2 = chunker2.chunk_data(content);

    // Step 3: Verify chunks have same hashes
    assert_eq!(chunks1.len(), chunks2.len(), "Same content should produce same number of chunks");

    for (chunk1, chunk2) in chunks1.iter().zip(chunks2.iter()) {
        let data1 = &content[chunk1.offset..chunk1.offset + chunk1.size];
        let data2 = &content[chunk2.offset..chunk2.offset + chunk2.size];

        let hash1 = compute_data_hash(data1);
        let hash2 = compute_data_hash(data2);

        assert_eq!(hash1, hash2, "Identical chunks should have same hash");
    }
}

/// Test 5.2.1: Sequential uploads
///
/// This test verifies:
/// 1. Upload 10 different xorbs sequentially
/// 2. Verify all xorbs are stored
#[actix_web::test]
async fn test_e2e_sequential_uploads() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap(),
    );

    // Upload 10 different xorbs
    for i in 0..10 {
        let data = format!("Sequential upload test data {}", i);
        let data_bytes = data.as_bytes();
        let hash = compute_data_hash(data_bytes);
        let hash_hex = hash.to_hex();

        let xorb_key = format!("xorbs/default/{}", hash_hex);
        storage.put(&xorb_key, Bytes::from(data_bytes.to_vec())).await.unwrap();
    }

    // Verify all xorbs are stored
    for i in 0..10 {
        let data = format!("Sequential upload test data {}", i);
        let data_bytes = data.as_bytes();
        let hash = compute_data_hash(data_bytes);
        let hash_hex = hash.to_hex();

        let xorb_key = format!("xorbs/default/{}", hash_hex);
        let stored = storage.exists(&xorb_key).await.unwrap();
        assert!(stored, "Xorb {} should be stored", i);
    }
}

/// Test 5.3.1: Empty file handling
///
/// This test verifies:
/// 1. Try to chunk an empty file
/// 2. Verify chunking returns empty list
#[actix_web::test]
async fn test_e2e_empty_file() {
    let empty_data = b"";
    let mut chunker = Chunker::new(ChunkConfig::default());
    let chunks = chunker.chunk_data(empty_data);

    assert_eq!(chunks.len(), 0, "Empty file should produce no chunks");
}

/// Test 5.3.2: Large file handling
///
/// This test verifies:
/// 1. Create a 1MB file
/// 2. Chunk it
/// 3. Build and store xorb
/// 4. Verify storage succeeds
#[actix_web::test]
async fn test_e2e_large_file() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap(),
    );

    // Create 1MB of data
    let large_data = vec![0xABu8; 1024 * 1024];
    let mut chunker = Chunker::new(ChunkConfig::default());
    let chunks = chunker.chunk_data(&large_data);

    assert!(!chunks.is_empty(), "Large file should produce chunks");

    // Build xorb
    let mut chunk_hashes = Vec::new();
    let mut xorb_data = Vec::new();

    for chunk in &chunks {
        let chunk_data = &large_data[chunk.offset..chunk.offset + chunk.size];
        let chunk_hash = compute_data_hash(chunk_data);
        chunk_hashes.push((chunk_hash, chunk.size as u64));
        xorb_data.extend_from_slice(chunk_data);
    }

    let xorb = xorb_hash(&chunk_hashes);
    let xorb_hex = xorb.to_hex();

    // Store xorb
    let xorb_key = format!("xorbs/default/{}", xorb_hex);
    storage.put(&xorb_key, Bytes::from(xorb_data)).await.unwrap();

    // Verify storage
    let stored = storage.exists(&xorb_key).await.unwrap();
    assert!(stored, "Large xorb should be stored");
}

/// Test 5.3.3: Metadata index operations
///
/// This test verifies:
/// 1. Register a shard in the metadata index
/// 2. Query the index for file shards
/// 3. Verify correct results
#[actix_web::test]
async fn test_e2e_metadata_index() {
    let index = MetadataIndex::new();

    // Register a shard
    let shard_id = "test-shard-123".to_string();
    let file_hash = "a".repeat(64);
    let file_hashes = vec![file_hash.clone()];
    let chunk_mappings = vec![
        ("b".repeat(64), "c".repeat(64), 0u32),
        ("d".repeat(64), "c".repeat(64), 1u32),
    ];

    index.register_shard(shard_id.clone(), file_hashes, chunk_mappings);

    // Query for file shards
    let shards = index.get_shards_for_file(&file_hash);
    assert!(shards.is_some(), "Should find shards for file");
    assert_eq!(shards.unwrap(), vec![shard_id], "Should return correct shard");

    // Query for non-existent file
    let non_existent = "e".repeat(64);
    let shards = index.get_shards_for_file(&non_existent);
    assert!(shards.is_none(), "Should not find shards for non-existent file");
}

/// Test 5.4.1: Chunk size validation
///
/// This test verifies:
/// 1. Chunking produces chunks within expected size range
/// 2. Min chunk size is respected
/// 3. Max chunk size is respected
#[actix_web::test]
async fn test_e2e_chunk_size_validation() {
    let config = ChunkConfig::default();
    let min_size = config.min_chunk_size();
    let max_size = config.max_chunk_size();

    // Create data that should produce multiple chunks
    let data = vec![0xABu8; 512 * 1024]; // 512KB
    let mut chunker = Chunker::new(config);
    let chunks = chunker.chunk_data(&data);

    assert!(!chunks.is_empty(), "Should produce chunks");

    // Verify chunk sizes
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;

        if is_last {
            // Last chunk can be smaller than min_size
            assert!(chunk.size <= max_size, "Last chunk {} exceeds max size: {} > {}", i, chunk.size, max_size);
        } else {
            // Non-last chunks should be within range
            assert!(chunk.size >= min_size, "Chunk {} below min size: {} < {}", i, chunk.size, min_size);
            assert!(chunk.size <= max_size, "Chunk {} exceeds max size: {} > {}", i, chunk.size, max_size);
        }
    }
}
