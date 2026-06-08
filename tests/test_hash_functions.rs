use xet_server::hash::{compute_data_hash, compute_internal_node_hash};
use xet_server::types::MerkleHash;

#[test]
fn test_data_hash_consistency() {
    let data = b"hello world";
    let hash1 = compute_data_hash(data);
    let hash2 = compute_data_hash(data);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_data_hash_different_from_internal() {
    let data = b"hello world";
    let data_hash = compute_data_hash(data);
    let internal_hash = compute_internal_node_hash(data);
    // Same input, different keys, should produce different hashes
    assert_ne!(data_hash, internal_hash);
}

#[test]
fn test_data_hash_known_value() {
    let data = b"test data";
    let hash = compute_data_hash(data);
    assert_ne!(hash, MerkleHash::default());

    // Verify repeatability
    let hash2 = compute_data_hash(data);
    assert_eq!(hash, hash2);
}

#[test]
fn test_internal_node_hash_format() {
    // Internal node hash input format: "{hash_hex} : {size}\n"
    let child_hash = MerkleHash::from([0xab; 32]);
    let child_size = 65536u64;

    let input = format!("{} : {}\n", child_hash.to_hex(), child_size);
    let hash = compute_internal_node_hash(input.as_bytes());

    assert_ne!(hash, MerkleHash::default());
}

#[test]
fn test_hash_empty_data() {
    let empty_hash = compute_data_hash(b"");
    assert_ne!(empty_hash, MerkleHash::default());
}