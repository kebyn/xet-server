mod blake3_hash;
mod merkle_tree;

pub use blake3_hash::{compute_data_hash, compute_internal_node_hash, hmac_hash};
pub use merkle_tree::{file_hash, xorb_hash};