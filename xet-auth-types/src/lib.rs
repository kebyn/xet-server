//! Shared authentication claim types for the Xet workspace.
//!
//! `XetClaims` is the wire format for Ed25519 JWT tokens exchanged between the
//! Hub (`hub-api`, which signs tokens) and the CAS server (`xet-server`, which
//! verifies them). Both crates depend on this crate so the struct is defined
//! exactly once — keeping the serialized token format authoritative and
//! preventing silent wire-compatibility drift.

use serde::{Deserialize, Serialize};

/// Claims embedded in a xet JWT token.
///
/// The serialized form of this struct is the on-the-wire token payload shared
/// between Hub and CAS. Field names and serde annotations are part of the wire
/// contract — do not change them without versioning the token format.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct XetClaims {
    /// Subject (user identity)
    pub sub: String,
    /// Scope(s) granted (space-separated, e.g., "read write")
    pub scope: String,
    /// Repository ID (HuggingFace-style repo identifier)
    pub repo_id: String,
    /// Repository type (e.g., "model", "dataset", "space")
    pub repo_type: String,
    /// Git revision being accessed
    pub revision: String,
    /// Expiration timestamp (Unix seconds)
    pub exp: u64,
    /// Issued-at timestamp (Unix seconds)
    pub iat: u64,
    /// Key ID identifying the signing key
    pub kid: String,
    /// Token type: "user" (default), "proxy", or "internal"
    /// Used for defense-in-depth to distinguish internal service tokens.
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// LFS object ID (for proxy tokens, binds token to specific object)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// LFS operation: "upload" or "download" (for proxy tokens)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

/// Default token type for backward compatibility with older tokens.
pub fn default_token_type() -> String {
    "user".to_string()
}
