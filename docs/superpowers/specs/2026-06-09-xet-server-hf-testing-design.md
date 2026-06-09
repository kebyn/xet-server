# Xet Server Upload/Download Testing with Hugging Face Commands

**Date:** 2026-06-09  
**Status:** Design Spec  
**Approach:** Complete Integration Test (Approach 1)

## Overview

This design specifies a complete integration test for the Xet Storage server using Git + hf-xet extension with a subset of Qwen 8B model files. The test validates upload, download, data integrity, and metrics tracking in a real-world workflow.

## Goals

- ✅ Verify files can be uploaded to the Xet server without errors
- ✅ Verify uploaded files can be downloaded back
- ✅ Verify downloaded files match original (data integrity via hash comparison)
- ✅ Verify server metrics are correctly tracked (upload/download counts, bytes)

## Non-Goals

- Performance testing or benchmarking
- Testing with the full Qwen 8B model (16GB+)
- Testing S3 storage backend (using local storage only)
- Testing authentication edge cases (using valid JWT tokens)

## Architecture

### System Components

1. **Xet Server Instance**
   - Runs on `127.0.0.1:8080`
   - Local storage backend at `./test-data/storage`
   - JWT secret: `test-secret-key`
   - Environment variables:
     - `XET_HOST=127.0.0.1`
     - `XET_PORT=8080`
     - `XET_STORAGE_BACKEND=local`
     - `XET_LOCAL_PATH=./test-data/storage`
     - `XET_JWT_SECRET=test-secret-key`

2. **Git Repository**
   - Local Git repository with Git LFS enabled
   - Configured to use Xet server as custom LFS endpoint
   - Authentication via JWT token

3. **Test Data**
   - Subset of Qwen 8B model files:
     - `config.json` (small config file)
     - 1-2 `*.safetensors` weight shards (~2-5GB total)
   - Alternative: small test files (1-10MB) for quick iteration

4. **Validation Tools**
   - SHA256 hash comparison for data integrity
   - curl to query `/metrics` endpoint
   - Git commands to verify repository state

### Data Flow

```
Upload Flow:
User → git add → git commit → git push
  ↓
Git LFS intercepts large files
  ↓
hf-xet extension activated
  ↓
Chunks file using CDC (Content-Defined Chunking)
  ↓
Builds Xorb from chunks
  ↓
POST /v1/xorbs/{prefix}/{hash} (upload xorb)
  ↓
POST /v1/shards (upload metadata shard)
  ↓
Xet server stores in local storage backend
  ↓
Metrics updated (upload count, bytes, latency)

Download Flow:
User → git clone → Git LFS pull
  ↓
hf-xet extension activated
  ↓
GET /v2/reconstructions/{file_id} (get file reconstruction info)
  ↓
Returns list of xorbs needed
  ↓
Download xorbs from storage
  ↓
Reconstruct file from chunks
  ↓
Metrics updated (download count, bytes, latency)
```

## Test Plan

### Phase 1: Server Setup

1. Build the Xet server:
   ```bash
   cargo build --release
   ```

2. Create test data directory:
   ```bash
   mkdir -p test-data/storage
   ```

3. Start the server:
   ```bash
   XET_HOST=127.0.0.1 \
   XET_PORT=8080 \
   XET_STORAGE_BACKEND=local \
   XET_LOCAL_PATH=./test-data/storage \
   XET_JWT_SECRET=test-secret-key \
   ./target/release/xet-server
   ```

4. Verify server is running:
   ```bash
   curl http://127.0.0.1:8080/health
   # Expected: {"status":"ok"}
   ```

### Phase 2: Git Repository Setup

1. Initialize Git repository:
   ```bash
   mkdir test-repo && cd test-repo
   git init
   git lfs install
   ```

2. Configure Git LFS to use Xet server:
   ```bash
   # Create .lfsconfig
   cat > .lfsconfig << EOF
   [lfs]
       url = http://127.0.0.1:8080
   EOF
   ```

3. Configure authentication:
   ```bash
   # Git LFS expects basic auth, but Xet server expects Bearer token
   # Solution: Create a credential helper that returns the JWT token as password
   
   # First, generate JWT token (see JWT Token Generation section below)
   export XET_JWT_TOKEN="your-jwt-token-here"
   
   # Create a credential helper script
   cat > git-credential-xet << 'EOF'
   #!/bin/bash
   case "$1" in
     get)
       echo "username=xet-user"
       echo "password=$XET_JWT_TOKEN"
       ;;
   esac
   EOF
   chmod +x git-credential-xet
   
   # Configure Git to use the credential helper
   git config credential.helper "$(pwd)/git-credential-xet"
   git config lfs.access basic
   git config lfs.url http://127.0.0.1:8080
   ```
   
   **Note:** The Xet server needs to be modified to accept the JWT token as a password in basic auth, or use a proxy that converts basic auth to Bearer tokens. For this test, we'll assume the server is modified to accept basic auth where the password is the JWT token.

4. Track large files with LFS:
   ```bash
   git lfs track "*.safetensors"
   git lfs track "*.bin"
   git add .gitattributes
   git commit -m "Configure LFS tracking"
   ```

### Phase 3: Test Data Preparation

**Option A: Use Qwen 8B subset (recommended for realistic testing)**

1. Download Qwen 8B model (if not already present):
   ```bash
   # Assuming model is at /path/to/qwen-8b
   # Copy subset of files
   cp /path/to/qwen-8b/config.json test-repo/
   cp /path/to/qwen-8b/model-00001-of-00004.safetensors test-repo/
   ```

2. Record original file hashes:
   ```bash
   sha256sum config.json model-00001-of-00004.safetensors > original-hashes.txt
   ```

**Option B: Use small test files (for quick iteration)**

1. Create test files:
   ```bash
   # Create a 5MB test file
   dd if=/dev/urandom of=test-model.bin bs=1M count=5
   sha256sum test-model.bin > original-hashes.txt
   ```

### Phase 4: Upload Test

1. Add files to Git:
   ```bash
   git add config.json model-00001-of-00004.safetensors
   git commit -m "Add model files"
   ```

2. Push to trigger upload:
   ```bash
   # For local testing, we need a bare repository as the remote
   cd ..
   git init --bare test-repo.git
   cd test-repo
   git remote add origin ../test-repo.git
   git push -u origin main
   ```

3. Verify upload on server:
   ```bash
   # Check storage directory
   ls -lh test-data/storage/xorbs/default/
   ls -lh test-data/storage/shards/
   
   # Check metrics
   curl http://127.0.0.1:8080/metrics | grep upload
   ```

4. Expected metrics:
   - `xet_upload_total` should increment
   - `xet_upload_bytes` should show total bytes uploaded
   - `xet_requests_total{status="200"}` should increment

### Phase 5: Download Test

1. Clone repository to new location:
   ```bash
   cd ..
   git clone test-repo test-repo-clone
   cd test-repo-clone
   ```

2. Verify files are downloaded:
   ```bash
   ls -lh
   # Should see config.json and model files
   ```

3. Verify data integrity:
   ```bash
   sha256sum config.json model-00001-of-00004.safetensors > downloaded-hashes.txt
   diff original-hashes.txt downloaded-hashes.txt
   # Expected: no differences
   ```

4. Check download metrics:
   ```bash
   curl http://127.0.0.1:8080/metrics | grep download
   ```

5. Expected metrics:
   - `xet_download_total` should increment
   - `xet_download_bytes` should show total bytes downloaded
   - `xet_requests_total{status="200"}` should increment

### Phase 6: Metrics Validation

1. Query all metrics:
   ```bash
   curl http://127.0.0.1:8080/metrics
   ```

2. Validate key metrics:
   ```
   # Upload metrics
   xet_upload_total 2  # 2 files uploaded
   xet_upload_bytes 5242880000  # ~5GB
   
   # Download metrics
   xet_download_total 2  # 2 files downloaded
   xet_download_bytes 5242880000
   
   # Request metrics
   xet_requests_total{status="200"} 10  # multiple successful requests
   
   # Storage operations
   xet_storage_operations_total 20  # put/get/exists operations
   
   # Latency
   xet_request_latency_seconds_bucket{le="0.1"} 5
   xet_request_latency_seconds_bucket{le="1.0"} 8
   ```

## Success Criteria

The test is successful if ALL of the following are true:

1. ✅ Server starts without errors and responds to health checks
2. ✅ Git LFS is configured correctly
3. ✅ Upload completes without errors (git push succeeds)
4. ✅ Download completes without errors (git clone succeeds)
5. ✅ File hashes match (data integrity verified)
6. ✅ Metrics show correct upload/download counts
7. ✅ Metrics show correct byte counts
8. ✅ Storage backend contains expected files

## Failure Modes and Mitigation

### Failure: hf-xet extension not installed or configured
**Mitigation:** Provide installation instructions for hf-xet extension

### Failure: Git LFS not configured to use Xet server
**Mitigation:** Provide detailed .lfsconfig example and verification steps

### Failure: Authentication fails
**Mitigation:** Provide JWT token generation script and configuration steps

### Failure: Large files cause timeout or memory issues
**Mitigation:** Use small test files (Option B) for initial testing

### Failure: Metrics not tracked correctly
**Mitigation:** Check server logs, verify metrics middleware is registered

## Implementation Notes

### Server Modification Required

The current Xet server implementation only accepts Bearer token authentication. However, Git LFS uses basic authentication by default. To make this test work, we need to modify the server's auth middleware to also accept basic auth where the password is the JWT token.

**Required change in `src/api/auth.rs`:**
```rust
// Add support for basic auth where password is JWT token
pub fn extract_token_from_request(req: &HttpRequest) -> Option<String> {
    // Try Bearer token first
    if let Some(auth_header) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            if auth_str.starts_with("Bearer ") {
                return Some(auth_str[7..].to_string());
            }
            // Try basic auth
            if auth_str.starts_with("Basic ") {
                if let Ok(decoded) = base64::decode(&auth_str[6..]) {
                    if let Ok(credentials) = String::from_utf8(decoded) {
                        // Format: username:password
                        if let Some(password) = credentials.split(':').nth(1) {
                            return Some(password.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}
```

This is a minimal change that allows Git LFS to work with the Xet server.

### JWT Token Generation

For testing, generate a JWT token with:
```python
import jwt
import time

token = jwt.encode({
    "sub": "test-user",
    "scope": "read write",
    "exp": int(time.time()) + 86400  # 24 hours
}, "test-secret-key", algorithm="HS256")
print(token)
```

Or use the server's built-in test token generation if available.

### Git LFS Configuration

The key challenge is configuring Git LFS to use the Xet server. This requires:
1. Setting `lfs.url` to point to the Xet server
2. Configuring authentication (Bearer token)
3. Ensuring hf-xet extension is installed and active

### Storage Verification

After upload, verify storage contents:
```bash
# List xorbs
find test-data/storage/xorbs -type f

# List shards
find test-data/storage/shards -type f

# Check file sizes
du -sh test-data/storage/*
```

## Future Enhancements

- Automated test script that runs all phases
- Integration with CI/CD pipeline
- Performance benchmarking with different file sizes
- Testing with S3 storage backend
- Testing concurrent uploads/downloads
- Testing error scenarios (network failures, invalid tokens)

## References

- Xet Server API documentation
- Git LFS specification
- Hugging Face hf-xet extension documentation
- Qwen 8B model documentation
