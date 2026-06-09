//! # Xet Server
//!
//! Core data structures and algorithms for a Xet Storage compatible server.
//!
//! ## Core Components
//!
//! - [`MerkleHash`](types::MerkleHash): 256-bit hash value
//! - [`compute_data_hash`](hash::compute_data_hash): BLAKE3 keyed hash (leaf nodes)
//! - [`compute_internal_node_hash`](hash::compute_internal_node_hash): BLAKE3 keyed hash (internal nodes)
//! - [`xorb_hash`](hash::xorb_hash): Aggregated Merkle tree (Xorb hash)
//! - [`file_hash`](hash::file_hash): Aggregated Merkle tree + HMAC (file hash)
//! - [`Chunker`](chunking::Chunker): GearHash CDC chunker
//!
//! ## Example
//!
//! ```rust
//! use xet_server::chunking::{Chunker, ChunkConfig};
//! use xet_server::hash::{compute_data_hash, xorb_hash};
//!
//! // CDC chunking
//! let data = b"test data".repeat(10000);
//! let mut chunker = Chunker::new(ChunkConfig::default());
//! let chunks = chunker.chunk_data(&data);
//!
//! // Compute chunk hashes
//! let chunk_hashes: Vec<_> = chunks
//!     .iter()
//!     .map(|c| {
//!         let chunk_data = &data[c.offset..c.offset + c.size];
//!         (compute_data_hash(chunk_data), c.size as u64)
//!     })
//!     .collect();
//!
//! // Compute xorb hash
//! let xorb = xorb_hash(&chunk_hashes);
//! ```

pub mod error;
pub mod types;
pub mod hash;
pub mod chunking;
pub mod format;
pub mod config;
pub mod storage;
pub mod api;
pub mod server;
pub mod index;
pub mod metrics;
pub mod middleware;

pub use error::{XetError, Result};