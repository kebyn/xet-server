use crate::types::MerkleHash;

// Fixed keys from xet-core (data_hash.rs lines 288-297)
const DATA_KEY: [u8; 32] = [
    102, 151, 245, 119, 91, 149, 80, 222, 49, 53, 203, 172, 165, 151, 24, 28,
    157, 228, 33, 16, 155, 235, 43, 88, 180, 208, 176, 75, 147, 173, 242, 41,
];

const INTERNAL_NODE_KEY: [u8; 32] = [
    1, 126, 197, 199, 165, 71, 41, 150, 253, 148, 102, 102, 180, 138, 2, 230,
    93, 221, 83, 111, 55, 199, 109, 210, 248, 99, 82, 230, 74, 83, 113, 63,
];

/// Compute leaf node hash (chunk data)
/// Uses BLAKE3 keyed hash with DATA_KEY
pub fn compute_data_hash(data: &[u8]) -> MerkleHash {
    let digest = blake3::keyed_hash(&DATA_KEY, data);
    MerkleHash::from(*digest.as_bytes())
}

/// Compute internal node hash (Merkle tree node)
/// Uses BLAKE3 keyed hash with INTERNAL_NODE_KEY
/// Input format typically: concatenation of "{child_hash_hex} : {size}\n"
pub fn compute_internal_node_hash(data: &[u8]) -> MerkleHash {
    let digest = blake3::keyed_hash(&INTERNAL_NODE_KEY, data);
    MerkleHash::from(*digest.as_bytes())
}

/// HMAC operation: use MerkleHash as key to HMAC another MerkleHash
/// Used for file hash salt processing
pub fn hmac_hash(key: &MerkleHash, message: &MerkleHash) -> MerkleHash {
    let key_bytes: [u8; 32] = (*key).into();
    let msg_bytes: [u8; 32] = (*message).into();

    let digest = blake3::keyed_hash(&key_bytes, &msg_bytes);
    MerkleHash::from(*digest.as_bytes())
}