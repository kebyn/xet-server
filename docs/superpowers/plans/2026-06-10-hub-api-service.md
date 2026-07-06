# Hub API Service Implementation Plan

**Status:** ✅ Completed  
**Date:** 2026-06-10  
**Implemented:** 2026-06-12  

> **Superseded auth note:** This historical plan predates the current Hub -> CAS token boundary. Any steps where Hub uses an `internal_xxx` token for CAS public object endpoints (`/objects/batch` or `/lfs/objects/{oid}`), or where Hub LFS object endpoints validate `hf_xxx` directly, are superseded. Current code uses short-lived `xet_xxx` user tokens for Hub -> CAS public batch/commit-inline/resolve-inline calls, forwards validated `proxy_xxx` tokens for Hub -> CAS LFS object proxy calls, and reserves `internal_xxx` for `/internal/*` service endpoints.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a new Rust HTTP service implementing HuggingFace Hub REST API compatibility, enabling `hf upload/download` and `git lfs push/pull` against a private deployment backed by the existing CAS (xet-server).

**Architecture:** The Hub API is a new crate (`hub/`) in a Cargo workspace alongside the existing CAS (`xet-server`). It manages structure metadata (repos, revisions, file trees) in SQLite, authenticates users via `hf_` tokens, issues Ed25519-signed `xet_` tokens for CAS access, and proxies Git LFS operations to CAS. The Hub communicates with CAS via internal HTTP endpoints (already implemented in Plan 1).

**Tech Stack:** Rust, actix-web, ed25519-dalek, rusqlite (bundled), reqwest (HTTP client), serde, tokio, sha2

**Spec:** `docs/superpowers/specs/2026-06-10-hf-hub-api-design.md`
**Dependency:** Plan 1 (CAS Modifications) — completed on branch `feat/cas-modifications-hf-hub-api`

---

## File Structure

```
/data/
├── Cargo.toml                    # Modified: add [workspace] with members
├── hub/
│   ├── Cargo.toml                # New: hub crate dependencies
│   └── src/
│       ├── main.rs               # New: entry point
│       ├── config.rs             # New: Hub configuration
│       ├── server.rs             # New: actix-web server setup + routes
│       ├── error.rs              # New: Hub error types
│       ├── auth/
│       │   ├── mod.rs            # New: auth module root
│       │   ├── token_store.rs    # New: hf_ token validation (SQLite)
│       │   └── xet_signer.rs     # New: Ed25519 xet_ token signing
│       ├── metadata/
│       │   ├── mod.rs            # New: MetadataStore trait + types
│       │   └── sqlite.rs         # New: SQLite implementation
│       ├── cas_client/
│       │   └── mod.rs            # New: HTTP client for CAS internal endpoints
│       └── api/
│           ├── mod.rs            # New: API module root
│           ├── whoami.rs         # New: GET /api/whoami-v2
│           ├── token_exchange.rs # New: GET /api/{type}s/{ns}/{repo}/xet-{read|write}-token/{rev}
│           ├── repo.rs           # New: POST/GET/DELETE /api/{type}s
│           ├── commit.rs         # New: POST /api/{type}s/{ns}/{repo}/commit/{rev}
│           ├── tree.rs           # New: GET /api/{type}s/{ns}/{repo}/tree/{rev}/{path}
│           ├── resolve.rs        # New: GET /{ns}/{repo}/resolve/{rev}/{path}
│           └── lfs_proxy.rs      # New: POST /objects/batch, PUT/GET /lfs/objects/{oid}
```

---

### Task 1: Workspace Setup and Hub Crate Scaffold

**Files:**
- Modify: `/data/Cargo.toml`
- Create: `/data/hub/Cargo.toml`
- Create: `/data/hub/src/main.rs`
- Create: `/data/hub/src/config.rs`

- [x] **Step 1: Convert to Cargo workspace**

Modify `/data/Cargo.toml` — add workspace section at the top (before `[package]`):

```toml
[workspace]
members = [".", "hub"]
resolver = "2"

[package]
name = "xet-server"
# ... rest unchanged
```

- [x] **Step 2: Create hub/Cargo.toml**

```toml
[package]
name = "hub-api"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "hub-api"
path = "src/main.rs"

[dependencies]
actix-web = "4.5"
tokio = { version = "1.36", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
rusqlite = { version = "0.31", features = ["bundled"] }
ed25519-dalek = { version = "2", features = ["pem", "rand_core", "pkcs8"] }
pkcs8 = { version = "0.10", features = ["pem"] }
base64 = "0.22"
sha2 = "0.10"
hex = "0.4"
reqwest = { version = "0.12", features = ["json"] }
tracing = "0.1"
tracing-subscriber = "0.3"
thiserror = "2.0"
async-trait = "0.1"
uuid = { version = "1", features = ["v4"] }
rand = "0.8"

[dev-dependencies]
tempfile = "3.10"
```

- [x] **Step 3: Create hub/src/main.rs**

```rust
use hub_api::config::HubConfig;
use hub_api::server::start_server;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();
    let config = HubConfig::from_env();
    start_server(config).await
}
```

- [x] **Step 4: Create hub/src/config.rs**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubConfig {
    pub server: ServerSettings,
    pub auth: AuthSettings,
    pub metadata: MetadataSettings,
    pub cas: CasSettings,
    pub storage: StorageSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_base_url: Option<String>,
}

impl ServerSettings {
    pub fn base_url(&self) -> String {
        self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
            .trim_end_matches('/').to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSettings {
    pub private_key_path: String,
    pub kid: String,
    pub token_ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataSettings {
    pub sqlite_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasSettings {
    pub base_url: String,
    pub internal_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    pub inline_threshold_bytes: u64,
    pub lfs_threshold_bytes: u64,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8080,
                public_base_url: None,
            },
            auth: AuthSettings {
                private_key_path: String::new(),
                kid: "hub-key-001".to_string(),
                token_ttl_seconds: 3600,
            },
            metadata: MetadataSettings {
                sqlite_path: "./data/hub_metadata.db".to_string(),
            },
            cas: CasSettings {
                base_url: "http://127.0.0.1:9090".to_string(),
                internal_timeout_seconds: 30,
            },
            storage: StorageSettings {
                inline_threshold_bytes: 1_048_576,      // 1 MB
                lfs_threshold_bytes: 10_485_760,         // 10 MB
            },
        }
    }
}

impl HubConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(v) = std::env::var("HUB_HOST") { config.server.host = v; }
        if let Ok(v) = std::env::var("HUB_PORT") {
            if let Ok(p) = v.parse() { config.server.port = p; }
        }
        if let Ok(v) = std::env::var("HUB_PUBLIC_BASE_URL") { config.server.public_base_url = Some(v); }
        if let Ok(v) = std::env::var("HUB_PRIVATE_KEY_PATH") { config.auth.private_key_path = v; }
        if let Ok(v) = std::env::var("HUB_KID") { config.auth.kid = v; }
        if let Ok(v) = std::env::var("HUB_TOKEN_TTL_SECONDS") {
            if let Ok(t) = v.parse() { config.auth.token_ttl_seconds = t; }
        }
        if let Ok(v) = std::env::var("HUB_SQLITE_PATH") { config.metadata.sqlite_path = v; }
        if let Ok(v) = std::env::var("CAS_BASE_URL") { config.cas.base_url = v; }
        if let Ok(v) = std::env::var("CAS_INTERNAL_TIMEOUT_SECONDS") {
            if let Ok(t) = v.parse() { config.cas.internal_timeout_seconds = t; }
        }
        if let Ok(v) = std::env::var("HUB_INLINE_THRESHOLD") {
            if let Ok(t) = v.parse() { config.storage.inline_threshold_bytes = t; }
        }
        if let Ok(v) = std::env::var("HUB_LFS_THRESHOLD") {
            if let Ok(t) = v.parse() { config.storage.lfs_threshold_bytes = t; }
        }
        config
    }
}
```

- [x] **Step 5: Create hub/src/lib.rs**

```rust
pub mod config;
pub mod server;
pub mod error;
pub mod auth;
pub mod metadata;
pub mod cas_client;
pub mod api;
```

- [x] **Step 6: Create stub modules**

Create empty files so the crate compiles:
- `hub/src/server.rs`: `pub async fn start_server(_config: crate::config::HubConfig) -> std::io::Result<()> { Ok(()) }`
- `hub/src/error.rs`: (empty for now)
- `hub/src/auth/mod.rs`: `pub mod token_store; pub mod xet_signer;`
- `hub/src/auth/token_store.rs`: (empty)
- `hub/src/auth/xet_signer.rs`: (empty)
- `hub/src/metadata/mod.rs`: `pub mod sqlite;`
- `hub/src/metadata/sqlite.rs`: (empty)
- `hub/src/cas_client/mod.rs`: (empty)
- `hub/src/api/mod.rs`: (empty)

- [x] **Step 7: Verify workspace compiles**

Run: `cd /data && cargo check -p hub-api 2>&1 | tail -5`
Expected: Compiles (may have warnings about unused imports in stubs).

Also verify CAS still compiles: `cargo check -p xet-server 2>&1 | tail -5`

- [x] **Step 8: Commit**

```bash
git add Cargo.toml hub/
git commit -m "feat(hub): scaffold Hub API crate in workspace"
```

---

### Task 2: Hub Error Types

**Files:**
- Modify: `hub/src/error.rs`

- [x] **Step 1: Implement Hub error types**

```rust
use actix_web::{HttpResponse, http::StatusCode};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Already exists: {0}")]
    Conflict(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Unprocessable: {0}")]
    Unprocessable(String),

    #[error("CAS error: {0}")]
    CasError(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    error_type: String,
}

impl HubError {
    pub fn error_type(&self) -> &'static str {
        match self {
            HubError::NotFound(_) => "NotFoundError",
            HubError::Conflict(_) => "ConflictError",
            HubError::Unauthorized(_) => "AuthenticationError",
            HubError::Forbidden(_) => "AuthorizationError",
            HubError::BadRequest(_) => "ValidationError",
            HubError::Unprocessable(_) => "UnprocessableEntity",
            HubError::CasError(_) => "BadGateway",
            HubError::Internal(_) => "InternalError",
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            HubError::NotFound(_) => StatusCode::NOT_FOUND,
            HubError::Conflict(_) => StatusCode::CONFLICT,
            HubError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            HubError::Forbidden(_) => StatusCode::FORBIDDEN,
            HubError::BadRequest(_) => StatusCode::BAD_REQUEST,
            HubError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            HubError::CasError(_) => StatusCode::BAD_GATEWAY,
            HubError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn into_response(self) -> HttpResponse {
        let status = self.status_code();
        HttpResponse::build(status).json(ErrorBody {
            error: self.to_string(),
            error_type: self.error_type().to_string(),
        })
    }
}

impl actix_web::ResponseError for HubError {
    fn error_response(&self) -> HttpResponse {
        let status = self.status_code();
        HttpResponse::build(status).json(ErrorBody {
            error: self.to_string(),
            error_type: self.error_type().to_string(),
        })
    }
}

impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        HubError::Internal(format!("Database error: {}", e))
    }
}

impl From<reqwest::Error> for HubError {
    fn from(e: reqwest::Error) -> Self {
        HubError::CasError(format!("CAS request failed: {}", e))
    }
}
```

- [x] **Step 2: Commit**

```bash
git add hub/src/error.rs
git commit -m "feat(hub): add Hub error types with HF-compatible responses"
```

---

### Task 3: MetadataStore Trait and SQLite Implementation

**Files:**
- Create: `hub/src/metadata/mod.rs`
- Create: `hub/src/metadata/sqlite.rs`
- Test: `hub/tests/test_metadata.rs`

- [x] **Step 1: Define types and trait in hub/src/metadata/mod.rs**

```rust
pub mod sqlite;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RepoType {
    Model,
    Dataset,
    Space,
}

impl RepoType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RepoType::Model => "model",
            RepoType::Dataset => "dataset",
            RepoType::Space => "space",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "model" => Some(RepoType::Model),
            "dataset" => Some(RepoType::Dataset),
            "space" => Some(RepoType::Space),
            _ => None,
        }
    }

    pub fn api_prefix(&self) -> &'static str {
        match self {
            RepoType::Model => "models",
            RepoType::Dataset => "datasets",
            RepoType::Space => "spaces",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub repo_id: String,       // "namespace/repo-name"
    pub repo_type: RepoType,
    pub namespace: String,
    pub name: String,
    pub private: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub rev_id: String,
    pub repo_id: String,
    pub parent_id: Option<String>,
    pub message: String,
    pub author: String,
    pub branch: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub rev_id: String,
    pub path: String,
    pub blob_oid: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Already exists: {0}")]
    AlreadyExists(String),
}

impl From<rusqlite::Error> for MetadataError {
    fn from(e: rusqlite::Error) -> Self {
        MetadataError::Database(e.to_string())
    }
}

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_repo(&self, repo: &Repo) -> Result<(), MetadataError>;
    async fn get_repo(&self, repo_id: &str) -> Result<Option<Repo>, MetadataError>;
    async fn delete_repo(&self, repo_id: &str) -> Result<(), MetadataError>;

    async fn add_revision(&self, rev: &Revision) -> Result<(), MetadataError>;
    async fn get_revision(&self, rev_id: &str) -> Result<Option<Revision>, MetadataError>;
    async fn get_head(&self, repo_id: &str, branch: &str) -> Result<Option<String>, MetadataError>;
    async fn set_head(&self, repo_id: &str, branch: &str, rev_id: &str) -> Result<(), MetadataError>;
    async fn get_commit_log(&self, repo_id: &str, branch: &str, limit: u32) -> Result<Vec<Revision>, MetadataError>;

    async fn add_file_entries(&self, rev_id: &str, entries: &[FileEntry]) -> Result<(), MetadataError>;
    async fn get_file_tree(&self, rev_id: &str) -> Result<Vec<FileEntry>, MetadataError>;
    async fn get_file_tree_prefix(&self, rev_id: &str, prefix: &str) -> Result<Vec<FileEntry>, MetadataError>;
    async fn resolve_file(&self, rev_id: &str, path: &str) -> Result<Option<FileEntry>, MetadataError>;
}
```

- [x] **Step 2: Implement SQLiteMetadataStore in hub/src/metadata/sqlite.rs**

Implement all trait methods using `rusqlite` with `Mutex<Connection>`. Key schema:

```sql
CREATE TABLE IF NOT EXISTS repos (
    repo_id      TEXT PRIMARY KEY,
    repo_type    TEXT NOT NULL,
    namespace    TEXT NOT NULL,
    name         TEXT NOT NULL,
    private      INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE(namespace, name, repo_type)
);

CREATE TABLE IF NOT EXISTS revisions (
    rev_id       TEXT PRIMARY KEY,
    repo_id      TEXT NOT NULL,
    parent_id    TEXT,
    message      TEXT NOT NULL,
    author       TEXT NOT NULL,
    branch       TEXT NOT NULL DEFAULT 'main',
    created_at   INTEGER NOT NULL,
    FOREIGN KEY (repo_id) REFERENCES repos(repo_id)
);

CREATE INDEX IF NOT EXISTS idx_revisions_repo_branch ON revisions(repo_id, branch);

CREATE TABLE IF NOT EXISTS file_tree (
    rev_id       TEXT NOT NULL,
    path         TEXT NOT NULL,
    blob_oid     TEXT NOT NULL,
    size         INTEGER NOT NULL,
    PRIMARY KEY (rev_id, path),
    FOREIGN KEY (rev_id) REFERENCES revisions(rev_id)
);

CREATE TABLE IF NOT EXISTS heads (
    repo_id      TEXT NOT NULL,
    branch       TEXT NOT NULL,
    rev_id       TEXT NOT NULL,
    PRIMARY KEY (repo_id, branch),
    FOREIGN KEY (repo_id) REFERENCES repos(repo_id)
);
```

Key implementation notes:
- `create_repo`: INSERT, return `AlreadyExists` on UNIQUE constraint violation
- `get_head`/`set_head`: Use the `heads` table (separate from revisions)
- `get_commit_log`: Walk parent_id chain from HEAD, limited by `limit`
- `get_file_tree_prefix`: WHERE `path LIKE '{prefix}%'`
- `resolve_file`: Single row lookup by (rev_id, path)

- [x] **Step 3: Write tests in hub/tests/test_metadata.rs**

Test at minimum:
1. `test_create_and_get_repo`
2. `test_create_duplicate_repo`
3. `test_delete_repo`
4. `test_add_and_get_revision`
5. `test_head_management`
6. `test_file_tree_operations`
7. `test_file_tree_prefix_filter`
8. `test_resolve_file`
9. `test_commit_log`

- [x] **Step 4: Verify tests pass and commit**

```bash
cargo test -p hub-api --test test_metadata
git add hub/src/metadata/ hub/tests/test_metadata.rs
git commit -m "feat(hub): add MetadataStore trait and SQLite implementation"
```

---

### Task 4: Hub Auth — Token Store and whoami

**Files:**
- Create: `hub/src/auth/token_store.rs`
- Modify: `hub/src/auth/mod.rs`
- Create: `hub/src/api/whoami.rs`
- Test: `hub/tests/test_token_store.rs`

- [x] **Step 1: Implement TokenStore in hub/src/auth/token_store.rs**

```rust
use rusqlite::{Connection, params};
use sha2::{Sha256, Digest};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub user_id: String,
    pub username: String,
    pub token_name: String,
    pub scope: String,
}

pub struct TokenStore {
    conn: Mutex<Connection>,
}

impl TokenStore {
    pub fn new(db_path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                user_id   TEXT PRIMARY KEY,
                username  TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tokens (
                token_hash  TEXT PRIMARY KEY,
                user_id     TEXT NOT NULL,
                name        TEXT NOT NULL,
                scope       TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                expires_at  INTEGER,
                revoked_at  INTEGER,
                FOREIGN KEY (user_id) REFERENCES users(user_id)
            );
            CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id);"
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Create a user and token (for admin setup). Returns the plaintext token.
    pub fn create_token(&self, username: &str, token_name: &str, scope: &str) -> Result<String, rusqlite::Error> {
        let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let token_hash = Self::hash_token(&token);
        let now = now_secs();
        let user_id = format!("user_{}", &token_hash[..16]);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO users (user_id, username, created_at) VALUES (?1, ?2, ?3)",
            params![user_id, username, now as i64],
        )?;
        conn.execute(
            "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token_hash, user_id, token_name, scope, now as i64],
        )?;
        Ok(token)
    }

    /// Validate a token. Returns TokenInfo if valid.
    pub fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, rusqlite::Error> {
        let token_hash = Self::hash_token(token);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT u.user_id, u.username, t.name, t.scope, t.expires_at, t.revoked_at
             FROM tokens t JOIN users u ON t.user_id = u.user_id
             WHERE t.token_hash = ?1"
        )?;

        let result = stmt.query_row(params![token_hash], |row| {
            let expires_at: Option<i64> = row.get(4)?;
            let revoked_at: Option<i64> = row.get(5)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                expires_at,
                revoked_at,
            ))
        });

        match result {
            Ok((user_id, username, name, scope, expires_at, revoked_at)) => {
                let now = now_secs() as i64;
                if revoked_at.is_some() { return Ok(None); }
                if let Some(exp) = expires_at {
                    if exp < now { return Ok(None); }
                }
                Ok(Some(TokenInfo {
                    user_id, username, token_name: name, scope,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn hash_token(token: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hex::encode(hasher.finalize())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
```

- [x] **Step 2: Write tests in hub/tests/test_token_store.rs**

Test: create_token, validate_token, expired token, revoked token, invalid token.

- [x] **Step 3: Implement whoami handler in hub/src/api/whoami.rs**

```rust
use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;

pub async fn whoami(
    req: HttpRequest,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
) -> HttpResponse {
    let token = match extract_bearer_token(&req) {
        Some(t) => t,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Missing or invalid authorization",
            "error_type": "AuthenticationError"
        })),
    };

    match token_store.validate_token(&token) {
        Ok(Some(info)) => HttpResponse::Ok().json(serde_json::json!({
            "name": info.username,
            "email": "",
            "orgs": [],
            "auth": {
                "type": "access_token",
                "accessToken": {
                    "name": info.token_name,
                    "role": info.scope
                }
            }
        })),
        Ok(None) => HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid token",
            "error_type": "AuthenticationError"
        })),
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Token validation error: {}", e),
            "error_type": "InternalError"
        })),
    }
}

fn extract_bearer_token(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    let auth_str = auth.to_str().ok()?;
    auth_str.strip_prefix("Bearer ").map(|s| s.to_string())
}
```

- [x] **Step 4: Commit**

```bash
git add hub/src/auth/ hub/src/api/whoami.rs hub/tests/test_token_store.rs
git commit -m "feat(hub): add token store and whoami endpoint"
```

---

### Task 5: Xet Token Signer and Token Exchange

**Files:**
- Create: `hub/src/auth/xet_signer.rs`
- Create: `hub/src/api/token_exchange.rs`

- [x] **Step 1: Implement XetSigner in hub/src/auth/xet_signer.rs**

The signer must produce tokens compatible with CAS verification (same format as Plan 1's `sign_xet_token`).

```rust
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
    kid: String,
}

#[derive(Debug, Serialize)]
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

pub struct XetSigner {
    signing_key: SigningKey,
    kid: String,
    ttl_seconds: u64,
}

impl XetSigner {
    pub fn from_pem(pem_bytes: &[u8], kid: &str, ttl_seconds: u64) -> Result<Self, String> {
        let signing_key = load_private_key_pem(pem_bytes)?;
        Ok(Self { signing_key, kid: kid.to_string(), ttl_seconds })
    }

    pub fn sign(&self, sub: &str, scope: &str, repo_id: &str, repo_type: &str, revision: &str) -> (String, u64) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let exp = now + self.ttl_seconds;

        let claims = XetClaims {
            sub: sub.to_string(),
            scope: scope.to_string(),
            repo_id: repo_id.to_string(),
            repo_type: repo_type.to_string(),
            revision: revision.to_string(),
            exp,
            iat: now,
            kid: self.kid.clone(),
        };

        let header = JwtHeader { alg: "EdDSA", typ: "JWT", kid: self.kid.clone() };
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let signing_input = format!("{}.{}", header_b64, claims_b64);
        let signature = self.signing_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        (format!("xet_{}.{}", signing_input, sig_b64), exp)
    }
}

fn load_private_key_pem(pem_bytes: &[u8]) -> Result<SigningKey, String> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    SigningKey::from_pkcs8_pem(std::str::from_utf8(pem_bytes).map_err(|e| e.to_string())?)
        .map_err(|e| format!("Failed to load private key: {}", e))
}
```

- [x] **Step 2: Implement token_exchange handler in hub/src/api/token_exchange.rs**

Handles `GET /api/{type}s/{ns}/{repo}/xet-{read|write}-token/{rev}` for all three repo types.

Two handler functions (one for read, one for write), both sharing logic via a helper:

```rust
use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::config::HubConfig;
use crate::metadata::{MetadataStore, RepoType};

/// Shared logic for token exchange
async fn exchange_token(
    ns: &str,
    repo: &str,
    revision: &str,
    scope: &str,           // "read" or "write"
    repo_type: RepoType,
    req: &HttpRequest,
    token_store: &TokenStore,
    signer: &XetSigner,
    metadata: &dyn MetadataStore,
    cas_base_url: &str,
) -> HttpResponse {
    // Extract and validate hf_ token
    let bearer = match req.headers().get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        Some(t) => t.to_string(),
        None => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Missing authorization", "error_type": "AuthenticationError"
        })),
    };

    let token_info = match token_store.validate_token(&bearer) {
        Ok(Some(info)) => info,
        Ok(None) => return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid token", "error_type": "AuthenticationError"
        })),
        Err(_) => return HttpResponse::InternalServerError().finish(),
    };

    // Check user has required scope
    if scope == "write" && !token_info.scope.contains("write") {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Write scope required", "error_type": "AuthorizationError"
        }));
    }

    let repo_id = format!("{}/{}", ns, repo);

    // Check repo exists
    match metadata.get_repo(&repo_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return HttpResponse::NotFound().json(serde_json::json!({
            "error": format!("Repository not found: {}", repo_id), "error_type": "NotFoundError"
        })),
        Err(_) => return HttpResponse::InternalServerError().finish(),
    }

    // Sign xet token
    let (access_token, exp) = signer.sign(
        &token_info.user_id, scope, &repo_id, repo_type.as_str(), revision,
    );

    HttpResponse::Ok().json(serde_json::json!({
        "accessToken": access_token,
        "exp": exp,
        "casUrl": cas_base_url,
    }))
}

// Thin wrappers for each route
pub async fn xet_read_token_models(
    path: web::Path<(String, String, String)>,
    req: HttpRequest,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    let (ns, repo, rev) = path.into_inner();
    exchange_token(&ns, &repo, &rev, "read", RepoType::Model, &req, &token_store, &signer, metadata.as_ref(), &config.cas.base_url).await
}

pub async fn xet_write_token_models(
    path: web::Path<(String, String, String)>,
    req: HttpRequest,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    let (ns, repo, rev) = path.into_inner();
    exchange_token(&ns, &repo, &rev, "write", RepoType::Model, &req, &token_store, &signer, metadata.as_ref(), &config.cas.base_url).await
}

// Same pattern for datasets and spaces:
// xet_read_token_datasets, xet_write_token_datasets
// xet_read_token_spaces, xet_write_token_spaces
// (Identical except RepoType::Dataset / RepoType::Space)
```

Routes in server.rs (Task 11) use explicit paths:
```rust
.route("/api/models/{ns}/{repo}/xet-read-token/{rev}", web::get().to(xet_read_token_models))
.route("/api/models/{ns}/{repo}/xet-write-token/{rev}", web::get().to(xet_write_token_models))
// same for datasets and spaces
```

- [x] **Step 3: Write tests and commit**

Test: sign and verify token format, token exchange returns valid response, wrong scope rejected.

```bash
git add hub/src/auth/xet_signer.rs hub/src/api/token_exchange.rs hub/tests/test_xet_signer.rs
git commit -m "feat(hub): add xet token signer and token exchange endpoint"
```

---

### Task 6: CAS Client (HTTP)

**Files:**
- Create: `hub/src/cas_client/mod.rs`

- [x] **Step 1: Implement CAS HTTP client**

```rust
use reqwest::Client;
use serde::Deserialize;
use crate::auth::xet_signer::XetSigner;
use crate::config::CasSettings;
use crate::error::HubError;

#[derive(Debug, Deserialize)]
pub struct BlobState {
    pub state: String,
    pub xet_file_id: Option<String>,
    pub size: u64,
    pub sha256: String,
}

pub struct CasClient {
    http: Client,
    base_url: String,
}

impl CasClient {
    pub fn new(settings: &CasSettings) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(settings.internal_timeout_seconds))
            .build()
            .expect("Failed to build HTTP client");
        Self { http, base_url: settings.base_url.trim_end_matches('/').to_string() }
    }

    /// Check if a blob exists in CAS (internal endpoint).
    /// Returns storage state headers.
    pub async fn head_blob(&self, oid: &str, internal_token: &str) -> Result<BlobState, HubError> {
        let url = format!("{}/internal/blob/{}", self.base_url, oid);
        let resp = self.http.head(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => {
                let state = resp.headers().get("X-Storage-State")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("raw_only")
                    .to_string();
                let file_id = resp.headers().get("X-File-Id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                Ok(BlobState {
                    state,
                    xet_file_id: file_id,
                    size: 0,
                    sha256: oid.to_string(),
                })
            }
            404 => Err(HubError::NotFound(format!("Blob not found in CAS: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Get state for a blob (internal endpoint).
    pub async fn get_state(&self, oid: &str, internal_token: &str) -> Result<Option<BlobState>, HubError> {
        let url = format!("{}/internal/state/{}", self.base_url, oid);
        let resp = self.http.get(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => {
                let state: BlobState = resp.json().await
                    .map_err(|e| HubError::CasError(format!("Failed to parse CAS response: {}", e)))?;
                Ok(Some(state))
            }
            404 => Ok(None),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Proxy a batch request to CAS.
    pub async fn proxy_batch(&self, body: &serde_json::Value, token: &str) -> Result<serde_json::Value, HubError> {
        let url = format!("{}/objects/batch", self.base_url);
        let resp = self.http.post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await
            .map_err(|e| HubError::CasError(format!("Failed to parse CAS batch response: {}", e)))?;

        if status >= 400 {
            return Err(HubError::CasError(format!("CAS batch error: {}", body)));
        }
        Ok(body)
    }

    /// Proxy LFS upload to CAS.
    pub async fn proxy_lfs_upload(&self, oid: &str, data: bytes::Bytes, token: &str) -> Result<(), HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.http.put(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(HubError::CasError(format!("CAS upload failed: {}", resp.status())))
        }
    }

    /// Proxy LFS download from CAS.
    pub async fn proxy_lfs_download(&self, oid: &str, token: &str) -> Result<bytes::Bytes, HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.http.get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.bytes().await.map_err(|e| HubError::CasError(e.to_string()))?),
            404 => Err(HubError::NotFound(format!("Object not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }
}
```

Add `bytes = "1.5"` to `hub/Cargo.toml` dependencies.

- [x] **Step 2: Commit**

```bash
git add hub/src/cas_client/ hub/Cargo.toml
git commit -m "feat(hub): add CAS HTTP client for internal communication"
```

---

### Task 7: Repo CRUD

**Files:**
- Create: `hub/src/api/repo.rs`

- [x] **Step 1: Implement repo CRUD handlers**

Implement handlers for:
- `POST /api/models` — create model repo
- `POST /api/datasets` — create dataset repo
- `POST /api/spaces` — create space repo
- `GET /api/models/{ns}/{repo}` — get repo info
- `DELETE /api/models/{ns}/{repo}` — delete repo

Request body for creation:
```json
{ "name": "my-model", "private": true }
```

The namespace is derived from the authenticated user's username.

Response for creation/get:
```json
{
  "id": "alice/my-model",
  "name": "my-model",
  "private": true,
  "createdAt": "2026-06-10T00:00:00Z",
  "updatedAt": "2026-06-10T00:00:00Z",
  "tags": [],
  "downloads": 0,
  "likes": 0
}
```

Each handler:
1. Extract Bearer token → validate via TokenStore
2. Extract repo_type from URL path
3. For creation: parse body, derive namespace from user, create via MetadataStore
4. For get/delete: look up repo_id = "{ns}/{repo}", call MetadataStore

Since all three repo types share the same logic, use a single handler that takes repo_type as a parameter:

```rust
pub async fn create_repo(
    repo_type: RepoType,
    body: web::Json<CreateRepoRequest>,
    token_store: ..., metadata: ...,
) -> HttpResponse { ... }
```

Register routes for all three types in server.rs:
```rust
.route("/api/models", web::post().to(create_model))
.route("/api/datasets", web::post().to(create_dataset))
.route("/api/spaces", web::post().to(create_space))
```

Each thin wrapper calls the shared handler with the appropriate RepoType.

- [x] **Step 2: Write tests and commit**

Test: create repo, get repo, create duplicate (409), get nonexistent (404), delete repo.

```bash
git add hub/src/api/repo.rs hub/tests/test_repo.rs
git commit -m "feat(hub): add repo CRUD endpoints for models/datasets/spaces"
```

---

### Task 8: Commit API (NDJSON)

**Files:**
- Create: `hub/src/api/commit.rs`
- Create: `hub/src/api/preupload.rs`

- [x] **Step 1: Implement commit handler**

```
POST /api/{type}s/{ns}/{repo}/commit/{revision}
Content-Type: application/x-ndjson

NDJSON operations:
  {"key":"header","value":{"summary":"...","parentRevision":"rev_abc"}}
  {"key":"file","value":{"path":"config.json","content":"base64:..."}}
  {"key":"lfsFile","value":{"path":"model.bin","oid":"abc...","size":1073741824}}
  {"key":"deletedEntry","value":{"path":"old.bin"}}
```

Handler logic:
1. Auth: validate hf_ token, check write scope
2. Check repo exists
3. Parse NDJSON line by line
4. Extract header: summary, parentRevision
5. Check parentRevision matches current HEAD (OCC → 409 if mismatch)
6. For each "file" operation:
   - Decode base64 content
   - Compute SHA256 oid
   - If size <= inline_threshold: store in CAS via `PUT /lfs/objects/{oid}` (internal token)
   - Record FileEntry
7. For each "lfsFile" operation:
   - Verify oid exists in CAS via `head_blob` (internal token)
   - Record FileEntry
8. For each "deletedEntry": skip (just don't include in new tree)
9. Generate new rev_id = SHA256(repo_id + parent + message + timestamp)
10. Copy parent's file_tree, apply additions/deletions
11. Add revision + file_entries to MetadataStore
12. Set HEAD to new revision
13. Return `{ "commitOid": "rev_xxx" }`

For the initial implementation, the file_tree is stored per-revision (not inherited). A commit copies all parent entries and applies changes. This is simple but uses more storage. Optimization (pointer-based tree inheritance) can come later.

- [x] **Step 2: Implement preupload check handler**

```
POST /api/{type}s/{ns}/{repo}/preupload/{revision}
Body: { "files": [{ "path": "model.bin", "size": 1073741824 }] }

Response: { "files": [{ "path": "model.bin", "uploadMode": "xet" }] }

uploadMode:
  size <= 1MB → "regular"
  1MB < size <= 10MB → "lfs"
  size > 10MB → "xet"
```

- [x] **Step 3: Write tests and commit**

Test: commit with inline file, commit with lfsFile reference, commit conflict (wrong parent), preupload mode classification.

```bash
git add hub/src/api/commit.rs hub/src/api/preupload.rs hub/tests/test_commit.rs
git commit -m "feat(hub): add commit API (NDJSON) and preupload check"
```

---

### Task 9: Tree Listing and File Download

**Files:**
- Create: `hub/src/api/tree.rs`
- Create: `hub/src/api/resolve.rs`

- [x] **Step 1: Implement tree listing handler**

```
GET /api/{type}s/{ns}/{repo}/tree/{revision}/{path}
```

Handler:
1. Auth: validate token
2. Resolve revision: if revision is "main" or branch name → lookup HEAD → get rev_id
3. Query file_tree for rev_id with path prefix
4. Return array of entries:
```json
[
  { "type": "file", "oid": "abc...", "size": 1024, "path": "model.bin" },
  { "type": "directory", "oid": null, "size": 0, "path": "subdir" }
]
```

Directories are inferred from paths with `/` separators.

- [x] **Step 2: Implement file download (resolve) handler**

```
GET /{ns}/{repo}/resolve/{revision}/{path}
```

Handler:
1. Auth: validate token
2. Resolve revision → rev_id → resolve_file(path) → blob_oid
3. Query CAS state for blob_oid (internal token)
4. If RAW_ONLY: trigger conversion via `POST /internal/convert/{oid}` (or return redirect to CAS raw URL)
5. Return 302 redirect to CAS download URL, or proxy the download

For simplicity in Phase 1: return 302 redirect to `{cas_base_url}/lfs/objects/{oid}` with auth header.

- [x] **Step 3: Write tests and commit**

Test: tree listing, tree with directories, resolve existing file, resolve missing file (404).

```bash
git add hub/src/api/tree.rs hub/src/api/resolve.rs hub/tests/test_tree.rs
git commit -m "feat(hub): add tree listing and file download endpoints"
```

---

### Task 10: Git LFS Batch Proxy

**Files:**
- Create: `hub/src/api/lfs_proxy.rs`

- [x] **Step 1: Implement LFS proxy handlers**

The Hub proxies Git LFS operations to CAS, rewriting URLs to point back to the Hub.

**POST /objects/batch and POST /lfs/objects/batch:**
1. Auth: validate hf_ token
2. Forward batch request to CAS via CasClient.proxy_batch() using an internal token
3. Rewrite response URLs: replace CAS URLs with Hub URLs
4. Return to client

**PUT /lfs/objects/{oid}:**
1. Auth: validate hf_ token
2. Forward upload to CAS via CasClient.proxy_lfs_upload()
3. Return CAS response

**GET /lfs/objects/{oid}:**
1. Auth: validate hf_ token
2. Forward download to CAS via CasClient.proxy_lfs_download()
3. Return data

For URL rewriting in batch responses:
```rust
fn rewrite_batch_urls(response: &mut serde_json::Value, hub_base_url: &str, cas_base_url: &str) {
    // Replace CAS URLs with Hub URLs in action hrefs
    if let Some(objects) = response.get_mut("objects") {
        if let Some(arr) = objects.as_array_mut() {
            for obj in arr {
                if let Some(actions) = obj.get_mut("actions") {
                    for key in ["upload", "download"] {
                        if let Some(action) = actions.get_mut(key) {
                            if let Some(href) = action.get("href") {
                                if let Some(href_str) = href.as_str() {
                                    let new_href = href_str.replace(cas_base_url, hub_base_url);
                                    action.as_object_mut().unwrap()
                                        .insert("href".to_string(), serde_json::Value::String(new_href));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

- [x] **Step 2: Write tests and commit**

Test: batch proxy rewrites URLs correctly, LFS upload proxy, LFS download proxy.

```bash
git add hub/src/api/lfs_proxy.rs hub/tests/test_lfs_proxy.rs
git commit -m "feat(hub): add Git LFS batch proxy to CAS"
```

---

### Task 11: Server Setup and Route Registration

**Files:**
- Modify: `hub/src/server.rs`
- Modify: `hub/src/api/mod.rs`

- [x] **Step 1: Wire everything together in server.rs**

```rust
use actix_web::{web, App, HttpServer, HttpResponse, middleware::Logger};
use std::sync::Arc;

use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;
use crate::metadata::sqlite::SqliteMetadataStore;
use crate::metadata::MetadataStore;

pub async fn start_server(config: HubConfig) -> std::io::Result<()> {
    // Initialize components
    let token_store = Arc::new(
        TokenStore::new(&config.metadata.sqlite_path)
            .expect("Failed to create token store")
    );

    let metadata: Arc<dyn MetadataStore> = Arc::new(
        SqliteMetadataStore::new(&config.metadata.sqlite_path)
            .expect("Failed to create metadata store")
    );

    let private_key_pem = std::fs::read(&config.auth.private_key_path)
        .expect("Failed to read private key");
    let signer = Arc::new(
        XetSigner::from_pem(&private_key_pem, &config.auth.kid, config.auth.token_ttl_seconds)
            .expect("Failed to create xet signer")
    );

    let cas_client = Arc::new(CasClient::new(&config.cas));

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    println!("Starting Hub API on {}", bind_addr);
    println!("CAS: {}", config.cas.base_url);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::from(token_store.clone()))
            .app_data(web::Data::from(metadata.clone()))
            .app_data(web::Data::from(signer.clone()))
            .app_data(web::Data::from(cas_client.clone()))
            // Auth
            .route("/api/whoami-v2", web::get().to(crate::api::whoami::whoami))
            // Token exchange — explicit routes for each repo type
            .route("/api/models/{ns}/{repo}/xet-read-token/{rev}", web::get().to(crate::api::token_exchange::xet_read_token_models))
            .route("/api/models/{ns}/{repo}/xet-write-token/{rev}", web::get().to(crate::api::token_exchange::xet_write_token_models))
            .route("/api/datasets/{ns}/{repo}/xet-read-token/{rev}", web::get().to(crate::api::token_exchange::xet_read_token_datasets))
            .route("/api/datasets/{ns}/{repo}/xet-write-token/{rev}", web::get().to(crate::api::token_exchange::xet_write_token_datasets))
            .route("/api/spaces/{ns}/{repo}/xet-read-token/{rev}", web::get().to(crate::api::token_exchange::xet_read_token_spaces))
            .route("/api/spaces/{ns}/{repo}/xet-write-token/{rev}", web::get().to(crate::api::token_exchange::xet_write_token_spaces))
            // Repo CRUD
            .route("/api/models", web::post().to(crate::api::repo::create_model))
            .route("/api/datasets", web::post().to(crate::api::repo::create_dataset))
            .route("/api/spaces", web::post().to(crate::api::repo::create_space))
            .route("/api/models/{ns}/{repo}", web::get().to(crate::api::repo::get_repo))
            .route("/api/datasets/{ns}/{repo}", web::get().to(crate::api::repo::get_repo))
            .route("/api/spaces/{ns}/{repo}", web::get().to(crate::api::repo::get_repo))
            .route("/api/models/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo))
            .route("/api/datasets/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo))
            .route("/api/spaces/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo))
            // Commit
            .route("/api/models/{ns}/{repo}/commit/{rev}", web::post().to(crate::api::commit::commit))
            .route("/api/datasets/{ns}/{repo}/commit/{rev}", web::post().to(crate::api::commit::commit))
            .route("/api/spaces/{ns}/{repo}/commit/{rev}", web::post().to(crate::api::commit::commit))
            // Preupload
            .route("/api/models/{ns}/{repo}/preupload/{rev}", web::post().to(crate::api::preupload::preupload))
            .route("/api/datasets/{ns}/{repo}/preupload/{rev}", web::post().to(crate::api::preupload::preupload))
            .route("/api/spaces/{ns}/{repo}/preupload/{rev}", web::post().to(crate::api::preupload::preupload))
            // Tree
            .route("/api/models/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree))
            .route("/api/datasets/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree))
            .route("/api/spaces/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree))
            // File download
            .route("/{ns}/{repo}/resolve/{rev}/{path:.*}", web::get().to(crate::api::resolve::resolve))
            // Git LFS proxy
            .route("/objects/batch", web::post().to(crate::api::lfs_proxy::batch))
            .route("/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::batch))
            .route("/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::upload))
            .route("/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::download))
            // Health
            .route("/health", web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({"status": "ok"})) }))
    })
    .bind(&bind_addr)?
    .run()
    .await
}
```

Note: Token exchange routes use explicit paths for each repo type and token type (read/write), matching the handler functions defined in Task 5.

- [x] **Step 2: Create a CLI admin command for token creation**

Add a subcommand to main.rs:
```rust
// hub-api create-token --username alice --scope write
```

This calls `TokenStore::create_token()` and prints the plaintext token.

- [x] **Step 3: Verify full crate compiles and all tests pass**

```bash
cargo test -p hub-api
cargo check -p hub-api
```

- [x] **Step 4: Commit**

```bash
git add hub/src/server.rs hub/src/api/mod.rs hub/src/main.rs
git commit -m "feat(hub): wire up server with all routes and admin token creation"
```

---

### Task 12: Integration Test

**Files:**
- Create: `hub/tests/test_integration.rs`

- [x] **Step 1: Write end-to-end integration test**

Start both Hub and CAS (using temp dirs), then:

1. Create token via admin command
2. `GET /api/whoami-v2` → verify user info
3. `POST /api/models` → create repo
4. `GET /api/models/{ns}/{repo}` → verify repo exists
5. `POST /api/models/{ns}/{repo}/commit/{rev}` with inline file → verify commit
6. `GET /api/models/{ns}/{repo}/tree/{rev}/` → verify file in tree
7. `GET /api/models/{ns}/{repo}/xet-read-token/{rev}` → verify xet token returned
8. `POST /objects/batch` → verify batch proxy works
9. `GET /{ns}/{repo}/resolve/{rev}/{path}` → verify file download

This test requires starting both services. Use actix-web test utilities for Hub and spawn CAS in a background thread.

For simplicity, this test can use a mock CAS (just verify Hub calls the right CAS endpoints) or a real CAS if the test infrastructure supports it.

- [x] **Step 2: Run all tests**

```bash
cargo test -p hub-api
cargo test  # full workspace
```

- [x] **Step 3: Commit**

```bash
git add hub/tests/test_integration.rs
git commit -m "test(hub): add integration test for full workflow"
```

---

## Summary

| Task | Description | Depends On |
|------|-------------|------------|
| 1 | Workspace + crate scaffold | — |
| 2 | Error types | 1 |
| 3 | MetadataStore trait + SQLite | 1 |
| 4 | Token store + whoami | 2, 3 |
| 5 | Xet signer + token exchange | 2, 3, 4 |
| 6 | CAS HTTP client | 2 |
| 7 | Repo CRUD | 4 |
| 8 | Commit API + preupload | 5, 6, 7 |
| 9 | Tree + resolve | 6, 7, 8 |
| 10 | LFS proxy | 6 |
| 11 | Server wiring + routes | all above |
| 12 | Integration test | all above |

After completing this plan, the Hub API service will be a fully functional HuggingFace Hub compatible REST API that:
- Authenticates users via `hf_` tokens
- Manages repos (models/datasets/spaces) with explicit creation
- Accepts commits via NDJSON (inline files + LFS references)
- Issues `xet_` tokens for CAS access
- Proxies Git LFS operations to CAS
- Serves file tree listings and downloads
