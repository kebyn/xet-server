use xet_server::hash::{compute_data_hash, file_hash, xorb_hash};
use xet_server::types::MerkleHash;

#[test]
fn test_xorb_hash_empty() {
    let chunks: Vec<(MerkleHash, u64)> = vec![];
    let hash = xorb_hash(&chunks);
    assert_eq!(hash, MerkleHash::default());
}

#[test]
fn test_xorb_hash_single_chunk() {
    let chunk_data = b"test chunk data";
    let chunk_hash = compute_data_hash(chunk_data);
    let chunk_size = chunk_data.len() as u64;

    let chunks = vec![(chunk_hash, chunk_size)];
    let hash = xorb_hash(&chunks);

    // Single chunk's xorb hash equals the chunk hash itself
    assert_eq!(hash, chunk_hash);
}

#[test]
fn test_xorb_hash_multiple_chunks() {
    let chunks: Vec<(MerkleHash, u64)> = (0..5)
        .map(|i| {
            let data = format!("chunk {}", i);
            let hash = compute_data_hash(data.as_bytes());
            (hash, data.len() as u64)
        })
        .collect();

    let hash = xorb_hash(&chunks);
    assert_ne!(hash, MerkleHash::default());

    // Verify repeatability
    let hash2 = xorb_hash(&chunks);
    assert_eq!(hash, hash2);
}

#[test]
fn test_xorb_hash_order_matters() {
    let chunk1 = (compute_data_hash(b"chunk1"), 6u64);
    let chunk2 = (compute_data_hash(b"chunk2"), 6u64);

    let hash1 = xorb_hash(&[chunk1, chunk2]);
    let hash2 = xorb_hash(&[chunk2, chunk1]);

    // Different order should produce different hash
    assert_ne!(hash1, hash2);
}

#[test]
fn test_file_hash_empty() {
    let chunks: Vec<(MerkleHash, u64)> = vec![];
    let hash = file_hash(&chunks);
    assert_eq!(hash, MerkleHash::default());
}

#[test]
fn test_file_hash_with_salt() {
    let chunks: Vec<(MerkleHash, u64)> = vec![(compute_data_hash(b"chunk1"), 6u64)];

    let hash1 = file_hash(&chunks); // default salt = [0; 32]
    let hash2 = xorb_hash(&chunks); // xorb hash has no salt

    // file_hash applies HMAC with salt, so they should differ
    assert_ne!(hash1, hash2);
}

#[test]
fn test_merkle_tree_consistency() {
    let chunks: Vec<(MerkleHash, u64)> = (0..10)
        .map(|i| {
            let data = format!("test chunk {}", i);
            (compute_data_hash(data.as_bytes()), data.len() as u64)
        })
        .collect();

    let hash1 = xorb_hash(&chunks);
    let hash2 = xorb_hash(&chunks);
    let hash3 = xorb_hash(&chunks);

    assert_eq!(hash1, hash2);
    assert_eq!(hash2, hash3);
}
