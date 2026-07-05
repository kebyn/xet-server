# HuggingFace Hub REST API for Xet Server - Design Spec

**Date:** 2026-06-10  
**Status:** ✅ Completed  
**Implemented:** 2026-06-12  

## 1. Overview

This spec defines the design for implementing a HuggingFace Hub REST API compatibility layer for the existing xet-server, enabling private deployment of a HuggingFace Hub-compatible service.

### 1.1 Goals

- Enable `hf upload`, `hf upload-large-folder`, and `hf download` to work against the xet server
- Support models, datasets, and spaces repositories
- Support Git LFS protocol (`git lfs push/pull`) with full interoperability with HF upload/download
- Achieve cross-protocol xet deduplication: files uploaded via Git LFS can be deduplicated when downloaded via HF protocol
- Achieve 1x long-term storage efficiency through lazy conversion

### 1.2 Non-Goals

- PR/discussion support
- OAuth/OIDC provider integration
- Gated repository access control
- Inference API

### 1.3 Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Compatibility scope | High (models/datasets/spaces, no PR/discussion) | Covers primary use cases |
| Storage model | Git-like content-addressable | True HF Hub semantic compatibility |
| File storage integration | Reuse xet native protocol (CDC, BLAKE3, xorb, shard) | Leverage existing dedup infrastructure |
| Authentication | Two-layer (Hub token -> xet CAS token) with Ed25519 asymmetric keys | Full HF Hub client compatibility |
| Deployment | Dual process (Hub API + CAS as separate services) | Independent scaling |
| Trust mechanism | Asymmetric keys (Hub signs, CAS verifies) | Hub private key never exposed to CAS |
| Repository creation | Explicit creation with minimal metadata | Clean API, no ambiguity |
| Service discovery | Environment variables with defaults | Consistent with existing xet-server config style |
| Metadata store | Trait abstraction, SQLite initial implementation, PostgreSQL-ready | Phase-based scaling |
| LFS-xet interoperability | Lazy conversion: raw blob stored first, converted to xorbs on first xet download, raw deleted after | 1x long-term storage, xet dedup across protocols |
| Auth implementation | Ed25519 only, no legacy JWT shared-secret | Clean break, stronger security |

---

## 2. System Architecture

### 2.1 Component Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Client Layer                                 │
│                                                                     │
│   hf CLI / huggingface_hub          git lfs client                  │
│   (HF Commit API + hf_xet)          (Git LFS protocol)              │
└──────────┬────────────────────────────────┬─────────────────────────┘
           │ HF_ENDPOINT                    │ .lfsconfig → Hub LFS URL
           ▼                                ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     Hub API Service (new)                            │
│                                                                     │
│  ┌─────────────┐ ┌──────────────┐ ┌─────────────┐ ┌─────────────┐ │
│  │ Auth        │ │ Repo Mgmt    │ │ Commit API  │ │ Token       │ │
│  │ /whoami-v2  │ │ models/      │ │ /commit/    │ │ Exchange    │ │
│  │             │ │ datasets/    │ │ /preupload/ │ │ xet-read/   │ │
│  │             │ │ spaces/      │ │             │ │ write-token │ │
│  └─────────────┘ └──────────────┘ └─────────────┘ └─────────────┘ │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │        Structure Metadata Store (SQLite, immutable)             ││
│  │  repos, revisions, file_tree: (rev,path) -> blob_oid            ││
│  └─────────────────────────────────────────────────────────────────┘│
│                                                                     │
│  Config: CAS_BASE_URL, JWT_PRIVATE_KEY_PATH                        │
└──────────┬──────────────────────────────────────────────────────────┘
           │ internal HTTP (state queries, conversion triggers)
           ▼
┌─────────────────────────────────────────────────────────────────────┐
│                  CAS Service (existing xet-server extended)          │
│                                                                     │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────────────────────┐ │
│  │ Xet Native  │ │ LFS Compat   │ │ Storage State Manager        │ │
│  │ /xorb/*     │ │ /lfs/objects │ │ state/{oid} -> {state,fid}   │ │
│  │ /shard/*    │ │ /objects/    │ │ lazy conversion engine       │ │
│  │ /reconstr.  │ │ batch        │ │                              │ │
│  │ /chunks/*   │ │              │ │                              │ │
│  └─────────────┘ └──────────────┘ └──────────────────────────────┘ │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │        Storage Backend (S3 / Local)                             ││
│  │  blobs/{oid}       raw bytes (permanent for small files, temporary for LFS pending conversion)│
│  │  xorbs/{prefix}/{hash}  chunk data (permanent, deduped)        ││
│  │  shards/{id}       reconstruction metadata (permanent)          ││
│  └─────────────────────────────────────────────────────────────────┘│
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │        Storage State DB (CAS-local SQLite)                      ││
│  │  file_states: oid -> {state, xet_file_id, size, ...}           ││
│  │  conversion_locks: oid -> {locked_at, locked_by}               ││
│  └─────────────────────────────────────────────────────────────────┘│
│                                                                     │
│  Config: JWT_PUBLIC_KEY_PATH, STORAGE_BACKEND                       │
└─────────────────────────────────────────────────────────────────────┘
```

### 2.2 Responsibility Separation

| Component | Manages | Does NOT manage |
|-----------|---------|-----------------|
| **Hub API** | Structure metadata (repos, revisions, file tree), authentication, token issuance, commit coordination | File content, storage state, chunking/dedup |
| **CAS** | File content, storage state (RAW_ONLY/XET_ONLY), lazy conversion, reconstruction | Repo structure, commit graph, user permissions |

### 2.3 Inter-Service Communication

Hub -> CAS internal endpoints (not exposed to external clients):

| Endpoint | Purpose |
|----------|---------|
| `GET /internal/state/{oid}` | Query file storage state |
| `POST /internal/convert/{oid}` | Trigger lazy conversion |
| `HEAD /internal/blob/{oid}` | Check if raw blob exists |
| `GET /internal/state/batch` | Batch state query (for tree listing) |

Internal auth: Hub signs service tokens using the same Ed25519 private key, with `scope: "internal"` claim. CAS verifies with the same public key.

---

## 3. Authentication

### 3.1 Two-Layer Architecture

**Layer 1: Hub Token (long-lived credential)**

- Format: `hf_{random_string}`
- Storage: Hub's `tokens` table (SQLite, stores SHA256 hash, not plaintext)
- Transport: `Authorization: Bearer hf_xxx`
- Validation: Hub looks up token_store -> user_id, scope
- Used by: `/api/whoami-v2`, `/api/models/*`, `/api/datasets/*`, `/api/spaces/*`
- Lifecycle: Long-lived, revocable

**Layer 2: Xet CAS Token (short-lived credential)**

- Format: `xet_{jwt_signed_by_hub_ed25519_private_key}`
- Issued by: Hub's token exchange endpoints
- Verified by: CAS using Hub's Ed25519 public key
- Transport: `Authorization: Bearer xet_xxx`
- JWT Claims:
  - `sub`: user_id
  - `scope`: "read" | "write" | "internal"
  - `repo_id`: "namespace/repo-name"
  - `repo_type`: "model" | "dataset" | "space"
  - `revision`: "main" | commit_hash
  - `exp`: unix_timestamp (default 1 hour)
  - `kid`: key identifier for rotation
- Used by: All CAS API endpoints
- Scope "internal" is used only for Hub->CAS internal endpoints; it does not supersede "read" or "write"

### 3.2 Key Management

```
Hub config:
  private_key_path = /etc/hub/keys/hub_private.pem    # Ed25519 private key
  kid = hub-key-001                                     # Key ID for rotation

CAS config:
  public_key_path = /etc/cas/keys/hub_public.pem       # Ed25519 public key
  trusted_kids = [hub-key-001]                          # Trusted Key IDs
  token_prefix = xet_
```

### 3.3 Token Exchange Endpoints

```
GET /api/models/{namespace}/{repo}/xet-read-token/{revision}
GET /api/models/{namespace}/{repo}/xet-write-token/{revision}
(same pattern for datasets and spaces)

Hub processing:
  1. Validate hf token -> get user_id
  2. Check user permission on repo (read/write)
  3. Check revision exists
  4. Sign JWT with Ed25519 private key
  5. Return: { accessToken: "xet_...", exp: ..., casUrl: CAS_BASE_URL }
```

### 3.4 CAS Token Verification

```
CAS receives request:
  Authorization: Bearer xet_eyJhbGc...

  1. Parse JWT header -> get kid
  2. Look up public key for kid
  3. Verify Ed25519 signature
  4. Verify exp (not expired)
  5. Extract claims: scope, repo_id, repo_type, revision
  6. Permission check:
     - GET /reconstruction/* -> requires "read" scope
     - POST /xorb/* -> requires "write" scope
     - POST /shard -> requires "write" scope
     - /internal/* -> requires "internal" scope
  7. Repo consistency check (for non-internal tokens):
     - Requested resource must belong to token's repo_id
```

### 3.5 Token Storage (Hub)

```sql
CREATE TABLE users (
    user_id      TEXT PRIMARY KEY,
    username     TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL
);

CREATE TABLE tokens (
    token_hash   TEXT PRIMARY KEY,  -- SHA256(hf_xxx), never store plaintext
    user_id      TEXT NOT NULL REFERENCES users(user_id),
    name         TEXT NOT NULL,     -- "My CI Token"
    scope        TEXT NOT NULL,     -- "read" | "write"
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER,           -- NULL = never expires
    revoked_at   INTEGER            -- NULL = not revoked
);

CREATE INDEX idx_tokens_user ON tokens(user_id);
```

### 3.6 Key Rotation

1. Generate new Ed25519 key pair (new_kid, new_private, new_public)
2. Deploy new public key to all CAS instances: `trusted_kids = [old-kid, new-kid]`
3. Restart CAS
4. Switch Hub to new private key (new tokens signed with new_kid)
5. Wait for all old tokens to expire (max TTL = 1h)
6. Remove old public key from CAS: `trusted_kids = [new-kid]`
7. Restart CAS

---

## 4. Hub API Endpoints

### 4.1 Endpoint Summary

All endpoints under `/api/models/`, `/api/datasets/`, `/api/spaces/` follow the same pattern (denoted as `/api/{type}s/` below).

#### Authentication

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/whoami-v2` | Validate token, return user info |

#### Token Exchange

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/{type}s/{ns}/{repo}/xet-read-token/{rev}` | Get xet read token |
| GET | `/api/{type}s/{ns}/{repo}/xet-write-token/{rev}` | Get xet write token |

#### Repository Management

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/{type}s` | Create repository |
| GET | `/api/{type}s/{ns}/{repo}` | Get repository info |
| DELETE | `/api/{type}s/{ns}/{repo}` | Delete repository |

#### File Browsing

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/{type}s/{ns}/{repo}/tree/{rev}/{path}` | List directory contents |
| GET | `/api/{type}s/{ns}/{repo}/treesize/{rev}/{path}` | Get folder size |
| POST | `/api/{type}s/{ns}/{repo}/paths-info/{rev}` | Batch path metadata |

#### Commit Operations

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/{type}s/{ns}/{repo}/commit/{rev}` | Create commit (NDJSON) |
| POST | `/api/{type}s/{ns}/{repo}/preupload/{rev}` | Check upload mode |
| GET | `/api/{type}s/{ns}/{repo}/commits/{rev}` | List commits |

#### Revision Management

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/{type}s/{ns}/{repo}/branch/{rev}` | Create branch |
| POST | `/api/{type}s/{ns}/{repo}/tag/{rev}` | Create tag |
| GET | `/api/{type}s/{ns}/{repo}/refs` | List references |

#### File Download

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/{ns}/{repo}/resolve/{rev}/{path}` | Download file (redirect or proxy) |
| GET | `/{ns}/{repo}/raw/{rev}/{path}` | Download file (raw) |

#### Git LFS Proxy (Hub proxies to CAS)

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/objects/batch` | Git LFS batch operation |
| POST | `/lfs/objects/batch` | Git LFS batch (alternate path) |
| PUT | `/lfs/objects/{oid}` | Upload LFS object (proxy to CAS) |
| GET | `/lfs/objects/{oid}` | Download LFS object (proxy to CAS) |

#### System

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |

### 4.2 Key Endpoint Details

#### whoami-v2

```
GET /api/whoami-v2
Authorization: Bearer hf_xxx

Response 200:
{
  "name": "alice",
  "email": "alice@example.com",
  "orgs": [],
  "auth": {
    "type": "access_token",
    "accessToken": { "name": "My CI Token", "role": "write" }
  }
}
```

#### Repo Creation

```
POST /api/models
Authorization: Bearer hf_xxx
{ "name": "my-model", "private": true }

Response 200:
{ "id": "alice/my-model", "name": "my-model", "private": true, ... }
Response 409: { "error": "Repository already exists" }
```

#### Commit (NDJSON)

```
POST /api/models/{ns}/{repo}/commit/{revision}
Authorization: Bearer hf_xxx
Content-Type: application/x-ndjson

{"key":"header","value":{"summary":"Upload model weights","parentRevision":"rev_abc123"}}
{"key":"file","value":{"path":"config.json","content":"base64:eyJ..."}}
{"key":"lfsFile","value":{"path":"model.safetensors","oid":"abc123...","size":1073741824}}
{"key":"deletedEntry","value":{"path":"old_file.bin"}}

Hub processing:
  1. Validate auth + permissions
  2. Parse NDJSON operations
  3. For "file": store inline if small, record in file_tree
  4. For "lfsFile": verify oid exists in CAS (HEAD /internal/blob/{oid})
  5. Create new revision (optimistic concurrency via parentRevision)
  6. Return: { commitOid: "rev_xyz789" }
```

#### Preupload Check

```
POST /api/models/{ns}/{repo}/preupload/{revision}
Authorization: Bearer hf_xxx
{ "files": [{ "path": "model.safetensors", "size": 1073741824 }] }

Response 200:
{ "files": [{ "path": "model.safetensors", "uploadMode": "xet" }] }

uploadMode: "xet" (size > 10MB) | "regular" (size <= 1MB) | "lfs" (1MB < size <= 10MB)
```

#### Git LFS Batch Proxy

Hub receives Git LFS batch requests, validates auth, and proxies to CAS. Upload/download URLs in the response point back to Hub (Hub proxies to CAS). This ensures:
- Clients don't need to know CAS address
- Hub can track LFS operations (update file_tree)
- Authentication is unified through Hub

---

## 5. CAS Service Modifications

### 5.1 Authentication Layer Rewrite

Replace existing HMAC-SHA256 JWT authentication with Ed25519:

```rust
pub enum CasToken {
    XetToken(XetClaims),  // Ed25519 signed by Hub
}

pub struct XetClaims {
    pub sub: String,
    pub scope: String,     // "read" | "write" | "internal"
    pub repo_id: String,
    pub repo_type: String,
    pub revision: String,
    pub exp: u64,
    pub kid: String,
}
```

Token identification: `token.starts_with("xet_")` -> strip prefix -> Ed25519 verify.

### 5.2 Storage State Manager

New `StorageStateManager` trait with SQLite implementation:

```rust
#[async_trait]
pub trait StorageStateManager: Send + Sync {
    async fn get_state(&self, oid: &str) -> StorageResult<FileState>;
    async fn mark_converted(&self, oid: &str, file_id: &str) -> StorageResult<()>;
    async fn get_states(&self, oids: &[String]) -> StorageResult<Vec<(String, FileState)>>;
}

pub struct FileState {
    pub state: StorageState,      // RawOnly | XetOnly
    pub xet_file_id: Option<String>,
    pub size: u64,
    pub sha256: String,
    pub converted_at: Option<u64>,
}
```

```sql
CREATE TABLE file_states (
    oid          TEXT PRIMARY KEY,
    state        TEXT NOT NULL,       -- "raw_only" | "xet_only"
    xet_file_id  TEXT,
    size         INTEGER NOT NULL,
    sha256       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    converted_at INTEGER
);

CREATE TABLE conversion_locks (
    oid        TEXT PRIMARY KEY,
    locked_at  INTEGER NOT NULL,
    locked_by  TEXT NOT NULL
);

-- Reserved for future GC
CREATE TABLE xorb_refs (
    xorb_hash  TEXT NOT NULL,
    shard_id   TEXT NOT NULL,
    PRIMARY KEY (xorb_hash, shard_id)
);
```

### 5.3 Lazy Conversion Engine

Triggered on first xet-path access to a RAW_ONLY file:

```rust
pub struct ConversionEngine {
    storage: Arc<dyn StorageBackend>,
    state_manager: Arc<dyn StorageStateManager>,
    chunker: CdcChunker,
    xorb_builder: XorbBuilder,
    instance_id: String,
}
```

Conversion steps (atomic within CAS):
1. Acquire conversion lock (prevent concurrent conversion)
2. Read raw blob from storage (streaming)
3. CDC chunk -> xorbs (pipeline, O(chunk_size + xorb_size) memory)
4. Store xorbs to storage (content-addressed, skip if exists = dedup)
5. Store shard metadata
6. Update file_states: state = XET_ONLY, file_id = ...
7. Delete raw blob
8. Release lock

Crash safety:
- Crash before step 6: raw blob still exists, safe to retry
- Crash after step 6, before step 7: both raw and xorbs exist, cleanup job deletes raw
- Crash after step 7: fully converted, consistent

Concurrent conversion handling:
- Second instance finds lock held -> returns `InProgress`
- Client can either wait+retry or fall back to raw blob download (raw not yet deleted)

### 5.4 LFS Reconstruction Endpoint

Modify `GET /lfs/objects/{oid}`:

```
state = RAW_ONLY:
  -> directly return raw blob (streaming)

state = XET_ONLY:
  -> read shard metadata
  -> stream reconstruction: read xorbs in order, decompress, concatenate
  -> return assembled raw bytes via streaming response
```

The `ReconstructionStream` implements `actix_web::Stream` for zero-copy streaming from xorbs to HTTP response.

### 5.5 Internal Endpoints

```
GET /internal/state/{oid}
  -> returns FileState JSON
  -> auth: scope = "internal"

POST /internal/convert/{oid}
  -> triggers lazy conversion
  -> returns: { status: "converted" | "already_converted" | "in_progress", file_id? }
  -> auth: scope = "internal"

HEAD /internal/blob/{oid}
  -> checks if blob is accessible (raw or xet)
  -> returns X-Storage-State header: "raw_only" | "xet_only"
  -> auth: scope = "internal"

GET /internal/state/batch
  -> batch state query for multiple oids
  -> auth: scope = "internal"
```

---

## 6. Storage Model and File Lifecycle

### 6.1 Storage Layout

```
Storage Backend (S3 / Local):
  blobs/{oid}                raw bytes (permanent for small/inline files, temporary for LFS files pending conversion)
  xorbs/{hash_prefix}/{hash} compressed chunk collections (permanent, deduped)
  shards/{file_id}           reconstruction metadata (permanent)

CAS-local SQLite:
  file_states table          storage state per oid
  conversion_locks table     distributed lock for conversion
```

Note: "inline" is an upload-path concept, not a storage concept. Small files uploaded via commit NDJSON are stored in `blobs/{oid}` (same as LFS uploads) with state `raw_only`. They are never converted (below conversion threshold).

### 6.2 File Classification

| File Size | Upload Path | Storage | Download |
|-----------|-------------|---------|----------|
| <= 1MB (INLINE_THRESHOLD) | Inline in commit | `blobs/{oid}`, state=raw_only (never converted) | Direct read from blob |
| > 1MB, <= 10MB | LFS path | `blobs/{oid}` -> `xorbs/` (after conversion) | Raw or reconstruction |
| > 10MB (LFS_THRESHOLD) | Xet path (CDC client-side) | `xorbs/` + `shards/` | Reconstruction |

### 6.3 File State Machine

```
git lfs push -> RAW_ONLY (blobs/{oid})
                 |
                 | first hf download triggers conversion
                 v
              CONVERTING (transient, lock held)
                 |
                 | CDC -> xorbs, store shard, update state, delete raw
                 v
              XET_ONLY (xorbs + shards)

Small files (hf upload): directly RAW_ONLY (blobs/{oid}, never converted)
```

### 6.4 Read Paths

**git lfs pull:**
- RAW_ONLY: proxy raw blob from CAS
- XET_ONLY: CAS performs reconstruction, streams raw bytes

**hf download:**
- RAW_ONLY (small file, never converted): direct read from blob
- RAW_ONLY (large file, not yet converted): Hub triggers conversion via `/internal/convert/{oid}`, this request returns raw blob, subsequent requests use xet path
- XET_ONLY: client uses hf_xet with reconstruction

**Browser direct download:**
- Hub proxies, same logic as above

### 6.5 Deduplication

Dedup occurs at xorb storage layer:
- `try_store_xorb(xorb)`: if `xorbs/{hash}` exists -> skip write (dedup)
- CDC is deterministic: same content -> same chunks -> same xorbs
- Dedup scope: global across repos and users (xorb key contains only content hash)

### 6.6 Thresholds

```toml
inline_threshold_bytes = 1048576     # 1MB
lfs_threshold_bytes = 10485760       # 10MB
```

### 6.7 Garbage Collection

Reserved for future implementation. Schema includes `xorb_refs` table for reference counting. Initial strategy: do not GC (xorb storage cost is low due to compression + dedup).

---

## 7. Data Flows

### 7.1 hf upload (large file)

1. `whoami` -> validate token
2. `create_repo` (if needed)
3. `preupload` -> determines upload mode (xet for large files)
4. `xet-write-token` -> Hub signs Ed25519 JWT
5. Client-side: CDC chunk -> xorbs
6. Upload xorbs to CAS (`POST /v1/xorbs/{prefix}/{hash}`)
7. Upload shard to CAS (`POST /v1/shards`)
8. CAS: stores xorbs (dedup), shard, updates file_states to XET_ONLY
9. `commit` (finalize) -> Hub verifies oid exists in CAS, records in file_tree

### 7.2 hf upload (small file)

1. `commit` with inline file content (base64)
2. Hub extracts content, calls CAS `PUT /lfs/objects/{oid}` to store in `blobs/{oid}`
3. CAS registers file_states as RAW_ONLY
4. Hub records file_tree: (rev, path) -> oid
5. Small files remain as RAW_ONLY permanently (never converted, below threshold)

### 7.3 git lfs push

1. `POST /objects/batch` -> Hub returns upload URLs pointing to Hub
2. `PUT /lfs/objects/{oid}` -> Hub proxies to CAS, CAS stores in `blobs/{oid}`, registers file_states as RAW_ONLY
3. Git commit tracking: The Hub provides LFS file transfer endpoints only. Git commit reception requires a Git server. Two deployment options:
   - **External Git server** (recommended for simplicity): Users configure a separate Git server (Gitea, bare repo, etc.). The Hub's file_tree is populated via the commit API (`POST /api/{type}s/{ns}/{repo}/commit/{rev}`) or via a webhook from the Git server that calls the Hub's internal API to register new commits.
   - **Integrated Git server** (future phase): Hub embeds a Git server that receives `git push`, parses LFS pointer files, and updates file_tree directly.

For the `hf upload/download` workflow, no Git server is needed (the commit API handles everything). Git LFS file transfer works independently of the Git server choice.

### 7.4 hf download (triggers lazy conversion)

1. `tree` listing -> Hub queries file_tree
2. `xet-read-token` -> Hub signs read token
3. Client requests reconstruction from CAS
4. If RAW_ONLY: CAS returns 412 or redirect to Hub
5. Hub calls `POST /internal/convert/{oid}` on CAS
6. CAS: acquires lock, reads raw blob, CDC chunks, stores xorbs (dedup), stores shard, updates state to XET_ONLY, deletes raw blob
7. Returns reconstruction data
8. Client downloads xorbs and assembles locally

### 7.5 git lfs pull (after conversion)

1. `POST /objects/batch` (download) -> Hub queries file_tree and file_states
2. Returns download URLs
3. `GET /lfs/objects/{oid}` -> Hub proxies to CAS
4. CAS: state = XET_ONLY -> reads shard, streams reconstruction from xorbs
5. Returns assembled raw bytes

### 7.6 Failure Recovery

**Conversion crash mid-way:** Raw blob intact, partial xorbs may exist. Next request retries conversion. Existing xorbs are deduped (content-addressed, hash match -> skip).

**State updated but raw delete failed:** Background cleanup job scans for XET_ONLY entries where `blobs/{oid}` still exists, deletes the raw blob.

**Hub crash after CAS write:** CAS objects become orphans. GC cleans up eventually. Client retries commit.

**Concurrent conversion:** Second instance gets lock conflict, falls back to raw blob download (still available).

---

## 8. Error Handling

### 8.1 Error Response Format

```json
{
  "error": "Human-readable message",
  "error_type": "ErrorCode"
}
```

### 8.2 HTTP Status Codes

| Code | Type | When | Client Action |
|------|------|------|---------------|
| 400 | ValidationError | Invalid params | Fix request |
| 401 | AuthenticationError | Invalid/expired token | Re-login |
| 403 | AuthorizationError | Insufficient scope | Check token scope |
| 404 | NotFoundError | Repo/revision/file not found | Check path |
| 409 | ConflictError | Repo exists, revision conflict | Pull latest, retry |
| 412 | PreconditionFailed | Conversion not complete | Wait, retry |
| 413 | PayloadTooLarge | Exceeds max body size | Use xet chunked upload |
| 422 | UnprocessableEntity | Referenced oid not in CAS | Upload file first |
| 429 | RateLimitExceeded | Too many requests | Exponential backoff |
| 500 | InternalError | Server error | Retry |
| 502 | BadGateway | Hub cannot reach CAS | Retry |
| 503 | ServiceUnavailable | CAS converting or unavailable | Wait, retry |

### 8.3 Retry Strategy

```
base_delay = 1s
max_delay = 60s
max_retries = 5
delay = min(base_delay * 2^attempt + random_jitter, max_delay)

Retry on: 409, 412, 429, 500, 502, 503
Do not retry on: 400, 401, 403, 404, 413, 422
```

### 8.4 Commit Concurrency

Optimistic concurrency control (OCC):
- Commit request includes `parentRevision`
- Hub checks current HEAD == parentRevision
- Mismatch -> 409 Conflict with currentHead
- Client pulls latest, merges if needed, retries with new parent

---

## 9. Metadata Store

### 9.1 Trait Abstraction

```rust
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_repo(&self, repo: &Repo) -> Result<()>;
    async fn get_repo(&self, repo_id: &str) -> Result<Option<Repo>>;
    async fn delete_repo(&self, repo_id: &str) -> Result<()>;
    async fn add_revision(&self, rev: &Revision) -> Result<()>;
    async fn get_revision(&self, rev_id: &str) -> Result<Option<Revision>>;
    async fn get_head(&self, repo_id: &str, branch: &str) -> Result<Option<String>>;
    async fn set_head(&self, repo_id: &str, branch: &str, rev_id: &str) -> Result<()>;
    async fn get_file_tree(&self, rev_id: &str) -> Result<Vec<FileEntry>>;
    async fn get_file_tree_prefix(&self, rev_id: &str, prefix: &str) -> Result<Vec<FileEntry>>;
    async fn resolve_file(&self, rev_id: &str, path: &str) -> Result<Option<String>>;
    async fn add_file_entries(&self, rev_id: &str, entries: &[FileEntry]) -> Result<()>;
    async fn get_commit_log(&self, repo_id: &str, branch: &str, limit: u32) -> Result<Vec<Revision>>;
}
```

Initial implementation: `SqliteMetadataStore`. Future: `PostgresMetadataStore`.

### 9.2 Schema (SQLite)

```sql
CREATE TABLE repos (
    repo_id      TEXT PRIMARY KEY,     -- "namespace/repo-name"
    repo_type    TEXT NOT NULL,         -- "model" | "dataset" | "space"
    namespace    TEXT NOT NULL,
    name         TEXT NOT NULL,
    private      INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE(namespace, name, repo_type)
);

CREATE TABLE revisions (
    rev_id       TEXT PRIMARY KEY,
    repo_id      TEXT NOT NULL REFERENCES repos(repo_id),
    parent_id    TEXT,                  -- NULL for initial commit
    message      TEXT NOT NULL,
    author       TEXT NOT NULL,
    branch       TEXT NOT NULL DEFAULT 'main',
    created_at   INTEGER NOT NULL
);

CREATE INDEX idx_revisions_repo_branch ON revisions(repo_id, branch);

CREATE TABLE file_tree (
    rev_id       TEXT NOT NULL REFERENCES revisions(rev_id),
    path         TEXT NOT NULL,
    blob_oid     TEXT NOT NULL,         -- SHA256 hex
    size         INTEGER NOT NULL,
    PRIMARY KEY (rev_id, path)
);

CREATE INDEX idx_file_tree_rev ON file_tree(rev_id);
```

### 9.3 Scaling Strategy

**Phase 1 (initial):** Single Hub instance + local SQLite. Handles ~5K commits/sec.

**Phase 2 (read scaling):** Multiple Hub instances + SQLite read replication (Litestream) or Redis cache layer. Data is immutable -> cache never stale.

**Phase 3 (write scaling):** Implement `PostgresMetadataStore` behind the same trait. Configuration change only.

---

## 10. Testing Strategy

### 10.1 Test Layers

- **Unit Tests:** MetadataStore trait, commit logic, auth (Hub); ConversionEngine, StorageStateManager, reconstruction, auth (CAS)
- **Integration Tests:** Hub <-> CAS interaction, token exchange -> CAS auth, lazy conversion trigger/completion, concurrent conversion safety
- **E2E Tests:** Full flow with hf CLI, git lfs client
- **Property Tests:** CDC determinism, xorb dedup correctness, reconstruction integrity

### 10.2 Key Test Scenarios

| Test | Type | Validates |
|------|------|-----------|
| `test_hf_upload_large_file` | E2E | hf upload -> xet path -> xorbs -> downloadable |
| `test_hf_upload_small_file` | E2E | hf upload -> inline -> downloadable |
| `test_git_lfs_push` | E2E | git lfs push -> raw blob -> state=raw_only |
| `test_git_lfs_pull_raw` | E2E | raw_only -> git lfs pull -> returns raw |
| `test_lazy_conversion` | Integration | raw_only -> hf download -> conversion -> xet_only |
| `test_conversion_dedup` | Property | Same content -> same xorbs -> zero extra storage |
| `test_reconstruction_integrity` | Property | Reconstruction output == original file (SHA256 match) |
| `test_concurrent_conversion` | Integration | Two clients convert same file -> one executes, one waits/falls back |
| `test_crash_recovery_raw_intact` | Integration | Conversion crash -> raw still exists -> retry succeeds |
| `test_crash_recovery_xorb_dedup` | Integration | Conversion crash -> partial xorbs -> retry dedups |
| `test_xet_token_auth` | Unit | Ed25519 signature, scope check, repo consistency |
| `test_token_exchange` | Integration | Hub signs xet token -> CAS verifies |
| `test_commit_conflict` | Integration | Concurrent commits -> 409 -> retry succeeds |
| `test_cross_protocol_download` | E2E | git lfs push -> hf download (via conversion) |
| `test_git_lfs_pull_after_conversion` | E2E | Conversion happens -> git lfs pull -> reconstruction |

### 10.3 Test Infrastructure

- Temp directory as local storage backend
- In-memory or temp-file SQLite
- Test Ed25519 key pair
- Optional: MinIO as S3 backend
- Memory-based `StorageBackend` and `MetadataStore` implementations for unit tests
- Real HTTP servers (random ports) for integration tests (no mocking inter-service HTTP)

---

## 11. Deployment

### 11.1 Configuration

**Hub API:**
```toml
[server]
host = "0.0.0.0"
port = 8080
public_base_url = "https://hub.example.com"

[auth]
private_key_path = "/etc/hub/keys/hub_private.pem"
kid = "hub-key-001"
token_ttl_seconds = 3600

[metadata]
backend = "sqlite"
sqlite_path = "/data/hub/metadata.db"

[cas]
base_url = "http://cas:9090"
internal_timeout_seconds = 30

[storage]
inline_threshold_bytes = 1048576
lfs_threshold_bytes = 10485760
```

**CAS:**
```toml
[server]
host = "0.0.0.0"
port = 9090

[auth]
public_key_path = "/etc/cas/keys/hub_public.pem"
trusted_kids = ["hub-key-001"]
token_prefix = "xet_"

[storage]
backend = "local"
local_path = "/data/xet-storage"

[state]
sqlite_path = "/data/cas/file_states.db"

[conversion]
trigger_mode = "lazy"
conversion_timeout_seconds = 600
max_concurrent_conversions = 10

[chunking]
min_chunk_size = 262144
avg_chunk_size = 1048576
max_chunk_size = 4194304

[xorb]
target_size = 67108864
```

### 11.2 Environment Variables

```
# Hub
HUB_PRIVATE_KEY_PATH, HUB_KID, HUB_METADATA_BACKEND, HUB_SQLITE_PATH,
CAS_BASE_URL, HUB_INLINE_THRESHOLD, HUB_LFS_THRESHOLD

# CAS
CAS_PUBLIC_KEY_PATH, CAS_TRUSTED_KIDS, CAS_STORAGE_BACKEND, CAS_LOCAL_PATH,
CAS_STATE_DB_PATH, CAS_CONVERSION_MODE, CAS_MAX_CONCURRENT_CONVERSIONS,
CAS_S3_BUCKET, CAS_S3_REGION, CAS_S3_ENDPOINT
```

### 11.3 Monitoring

**Hub metrics:**
- `hub_requests_total{method, path, status}`
- `hub_request_duration_seconds{method, path}`
- `hub_commits_total{repo_type, status}`
- `hub_token_exchange_total{token_type, status}`
- `hub_cas_call_duration_seconds{endpoint}`
- `hub_cas_call_errors_total{endpoint, error_type}`

**CAS metrics:**
- `cas_requests_total{method, path, status}`
- `cas_uploads_total{type}`, `cas_upload_bytes_total{type}`
- `cas_downloads_total{type}`, `cas_download_bytes_total{type}`
- `cas_conversions_total{status}`, `cas_conversion_duration_seconds`
- `cas_concurrent_conversions`
- `cas_storage_state_count{state}`
- `cas_dedup_savings_bytes`

### 11.4 Backup

- **Hub metadata.db:** Periodic file copy (Litestream -> S3). Data is immutable -> hourly backup sufficient.
- **CAS file_states.db:** Periodic copy. Recoverable by scanning storage backend.
- **Storage backend:** Local: rsync/rclone. S3: cross-region replication.

---

## 12. Implementation Phases

### Phase 1: Core (MVP)

- Hub: auth (whoami), repo CRUD, commit API, token exchange, tree listing, file download (resolve)
- Hub: Git LFS batch proxy
- CAS: Ed25519 auth (replace JWT), storage state manager, LFS reconstruction endpoint
- CAS: internal endpoints (state query, blob check)
- MetadataStore trait + SQLite implementation

### Phase 2: Lazy Conversion

- CAS: conversion engine
- CAS: `/internal/convert` endpoint
- Hub: trigger conversion on resolve for RAW_ONLY files
- Background cleanup job (delete raw after conversion)

### Phase 3: Polish

- Hub: branch/tag management
- Hub: commit log
- Hub: treesize, paths-info
- CAS: metrics (dedup savings, conversion stats)
- Test suite completion

### Phase 4: Scaling (as needed)

- PostgresMetadataStore implementation
- Hub read replicas (Litestream or Redis cache)
- CAS horizontal scaling (multiple instances)
- GC implementation
