# CAS Modifications Implementation Plan

**Status:** ✅ Completed  
**Date:** 2026-06-10  
**Implemented:** 2026-06-12  

> **Superseded auth note:** This historical plan predates the current token boundary. Any examples where `internal` scope supersedes `read`/`write`, or where internal endpoints call `check_scope(..., "internal")`, are superseded. Current code requires `internal_xxx` tokens with `sub=hub-service`, `scope=internal`, and `token_type=internal`, and internal authorization must go through `AuthNeed::Internal` / `is_internal_token`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Modify the existing xet-server (CAS) to support Ed25519 token authentication, storage state tracking, internal endpoints for the Hub API, and state-aware LFS download with reconstruction.

**Architecture:** Replace the HMAC-SHA256 JWT auth with Ed25519 asymmetric key verification. Add a SQLite-based storage state manager that tracks whether each blob is in `raw_only` or `xet_only` state. Add internal HTTP endpoints for Hub-to-CAS communication. Modify the LFS download endpoint to serve from raw blob or reconstruction based on state.

**Tech Stack:** Rust, actix-web, ed25519-dalek, rusqlite, serde, tokio

**Spec:** `docs/superpowers/specs/2026-06-10-hf-hub-api-design.md`

---

## File Structure

### Modified Files
| File | Responsibility |
|------|---------------|
| `Cargo.toml` | Add ed25519-dalek, rusqlite, uuid dependencies; remove jsonwebtoken |
| `src/config.rs` | Replace `AuthConfig.jwt_secret` with Ed25519 key config; add state DB config |
| `src/api/auth.rs` | Complete rewrite: Ed25519 token verification, XetClaims type |
| `src/api/mod.rs` | Add `internal` module |
| `src/server.rs` | Add internal routes; inject StateManager into app data |
| `src/api/lfs.rs` | Register state on upload; state-aware download with reconstruction |
| `src/api/xorb.rs` | Update auth calls (new API) |
| `src/api/shard.rs` | Update auth calls (new API) |
| `src/api/batch.rs` | Update auth calls (new API) |
| `src/lib.rs` | Add `state` module |

### New Files
| File | Responsibility |
|------|---------------|
| `src/state/mod.rs` | StorageStateManager trait, FileState type |
| `src/state/sqlite.rs` | SQLite implementation of StorageStateManager |
| `src/api/internal.rs` | Internal endpoints: state query, blob check, batch state |

### Test Files
| File | Responsibility |
|------|---------------|
| `tests/test_auth.rs` | Rewrite for Ed25519 |
| `tests/test_api.rs` | Update token generation |
| `tests/test_advanced_api.rs` | Update token generation |
| `tests/test_e2e.rs` | Update token generation |
| `tests/test_streaming_upload.rs` | Update token generation |
| `tests/test_state.rs` | New: StorageStateManager tests |
| `tests/test_internal_api.rs` | New: internal endpoint tests |

---

### Task 1: Add Dependencies and Generate Test Keys

**Files:**
- Modify: `Cargo.toml`

- [x] **Step 1: Add ed25519-dalek, rusqlite, uuid dependencies**

Open `Cargo.toml` and replace `jsonwebtoken = "9.2"` with:

```toml
ed25519-dalek = { version = "2", features = ["pem", "rand_core"] }
rusqlite = { version = "0.31", features = ["bundled"] }
uuid = { version = "1", features = ["v4"] }
rand = "0.8"
```

Keep `rand = "0.8"` in `[dev-dependencies]` as well (it's already there).

Also remove `jsonwebtoken = "9.2"` and `base64 = "0.22"` from `[dependencies]` (base64 is still needed for batch.rs, so keep it — actually let me check... base64 is used in auth.rs for Basic auth extraction. We'll keep it for now).

Remove only `jsonwebtoken = "9.2"`.

The final `[dependencies]` section:

```toml
[dependencies]
blake3 = "1.8"
gearhash = "0.1"
serde = { version = "1.0", features = ["derive"] }
thiserror = "2.0"
hex = "0.4"
lz4_flex = "0.11"
tokio = { version = "1.36", features = ["full"] }
actix-web = "4.5"
serde_json = "1.0"
async-trait = "0.1"
bytes = "1.5"
aws-sdk-s3 = "1.15"
aws-config = "1.1"
tracing = "0.1"
tracing-subscriber = "0.3"
lazy_static = "1.4"
base64 = "0.22"
futures-util = "0.3"
ed25519-dalek = { version = "2", features = ["pem", "rand_core"] }
rusqlite = { version = "0.31", features = ["bundled"] }
uuid = { version = "1", features = ["v4"] }
rand = "0.8"
```

- [x] **Step 2: Verify dependencies compile**

Run: `cargo check 2>&1 | tail -5`
Expected: Compilation succeeds (may have warnings about unused imports later, but no errors from new deps).

- [x] **Step 3: Generate test Ed25519 key pair**

Run:
```bash
cd /data
cargo run --example generate_test_keys 2>/dev/null || true
```

This won't work yet (no example). Instead, create a helper script:

```bash
mkdir -p /data/tests/keys
```

We'll generate keys programmatically in tests. For now, just verify the dep compiles.

- [x] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add ed25519-dalek, rusqlite, uuid for auth rewrite"
```

---

### Task 2: Ed25519 Auth Types and Key Operations

**Files:**
- Create: `src/api/auth.rs` (complete rewrite)
- Test: `tests/test_auth.rs` (complete rewrite)

- [x] **Step 1: Write failing tests for Ed25519 auth**

Replace `tests/test_auth.rs` entirely:

```rust
//! Tests for Ed25519 token authentication

use xet_server::api::auth::{
    sign_xet_token, verify_xet_token, XetClaims,
    extract_token_from_request, check_scope, KeyPair,
};

fn test_keypair() -> KeyPair {
    KeyPair::generate()
}

#[test]
fn test_sign_and_verify_token() {
    let kp = test_keypair();
    let claims = XetClaims {
        sub: "user123".to_string(),
        scope: "read write".to_string(),
        repo_id: "alice/my-model".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key-001".to_string(),
    };

    let token = sign_xet_token(&claims, &kp).unwrap();
    assert!(token.starts_with("xet_"));

    let verified = verify_xet_token(&token, &kp.public_key, "test-key-001").unwrap();
    assert_eq!(verified.sub, "user123");
    assert_eq!(verified.scope, "read write");
    assert_eq!(verified.repo_id, "alice/my-model");
}

#[test]
fn test_expired_token() {
    let kp = test_keypair();
    let claims = XetClaims {
        sub: "user123".to_string(),
        scope: "read".to_string(),
        repo_id: "alice/my-model".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 1,  // long expired
        iat: 0,
        kid: "test-key-001".to_string(),
    };

    let token = sign_xet_token(&claims, &kp).unwrap();
    assert!(verify_xet_token(&token, &kp.public_key, "test-key-001").is_err());
}

#[test]
fn test_wrong_key_rejected() {
    let kp1 = test_keypair();
    let kp2 = test_keypair();
    let claims = XetClaims {
        sub: "user123".to_string(),
        scope: "read".to_string(),
        repo_id: "alice/my-model".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key-001".to_string(),
    };

    let token = sign_xet_token(&claims, &kp1).unwrap();
    // Verify with different key should fail
    assert!(verify_xet_token(&token, &kp2.public_key, "test-key-001").is_err());
}

#[test]
fn test_wrong_kid_rejected() {
    let kp = test_keypair();
    let claims = XetClaims {
        sub: "user123".to_string(),
        scope: "read".to_string(),
        repo_id: "alice/my-model".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key-001".to_string(),
    };

    let token = sign_xet_token(&claims, &kp).unwrap();
    // Verify with different kid should fail
    assert!(verify_xet_token(&token, &kp.public_key, "wrong-kid").is_err());
}

#[test]
fn test_check_scope() {
    let claims = XetClaims {
        sub: "user123".to_string(),
        scope: "read write".to_string(),
        repo_id: "alice/my-model".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key-001".to_string(),
    };

    assert!(check_scope(&claims, "read"));
    assert!(check_scope(&claims, "write"));
    assert!(!check_scope(&claims, "admin"));
}

#[test]
fn test_internal_scope_supersedes_write() {
    let claims = XetClaims {
        sub: "hub-service".to_string(),
        scope: "internal".to_string(),
        repo_id: "".to_string(),
        repo_type: "".to_string(),
        revision: "".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key-001".to_string(),
    };

    assert!(check_scope(&claims, "read"));
    assert!(check_scope(&claims, "write"));
    assert!(check_scope(&claims, "internal"));
}

#[test]
fn test_extract_token_from_request() {
    // This test will use actix_web test utilities
    // For now, test the prefix stripping logic
    let token_with_prefix = "xet_eyJhbGciOiJFZDI1NTE5";
    assert!(token_with_prefix.starts_with("xet_"));
    let raw = token_with_prefix.strip_prefix("xet_").unwrap();
    assert_eq!(raw, "eyJhbGciOiJFZDI1NTE5");
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --test test_auth 2>&1 | tail -10`
Expected: FAIL — `module xet_server::api::auth` exports don't match (sign_xet_token, verify_xet_token, XetClaims, KeyPair not found).

- [x] **Step 3: Rewrite `src/api/auth.rs`**

Replace the entire file:

```rust
//! Ed25519 token authentication for Xet CAS server
//!
//! Tokens are JWTs signed by the Hub API's Ed25519 private key.
//! Format: "xet_" prefix + base64url(JSON header + "." + JSON payload + "." + signature)
//!
//! The CAS server verifies tokens using the Hub's Ed25519 public key.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey, Verifier, Signature};
use serde::{Deserialize, Serialize};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

/// Ed25519 key pair for token operations
pub struct KeyPair {
    pub signing_key: SigningKey,
    pub public_key: VerifyingKey,
}

impl KeyPair {
    /// Generate a new random key pair
    pub fn generate() -> Self {
        let mut csprng = rand::rngs::OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let public_key = signing_key.verifying_key();
        Self { signing_key, public_key }
    }

    /// Load public key from PEM bytes
    pub fn public_key_from_pem(pem: &[u8]) -> Result<VerifyingKey, AuthError> {
        let pem_str = std::str::from_utf8(pem)
            .map_err(|e| AuthError::InvalidKey(format!("Invalid UTF-8 in PEM: {}", e)))?;

        // Parse simple PEM: look for base64 between BEGIN/END lines
        let b64: String = pem_str
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");

        let der = base64::engine::general_purpose::STANDARD.decode(&b64)
            .map_err(|e| AuthError::InvalidKey(format!("Invalid base64 in PEM: {}", e)))?;

        // Ed25519 public key in SubjectPublicKeyInfo (SPKI) DER format
        // The last 32 bytes are the raw public key
        if der.len() < 32 {
            return Err(AuthError::InvalidKey("DER too short".to_string()));
        }
        let raw_key_bytes = &der[der.len() - 32..];
        let key_bytes: [u8; 32] = raw_key_bytes.try_into()
            .map_err(|_| AuthError::InvalidKey("Invalid key length".to_string()))?;

        VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| AuthError::InvalidKey(format!("Invalid Ed25519 key: {}", e)))
    }

    /// Load private key from PEM bytes
    pub fn private_key_from_pem(pem: &[u8]) -> Result<SigningKey, AuthError> {
        let pem_str = std::str::from_utf8(pem)
            .map_err(|e| AuthError::InvalidKey(format!("Invalid UTF-8 in PEM: {}", e)))?;

        let b64: String = pem_str
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");

        let der = base64::engine::general_purpose::STANDARD.decode(&b64)
            .map_err(|e| AuthError::InvalidKey(format!("Invalid base64 in PEM: {}", e)))?;

        // Ed25519 private key in PKCS8 DER format
        // The raw 32-byte seed is at a fixed offset in the PKCS8 structure
        // For Ed25519 PKCS8: the seed is the last 32 bytes of the DER
        if der.len() < 32 {
            return Err(AuthError::InvalidKey("DER too short".to_string()));
        }
        let raw_key_bytes = &der[der.len() - 32..];
        let key_bytes: [u8; 32] = raw_key_bytes.try_into()
            .map_err(|_| AuthError::InvalidKey("Invalid key length".to_string()))?;

        Ok(SigningKey::from_bytes(&key_bytes))
    }
}

/// JWT claims for xet tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XetClaims {
    pub sub: String,
    pub scope: String,
    pub repo_id: String,
    pub repo_type: String,
    pub revision: String,
    pub exp: u64,
    pub iat: u64,
    pub kid: String,
}

/// JWT header
#[derive(Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
    kid: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Invalid token: {0}")]
    InvalidToken(String),
    #[error("Token expired")]
    Expired,
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Unknown key id: {0}")]
    UnknownKid(String),
    #[error("Invalid key: {0}")]
    InvalidKey(String),
}

/// Sign claims into a xet token string (used by Hub API, not CAS)
pub fn sign_xet_token(claims: &XetClaims, keypair: &KeyPair) -> Result<String, AuthError> {
    let header = JwtHeader {
        alg: "EdDSA",
        typ: "JWT",
        kid: claims.kid.clone(),
    };

    let header_json = serde_json::to_vec(&header)
        .map_err(|e| AuthError::InvalidToken(format!("Failed to serialize header: {}", e)))?;
    let claims_json = serde_json::to_vec(&claims)
        .map_err(|e| AuthError::InvalidToken(format!("Failed to serialize claims: {}", e)))?;

    let header_b64 = URL_SAFE_NO_PAD.encode(&header_json);
    let claims_b64 = URL_SAFE_NO_PAD.encode(&claims_json);

    let signing_input = format!("{}.{}", header_b64, claims_b64);
    let signature = keypair.signing_key.sign(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!("xet_{}.{}", signing_input, sig_b64))
}

/// Verify a xet token and return claims
pub fn verify_xet_token(
    token: &str,
    public_key: &VerifyingKey,
    expected_kid: &str,
) -> Result<XetClaims, AuthError> {
    // Strip "xet_" prefix
    let jwt = token.strip_prefix("xet_")
        .ok_or_else(|| AuthError::InvalidToken("Missing xet_ prefix".to_string()))?;

    // Split into header.payload.signature
    let parts: Vec<&str> = jwt.splitn(3, '.').collect();
    if parts.len() != 3 {
        return Err(AuthError::InvalidToken("Invalid JWT format".to_string()));
    }

    let (header_b64, payload_b64, sig_b64) = (parts[0], parts[1], parts[2]);

    // Decode and check header for kid
    let header_bytes = URL_SAFE_NO_PAD.decode(header_b64)
        .map_err(|e| AuthError::InvalidToken(format!("Invalid header base64: {}", e)))?;
    let header: serde_json::Value = serde_json::from_slice(&header_bytes)
        .map_err(|e| AuthError::InvalidToken(format!("Invalid header JSON: {}", e)))?;

    // Verify kid matches
    let token_kid = header.get("kid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AuthError::InvalidToken("Missing kid in header".to_string()))?;

    if token_kid != expected_kid {
        return Err(AuthError::UnknownKid(format!(
            "Expected {}, got {}", expected_kid, token_kid
        )));
    }

    // Verify signature
    let signing_input = format!("{}.{}", header_b64, payload_b64);
    let sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64)
        .map_err(|e| AuthError::InvalidToken(format!("Invalid signature base64: {}", e)))?;

    let sig_array: [u8; 64] = sig_bytes.try_into()
        .map_err(|_| AuthError::InvalidToken("Invalid signature length".to_string()))?;
    let signature = Signature::from_bytes(&sig_array);

    public_key.verify(signing_input.as_bytes(), &signature)
        .map_err(|_| AuthError::InvalidSignature)?;

    // Decode claims
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64)
        .map_err(|e| AuthError::InvalidToken(format!("Invalid payload base64: {}", e)))?;
    let claims: XetClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| AuthError::InvalidToken(format!("Invalid claims JSON: {}", e)))?;

    // Check expiration
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if claims.exp < now {
        return Err(AuthError::Expired);
    }

    Ok(claims)
}

/// Extract bearer token from HTTP request
pub fn extract_token_from_request(req: &actix_web::HttpRequest) -> Option<String> {
    let auth_header = req.headers().get("Authorization")?;
    let auth_str = auth_header.to_str().ok()?;
    auth_str.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Check if claims have the required scope.
/// "internal" scope supersedes all other scopes.
pub fn check_scope(claims: &XetClaims, required: &str) -> bool {
    if claims.scope.split_whitespace().any(|s| s == "internal") {
        return true;
    }
    claims.scope.split_whitespace().any(|s| s == required)
}
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --test test_auth 2>&1 | tail -20`
Expected: All tests PASS.

Note: Other tests (test_api, etc.) will fail to compile because they reference the old `create_jwt`/`JwtClaims`. That's expected and fixed in Task 3.

- [x] **Step 5: Commit**

```bash
git add src/api/auth.rs tests/test_auth.rs
git commit -m "feat: rewrite auth module with Ed25519 token verification"
```

---

### Task 3: Update Config for Ed25519 and State DB

**Files:**
- Modify: `src/config.rs`

- [x] **Step 1: Update ServerConfig**

Replace `src/config.rs` with:

```rust
//! Configuration management for Xet Storage server

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub state: StateConfig,
}

/// HTTP server settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_base_url: Option<String>,
    pub max_body_size_mb: u64,
}

impl ServerSettings {
    pub fn base_url(&self) -> String {
        let url = self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port));
        url.trim_end_matches('/').to_string()
    }

    pub fn max_body_size_bytes(&self) -> usize {
        self.max_body_size_mb
            .saturating_mul(1024 * 1024)
            .try_into()
            .unwrap_or(usize::MAX)
    }
}

/// Storage backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_endpoint: Option<String>,
    pub local_path: Option<String>,
    pub upload_temp_dir: Option<String>,
}

impl StorageConfig {
    pub fn resolve_upload_temp_dir(&self) -> PathBuf {
        if let Some(dir) = &self.upload_temp_dir {
            PathBuf::from(dir)
        } else if let Some(local_path) = &self.local_path {
            PathBuf::from(local_path).join(".tmp")
        } else {
            PathBuf::from("/tmp/xet-uploads")
        }
    }
}

/// Authentication configuration (Ed25519)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Path to Hub's Ed25519 public key (PEM format)
    pub public_key_path: String,
    /// Trusted key IDs
    pub trusted_kids: Vec<String>,
    /// Expected token prefix
    pub token_prefix: String,
}

/// Storage state database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    /// Path to SQLite database for file state tracking
    pub sqlite_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8080,
                public_base_url: None,
                max_body_size_mb: 2048,
            },
            storage: StorageConfig {
                backend: "local".to_string(),
                s3_bucket: None,
                s3_region: None,
                s3_endpoint: None,
                local_path: Some("./data".to_string()),
                upload_temp_dir: None,
            },
            auth: AuthConfig {
                public_key_path: String::new(),
                trusted_kids: vec!["default".to_string()],
                token_prefix: "xet_".to_string(),
            },
            state: StateConfig {
                sqlite_path: "./data/file_states.db".to_string(),
            },
        }
    }
}

impl ServerConfig {
    /// Load configuration from environment variables with defaults
    pub fn from_env() -> Self {
        let host = std::env::var("XET_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = std::env::var("XET_PORT")
            .ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
        let public_base_url = std::env::var("XET_PUBLIC_BASE_URL").ok();
        let max_body_size_mb = std::env::var("XET_MAX_BODY_SIZE_MB")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(2048);

        let backend = std::env::var("XET_STORAGE_BACKEND").unwrap_or_else(|_| "local".to_string());
        let s3_bucket = std::env::var("XET_S3_BUCKET").ok();
        let s3_region = std::env::var("XET_S3_REGION").ok();
        let s3_endpoint = std::env::var("XET_S3_ENDPOINT").ok();
        let local_path = std::env::var("XET_LOCAL_PATH").ok();
        let upload_temp_dir = std::env::var("XET_UPLOAD_TEMP_DIR").ok();

        let public_key_path = std::env::var("CAS_PUBLIC_KEY_PATH")
            .unwrap_or_else(|_| "/etc/cas/keys/hub_public.pem".to_string());
        let trusted_kids = std::env::var("CAS_TRUSTED_KIDS")
            .unwrap_or_else(|_| "default".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();
        let token_prefix = std::env::var("CAS_TOKEN_PREFIX")
            .unwrap_or_else(|_| "xet_".to_string());

        let state_sqlite_path = std::env::var("CAS_STATE_DB_PATH")
            .unwrap_or_else(|_| "./data/file_states.db".to_string());

        Self {
            server: ServerSettings { host, port, public_base_url, max_body_size_mb },
            storage: StorageConfig {
                backend, s3_bucket, s3_region, s3_endpoint, local_path, upload_temp_dir,
            },
            auth: AuthConfig { public_key_path, trusted_kids, token_prefix },
            state: StateConfig { sqlite_path: state_sqlite_path },
        }
    }
}
```

- [x] **Step 2: Verify compilation**

Run: `cargo check 2>&1 | tail -10`
Expected: Errors in files that reference `config.auth.jwt_secret` (removed). These will be fixed in subsequent tasks.

- [x] **Step 3: Commit**

```bash
git add src/config.rs
git commit -m "feat: update config for Ed25519 auth and state DB"
```

---

### Task 4: Update API Handlers for New Auth

**Files:**
- Modify: `src/api/xorb.rs`
- Modify: `src/api/shard.rs`
- Modify: `src/api/batch.rs`
- Modify: `src/api/lfs.rs`

Each handler currently calls `validate_jwt(&token, &config.auth.jwt_secret)` and `check_scope(&claims, ...)`. These need to be updated to use `verify_xet_token`.

The pattern for each handler changes from:

```rust
// OLD
let claims = match validate_jwt(&token, &config.auth.jwt_secret) {
    Ok(c) => c,
    Err(_) => return HttpResponse::Unauthorized().json(...)
};
if !check_scope(&claims, "write") { ... }
```

To:

```rust
// NEW
let claims = match verify_token(&token, &config.auth) {
    Ok(c) => c,
    Err(e) => return HttpResponse::Unauthorized().json(...)
};
if !check_scope(&claims, "write") { ... }
```

- [x] **Step 1: Add verify_token helper to auth.rs**

Add this function to the end of `src/api/auth.rs`:

```rust
/// Verify a token using the auth config (tries each trusted kid)
pub fn verify_token(
    token: &str,
    auth_config: &crate::config::AuthConfig,
) -> Result<XetClaims, AuthError> {
    // For now, we need the public key loaded.
    // In production, this would be loaded once at startup and cached.
    // For simplicity, we load it each time (can optimize later).
    let pem = std::fs::read(&auth_config.public_key_path)
        .map_err(|e| AuthError::InvalidKey(format!("Failed to read public key: {}", e)))?;
    let public_key = KeyPair::public_key_from_pem(&pem)?;

    // Try each trusted kid
    let mut last_err = AuthError::UnknownKid("no trusted kids configured".to_string());
    for kid in &auth_config.trusted_kids {
        match verify_xet_token(token, &public_key, kid) {
            Ok(claims) => return Ok(claims),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}
```

- [x] **Step 2: Update xorb.rs**

In `src/api/xorb.rs`, replace:

```rust
use crate::api::auth::{check_scope, extract_token_from_request, validate_jwt};
```

with:

```rust
use crate::api::auth::{check_scope, extract_token_from_request, verify_token};
```

And replace every occurrence of:

```rust
let claims = match validate_jwt(&token, &config.auth.jwt_secret) {
    Ok(c) => c,
    Err(_) => {
```

with:

```rust
let claims = match verify_token(&token, &config.auth) {
    Ok(c) => c,
    Err(_) => {
```

There are 2 occurrences in xorb.rs (upload and download handlers). Use `replace_all` for the pattern `validate_jwt(&token, &config.auth.jwt_secret)` → `verify_token(&token, &config.auth)`.

- [x] **Step 3: Update shard.rs**

Same pattern: replace `validate_jwt` import and calls with `verify_token`.

Replace:
```rust
use crate::api::auth::{check_scope, extract_token_from_request, validate_jwt};
```
with:
```rust
use crate::api::auth::{check_scope, extract_token_from_request, verify_token};
```

Replace all: `validate_jwt(&token, &config.auth.jwt_secret)` → `verify_token(&token, &config.auth)`

- [x] **Step 4: Update batch.rs**

Same pattern: replace `validate_jwt` import and calls with `verify_token`.

Replace:
```rust
use crate::api::auth::{extract_token_from_request, validate_jwt};
```
with:
```rust
use crate::api::auth::{extract_token_from_request, verify_token};
```

Replace all: `validate_jwt(&token, &config.auth.jwt_secret)` → `verify_token(&token, &config.auth)`

- [x] **Step 5: Update lfs.rs**

Same pattern.

Replace:
```rust
use crate::api::auth::{check_scope, extract_token_from_request, validate_jwt};
```
with:
```rust
use crate::api::auth::{check_scope, extract_token_from_request, verify_token};
```

Replace all: `validate_jwt(&token, &config.auth.jwt_secret)` → `verify_token(&token, &config.auth)`

- [x] **Step 6: Verify compilation**

Run: `cargo check 2>&1 | tail -10`
Expected: Errors only in test files (which still reference old auth API). Source files should compile.

- [x] **Step 7: Commit**

```bash
git add src/api/auth.rs src/api/xorb.rs src/api/shard.rs src/api/batch.rs src/api/lfs.rs
git commit -m "feat: update all API handlers to use Ed25519 token verification"
```

---

### Task 5: Fix Existing Tests for New Auth

**Files:**
- Modify: `tests/test_api.rs`
- Modify: `tests/test_advanced_api.rs`
- Modify: `tests/test_e2e.rs`
- Modify: `tests/test_streaming_upload.rs`
- Modify: `tests/test_auth.rs` (remove tests for removed functions)

All test files use `create_jwt(&JwtClaims {...}, &config.auth.jwt_secret)` to generate tokens. This needs to change to `sign_xet_token(&XetClaims {...}, &keypair)`.

- [x] **Step 1: Create a test helper module**

Create `tests/common/mod.rs`:

```rust
//! Common test utilities

use xet_server::api::auth::{sign_xet_token, XetClaims, KeyPair};

/// Create a test keypair and return (keypair, token) for use in tests
pub fn test_token(scope: &str) -> (KeyPair, String) {
    let kp = KeyPair::generate();
    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: scope.to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key".to_string(),
    };
    let token = sign_xet_token(&claims, &kp).unwrap();
    (kp, token)
}

/// Create a config suitable for testing with the given keypair
pub fn test_config_with_key(kp: &KeyPair) -> xet_server::config::ServerConfig {
    let mut config = xet_server::config::ServerConfig::default();
    // Write public key to a temp file
    let pub_key_bytes = kp.public_key.to_bytes();
    // Create a minimal PEM-like format that our parser can handle
    // For tests, we'll use a raw binary format and a special test path
    let temp_dir = std::env::temp_dir().join("xet-test-keys");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let key_path = temp_dir.join("test_public.key");
    std::fs::write(&key_path, &pub_key_bytes).unwrap();
    config.auth.public_key_path = key_path.to_str().unwrap().to_string();
    config.auth.trusted_kids = vec!["test-key".to_string()];
    config
}
```

Wait — the current `KeyPair::public_key_from_pem` expects PEM format. For tests, we need a simpler approach. Let me add a test-friendly constructor.

Actually, let me change the approach. Instead of PEM files for tests, let me modify the auth module to support both PEM file and raw key bytes. Or better: let me add a helper that writes the public key in a format the parser can read.

For simplicity in tests, let me add a method to write the public key in SPKI DER format wrapped in PEM markers:

Add to `src/api/auth.rs`:

```rust
impl KeyPair {
    /// Write public key as PEM bytes (for testing and deployment)
    pub fn public_key_to_pem(&self) -> Vec<u8> {
        use ed25519_dalek::pkcs8::EncodePublicKey;
        let der = self.public_key.to_public_key_der().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_bytes());
        let mut pem = String::new();
        pem.push_str("-----BEGIN PUBLIC KEY-----\n");
        for chunk in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(chunk).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END PUBLIC KEY-----\n");
        pem.into_bytes()
    }
}
```

This requires adding `pkcs8` feature to ed25519-dalek. Update Cargo.toml:

```toml
ed25519-dalek = { version = "2", features = ["pem", "rand_core", "pkcs8"] }
```

Now the test helper becomes:

```rust
// tests/common/mod.rs
pub fn test_config_with_key(kp: &KeyPair) -> xet_server::config::ServerConfig {
    let mut config = xet_server::config::ServerConfig::default();
    let temp_dir = std::env::temp_dir().join("xet-test-keys");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let key_path = temp_dir.join("test_public.pem");
    std::fs::write(&key_path, kp.public_key_to_pem()).unwrap();
    config.auth.public_key_path = key_path.to_str().unwrap().to_string();
    config.auth.trusted_kids = vec!["test-key".to_string()];
    config
}
```

- [x] **Step 2: Create tests/common/mod.rs**

Create `tests/common/mod.rs` with the helper functions shown above.

- [x] **Step 3: Update test_api.rs**

In `tests/test_api.rs`:

Replace:
```rust
use xet_server::api::auth::{create_jwt, JwtClaims};
```
with:
```rust
mod common;
use xet_server::api::auth::{sign_xet_token, XetClaims, KeyPair};
```

Replace every token creation block:
```rust
let config = ServerConfig::default();
let token = create_jwt(
    &JwtClaims {
        sub: "test".to_string(),
        scope: "read write".to_string(),
        exp: 9999999999,
    },
    &config.auth.jwt_secret,
).unwrap();
```

with:
```rust
let kp = KeyPair::generate();
let config = common::test_config_with_key(&kp);
let claims = XetClaims {
    sub: "test".to_string(),
    scope: "read write".to_string(),
    repo_id: "test/repo".to_string(),
    repo_type: "model".to_string(),
    revision: "main".to_string(),
    exp: 9999999999,
    iat: 1000000000,
    kid: "test-key".to_string(),
};
let token = sign_xet_token(&claims, &kp).unwrap();
```

Note: each test function needs its own keypair because `verify_token` reads the public key from the file path in config, and each test may have different keys.

- [x] **Step 4: Update test_advanced_api.rs, test_e2e.rs, test_streaming_upload.rs**

Apply the same pattern: replace `create_jwt`/`JwtClaims` with `sign_xet_token`/`XetClaims`/`KeyPair`, and use `common::test_config_with_key(&kp)` to generate configs.

- [x] **Step 5: Fix test_auth.rs**

Remove tests for `validate_jwt`, `create_jwt`, `extract_bearer_token` (functions that no longer exist). Keep only tests for `sign_xet_token`, `verify_xet_token`, `check_scope`. The test file was already rewritten in Task 2.

- [x] **Step 6: Run all tests**

Run: `cargo test 2>&1 | tail -30`
Expected: All tests PASS.

- [x] **Step 7: Commit**

```bash
git add tests/ Cargo.toml
git commit -m "test: update all tests for Ed25519 auth"
```

---

### Task 6: StorageStateManager Trait and SQLite Implementation

**Files:**
- Create: `src/state/mod.rs`
- Create: `src/state/sqlite.rs`
- Modify: `src/lib.rs`
- Create: `tests/test_state.rs`

- [x] **Step 1: Write failing tests**

Create `tests/test_state.rs`:

```rust
//! Tests for StorageStateManager

use xet_server::state::{StorageStateManager, FileState, StorageState};
use xet_server::state::sqlite::SqliteStateManager;
use tempfile::tempdir;

fn create_test_manager() -> SqliteStateManager {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_states.db");
    SqliteStateManager::new(db_path.to_str().unwrap()).unwrap()
}

#[test]
fn test_register_raw_blob() {
    let mgr = create_test_manager();
    mgr.register_raw_blob("abc123", 1024).unwrap();

    let state = mgr.get_state("abc123").unwrap().unwrap();
    assert_eq!(state.state, StorageState::RawOnly);
    assert_eq!(state.size, 1024);
    assert!(state.xet_file_id.is_none());
}

#[test]
fn test_mark_converted() {
    let mgr = create_test_manager();
    mgr.register_raw_blob("abc123", 1024).unwrap();
    mgr.mark_converted("abc123", "file_001").unwrap();

    let state = mgr.get_state("abc123").unwrap().unwrap();
    assert_eq!(state.state, StorageState::XetOnly);
    assert_eq!(state.xet_file_id.as_deref(), Some("file_001"));
    assert!(state.converted_at.is_some());
}

#[test]
fn test_get_nonexistent() {
    let mgr = create_test_manager();
    let result = mgr.get_state("nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_get_states_batch() {
    let mgr = create_test_manager();
    mgr.register_raw_blob("aaa", 100).unwrap();
    mgr.register_raw_blob("bbb", 200).unwrap();
    mgr.mark_converted("bbb", "file_002").unwrap();

    let states = mgr.get_states(&["aaa".to_string(), "bbb".to_string(), "ccc".to_string()]).unwrap();
    assert_eq!(states.len(), 3);

    let aaa = states.iter().find(|(oid, _)| oid == "aaa").unwrap();
    assert_eq!(aaa.1.state, StorageState::RawOnly);

    let bbb = states.iter().find(|(oid, _)| oid == "bbb").unwrap();
    assert_eq!(bbb.1.state, StorageState::XetOnly);

    let ccc = states.iter().find(|(oid, _)| oid == "ccc").unwrap();
    assert!(ccc.1.is_none());
}

#[test]
fn test_register_xet_only() {
    let mgr = create_test_manager();
    mgr.register_xet_only("xyz789", "file_003", 2048).unwrap();

    let state = mgr.get_state("xyz789").unwrap().unwrap();
    assert_eq!(state.state, StorageState::XetOnly);
    assert_eq!(state.xet_file_id.as_deref(), Some("file_003"));
    assert_eq!(state.size, 2048);
}

#[test]
fn test_idempotent_register() {
    let mgr = create_test_manager();
    mgr.register_raw_blob("abc123", 1024).unwrap();
    // Registering again should not error (idempotent)
    mgr.register_raw_blob("abc123", 1024).unwrap();

    let state = mgr.get_state("abc123").unwrap().unwrap();
    assert_eq!(state.state, StorageState::RawOnly);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --test test_state 2>&1 | tail -10`
Expected: FAIL — `xet_server::state` module not found.

- [x] **Step 3: Create `src/state/mod.rs`**

```rust
//! Storage state management
//!
//! Tracks whether each blob is stored as raw bytes (RAW_ONLY) or
//! as xet chunks (XET_ONLY). This is the source of truth for
//! how to serve download requests.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod sqlite;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StorageState {
    RawOnly,
    XetOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub state: StorageState,
    pub xet_file_id: Option<String>,
    pub size: u64,
    pub sha256: String,
    pub created_at: u64,
    pub converted_at: Option<u64>,
}

#[async_trait]
pub trait StorageStateManager: Send + Sync {
    /// Get the storage state for a blob. Returns None if not registered.
    async fn get_state(&self, oid: &str) -> Result<Option<FileState>, StateError>;

    /// Register a new raw blob (from git lfs push).
    /// Idempotent: if already registered, no-op.
    async fn register_raw_blob(&self, oid: &str, size: u64) -> Result<(), StateError>;

    /// Register a file directly as XET_ONLY (from xet upload or inline commit).
    async fn register_xet_only(&self, oid: &str, file_id: &str, size: u64) -> Result<(), StateError>;

    /// Mark a raw blob as converted to xet format.
    /// Called after xorbs are stored and before raw blob is deleted.
    async fn mark_converted(&self, oid: &str, file_id: &str) -> Result<(), StateError>;

    /// Batch state query for multiple oids.
    /// Returns (oid, Option<FileState>) for each requested oid.
    async fn get_states(&self, oids: &[String]) -> Result<Vec<(String, Option<FileState>)>, StateError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("Internal error: {0}")]
    Internal(String),
}
```

- [x] **Step 4: Create `src/state/sqlite.rs`**

```rust
//! SQLite implementation of StorageStateManager

use async_trait::async_trait;
use rusqlite::{Connection, params};
use std::sync::Mutex;

use super::{StorageStateManager, StorageState, FileState, StateError};

pub struct SqliteStateManager {
    conn: Mutex<Connection>,
}

impl SqliteStateManager {
    pub fn new(db_path: &str) -> Result<Self, StateError> {
        let conn = Connection::open(db_path)
            .map_err(|e| StateError::Database(format!("Failed to open DB: {}", e)))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_states (
                oid          TEXT PRIMARY KEY,
                state        TEXT NOT NULL,
                xet_file_id  TEXT,
                size         INTEGER NOT NULL,
                sha256       TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                converted_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS conversion_locks (
                oid        TEXT PRIMARY KEY,
                locked_at  INTEGER NOT NULL,
                locked_by  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS xorb_refs (
                xorb_hash  TEXT NOT NULL,
                shard_id   TEXT NOT NULL,
                PRIMARY KEY (xorb_hash, shard_id)
            );"
        ).map_err(|e| StateError::Database(format!("Failed to create tables: {}", e)))?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}

#[async_trait]
impl StorageStateManager for SqliteStateManager {
    async fn get_state(&self, oid: &str) -> Result<Option<FileState>, StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT state, xet_file_id, size, sha256, created_at, converted_at FROM file_states WHERE oid = ?1"
        ).map_err(|e| StateError::Database(e.to_string()))?;

        let result = stmt.query_row(params![oid], |row| {
            let state_str: String = row.get(0)?;
            Ok(FileState {
                state: if state_str == "xet_only" { StorageState::XetOnly } else { StorageState::RawOnly },
                xet_file_id: row.get(1)?,
                size: row.get(2)?,
                sha256: row.get(3)?,
                created_at: row.get(4)?,
                converted_at: row.get(5)?,
            })
        });

        match result {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StateError::Database(e.to_string())),
        }
    }

    async fn register_raw_blob(&self, oid: &str, size: u64) -> Result<(), StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let now = Self::now_secs();
        conn.execute(
            "INSERT OR IGNORE INTO file_states (oid, state, size, sha256, created_at) VALUES (?1, 'raw_only', ?2, ?1, ?3)",
            params![oid, size as i64, now as i64],
        ).map_err(|e| StateError::Database(e.to_string()))?;
        Ok(())
    }

    async fn register_xet_only(&self, oid: &str, file_id: &str, size: u64) -> Result<(), StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let now = Self::now_secs();
        conn.execute(
            "INSERT OR REPLACE INTO file_states (oid, state, xet_file_id, size, sha256, created_at, converted_at) VALUES (?1, 'xet_only', ?2, ?3, ?1, ?4, ?4)",
            params![oid, file_id, size as i64, now as i64],
        ).map_err(|e| StateError::Database(e.to_string()))?;
        Ok(())
    }

    async fn mark_converted(&self, oid: &str, file_id: &str) -> Result<(), StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let now = Self::now_secs();
        let rows = conn.execute(
            "UPDATE file_states SET state = 'xet_only', xet_file_id = ?2, converted_at = ?3 WHERE oid = ?1",
            params![oid, file_id, now as i64],
        ).map_err(|e| StateError::Database(e.to_string()))?;

        if rows == 0 {
            return Err(StateError::Database(format!("No state found for oid {}", oid)));
        }
        Ok(())
    }

    async fn get_states(&self, oids: &[String]) -> Result<Vec<(String, Option<FileState>)>, StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let mut results = Vec::with_capacity(oids.len());

        for oid in oids {
            let mut stmt = conn.prepare(
                "SELECT state, xet_file_id, size, sha256, created_at, converted_at FROM file_states WHERE oid = ?1"
            ).map_err(|e| StateError::Database(e.to_string()))?;

            let result = stmt.query_row(params![oid.as_str()], |row| {
                let state_str: String = row.get(0)?;
                Ok(FileState {
                    state: if state_str == "xet_only" { StorageState::XetOnly } else { StorageState::RawOnly },
                    xet_file_id: row.get(1)?,
                    size: row.get(2)?,
                    sha256: row.get(3)?,
                    created_at: row.get(4)?,
                    converted_at: row.get(5)?,
                })
            });

            match result {
                Ok(state) => results.push((oid.clone(), Some(state))),
                Err(rusqlite::Error::QueryReturnedNoRows) => results.push((oid.clone(), None)),
                Err(e) => return Err(StateError::Database(e.to_string())),
            }
        }

        Ok(results)
    }
}
```

- [x] **Step 5: Add state module to lib.rs**

In `src/lib.rs`, add:
```rust
pub mod state;
```

- [x] **Step 6: Run tests**

Run: `cargo test --test test_state 2>&1 | tail -20`
Expected: All tests PASS.

- [x] **Step 7: Commit**

```bash
git add src/state/ src/lib.rs tests/test_state.rs
git commit -m "feat: add StorageStateManager trait and SQLite implementation"
```

---

### Task 7: CAS Internal Endpoints

**Files:**
- Create: `src/api/internal.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/server.rs`
- Create: `tests/test_internal_api.rs`

- [x] **Step 1: Write failing tests**

Create `tests/test_internal_api.rs`:

```rust
//! Tests for CAS internal endpoints

use actix_web::{test, web, App};
use tempfile::tempdir;

mod common;
use xet_server::api::auth::{sign_xet_token, XetClaims, KeyPair};
use xet_server::config::ServerConfig;
use xet_server::state::sqlite::SqliteStateManager;
use xet_server::state::StorageStateManager;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

fn internal_claims() -> XetClaims {
    XetClaims {
        sub: "hub-service".to_string(),
        scope: "internal".to_string(),
        repo_id: String::new(),
        repo_type: String::new(),
        revision: String::new(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key".to_string(),
    }
}

#[actix_web::test]
async fn test_internal_get_state_raw() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );
    let state_dir = tempdir().unwrap();
    let state_mgr = std::sync::Arc::new(
        SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
    );

    // Register a raw blob
    state_mgr.register_raw_blob("abc123def456", 1024).await.unwrap();

    let kp = KeyPair::generate();
    let config = common::test_config_with_key(&kp);
    let token = sign_xet_token(&internal_claims(), &kp).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::from(state_mgr))
            .app_data(web::Data::new(config))
            .route("/internal/state/{oid}", web::get().to(xet_server::api::internal::get_state))
    ).await;

    let req = test::TestRequest::get()
        .uri("/internal/state/abc123def456")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["state"], "raw_only");
    assert_eq!(body["size"], 1024);
}

#[actix_web::test]
async fn test_internal_get_state_not_found() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );
    let state_dir = tempdir().unwrap();
    let state_mgr = std::sync::Arc::new(
        SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
    );

    let kp = KeyPair::generate();
    let config = common::test_config_with_key(&kp);
    let token = sign_xet_token(&internal_claims(), &kp).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::from(state_mgr))
            .app_data(web::Data::new(config))
            .route("/internal/state/{oid}", web::get().to(xet_server::api::internal::get_state))
    ).await;

    let req = test::TestRequest::get()
        .uri("/internal/state/nonexistent")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn test_internal_head_blob_raw() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );
    let state_dir = tempdir().unwrap();
    let state_mgr = std::sync::Arc::new(
        SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
    );

    // Store a raw blob
    storage.put("lfs/objects/abc123", bytes::Bytes::from("test data")).await.unwrap();
    state_mgr.register_raw_blob("abc123", 9).await.unwrap();

    let kp = KeyPair::generate();
    let config = common::test_config_with_key(&kp);
    let token = sign_xet_token(&internal_claims(), &kp).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::from(state_mgr))
            .app_data(web::Data::new(config))
            .route("/internal/blob/{oid}", web::head().to(xet_server::api::internal::head_blob))
    ).await;

    let req = test::TestRequest::head()
        .uri("/internal/blob/abc123")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("X-Storage-State").unwrap().to_str().unwrap(),
        "raw_only"
    );
}

#[actix_web::test]
async fn test_internal_rejects_non_internal_scope() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );
    let state_dir = tempdir().unwrap();
    let state_mgr = std::sync::Arc::new(
        SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
    );

    let kp = KeyPair::generate();
    let config = common::test_config_with_key(&kp);

    // Token with "read" scope, not "internal"
    let claims = XetClaims {
        sub: "user".to_string(),
        scope: "read".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key".to_string(),
    };
    let token = sign_xet_token(&claims, &kp).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::from(state_mgr))
            .app_data(web::Data::new(config))
            .route("/internal/state/{oid}", web::get().to(xet_server::api::internal::get_state))
    ).await;

    let req = test::TestRequest::get()
        .uri("/internal/state/abc123")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --test test_internal_api 2>&1 | tail -10`
Expected: FAIL — `xet_server::api::internal` not found.

- [x] **Step 3: Create `src/api/internal.rs`**

```rust
//! Internal API endpoints for Hub-to-CAS communication
//!
//! These endpoints require "internal" scope tokens.

use actix_web::{web, HttpResponse};
use tracing::error;

use crate::api::auth::{check_scope, extract_token_from_request, verify_token};
use crate::config::ServerConfig;
use crate::state::StorageStateManager;
use crate::storage::StorageBackend;

/// GET /internal/state/{oid}
/// Returns the storage state for a blob.
pub async fn get_state(
    path: web::Path<String>,
    state_mgr: web::Data<std::sync::Arc<dyn StorageStateManager>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let oid = path.into_inner();

    // Auth: require "internal" scope
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Missing authorization"
        })),
    };

    let claims = match verify_token(&token, &config.auth) {
        Ok(c) => c,
        Err(_) => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid token"
        })),
    };

    if !check_scope(&claims, "internal") {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Internal scope required"
        }));
    }

    match state_mgr.get_state(&oid).await {
        Ok(Some(state)) => HttpResponse::Ok().json(serde_json::json!({
            "state": match state.state {
                crate::state::StorageState::RawOnly => "raw_only",
                crate::state::StorageState::XetOnly => "xet_only",
            },
            "xet_file_id": state.xet_file_id,
            "size": state.size,
            "sha256": state.sha256,
            "converted_at": state.converted_at,
        })),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": format!("No state found for oid: {}", oid)
        })),
        Err(e) => {
            error!("Failed to get state: {}", e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("State query failed: {}", e)
            }))
        }
    }
}

/// HEAD /internal/blob/{oid}
/// Check if a blob is accessible (raw or xet).
pub async fn head_blob(
    path: web::Path<String>,
    storage: web::Data<Box<dyn StorageBackend>>,
    state_mgr: web::Data<std::sync::Arc<dyn StorageStateManager>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let oid = path.into_inner();

    // Auth
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => return HttpResponse::Unauthorized().finish(),
    };

    let claims = match verify_token(&token, &config.auth) {
        Ok(c) => c,
        Err(_) => return HttpResponse::Unauthorized().finish(),
    };

    if !check_scope(&claims, "internal") {
        return HttpResponse::Forbidden().finish();
    }

    // Check state
    match state_mgr.get_state(&oid).await {
        Ok(Some(state)) => {
            match state.state {
                crate::state::StorageState::XetOnly => {
                    HttpResponse::Ok()
                        .append_header(("X-Storage-State", "xet_only"))
                        .append_header(("X-File-Id", state.xet_file_id.unwrap_or_default()))
                        .finish()
                }
                crate::state::StorageState::RawOnly => {
                    HttpResponse::Ok()
                        .append_header(("X-Storage-State", "raw_only"))
                        .finish()
                }
            }
        }
        Ok(None) => {
            // No state record: check if raw blob exists in storage
            let key = format!("lfs/objects/{}", oid);
            match storage.exists(&key).await {
                Ok(true) => HttpResponse::Ok()
                    .append_header(("X-Storage-State", "raw_only"))
                    .finish(),
                Ok(false) => HttpResponse::NotFound().finish(),
                Err(_) => HttpResponse::InternalServerError().finish(),
            }
        }
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}
```

- [x] **Step 4: Update `src/api/mod.rs`**

Add:
```rust
pub mod internal;
```

- [x] **Step 5: Update `src/server.rs` to register internal routes and state manager**

Add to `start_server`:

```rust
let state_mgr = std::sync::Arc::new(
    crate::state::sqlite::SqliteStateManager::new(&config.state.sqlite_path)
        .expect("Failed to create state database")
);
```

Add to the App builder (before `.route("/health", ...)`):

```rust
.app_data(web::Data::from(state_mgr.clone()))
.route("/internal/state/{oid}", web::get().to(crate::api::internal::get_state))
.route("/internal/blob/{oid}", web::head().to(crate::api::internal::head_blob))
```

- [x] **Step 6: Run tests**

Run: `cargo test --test test_internal_api 2>&1 | tail -20`
Expected: All tests PASS.

- [x] **Step 7: Commit**

```bash
git add src/api/internal.rs src/api/mod.rs src/server.rs tests/test_internal_api.rs
git commit -m "feat: add CAS internal endpoints for Hub communication"
```

---

### Task 8: State-Aware LFS Upload (Register State)

**Files:**
- Modify: `src/api/lfs.rs`

After a successful LFS upload, register the blob as `raw_only` in the state manager.

- [x] **Step 1: Update upload_lfs_object handler**

In `src/api/lfs.rs`, after the line:

```rust
info!("Uploaded LFS object {} ({} bytes)", oid, total_bytes);
```

Add:

```rust
// Register as raw_only in state manager
let state_mgr = req.app_data::<web::Data<std::sync::Arc<dyn crate::state::StorageStateManager>>>()
    .expect("StateManager not configured");
if let Err(e) = state_mgr.register_raw_blob(&oid, total_bytes).await {
    tracing::error!("Failed to register state for {}: {}", oid, e);
    // Non-fatal: file is stored, state tracking can be repaired
}
```

Also add the import at the top of lfs.rs:

```rust
use crate::state::StorageStateManager;
```

- [x] **Step 2: Verify existing tests still pass**

Run: `cargo test --test test_streaming_upload 2>&1 | tail -10`
Expected: PASS (the state manager is optional in tests — if not present in app_data, it will panic. We need to handle this gracefully.)

Actually, let me handle the case where state_mgr is not configured (for backward compat in tests):

Replace the unwrap with:

```rust
if let Some(state_mgr) = req.app_data::<web::Data<std::sync::Arc<dyn crate::state::StorageStateManager>>>() {
    if let Err(e) = state_mgr.register_raw_blob(&oid, total_bytes).await {
        tracing::warn!("Failed to register state for {}: {}", oid, e);
    }
}
```

- [x] **Step 3: Write test for state registration**

Add to `tests/test_streaming_upload.rs` (or create a new test):

```rust
#[actix_web::test]
async fn test_lfs_upload_registers_state() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );
    let state_dir = tempdir().unwrap();
    let state_mgr = std::sync::Arc::new(
        SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
    );

    let kp = KeyPair::generate();
    let config = common::test_config_with_key(&kp);
    let claims = XetClaims {
        sub: "test".to_string(),
        scope: "write".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key".to_string(),
    };
    let token = sign_xet_token(&claims, &kp).unwrap();

    // Compute BLAKE3 hash of test data
    let data = b"test lfs data for state registration";
    let hash = blake3::hash(data).to_hex().to_string();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::from(state_mgr.clone()))
            .app_data(web::Data::new(config))
            .route("/lfs/objects/{oid}", web::put().to(xet_server::api::lfs::upload_lfs_object))
    ).await;

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(data.to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify state was registered
    let state = state_mgr.get_state(&hash).await.unwrap().unwrap();
    assert_eq!(state.state, xet_server::state::StorageState::RawOnly);
    assert_eq!(state.size, data.len() as u64);
}
```

- [x] **Step 4: Run all tests**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests PASS.

- [x] **Step 5: Commit**

```bash
git add src/api/lfs.rs tests/test_streaming_upload.rs
git commit -m "feat: register raw_only state on LFS upload"
```

---

### Task 9: State-Aware LFS Download with Reconstruction

**Files:**
- Modify: `src/api/lfs.rs`

Modify `download_lfs_object` to check state and serve from raw blob or reconstruction.

- [x] **Step 1: Write failing test for reconstruction download**

Add to `tests/test_streaming_upload.rs` or create new test:

```rust
#[actix_web::test]
async fn test_lfs_download_raw_only() {
    // Upload a blob, then download it — should serve from raw
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );
    let state_dir = tempdir().unwrap();
    let state_mgr = std::sync::Arc::new(
        SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
    );

    let kp = KeyPair::generate();
    let config = common::test_config_with_key(&kp);

    let data = b"download test data";
    let hash = blake3::hash(data).to_hex().to_string();

    // Pre-populate storage and state
    storage.put(&format!("lfs/objects/{}", hash), Bytes::from(data.to_vec())).await.unwrap();
    state_mgr.register_raw_blob(&hash, data.len() as u64).await.unwrap();

    let read_claims = XetClaims {
        sub: "test".to_string(),
        scope: "read".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 1000000000,
        kid: "test-key".to_string(),
    };
    let token = sign_xet_token(&read_claims, &kp).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::from(state_mgr))
            .app_data(web::Data::new(config))
            .route("/lfs/objects/{oid}", web::get().to(xet_server::api::lfs::download_lfs_object))
    ).await;

    let req = test::TestRequest::get()
        .uri(&format!("/lfs/objects/{}", hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body = test::read_body(resp).await;
    assert_eq!(body.as_ref(), data);
}
```

- [x] **Step 2: Modify download_lfs_object**

In `src/api/lfs.rs`, modify the `download_lfs_object` function. After auth validation, before fetching from storage, add state check:

```rust
pub async fn download_lfs_object(
    path: web::Path<String>,
    storage: web::Data<Box<dyn StorageBackend>>,
    state_mgr: web::Data<std::sync::Arc<dyn StorageStateManager>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // ... (existing validation code for oid format, auth, scope — keep as is)

    // Check storage state
    let state = match state_mgr.get_state(&oid).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            // No state record — try raw blob directly (backward compat)
            let object_key = format!("lfs/objects/{}", oid);
            match storage.get(&object_key).await {
                Ok(data) => {
                    GLOBAL_METRICS.record_request(200);
                    GLOBAL_METRICS.record_download_bytes(data.len() as u64);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(data);
                }
                Err(StorageError::NotFound(_)) => {
                    GLOBAL_METRICS.record_request(404);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Object not found: {}", oid)
                    }));
                }
                Err(e) => {
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Storage error: {}", e)
                    }));
                }
            }
        }
        Err(e) => {
            error!("Failed to get state: {}", e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("State query failed: {}", e)
            }));
        }
    };

    match state.state {
        crate::state::StorageState::RawOnly => {
            // Serve from raw blob
            let object_key = format!("lfs/objects/{}", oid);
            match storage.get(&object_key).await {
                Ok(data) => {
                    info!("Downloaded LFS object {} ({} bytes, raw)", oid, data.len());
                    GLOBAL_METRICS.record_request(200);
                    GLOBAL_METRICS.record_storage_operation();
                    GLOBAL_METRICS.record_download_bytes(data.len() as u64);
                    GLOBAL_METRICS.record_latency(start);
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(data)
                }
                Err(StorageError::NotFound(_)) => {
                    GLOBAL_METRICS.record_request(404);
                    GLOBAL_METRICS.record_latency(start);
                    HttpResponse::NotFound().json(serde_json::json!({
                        "error": "Raw blob missing"
                    }))
                }
                Err(e) => {
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_latency(start);
                    HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Storage error: {}", e)
                    }))
                }
            }
        }
        crate::state::StorageState::XetOnly => {
            // Serve via reconstruction
            let file_id = match state.xet_file_id {
                Some(fid) => fid,
                None => {
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": "XET_ONLY state but no file_id"
                    }));
                }
            };

            // Read shard metadata and reconstruct
            let shard_key = format!("shards/{}", file_id);
            let shard_data = match storage.get(&shard_key).await {
                Ok(d) => d,
                Err(e) => {
                    error!("Failed to read shard {}: {}", shard_key, e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Failed to read shard: {}", e)
                    }));
                }
            };

            let shard = match crate::format::shard::MDBShardFile::from_bytes(&shard_data) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to parse shard {}: {}", shard_key, e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_latency(start);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Failed to parse shard: {}", e)
                    }));
                }
            };

            // Reconstruct file from xorbs
            match reconstruct_file(&storage, &shard).await {
                Ok(data) => {
                    info!("Downloaded LFS object {} ({} bytes, reconstructed)", oid, data.len());
                    GLOBAL_METRICS.record_request(200);
                    GLOBAL_METRICS.record_download_bytes(data.len() as u64);
                    GLOBAL_METRICS.record_latency(start);
                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(data)
                }
                Err(e) => {
                    error!("Reconstruction failed for {}: {}", oid, e);
                    GLOBAL_METRICS.record_request(500);
                    GLOBAL_METRICS.record_latency(start);
                    HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Reconstruction failed: {}", e)
                    }))
                }
            }
        }
    }
}
```

Add the reconstruction helper function at the end of the file:

```rust
/// Reconstruct a file from its shard metadata by reading and assembling xorbs.
async fn reconstruct_file(
    storage: &Box<dyn StorageBackend>,
    shard: &crate::format::shard::MDBShardFile,
) -> Result<bytes::Bytes, String> {
    // Get the file's chunk list from the shard
    // The shard contains mappings from file hashes to chunk locations
    // For now, use the shard's reconstruction info to find xorbs

    // This is a simplified reconstruction that reads the shard's file info
    // and assembles the data from the referenced xorbs.
    let file_info = shard.get_file_reconstruction_info(&shard.file_hash().to_hex())
        .ok_or_else(|| "File not found in shard".to_string())?;

    let mut result = Vec::new();

    for chunk_info in &file_info.chunk_infos {
        // Read the xorb containing this chunk
        let xorb_hash = chunk_info.xorb_hash.to_hex();
        let prefix = &xorb_hash[..2];
        let xorb_key = format!("xorbs/{}/{}", prefix, xorb_hash);

        let xorb_data = storage.get(&xorb_key).await
            .map_err(|e| format!("Failed to read xorb {}: {}", xorb_hash, e))?;

        // Extract the chunk from the xorb
        // The xorb format: [chunk_data...][footer]
        // For reconstruction, we need the chunk at the specified offset
        let chunk_start = chunk_info.xorb_offset as usize;
        let chunk_end = chunk_start + chunk_info.chunk_length as usize;

        if chunk_end > xorb_data.len() {
            return Err(format!(
                "Chunk extends beyond xorb: offset={} length={} xorb_size={}",
                chunk_start, chunk_info.chunk_length, xorb_data.len()
            ));
        }

        result.extend_from_slice(&xorb_data[chunk_start..chunk_end]);
    }

    Ok(bytes::Bytes::from(result))
}
```

Note: The exact shard API (`get_file_reconstruction_info`, chunk_info fields) needs to match the existing `MDBShardFile` implementation. Check `src/format/shard.rs` for the actual field names and adjust accordingly.

- [x] **Step 3: Run tests**

Run: `cargo test --test test_streaming_upload 2>&1 | tail -20`
Expected: `test_lfs_download_raw_only` PASS. Other tests may need adjustment if the handler signature changed (added `state_mgr` parameter — existing tests that don't provide it will fail to initialize the service).

For existing tests that don't provide `state_mgr`, add a state manager to their test setup:

```rust
let state_dir = tempdir().unwrap();
let state_mgr = std::sync::Arc::new(
    SqliteStateManager::new(state_dir.path().join("states.db").to_str().unwrap()).unwrap()
);
// Add to App::new():
.app_data(web::Data::from(state_mgr))
```

- [x] **Step 4: Run full test suite**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests PASS.

- [x] **Step 5: Commit**

```bash
git add src/api/lfs.rs tests/test_streaming_upload.rs
git commit -m "feat: state-aware LFS download with reconstruction support"
```

---

### Task 10: Final Integration Verification

- [x] **Step 1: Run full test suite**

Run: `cargo test 2>&1`
Expected: All tests PASS.

- [x] **Step 2: Run clippy**

Run: `cargo clippy 2>&1 | grep -E "error|warning" | head -20`
Expected: No errors. Fix any new warnings.

- [x] **Step 3: Verify server starts**

Generate test keys and start the server:

```bash
# Create a test keypair (using a small Rust program or openssl)
mkdir -p /tmp/xet-test
# Generate Ed25519 key pair using openssl
openssl genpkey -algorithm Ed25519 -out /tmp/xet-test/hub_private.pem
openssl pkey -in /tmp/xet-test/hub_private.pem -pubout -out /tmp/xet-test/hub_public.pem

# Start CAS with test config
CAS_PUBLIC_KEY_PATH=/tmp/xet-test/hub_public.pem \
CAS_TRUSTED_KIDS=test-key \
XET_STORAGE_BACKEND=local \
XET_LOCAL_PATH=/tmp/xet-test/storage \
CAS_STATE_DB_PATH=/tmp/xet-test/states.db \
cargo run 2>&1 &

# Verify health endpoint
curl http://127.0.0.1:8080/health
# Expected: {"status":"ok"}

# Clean up
kill %1 2>/dev/null
```

- [x] **Step 4: Commit any fixes**

```bash
git add -A
git commit -m "fix: resolve clippy warnings and integration issues"
```

---

## Summary

After completing this plan, the CAS (xet-server) will have:

1. **Ed25519 token auth** — Replaces HMAC JWT. Tokens are signed by Hub's private key, verified by CAS's public key. Supports `read`, `write`, `internal` scopes and key rotation via `kid`.

2. **Storage state tracking** — SQLite database tracks whether each blob is `raw_only` or `xet_only`. This is the source of truth for how to serve downloads.

3. **Internal endpoints** — `GET /internal/state/{oid}` and `HEAD /internal/blob/{oid}` for Hub-to-CAS communication. Require `internal` scope.

4. **State-aware LFS** — Upload registers state as `raw_only`. Download checks state: serves raw blob for `raw_only`, performs reconstruction for `xet_only`.

The Hub API service (Plan 2) will build on top of this foundation.
