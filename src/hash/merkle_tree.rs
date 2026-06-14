use crate::hash::{compute_internal_node_hash, hmac_hash};
use crate::types::MerkleHash;

const MEAN_BRANCHING_FACTOR: u64 = 4;
const MIN_GROUP_SIZE: usize = 2;
const MAX_GROUP_SIZE: usize = 2 * MEAN_BRANCHING_FACTOR as usize + 1; // 9

/// Check if hash value satisfies natural cut condition
#[inline]
fn is_natural_cut(hash: &MerkleHash) -> bool {
    (*hash % MEAN_BRANCHING_FACTOR) == 0
}

/// Find next cut point in sequence of hashes
///
/// Starting from index 2, scan for first position where hash % 4 == 0.
/// Cut after that position (i+1). If no natural cut found, return min(MAX_GROUP_SIZE, remaining).
fn next_merge_cut(hashes: &[(MerkleHash, u64)]) -> usize {
    if hashes.len() <= MIN_GROUP_SIZE {
        return hashes.len();
    }

    let end = MAX_GROUP_SIZE.min(hashes.len());

    // Start from index 2 (minimum 2 children required)
    for (i, (hash, _)) in hashes.iter().enumerate().take(end).skip(MIN_GROUP_SIZE) {
        if is_natural_cut(hash) {
            return i + 1; // Cut after position i
        }
    }

    end
}

/// Merge a group of nodes' hashes
///
/// Format: "{hash_hex} : {size_decimal}\n" for each node, concatenated
fn merged_hash_of_sequence(hashes: &[(MerkleHash, u64)]) -> (MerkleHash, u64) {
    let mut buf = String::with_capacity(hashes.len() * 88);
    let mut total_size = 0u64;

    // M2 fix: Use std::fmt::Write to format directly into buf,
    // avoiding intermediate String allocation from format!().
    use std::fmt::Write;
    for (hash, size) in hashes {
        write!(buf, "{} : {}\n", hash.to_hex(), size).unwrap();
        total_size += size;
    }

    let merged_hash = compute_internal_node_hash(buf.as_bytes());
    (merged_hash, total_size)
}

/// Aggregated node hash computation
///
/// Iteratively collapse the node list until only one root remains
fn aggregated_node_hash(chunks: &[(MerkleHash, u64)]) -> MerkleHash {
    if chunks.is_empty() {
        return MerkleHash::default();
    }

    let mut nodes: Vec<(MerkleHash, u64)> = chunks.to_vec();

    while nodes.len() > 1 {
        let mut new_nodes = Vec::new();
        let mut read_idx = 0;

        while read_idx < nodes.len() {
            let cut_point = read_idx + next_merge_cut(&nodes[read_idx..]);
            let group = &nodes[read_idx..cut_point];

            let (merged_hash, merged_size) = merged_hash_of_sequence(group);
            new_nodes.push((merged_hash, merged_size));

            read_idx = cut_point;
        }

        nodes = new_nodes;
    }

    nodes[0].0
}

/// Compute Xorb hash
///
/// Input: list of (chunk_hash, uncompressed_size) pairs
/// Output: aggregated Merkle tree root hash (no salt)
pub fn xorb_hash(chunks: &[(MerkleHash, u64)]) -> MerkleHash {
    aggregated_node_hash(chunks)
}

/// Compute file hash
///
/// Input: list of (chunk_hash, chunk_uncompressed_size) pairs
/// Output: aggregated Merkle tree root hash + HMAC(salt)
/// Default salt is all zeros
pub fn file_hash(chunks: &[(MerkleHash, u64)]) -> MerkleHash {
    if chunks.is_empty() {
        return MerkleHash::default();
    }

    let base_hash = aggregated_node_hash(chunks);
    let salt = MerkleHash::default(); // [0; 32]

    hmac_hash(&salt, &base_hash)
}

/// Compute file hash with custom salt
#[allow(dead_code)]
pub fn file_hash_with_salt(chunks: &[(MerkleHash, u64)], salt: &MerkleHash) -> MerkleHash {
    if chunks.is_empty() {
        return MerkleHash::default();
    }

    let base_hash = aggregated_node_hash(chunks);
    hmac_hash(salt, &base_hash)
}