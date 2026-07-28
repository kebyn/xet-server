//! API modules for Xet Storage server

pub mod auth;
pub mod batch;
pub mod global_dedup;
pub mod guard;
pub mod internal;
pub mod lfs;
pub mod reconstruction;
pub mod shard;
pub mod xorb;

pub(crate) const INTERNAL_ERROR_MESSAGE: &str = "Internal server error";
