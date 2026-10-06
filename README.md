# Xet Server

English | [简体中文](README.zh-CN.md)

Xet Server is a high-performance **Content-Addressable Storage (CAS)** server designed for managing large-scale machine learning models and datasets. It speaks both the **Git LFS protocol** and the **HuggingFace Hub API**, with intelligent cross-protocol deduplication.

## ✨ Core Features

### Storage Engine
- **Content-addressable storage (CAS)** - hash-based deduplicated storage that automatically eliminates duplicate data
- **Content-Defined Chunking (CDC)** - variable-size chunking (8KB-128KB) using the GearHash algorithm
- **BLAKE3 hashing** - high-speed cryptographic hashing with Merkle tree aggregate verification
- **LZ4 compression** - fast compression balancing performance and storage efficiency
- **Multiple storage backends** - local filesystem and S3/MinIO object storage

### Protocol Support
- **Git LFS compatible** - full Git Large File Storage protocol support
- **HuggingFace Hub API** - compatible with the HuggingFace Hub REST API; works with the `hf` CLI
- **Xet native protocol** - high-performance native protocol for xorbs and shards
- **Cross-protocol deduplication** - files uploaded via Git LFS can be downloaded deduplicated through the HF API

### Security
- **Ed25519 authentication** - JWTs signed with asymmetric Ed25519 keys
- **Layered authentication** - Hub tokens (`hf_xxx`) + CAS user tokens (`xet_xxx`) + LFS proxy tokens (`proxy_xxx`) + internal service tokens (`internal_xxx`)
- **Scope control** - `read`, `write`, `internal`, `lfs-upload`, `lfs-download`; `internal` is reserved for Hub -> CAS internal endpoints and implies neither `read` nor `write`
- **Key rotation** - multi-key management keyed by key ID (`kid`)

## 🏗️ Architecture Overview

Xet Server uses a **two-process architecture** made up of two independent services:

```
┌─────────────────────────────────────────────────────────────┐
│                         Clients                              │
│  (git lfs, hf CLI, xet-tools, custom clients)              │
└────────────┬──────────────────────────────┬─────────────────┘
             │                              │
             │ Git LFS / HF Hub API         │ Xet native protocol
             │ (HTTP :8080)                 │ (HTTP :8081)
             ▼                              ▼
┌────────────────────────┐      ┌────────────────────────┐
│     Hub API Server     │      │    CAS Server (xet)    │
│     (HuggingFace       │─────▶│   (Content Addressable │
│      Compatible)       │      │        Storage)        │
│                        │      │                        │
│  • Repository CRUD     │      │  • Xorb storage        │
│  • Commit API          │      │  • Shard storage       │
│  • Token Exchange      │      │  • File reconstruction │
│  • Tree Listing        │      │  • Global dedup        │
│  • File Resolve        │      │  • LFS object storage  │
│  • LFS Proxy           │      │  • State management    │
└────────────────────────┘      └──────────┬─────────────┘
                                           │
                                           ▼
                              ┌────────────────────────┐
                              │    Storage Backend     │
                              │                        │
                              │  • Local Filesystem    │
                              │  • S3 / MinIO          │
                              └────────────────────────┘
```

### Components

**Hub API Server** (`hub-api`)
- Port: 8080 (default)
- Purpose: serves a HuggingFace Hub-compatible REST API
- Responsibilities: repository management, commit API, token exchange, LFS proxy
- Database: SQLite (metadata store; a built-in migration runner executes at startup)

**CAS Server** (`xet-server`)
- Port: 8081 (default, to avoid clashing with the Hub API on 8080)
- Purpose: the content-addressable storage engine
- Responsibilities: xorb/shard storage, file reconstruction, deduplication, LFS object management
- No database (in-memory index only, rebuilt from the storage backend at startup)

## 🚀 Quick Start

### Requirements

- **Rust** 1.94.1+ (Edition 2024)
- **SQLite** 3.35+ (Hub API only)
- **Optional**: an S3/MinIO storage backend

### Build

```bash
# Clone the repository
git clone https://github.com/your-org/xet-server.git
cd xet-server

# Build (release mode; builds both workspace binaries)
cargo build --release --workspace --bins

# Binary locations
# CAS Server: target/release/xet-server
# Hub API:    target/release/hub-api
```

Release tags are built reproducibly — see any
[release](https://github.com/kebyn/xet-server/releases) for byte-exact
reproduction instructions.

### Generate Authentication Keys

```bash
# Generate a Hub user token (for Hub API authentication)
./target/release/hub-api create-token \
  --username admin \
  --name "admin-token" \
  --scope "read write" \
  --db hub.db

# Generate an Ed25519 key pair (for CAS token signing)
openssl genpkey -algorithm Ed25519 -out private_key.pem
openssl pkey -in private_key.pem -pubout -out public_key.pem
```

### Configure Environment Variables

**CAS Server**:
```bash
# Server settings (default port 8081, to avoid clashing with the Hub API on 8080)
export XET_HOST=0.0.0.0
export XET_PORT=8081
export XET_PUBLIC_BASE_URL=http://localhost:8081
export XET_MAX_BODY_SIZE_MB=2048
export XET_INDEX_REBUILD_STRICT=false

# Storage settings
export XET_STORAGE_BACKEND=local
export XET_LOCAL_PATH=/data/xet-storage

# Authentication settings
export CAS_PUBLIC_KEYS=hub-key-1=/path/to/public_key.pem
export CAS_TRUSTED_KIDS=hub-key-1
```

**Hub API**:
```bash
# Server settings
export HUB_HOST=0.0.0.0
export HUB_PORT=8080
export HUB_PUBLIC_BASE_URL=http://localhost:8080

# Authentication settings
export HUB_PRIVATE_KEY_PATH=/path/to/private_key.pem
export HUB_KID=hub-key-1
export HUB_TOKEN_TTL_SECONDS=3600

# CAS client settings
export HUB_CAS_BASE_URL=http://localhost:8081

# Metadata database
export HUB_SQLITE_PATH=/data/hub-metadata.db
```

### Start the Services

```bash
# Terminal 1: start the CAS Server
./target/release/xet-server

# Terminal 2: start the Hub API
./target/release/hub-api
```

## 💡 Usage Examples

### Option 1: Git LFS Workflow

Interact with Xet Server using standard Git LFS commands:

```bash
# Initialize a repository
mkdir my-model && cd my-model
git init
git lfs install

# Point LFS at Xet Server
cat > .lfsconfig << EOF
[lfs]
    url = http://localhost:8081/lfs
EOF

# Add a large file
echo "*.safetensors filter=lfs diff=lfs merge=lfs -text" > .gitattributes
cp /path/to/model.safetensors .

# Commit and push
git add .
git commit -m "Add model"
git remote add origin http://localhost:8081/repo.git
git push origin master
```

### Option 2: HuggingFace CLI Workflow

Interact with the Hub API using the `hf` CLI:

**Note**: `hf` is shorthand for
[huggingface-cli](https://huggingface.co/docs/huggingface_hub/main/en/guides/cli).
The standard `huggingface-cli` command works identically. Xet Server implements a
HuggingFace Hub-compatible REST API, so the standard HuggingFace toolchain is
supported.

```bash
# Set environment variables
export HF_ENDPOINT=http://localhost:8080
export HF_TOKEN=hf_your_token_here

# Create a repository
hf repo create my-model --type model

# Upload a file
hf upload my-model ./model.safetensors model.safetensors

# Download a file
hf download my-org/my-model model.safetensors --local-dir ./downloaded
```

### Option 3: Hybrid Workflow (Cross-Protocol Deduplication)

Combine Git LFS and the HF API for cross-protocol deduplication:

```bash
# Step 1: upload a large file via Git LFS
git lfs track "*.bin"
git add model.bin
git commit -m "Add model"
git push origin master

# Step 2: download via the HF API (deduplicated automatically)
export HF_ENDPOINT=http://localhost:8080
hf download my-org/my-repo model.bin --local-dir ./downloaded
# The file is served straight from CAS — no duplicate storage
```

## 📚 API Reference

### CAS Server API (port 8081)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/xorbs/{prefix}/{hash}` | POST/PUT | Upload a xorb object |
| `/v1/xorbs/{prefix}/{hash}/download` | GET | Download a xorb object |
| `/lfs/objects/{oid}` | PUT | Upload an LFS object |
| `/lfs/objects/{oid}` | GET | Download an LFS object |
| `/v1/shards` | POST | Upload shard metadata |
| `/v1/reconstructions/{file_id}` | GET | Get file reconstruction info |
| `/v2/reconstructions/{file_id}` | GET | Get file reconstruction info (V2) |
| `/v1/chunks/{prefix}/{hash}` | GET | Global deduplication query |
| `/objects/batch` | POST | Git LFS batch API |
| `/lfs/objects/batch` | POST | Git LFS batch API (LFS path) |
| `/health` | GET | Liveness check |
| `/ready` | GET | Readiness check (storage + MetadataIndex) |
| `/metrics` | GET | Prometheus metrics |

CAS object access is content-capability based: clients holding a valid CAS token
access content capabilities according to the token scope; repository-scoped object
isolation is not enforced. LFS batch actions prefer short-lived `proxy_xxx`
tokens; if `CAS_PRIVATE_KEY_PATH` is not configured, the server falls back to the
caller's `xet_xxx` token for compatibility — avoid this mode in production.

Detailed reference: [CAS API Reference](docs/api/cas-api.md)

### Hub API (port 8080)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/whoami-v2` | GET | User identity |
| `/api/repos/create` | POST | Create a repository |
| `/api/models` | POST | Create a model repository |
| `/api/datasets` | POST | Create a dataset repository |
| `/api/spaces` | POST | Create a Space repository |
| `/api/{type}/{ns}/{repo}/commit/{rev}` | POST | Commit files (NDJSON) |
| `/api/{type}/{ns}/{repo}/tree/{rev}` | GET | List the file tree |
| `/{type}/{ns}/{repo}/resolve/{rev}/{path}` | GET | Download a file |
| `/api/{type}/{ns}/{repo}/xet-read-token/{rev}` | GET | Get a read token |
| `/api/{type}/{ns}/{repo}/xet-write-token/{rev}` | GET | Get a write token |
| `/health` | GET | Liveness check |
| `/ready` | GET | Readiness check (SQLite + CAS) |

Detailed reference: [Hub API Reference](docs/api/hub-api.md)

### Protocol & Operational Guarantees

- The commit API `header` must be the first non-empty NDJSON operation and may
  appear only once; subsequent file/LFS/delete operations keep request order.
  When the same path appears more than once, the later operation wins.
- Every commit is a full file-tree snapshot: SQLite copies the parent commit's
  unmodified entries, applies this delta in order, and updates HEAD — all inside
  a single write transaction, without loading the full parent tree into Hub
  memory; commits after the first must submit a `parentRevision` matching the
  current HEAD.
- The commit API's LFS `oid` is a 64-character hex value without the `sha256:`
  prefix. The Hub validates the declared size against the mandatory
  `X-Blob-Size` from CAS `HEAD /internal/blob/{oid}`; a missing object or a size
  mismatch returns 422.
- Shards are parsed with bounded memory from a local file or a remote temp file,
  without retaining a full copy of the raw bytes; the startup rebuild streams
  key listings and processes shards in batches of at most 10. S3 listings use a
  fixed page size of 1000 and validate continuation-token progress. S3
  multipart uploads are tracked by unique upload ID and best-effort aborted on
  errors, cancellation, and shutdown; a bucket lifecycle rule remains the final
  backstop if the process crashes.
- Hub inline resolve verifies snapshot size and SHA-256 OID before returning
  small files; corrupted CAS responses never degrade into redirects. Internal
  SQL, paths, S3 configuration, and CAS upstream bodies are logged server-side
  only. Clients receive stable, generic 500/502 wording; a plain CAS 500 JSON
  body is `{"error":"Internal server error"}`.
- Hub tree listing uses SQLite keyset pagination of 1000 snapshot file entries
  per page, with transparent `huggingface_hub` paging via the standard
  `Link rel="next"` header.

## ⚙️ Configuration Reference

### CAS Server Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `XET_HOST` | Server bind address | `127.0.0.1` |
| `XET_PORT` | Server port | `8081` |
| `XET_PUBLIC_BASE_URL` | Public access URL | `http://{host}:{port}` |
| `XET_MAX_BODY_SIZE_MB` | Maximum request body size (MB) | `2048` |
| `XET_INDEX_REBUILD_STRICT` | Fail startup if MetadataIndex rebuild fails | `false` |
| `XET_STORAGE_BACKEND` | Storage backend type | `local` |
| `XET_LOCAL_PATH` | Local storage path (required for the local backend) | `./data` |
| `XET_S3_BUCKET` | S3 bucket name (required for the s3 backend) | - |
| `XET_S3_REGION` | S3 region | - |
| `XET_S3_ENDPOINT` | S3 endpoint URL | - |
| `XET_UPLOAD_TEMP_DIR` | Temp directory for upload staging | automatic |
| `XET_RECONSTRUCTION_TEMP_DIR` | Temp directory for file reconstruction and bounded remote xorb/shard/LFS downloads | automatic |
| `XET_VERIFY_DOWNLOAD_INTEGRITY` | Verify download integrity | `false` |

> **⚠️ Important: S3 Lifecycle Rules**
>
> When using the S3 storage backend, you **must** configure S3 Lifecycle Rules to
> abort incomplete multipart uploads automatically; otherwise they incur ongoing
> storage costs.
>
> **Steps:**
> 1. Edit the bucket's lifecycle rules in the AWS S3 console or with the AWS CLI
> 2. Add a rule: abort incomplete multipart uploads
> 3. Recommended: abort uploads incomplete after 7 days
>
> **AWS CLI example:**
> ```bash
> aws s3api put-bucket-lifecycle-configuration \
>   --bucket your-bucket-name \
>   --lifecycle-configuration '{
>     "Rules": [
>       {
>         "ID": "AbortIncompleteMultipartUploads",
>         "Status": "Enabled",
>         "Filter": {"Prefix": ""},
>         "AbortIncompleteMultipartUpload": {
>           "DaysAfterInitiation": 7
>         }
>       }
>     ]
>   }'
> ```
>
> The process tracks uploads by unique upload ID and aborts them best-effort on
> errors, request cancellation, and normal shutdown; multipart parts can still be
> left behind if the process crashes, the runtime has exited, or the abort itself
> fails. The lifecycle rule is therefore not optional.

| Variable | Description | Default |
|----------|-------------|---------|
| `CAS_PUBLIC_KEYS` | Ordered `kid=/path/to/public.pem` keyring | empty (single-public-key compatibility mode) |
| `CAS_PUBLIC_KEY_PATH` | Single Ed25519 public key path for compatibility mode | `/etc/xet/public-key.pem` |
| `CAS_TRUSTED_KIDS` | Key ID allowlist | all keyring kids; `hub-key-1` in compatibility mode |
| `CAS_PRIVATE_KEY_PATH` | Ed25519 private key path (issues LFS proxy tokens) | empty (compatibility mode; set in production) |
| `CAS_SIGNING_KID` | Key ID used to sign proxy tokens | empty (first trusted mapping in the keyring) |

`CAS_PUBLIC_KEYS=kid1=/path/old-public.pem,kid2=/path/new-public.pem` establishes
a real `kid`-to-public-key mapping; `CAS_TRUSTED_KIDS` is only an allowlist. The
legacy `CAS_PUBLIC_KEY_PATH` is used only when the keyring is unset, and maps
that single public key to every trusted kid. When `CAS_PRIVATE_KEY_PATH` is set,
the private key must match the public key for `CAS_SIGNING_KID`, otherwise CAS
fails to start.

When `CAS_PRIVATE_KEY_PATH` is not configured, the CAS batch API places the
caller's `xet_xxx` token into LFS action headers instead of issuing a
short-lived, single-OID, single-operation `proxy_xxx` token. This widens the
blast radius if an action token leaks; configure the private key in production.

`/health` only indicates that the HTTP server is alive; `/ready` is intended for
load-balancer and orchestrator readiness probes. CAS `/ready` checks the storage
backend and the MetadataIndex rebuild state; Hub `/ready` checks SQLite and CAS
`/ready`. Explicitly set numeric or boolean environment variables that fail to
parse abort startup — there is no silent fallback to defaults; booleans accept
only `true`/`false`/`1`/`0`, effective URLs must be HTTP(S) with a host, and
zero values or inconsistent cross-field limits on upload/download caps, TTLs,
rates, and pool sizes are rejected.

### Hub API Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `HUB_HOST` | Server bind address | `0.0.0.0` |
| `HUB_PORT` | Server port | `8080` |
| `HUB_PUBLIC_BASE_URL` | Public access URL | `http://{host}:{port}` |
| `HUB_PRIVATE_KEY_PATH` | Ed25519 private key path | `private_key.pem` |
| `HUB_KID` | Key identifier | `hub-key-1` |
| `HUB_TOKEN_TTL_SECONDS` | Token lifetime (seconds, range `1..=604800`) | `3600` |
| `HUB_PROXY_TOKEN_TTL_SECONDS` | Proxy token lifetime (seconds, range `1..=604800`) | `300` (5 minutes) |
| `HUB_INTERNAL_TOKEN_TTL_SECONDS` | Internal token lifetime for Hub→CAS internal endpoints (seconds, range `1..=604800`) | `86400` (24 hours) |
| `HUB_SQLITE_PATH` | Metadata database path | `hub.db` |
| `HUB_DB_POOL_SIZE` | SQLite connection pool size | `5` |
| `HUB_CAS_BASE_URL` | CAS server URL | `http://localhost:8081` |
| `HUB_CAS_TIMEOUT_SECS` | CAS request timeout (seconds) | `30` |
| `HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS` | Startup CAS health check timeout (seconds) | `10` |
| `HUB_INLINE_THRESHOLD` | Inline file threshold (bytes) | `1048576` (1MB) |
| `HUB_UPLOAD_TEMP_DIR` | Temp directory for upload staging | `./data/hub-uploads` |
| `HUB_MAX_UPLOAD_SIZE` | Maximum upload size (bytes) | `536870912` (512MB) |
| `HUB_MAX_DOWNLOAD_SIZE` | CAS download size limit (bytes) | `536870912` (512MB) |

**Security**:
| Variable | Description | Default |
|----------|-------------|---------|
| `HUB_TOKEN_HASH_SALT` | Token hash salt (must be identical across multi-instance deployments) | generated |

The Hub API currently offers only the SQLite metadata backend. Schema
initialization and validation run automatically at startup, and SQLite
connections get WAL, foreign keys, and a busy timeout. Multi-instance Hub
deployments must share one SQLite file, use the same `HUB_TOKEN_HASH_SALT` and
Hub signing key, and accept SQLite's single-writer limit; the project currently
ships no distributed database backend such as Postgres/MySQL.

Detailed reference: [Configuration Guide](docs/configuration.md)

## 🧪 Testing

```bash
# Run all tests
cargo test

# Run integration tests
cargo test --test '*'

# Run benchmarks
cargo bench

# Run a specific test
cargo test test_name
```

Test coverage:
- Unit tests: hashing, chunking, formats, storage
- Integration tests: API endpoints, authentication, workflows
- End-to-end tests: full upload/download flows

## 📖 Documentation

- [API docs](docs/api/) - detailed CAS and Hub API references
- [Configuration guide](docs/configuration.md) - full configuration options
- [Architecture](docs/architecture.md) - system architecture and data flows
- [Integration guide](HF_XET_INTEGRATION_GUIDE.md) - HuggingFace integration workflows

## 🤝 Contributing

Contributions are welcome! Please follow these steps:

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

### Development Guide

```bash
# Run in development mode
cargo run --bin xet-server
cargo run --bin hub-api

# Lint
cargo clippy

# Format
cargo fmt
```

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file

## 🙏 Acknowledgments

- [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) - fast cryptographic hashing
- [Actix Web](https://actix.rs/) - high-performance web framework
- [HuggingFace](https://huggingface.co/) - Hub API design reference
- [Git LFS](https://git-lfs.github.com/) - large file storage protocol

## 📞 Support

- 📧 Email: support@example.com
- 💬 Issues: [GitHub Issues](https://github.com/your-org/xet-server/issues)
- 📚 Docs: [full documentation](docs/)
