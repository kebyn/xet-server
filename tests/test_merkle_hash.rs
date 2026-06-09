use xet_server::types::MerkleHash;

#[test]
fn test_default_hash() {
    let hash = MerkleHash::default();
    assert_eq!(hash.as_bytes(), [0u8; 32]);
}

#[test]
fn test_from_bytes() {
    let bytes = [1u8; 32];
    let hash = MerkleHash::from(bytes);
    assert_eq!(hash.as_bytes(), bytes);
}

#[test]
fn test_from_hex() {
    let hex_str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let hash = MerkleHash::from_hex(hex_str).unwrap();
    let expected_bytes: Vec<u8> = (0..32).map(|i| i as u8).collect();
    assert_eq!(hash.as_bytes(), expected_bytes.as_slice());
}

#[test]
fn test_to_hex() {
    let bytes = [0xabu8; 32];
    let hash = MerkleHash::from(bytes);
    let hex_str = hash.to_hex();
    assert_eq!(hex_str.len(), 64);
    assert!(hex_str.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_modulo() {
    let mut bytes = [0u8; 32];
    bytes[0] = 100; // first byte = 100
    let hash = MerkleHash::from(bytes);
    assert_eq!(hash % 10, 0);
    assert_eq!(hash % 7, 100 % 7);
}

#[test]
fn test_equality() {
    let hash1 = MerkleHash::from([1u8; 32]);
    let hash2 = MerkleHash::from([1u8; 32]);
    let hash3 = MerkleHash::from([2u8; 32]);
    assert_eq!(hash1, hash2);
    assert_ne!(hash1, hash3);
}