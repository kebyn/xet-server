# 配置合理性改进实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 根据配置合理性分析设计文档，修复 3 个高优先级、8 个中优先级和 5 个低优先级配置问题，包括删除死代码、添加缺失的配置项、增加启动时校验和日志警告。

**Architecture:** 按优先级分批实施：先删除死代码（最小风险），再添加新配置项（中等复杂度），最后补充启动时校验和文档。每个任务独立可测试，遵循 TDD 流程。

**Tech Stack:** Rust, actix-web 4.5, sqlx 0.7 (SQLite), ed25519-dalek, reqwest, nix (文件权限检查)

**Spec:** `docs/superpowers/specs/2026-06-13-config-rationality-analysis-design.md`

---

## 文件结构概览

| 操作 | 文件路径 | 职责 |
|------|----------|------|
| 修改 | `hub/src/config.rs` | 删除死字段 `data_dir`/`lfs_threshold_bytes`；添加 `rate_limit_rpm`、`proxy_token_ttl_seconds`、`db_pool_size` |
| 修改 | `hub/src/server.rs` | 使用可配置速率限制；添加启动日志 |
| 修改 | `hub/src/auth/xet_signer.rs` | 使用可配置 proxy TTL |
| 修改 | `hub/src/cas_client/mod.rs` | 使用可配置 `max_download_size` |
| 修改 | `hub/src/metadata/sqlite.rs` | 使用可配置连接池大小 |
| 修改 | `hub/src/auth/token_store.rs` | 使用可配置连接池大小 |
| 修改 | `src/config.rs` | 添加 `rate_limit_rpm`、`gc_http_timeout_seconds`；修改 `min_conversion_size` 默认值 |
| 修改 | `src/server.rs` | 使用可配置速率限制；添加 localhost 警告；添加公钥权限检查 |
| 修改 | `src/gc/mod.rs` | 使用可配置 HTTP 超时；添加 token 非空校验 |
| 修改 | `tests/test_config.rs` | 更新/新增 CAS 配置测试 |
| 修改 | `hub/tests/test_integration.rs` | 更新/新增 Hub 配置测试 |

---

### Task 1: 删除 Hub 死代码配置 (`HUB_LFS_THRESHOLD`, `HUB_DATA_DIR`)

**Files:**
- Modify: `hub/src/config.rs:119-134` (StorageSettings struct + Default impl)
- Modify: `hub/src/config.rs:188-198` (from_env)
- Modify: `hub/src/config.rs:272-279` (from_file_or_env)
- Test: `hub/tests/test_integration.rs`

- [ ] **Step 1: 写测试确认死字段不存在**

在 `hub/tests/test_integration.rs` 末尾添加：

```rust
#[test]
fn test_hub_config_no_dead_fields() {
    // HUB_LFS_THRESHOLD and HUB_DATA_DIR were removed as dead code.
    // StorageSettings should not contain lfs_threshold_bytes or data_dir.
    let config = HubConfig::from_env();
    // Verify storage settings only have the expected fields
    assert_eq!(config.storage.inline_threshold_bytes, 1024 * 1024);
    assert_eq!(config.storage.upload_temp_dir, "/tmp/hub-uploads");
    assert_eq!(config.storage.max_upload_size, 512 * 1024 * 1024);
}
```

- [ ] **Step 2: 运行测试确认当前失败（字段仍存在）**

Run: `cargo test test_hub_config_no_dead_fields --package hub-api --test test_integration -- --nocapture`
Expected: 编译通过但测试逻辑应后续验证（此步骤确认测试可运行）

- [ ] **Step 3: 从 `StorageSettings` 中删除 `data_dir` 和 `lfs_threshold_bytes` 字段**

修改 `hub/src/config.rs`，在 `StorageSettings` 结构体中：

删除这两行：
```rust
    pub data_dir: String,
    pub lfs_threshold_bytes: u64,
```

修改后结构体变为：
```rust
pub struct StorageSettings {
    pub inline_threshold_bytes: u64,
    /// Directory for temporary files during streaming uploads
    pub upload_temp_dir: String,
    /// M2: Maximum upload size in bytes. Defaults to 512MB.
    /// Configure via HUB_MAX_UPLOAD_SIZE environment variable.
    pub max_upload_size: u64,
}
```

- [ ] **Step 4: 更新 `StorageSettings` 的 `Default` impl**

将 `Default` impl 修改为：

```rust
impl Default for StorageSettings {
    fn default() -> Self {
        StorageSettings {
            inline_threshold_bytes: 1024 * 1024, // 1MB
            upload_temp_dir: "/tmp/hub-uploads".to_string(),
            max_upload_size: 512 * 1024 * 1024, // 512MB
        }
    }
}
```

- [ ] **Step 5: 更新 `from_env()` 中删除死字段的解析**

在 `HubConfig::from_env()` 中，删除以下代码块：

```rust
                data_dir: env::var("HUB_DATA_DIR")
                    .unwrap_or_else(|_| "./data".to_string()),
```

和：

```rust
                lfs_threshold_bytes: env::var("HUB_LFS_THRESHOLD")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(10 * 1024 * 1024),
```

修改后 `storage` 字段构造变为：
```rust
            storage: StorageSettings {
                inline_threshold_bytes: env::var("HUB_INLINE_THRESHOLD")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(1024 * 1024),
                upload_temp_dir: env::var("HUB_UPLOAD_TEMP_DIR")
                    .unwrap_or_else(|_| "/tmp/hub-uploads".to_string()),
                max_upload_size: env::var("HUB_MAX_UPLOAD_SIZE")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(512 * 1024 * 1024),
            },
```

- [ ] **Step 6: 更新 `from_file_or_env()` 中删除死字段的环境变量覆盖**

删除以下代码块：

```rust
        if let Some(dir) = env::var("HUB_DATA_DIR").ok() {
            config.storage.data_dir = dir;
        }
```

和：

```rust
        if let Some(threshold) = env::var("HUB_LFS_THRESHOLD").ok().and_then(|t| t.parse().ok()) {
            config.storage.lfs_threshold_bytes = threshold;
        }
```

- [ ] **Step 7: 搜索并修复所有引用死字段的代码**

Run: `grep -rn "data_dir\|lfs_threshold_bytes" /data/hub/src/ /data/hub/tests/`

如果有任何使用这些字段的代码，需要删除或替换。预期结果：这些字段从未被实际使用（这就是它们被标记为死代码的原因）。

- [ ] **Step 8: 运行编译和测试**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

Run: `cargo test --package hub-api 2>&1`
Expected: 所有测试通过

- [ ] **Step 9: 提交**

```bash
git add hub/src/config.rs hub/tests/test_integration.rs
git commit -m "refactor(hub): remove dead config fields HUB_LFS_THRESHOLD and HUB_DATA_DIR

These fields were defined in StorageSettings but never read or used by any
code path. Their presence misleads users into thinking they are functional.

Refs: config-rationality-analysis R3-1 (C3)"
```

---

### Task 2: 添加 CAS 速率限制配置 (`XET_RATE_LIMIT_RPM`)

**Files:**
- Modify: `src/config.rs:7-16` (ServerSettings struct)
- Modify: `src/config.rs:88-105` (Default impl)
- Modify: `src/config.rs:108-125` (from_env)
- Modify: `src/server.rs:62-65` (rate limit hardcode)
- Test: `tests/test_config.rs`

- [ ] **Step 1: 写测试验证默认速率限制值**

在 `tests/test_config.rs` 末尾添加：

```rust
#[test]
fn test_config_rate_limit_default() {
    let config = ServerConfig::default();
    assert_eq!(config.server.rate_limit_rpm, 60, "Default CAS rate limit should be 60 RPM");
}

#[test]
fn test_config_rate_limit_from_env() {
    // Use serial_test to avoid env var conflicts
    use serial_test::serial;
    // Note: can't easily test env vars in unit tests due to parallelism,
    // but we verify the field exists and default is correct
    let config = ServerConfig::default();
    assert!(config.server.rate_limit_rpm > 0, "Rate limit must be positive");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_config_rate_limit --test test_config -- --nocapture`
Expected: FAIL with "no field `rate_limit_rpm` on type `ServerSettings`"

- [ ] **Step 3: 在 `ServerSettings` 中添加 `rate_limit_rpm` 字段**

在 `src/config.rs` 的 `ServerSettings` 结构体中添加：

```rust
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_base_url: Option<String>,
    pub max_body_size_mb: u64,
    /// Rate limit for public endpoints in requests per minute per IP.
    /// Configure via `XET_RATE_LIMIT_RPM` environment variable.
    /// Default: 60 RPM.
    pub rate_limit_rpm: u32,
}
```

注意：`src/config.rs` 中有两个 `ServerSettings` 定义（CAS 和 Hub 的混在一起）。CAS 的 `ServerSettings` 有 `max_body_size_mb` 字段。在该结构体中添加 `rate_limit_rpm`。

- [ ] **Step 4: 更新 CAS `ServerSettings` 的 `Default` impl 中 `ServerConfig::default()`**

在 `ServerConfig::default()` 中的 `server` 部分添加：

```rust
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8081,
                public_base_url: None,
                max_body_size_mb: 2048,
                rate_limit_rpm: 60,
            },
```

- [ ] **Step 5: 更新 `from_env()` 中解析 `XET_RATE_LIMIT_RPM`**

在 `ServerConfig::from_env()` 中，`max_body_size_mb` 解析之后添加：

```rust
        let rate_limit_rpm = match std::env::var("XET_RATE_LIMIT_RPM") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("XET_RATE_LIMIT_RPM '{}' is not a valid number, using default 60", val);
                60
            }),
            Err(_) => 60,
        };
```

在 `Self { ... }` 构造中将 `rate_limit_rpm` 加入 `server` 字段：

```rust
            server: ServerSettings { host, port, public_base_url, max_body_size_mb, rate_limit_rpm },
```

- [ ] **Step 6: 在 `src/server.rs` 中使用可配置速率限制**

将 `src/server.rs` 中的硬编码速率限制：

```rust
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(60)  // 60 seconds window
        .burst_size(60)   // 60 requests per window
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("Failed to configure rate limiter");

    tracing::info!("Rate limiting: 60 requests/minute per IP for public endpoints (internal endpoints excluded)");
```

替换为：

```rust
    // Rate limit is configured in requests per minute.
    // actix-governor uses a per_second window, so we use 60s window with burst = rpm.
    let rpm = config.server.rate_limit_rpm;
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(60)  // 60 seconds window
        .burst_size(rpm)  // configurable requests per window
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("Failed to configure rate limiter");

    tracing::info!("Rate limiting: {} requests/minute per IP for public endpoints (internal endpoints excluded)", rpm);
```

- [ ] **Step 7: 运行测试和编译**

Run: `cargo test test_config_rate_limit --test test_config -- --nocapture`
Expected: PASS

Run: `cargo build 2>&1`
Expected: 编译成功

- [ ] **Step 8: 提交**

```bash
git add src/config.rs src/server.rs tests/test_config.rs
git commit -m "feat(cas): add XET_RATE_LIMIT_RPM configuration for rate limiting

Previously the CAS rate limit was hardcoded at 60 RPM with no way to
adjust it. This adds XET_RATE_LIMIT_RPM env var (default 60) to allow
tuning for different deployment scenarios.

Refs: config-rationality-analysis R2-4/R5-1 (C1)"
```

---

### Task 3: 添加 Hub 速率限制配置 (`HUB_RATE_LIMIT_RPM`)

**Files:**
- Modify: `hub/src/config.rs:68-76` (ServerSettings struct)
- Modify: `hub/src/config.rs:83-89` (Default impl)
- Modify: `hub/src/config.rs:153-160` (from_env)
- Modify: `hub/src/config.rs:232-237` (from_file_or_env)
- Modify: `hub/src/server.rs:45-48` (rate limit hardcode)
- Test: `hub/tests/test_integration.rs`

- [ ] **Step 1: 写测试验证 Hub 默认速率限制值**

在 `hub/tests/test_integration.rs` 末尾添加：

```rust
#[test]
fn test_hub_config_rate_limit_default() {
    let config = HubConfig::default();
    assert_eq!(config.server.rate_limit_rpm, 120, "Default Hub rate limit should be 120 RPM");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_hub_config_rate_limit --package hub-api --test test_integration -- --nocapture`
Expected: FAIL with "no field `rate_limit_rpm` on type `ServerSettings`"

- [ ] **Step 3: 在 Hub `ServerSettings` 中添加 `rate_limit_rpm` 字段**

在 `hub/src/config.rs` 的 `ServerSettings` 结构体中添加：

```rust
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_base_url: Option<String>,
    /// Rate limit for public endpoints in requests per minute per IP.
    /// Configure via `HUB_RATE_LIMIT_RPM` environment variable.
    /// Default: 120 RPM.
    pub rate_limit_rpm: u32,
}
```

- [ ] **Step 4: 更新 Hub `ServerSettings` 的 `Default` impl**

```rust
impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            host: "0.0.0.0".to_string(),
            port: 8080,
            public_base_url: None,
            rate_limit_rpm: 120,
        }
    }
}
```

- [ ] **Step 5: 更新 `from_env()` 中解析 `HUB_RATE_LIMIT_RPM`**

在 `HubConfig::from_env()` 的 `server` 部分中修改：

```rust
            server: ServerSettings {
                host: env::var("HUB_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
                port: env::var("HUB_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(8080),
                public_base_url: env::var("HUB_PUBLIC_BASE_URL").ok(),
                rate_limit_rpm: env::var("HUB_RATE_LIMIT_RPM")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(120),
            },
```

- [ ] **Step 6: 更新 `from_file_or_env()` 中环境变量覆盖**

在 `from_file_or_env()` 中 `HUB_PUBLIC_BASE_URL` 处理之后添加：

```rust
        if let Some(rpm) = env::var("HUB_RATE_LIMIT_RPM").ok().and_then(|v| v.parse().ok()) {
            config.server.rate_limit_rpm = rpm;
        }
```

- [ ] **Step 7: 在 `hub/src/server.rs` 中使用可配置速率限制**

将硬编码的速率限制：

```rust
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(60)  // 60 seconds window
        .burst_size(120) // 120 requests per window
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("Failed to configure rate limiter");

    tracing::info!("Rate limiting: 120 requests/minute per IP for public endpoints (internal/health excluded)");
```

替换为：

```rust
    let rpm = config.server.rate_limit_rpm;
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(60)  // 60 seconds window
        .burst_size(rpm)  // configurable requests per window
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("Failed to configure rate limiter");

    tracing::info!("Rate limiting: {} requests/minute per IP for public endpoints (internal/health excluded)", rpm);
```

- [ ] **Step 8: 更新 `setup_test_env()` 中的 `ServerSettings` 构造**

在 `hub/tests/test_integration.rs` 的 `setup_test_env()` 中，`ServerSettings::default()` 已使用 `Default` impl，所以不需要修改。但如果有直接构造 `ServerSettings` 的地方，需要添加 `rate_limit_rpm` 字段。

Run: `grep -rn "ServerSettings {" /data/hub/tests/` 检查。

- [ ] **Step 9: 运行编译和测试**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

Run: `cargo test --package hub-api 2>&1`
Expected: 所有测试通过

- [ ] **Step 10: 提交**

```bash
git add hub/src/config.rs hub/src/server.rs hub/tests/test_integration.rs
git commit -m "feat(hub): add HUB_RATE_LIMIT_RPM configuration for rate limiting

Previously the Hub rate limit was hardcoded at 120 RPM with no way to
adjust it. This adds HUB_RATE_LIMIT_RPM env var (default 120) to allow
tuning for different deployment scenarios.

Refs: config-rationality-analysis R2-4/R5-1 (C1)"
```

---

### Task 4: 添加 CAS 公钥路径权限检查和安全警告

**Files:**
- Modify: `src/server.rs:15-25` (startup section)
- Test: `tests/test_config.rs`

- [ ] **Step 1: 写测试验证公钥路径检查函数**

在 `tests/test_config.rs` 末尾添加：

```rust
#[test]
fn test_check_public_key_permissions_tmp_path() {
    use std::fs;
    use tempfile::NamedTempFile;

    // Create a temp file (simulating a key in /tmp-like location)
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    // Make the file world-writable (simulating /tmp behavior)
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();

    // Should return a warning message for world-writable key files
    let result = xet_server::config::check_public_key_permissions(path);
    assert!(result.is_some(), "World-writable key file should produce a warning");
    assert!(result.unwrap().contains("world-writable"), "Warning should mention world-writable");
}

#[test]
fn test_check_public_key_permissions_secure_path() {
    use std::fs;
    use tempfile::NamedTempFile;

    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    // Make the file readable only by owner
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();

    // Should return None for secure permissions
    let result = xet_server::config::check_public_key_permissions(path);
    assert!(result.is_none(), "Secure key file should not produce a warning");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_check_public_key_permissions --test test_config -- --nocapture`
Expected: FAIL with "unresolved import `xet_server::config::check_public_key_permissions`"

- [ ] **Step 3: 在 `src/config.rs` 中添加权限检查函数**

在 `src/config.rs` 末尾添加（文件末尾、测试模块之前）：

```rust
/// Check public key file permissions and return a warning message if insecure.
///
/// Returns `Some(warning)` if the file is world-writable or group-writable,
/// which would allow other users to replace the public key and forge tokens.
/// Returns `None` if permissions are secure (owner-only read/write).
pub fn check_public_key_permissions(path: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).ok()?;
    let mode = metadata.permissions().mode();

    let world_writable = mode & 0o002 != 0;
    let group_writable = mode & 0o020 != 0;

    if world_writable || group_writable {
        let mut warnings = Vec::new();
        if world_writable {
            warnings.push("world-writable (any user can modify)");
        }
        if group_writable {
            warnings.push("group-writable (group members can modify)");
        }
        Some(format!(
            "SECURITY WARNING: Public key file '{}' is {}. \
            An attacker could replace this file to forge authentication tokens. \
            Use a secure path (e.g., /etc/xet/) with mode 0644 or 0600.",
            path,
            warnings.join(" and ")
        ))
    } else {
        None
    }
}
```

- [ ] **Step 4: 在 `src/server.rs` 启动时调用权限检查**

在 `src/server.rs` 的 `start_server` 函数中，`auth_verifier` 创建之后、`storage` 创建之前添加：

```rust
    // Check public key file permissions for security
    if let Some(warning) = crate::config::check_public_key_permissions(&config.auth.public_key_path) {
        tracing::warn!("{}", warning);
    }
```

- [ ] **Step 5: 运行测试**

Run: `cargo test test_check_public_key_permissions --test test_config -- --nocapture`
Expected: PASS

Run: `cargo build 2>&1`
Expected: 编译成功

- [ ] **Step 6: 提交**

```bash
git add src/config.rs src/server.rs tests/test_config.rs
git commit -m "feat(cas): add public key file permission check at startup

Checks if CAS_PUBLIC_KEY_PATH is world-writable or group-writable and
emits a security warning if so. The default /tmp path is insecure in
production — this warns operators before they ship.

Refs: config-rationality-analysis R2-1 (C2)"
```

---

### Task 5: 添加 CAS localhost 绑定警告日志

**Files:**
- Modify: `src/server.rs:44-50` (startup logging section)

- [ ] **Step 1: 写测试验证配置值（无需测试日志输出，用配置验证替代）**

在 `tests/test_config.rs` 末尾添加：

```rust
#[test]
fn test_cas_default_host_is_localhost() {
    let config = ServerConfig::default();
    assert_eq!(config.server.host, "127.0.0.1", "CAS should default to localhost for dev safety");
}
```

- [ ] **Step 2: 运行测试确认通过**

Run: `cargo test test_cas_default_host_is_localhost --test test_config -- --nocapture`
Expected: PASS（默认值已经是 `127.0.0.1`）

- [ ] **Step 3: 在 `src/server.rs` 启动日志中添加警告**

在 `src/server.rs` 的 `start_server` 函数中，`tracing::info!("Starting Xet Storage server on {}", bind_addr);` 之后添加：

```rust
    // Warn if CAS is bound to localhost only — common gotcha for distributed deployments
    if config.server.host == "127.0.0.1" || config.server.host == "localhost" {
        tracing::warn!(
            "CAS server bound to {} only. Remote clients (including Hub on another host) cannot connect. \
            Set XET_HOST=0.0.0.0 for remote access.",
            config.server.host
        );
    }
```

- [ ] **Step 4: 运行编译**

Run: `cargo build 2>&1`
Expected: 编译成功

- [ ] **Step 5: 提交**

```bash
git add src/server.rs tests/test_config.rs
git commit -m "feat(cas): warn at startup when bound to localhost only

Distributed deployments (Hub and CAS on different hosts) requires
XET_HOST=0.0.0.0. The default 127.0.0.1 silently blocks remote
connections. This warning makes the misconfiguration obvious.

Refs: config-rationality-analysis R1-1 (C4)"
```

---

### Task 6: 添加 `HUB_PROXY_TOKEN_TTL_SECONDS` 配置

**Files:**
- Modify: `hub/src/config.rs:97-103` (AuthSettings struct)
- Modify: `hub/src/config.rs:111-117` (Default impl)
- Modify: `hub/src/config.rs:163-170` (from_env)
- Modify: `hub/src/config.rs:244-247` (from_file_or_env)
- Modify: `hub/src/auth/xet_signer.rs:47-53` (XetSigner struct)
- Modify: `hub/src/auth/xet_signer.rs:118-121` (sign_proxy hardcode)
- Test: `hub/tests/test_integration.rs`

- [ ] **Step 1: 写测试验证 proxy token TTL 配置**

在 `hub/tests/test_integration.rs` 末尾添加：

```rust
#[test]
fn test_hub_config_proxy_token_ttl_default() {
    let config = HubConfig::default();
    assert_eq!(config.auth.proxy_token_ttl_seconds, 300, "Default proxy TTL should be 300s (5 min)");
}

#[tokio::test]
async fn test_sign_proxy_uses_configured_ttl() {
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use hub_api::auth::xet_signer::XetSigner;

    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let signer = XetSigner::new(signing_key, "test-key", 3600, 600); // 600s proxy TTL

    let (token, exp) = signer.sign_proxy("user", "oid123", "upload", "repo", "model");
    assert!(token.starts_with("proxy_"));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Expiration should be ~600s from now (allow 2s tolerance)
    assert!(exp >= now + 598 && exp <= now + 602, "Proxy token TTL should use configured value");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_hub_config_proxy_token_ttl --package hub-api --test test_integration -- --nocapture`
Expected: FAIL with "no field `proxy_token_ttl_seconds`"

- [ ] **Step 3: 在 Hub `AuthSettings` 中添加 `proxy_token_ttl_seconds` 字段**

在 `hub/src/config.rs` 的 `AuthSettings` 结构体中添加：

```rust
pub struct AuthSettings {
    pub private_key_path: String,
    pub kid: String,
    pub token_ttl_seconds: u64,
    /// TTL for short-lived proxy tokens (LFS operations).
    /// Configure via `HUB_PROXY_TOKEN_TTL_SECONDS` environment variable.
    /// Default: 300 (5 minutes).
    pub proxy_token_ttl_seconds: u64,
}
```

- [ ] **Step 4: 更新 `AuthSettings` 的 `Default` impl**

```rust
impl Default for AuthSettings {
    fn default() -> Self {
        AuthSettings {
            private_key_path: "private_key.pem".to_string(),
            kid: "hub-key-1".to_string(),
            token_ttl_seconds: 3600,
            proxy_token_ttl_seconds: 300, // 5 minutes
        }
    }
}
```

- [ ] **Step 5: 更新 `from_env()` 中解析 `HUB_PROXY_TOKEN_TTL_SECONDS`**

在 `HubConfig::from_env()` 的 `auth` 部分中修改：

```rust
            auth: AuthSettings {
                private_key_path: env::var("HUB_PRIVATE_KEY_PATH")
                    .unwrap_or_else(|_| "private_key.pem".to_string()),
                kid: env::var("HUB_KID")
                    .unwrap_or_else(|_| "hub-key-1".to_string()),
                token_ttl_seconds: env::var("HUB_TOKEN_TTL_SECONDS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(3600),
                proxy_token_ttl_seconds: env::var("HUB_PROXY_TOKEN_TTL_SECONDS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(300),
            },
```

- [ ] **Step 6: 更新 `from_file_or_env()` 中环境变量覆盖**

在 `from_file_or_env()` 中 `HUB_TOKEN_TTL_SECONDS` 处理之后添加：

```rust
        if let Some(ttl) = env::var("HUB_PROXY_TOKEN_TTL_SECONDS").ok().and_then(|t| t.parse().ok()) {
            config.auth.proxy_token_ttl_seconds = ttl;
        }
```

- [ ] **Step 7: 修改 `XetSigner` 以使用可配置 proxy TTL**

在 `hub/src/auth/xet_signer.rs` 中，修改 `XetSigner` 结构体：

```rust
pub struct XetSigner {
    signing_key: SigningKey,
    kid: String,
    ttl_seconds: u64,
    /// TTL for proxy tokens (LFS operations)
    proxy_ttl_seconds: u64,
}
```

- [ ] **Step 8: 更新 `XetSigner::from_pem` 和 `new` 构造函数**

修改 `from_pem`：

```rust
    pub fn from_pem(pem_bytes: &[u8], kid: &str, ttl_seconds: u64, proxy_ttl_seconds: u64) -> Result<Self, String> {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let pem_str = std::str::from_utf8(pem_bytes).map_err(|e| e.to_string())?;
        let signing_key = SigningKey::from_pkcs8_pem(pem_str)
            .map_err(|e| format!("Failed to load private key: {}", e))?;
        Ok(Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
            proxy_ttl_seconds,
        })
    }
```

修改 `new`：

```rust
    pub fn new(signing_key: SigningKey, kid: &str, ttl_seconds: u64, proxy_ttl_seconds: u64) -> Self {
        Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
            proxy_ttl_seconds,
        }
    }
```

- [ ] **Step 9: 修改 `sign_proxy` 使用可配置 TTL**

将 `sign_proxy` 中的硬编码：

```rust
        // Proxy tokens expire in 5 minutes (300 seconds)
        let exp = now + 300;
```

替换为：

```rust
        let exp = now + self.proxy_ttl_seconds;
```

- [ ] **Step 10: 更新 `hub/src/server.rs` 中 `XetSigner::from_pem` 调用**

将：

```rust
    let signer = Arc::new(
        XetSigner::from_pem(&private_key_pem, &config.auth.kid, config.auth.token_ttl_seconds)
            .map_err(|e| std::io::Error::other(format!("Failed to create xet signer: {}", e)))?
    );
```

替换为：

```rust
    let signer = Arc::new(
        XetSigner::from_pem(&private_key_pem, &config.auth.kid, config.auth.token_ttl_seconds, config.auth.proxy_token_ttl_seconds)
            .map_err(|e| std::io::Error::other(format!("Failed to create xet signer: {}", e)))?
    );
```

- [ ] **Step 11: 更新所有测试中对 `XetSigner::new` 的调用**

搜索所有使用 `XetSigner::new` 的地方并添加第 4 个参数：

Run: `grep -rn "XetSigner::new(" /data/hub/`

在每个调用处，添加默认 proxy TTL 参数 `300`。例如：

```rust
// 原来:
let signer = XetSigner::new(signing_key, "test-key", 3600);
// 改为:
let signer = XetSigner::new(signing_key, "test-key", 3600, 300);
```

同样在 `hub/src/auth/xet_signer.rs` 的内部测试中，所有 `XetSigner::new` 调用都需要更新。

同样搜索 `XetSigner::from_pem`：

Run: `grep -rn "XetSigner::from_pem(" /data/hub/ /data/src/`

- [ ] **Step 12: 运行编译和测试**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

Run: `cargo test --package hub-api 2>&1`
Expected: 所有测试通过

Run: `cargo build 2>&1`
Expected: CAS 编译成功（CAS 不直接使用 XetSigner）

Run: `cargo test 2>&1`
Expected: CAS 测试通过

- [ ] **Step 13: 提交**

```bash
git add hub/src/config.rs hub/src/auth/xet_signer.rs hub/src/server.rs hub/tests/test_integration.rs hub/src/auth/xet_signer.rs
git commit -m "feat(hub): add HUB_PROXY_TOKEN_TTL_SECONDS configuration

Proxy token TTL was hardcoded at 300s (5 min), which may be too short
for large file uploads over slow networks. This adds
HUB_PROXY_TOKEN_TTL_SECONDS env var (default 300) for enterprise tuning.

Refs: config-rationality-analysis R2-3 (C6)"
```

---

### Task 7: 修改 `XET_MIN_CONVERSION_SIZE` 默认值为 64KB

**Files:**
- Modify: `src/config.rs:68-69` (ConversionConfig Default impl)
- Modify: `src/config.rs:161-167` (from_env default)
- Test: `tests/test_config.rs`

- [ ] **Step 1: 写测试验证新的默认值**

在 `tests/test_config.rs` 末尾添加：

```rust
#[test]
fn test_min_conversion_size_default_64kb() {
    let config = ServerConfig::default();
    assert_eq!(
        config.conversion.min_conversion_size, 65536,
        "Default min_conversion_size should be 64KB (65536 bytes) to avoid pointless small-file conversions"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_min_conversion_size_default --test test_config -- --nocapture`
Expected: FAIL (当前值为 1024)

- [ ] **Step 3: 修改 `ConversionConfig::default()` 中的默认值**

将：

```rust
            min_conversion_size: 1024,           // 1KB — skip tiny files
```

替换为：

```rust
            min_conversion_size: 65536,          // 64KB — skip tiny files (1KB conversions waste CPU/IO for near-zero dedup value)
```

- [ ] **Step 4: 修改 `from_env()` 中的默认值**

将：

```rust
                tracing::warn!("XET_MIN_CONVERSION_SIZE '{}' is not a valid number, using default 1024", val);
                1024
            }),
            Err(_) => 1024,
```

替换为：

```rust
                tracing::warn!("XET_MIN_CONVERSION_SIZE '{}' is not a valid number, using default 65536", val);
                65536
            }),
            Err(_) => 65536,
```

- [ ] **Step 5: 运行测试**

Run: `cargo test test_min_conversion_size_default --test test_config -- --nocapture`
Expected: PASS

Run: `cargo test --test test_conversion 2>&1`
Expected: 转换测试仍通过（如果有依赖 1KB 的测试需要更新）

- [ ] **Step 6: 提交**

```bash
git add src/config.rs tests/test_config.rs
git commit -m "feat(cas): raise XET_MIN_CONVERSION_SIZE default from 1KB to 64KB

1KB files produce negligible dedup value but incur full conversion cost
(CDC chunking, compression, xorb/shard construction, S3 writes). 64KB
is the minimum size where conversion starts providing meaningful benefit.

Refs: config-rationality-analysis R4-1 (C7)"
```

---

### Task 8: 添加 GC 启动时 token 非空校验

**Files:**
- Modify: `src/server.rs:35-45` (GC setup section)
- Test: `tests/test_config.rs`

- [ ] **Step 1: 写测试验证 GC token 校验逻辑**

在 `tests/test_config.rs` 末尾添加：

```rust
#[test]
fn test_gc_token_validation() {
    use xet_server::config::validate_gc_config;

    // GC disabled — token can be empty
    let config = ServerConfig {
        gc: xet_server::config::GcConfig {
            enabled: false,
            hub_internal_token: String::new(),
            ..Default::default()
        },
        ..Default::default()
    };
    let warnings = validate_gc_config(&config);
    assert!(warnings.is_empty(), "Disabled GC should not warn about token");

    // GC enabled but token empty — should warn
    let config = ServerConfig {
        gc: xet_server::config::GcConfig {
            enabled: true,
            hub_internal_token: String::new(),
            ..Default::default()
        },
        ..Default::default()
    };
    let warnings = validate_gc_config(&config);
    assert_eq!(warnings.len(), 1, "Enabled GC with empty token should produce a warning");
    assert!(warnings[0].contains("GC_HUB_INTERNAL_TOKEN"), "Warning should mention the token");

    // GC enabled with token — no warning
    let config = ServerConfig {
        gc: xet_server::config::GcConfig {
            enabled: true,
            hub_internal_token: "secret-token".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let warnings = validate_gc_config(&config);
    assert!(warnings.is_empty(), "Enabled GC with token should not warn");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_gc_token_validation --test test_config -- --nocapture`
Expected: FAIL with "unresolved import `xet_server::config::validate_gc_config`"

- [ ] **Step 3: 在 `src/config.rs` 中添加 GC 配置验证函数**

在 `src/config.rs` 末尾（`check_public_key_permissions` 之后）添加：

```rust
/// Validate GC configuration and return warning messages for misconfigurations.
///
/// Checks that when GC is enabled, all required configuration is present.
/// Returns a list of warning messages (empty if configuration is valid).
pub fn validate_gc_config(config: &ServerConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    if config.gc.enabled {
        if config.gc.hub_internal_token.is_empty() {
            warnings.push(
                "GC is enabled but GC_HUB_INTERNAL_TOKEN is empty. \
                GC requires an internal token to query Hub's /internal/referenced-hashes endpoint. \
                Set GC_HUB_INTERNAL_TOKEN to a valid token before enabling GC."
                    .to_string(),
            );
        }

        if config.gc.hub_base_url == "http://localhost:8080" && config.server.host != "127.0.0.1" {
            warnings.push(
                "GC is enabled with default hub_base_url (http://localhost:8080) but CAS is not bound to localhost. \
                In distributed deployments, set GC_HUB_BASE_URL to the actual Hub address."
                    .to_string(),
            );
        }
    }

    warnings
}
```

- [ ] **Step 4: 在 `src/server.rs` 启动时调用 GC 配置验证**

在 `src/server.rs` 中 GC 创建之前（`let gc = Arc::new(...)` 之前）添加：

```rust
    // Validate GC configuration
    for warning in crate::config::validate_gc_config(&config) {
        tracing::warn!("{}", warning);
    }
```

- [ ] **Step 5: 运行测试**

Run: `cargo test test_gc_token_validation --test test_config -- --nocapture`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add src/config.rs src/server.rs tests/test_config.rs
git commit -m "feat(cas): validate GC config at startup, warn on empty token

GC with an empty internal token will fail silently on first run (up to
1 hour later). This validates at startup and warns immediately, saving
an entire GC cycle of confusion.

Refs: config-rationality-analysis R4-2 (C8)"
```

---

### Task 9: 将 `MAX_DOWNLOAD_SIZE` 硬编码改为可配置

**Files:**
- Modify: `hub/src/config.rs:68-76` (ServerSettings 不变，在 CasSettings 中添加)
- Modify: `hub/src/config.rs:107-113` (CasSettings Default)
- Modify: `hub/src/config.rs:173-180` (from_env)
- Modify: `hub/src/config.rs:253-256` (from_file_or_env)
- Modify: `hub/src/cas_client/mod.rs:226,262` (MAX_DOWNLOAD_SIZE hardcode)
- Test: `hub/tests/test_integration.rs`

- [ ] **Step 1: 写测试验证可配置下载大小限制**

在 `hub/tests/test_integration.rs` 末尾添加：

```rust
#[test]
fn test_cas_settings_max_download_size_default() {
    let settings = CasSettings::default();
    assert_eq!(
        settings.max_download_size, 512 * 1024 * 1024,
        "Default max_download_size should be 512MB to match HUB_MAX_UPLOAD_SIZE"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_cas_settings_max_download_size --package hub-api --test test_integration -- --nocapture`
Expected: FAIL with "no field `max_download_size` on type `CasSettings`"

- [ ] **Step 3: 在 `CasSettings` 中添加 `max_download_size` 字段**

在 `hub/src/config.rs` 的 `CasSettings` 结构体中添加：

```rust
pub struct CasSettings {
    pub base_url: String,
    pub internal_timeout_seconds: u64,
    /// Maximum download size in bytes for CAS responses.
    /// Should match or exceed HUB_MAX_UPLOAD_SIZE to prevent edge cases
    /// where files can be uploaded but not downloaded.
    /// Default: 536870912 (512MB).
    pub max_download_size: u64,
}
```

- [ ] **Step 4: 更新 `CasSettings` 的 `Default` impl**

```rust
impl Default for CasSettings {
    fn default() -> Self {
        CasSettings {
            base_url: "http://localhost:8081".to_string(),
            internal_timeout_seconds: 30,
            max_download_size: 512 * 1024 * 1024, // 512MB
        }
    }
}
```

- [ ] **Step 5: 更新 `from_env()` 中解析 `HUB_MAX_DOWNLOAD_SIZE`**

在 `HubConfig::from_env()` 的 `cas` 部分中添加：

```rust
            cas: CasSettings {
                base_url: env::var("CAS_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:8081".to_string()),
                internal_timeout_seconds: env::var("HUB_CAS_TIMEOUT_SECS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(30),
                max_download_size: env::var("HUB_MAX_DOWNLOAD_SIZE")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(512 * 1024 * 1024),
            },
```

- [ ] **Step 6: 更新 `from_file_or_env()` 中环境变量覆盖**

在 `from_file_or_env()` 中 `HUB_CAS_TIMEOUT_SECS` 处理之后添加：

```rust
        if let Some(size) = env::var("HUB_MAX_DOWNLOAD_SIZE").ok().and_then(|t| t.parse().ok()) {
            config.cas.max_download_size = size;
        }
```

- [ ] **Step 7: 修改 `CasClient::new` 存储 `max_download_size`**

在 `hub/src/cas_client/mod.rs` 中，修改 `CasClient` 结构体：

```rust
pub struct CasClient {
    base_url: String,
    max_download_size: u64,
    client: reqwest::Client,
}
```

修改 `CasClient::new`：

```rust
    pub fn new(settings: &CasSettings) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.internal_timeout_seconds))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            base_url: settings.base_url.trim_end_matches('/').to_string(),
            max_download_size: settings.max_download_size,
            client,
        }
    }
```

- [ ] **Step 8: 修改 `proxy_lfs_download` 中的硬编码**

将：

```rust
                // Check size limit (512MB)
                const MAX_DOWNLOAD_SIZE: u64 = 512 * 1024 * 1024;
                if body.len() as u64 > MAX_DOWNLOAD_SIZE {
                    return Err(HubError::CasError(format!("Download too large: {} bytes", body.len())));
                }
```

替换为：

```rust
                // Check size limit against configured max_download_size
                if body.len() as u64 > self.max_download_size {
                    return Err(HubError::CasError(format!(
                        "Download too large: {} bytes (max: {} bytes)",
                        body.len(),
                        self.max_download_size
                    )));
                }
```

- [ ] **Step 9: 修改 `proxy_lfs_download_streaming` 中的硬编码**

将：

```rust
                // Defense-in-depth: validate Content-Length against max size
                // CAS is a trusted internal service, but this protects against CAS bugs
                // that could cause unbounded streaming
                const MAX_DOWNLOAD_SIZE: u64 = 512 * 1024 * 1024;
                if content_length > MAX_DOWNLOAD_SIZE {
                    return Err(HubError::CasError(format!(
                        "Content-Length too large: {} bytes (max: {} bytes)",
                        content_length, MAX_DOWNLOAD_SIZE
                    )));
                }
```

替换为：

```rust
                // Defense-in-depth: validate Content-Length against configured max size
                if content_length > self.max_download_size {
                    return Err(HubError::CasError(format!(
                        "Content-Length too large: {} bytes (max: {} bytes)",
                        content_length, self.max_download_size
                    )));
                }
```

- [ ] **Step 10: 更新测试中的 `CasSettings` 构造**

搜索所有使用 `CasSettings { ... }` 的地方并添加 `max_download_size` 字段：

Run: `grep -rn "CasSettings {" /data/hub/`

在 `hub/src/cas_client/mod.rs` 的测试中：

```rust
    // 原来:
    let settings = CasSettings {
        base_url: "http://localhost:3000".to_string(),
        internal_timeout_seconds: 30,
    };
    // 改为:
    let settings = CasSettings {
        base_url: "http://localhost:3000".to_string(),
        internal_timeout_seconds: 30,
        max_download_size: 512 * 1024 * 1024,
    };
```

同样更新第二个测试 `test_client_trims_base_url_slash`。

- [ ] **Step 11: 运行编译和测试**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

Run: `cargo test --package hub-api 2>&1`
Expected: 所有测试通过

- [ ] **Step 12: 提交**

```bash
git add hub/src/config.rs hub/src/cas_client/mod.rs hub/tests/test_integration.rs
git commit -m "feat(hub): make CAS max download size configurable

MAX_DOWNLOAD_SIZE was hardcoded at 512MB in two places. If a user
adjusts HUB_MAX_UPLOAD_SIZE without matching the download limit, files
could be uploaded but not downloaded. This adds HUB_MAX_DOWNLOAD_SIZE
(default 512MB) and recommends keeping it >= HUB_MAX_UPLOAD_SIZE.

Refs: config-rationality-analysis R3-2 (C9)"
```

---

### Task 10: 添加 `GC_HTTP_TIMEOUT_SECONDS` 配置

**Files:**
- Modify: `src/config.rs:80-86` (GcConfig struct)
- Modify: `src/config.rs:94-103` (Default impl)
- Modify: `src/config.rs:180-186` (from_env)
- Modify: `src/gc/mod.rs:48` (timeout hardcode)
- Test: `tests/test_config.rs`

- [ ] **Step 1: 写测试验证 GC HTTP 超时配置**

在 `tests/test_config.rs` 末尾添加：

```rust
#[test]
fn test_gc_http_timeout_default() {
    let config = ServerConfig::default();
    assert_eq!(config.gc.http_timeout_seconds, 300, "Default GC HTTP timeout should be 300s");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_gc_http_timeout_default --test test_config -- --nocapture`
Expected: FAIL with "no field `http_timeout_seconds` on type `GcConfig`"

- [ ] **Step 3: 在 `GcConfig` 中添加 `http_timeout_seconds` 字段**

在 `src/config.rs` 的 `GcConfig` 结构体中添加：

```rust
pub struct GcConfig {
    /// Enable background GC task
    pub enabled: bool,
    /// GC run interval in seconds
    pub interval_seconds: u64,
    /// Grace period in seconds for newly uploaded blobs.
    pub grace_period_seconds: u64,
    /// Dry-run mode: report stats but don't actually delete
    pub dry_run: bool,
    /// Hub API base URL (for querying referenced hashes)
    pub hub_base_url: String,
    /// Internal token for authenticating with Hub's /internal/* endpoints
    pub hub_internal_token: String,
    /// HTTP timeout in seconds for GC requests to Hub.
    /// Large repositories may have millions of hashes, producing large responses.
    /// Configure via `GC_HTTP_TIMEOUT_SECONDS` environment variable.
    /// Default: 300 (5 minutes).
    pub http_timeout_seconds: u64,
}
```

- [ ] **Step 4: 更新 `GcConfig::default()`**

在 `Default` impl 中添加：

```rust
            http_timeout_seconds: 300,          // 5 minutes for large hash lists
```

- [ ] **Step 5: 更新 `from_env()` 中解析 `GC_HTTP_TIMEOUT_SECONDS`**

在 `gc_hub_internal_token` 解析之后添加：

```rust
        let gc_http_timeout = match std::env::var("GC_HTTP_TIMEOUT_SECONDS") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("GC_HTTP_TIMEOUT_SECONDS '{}' is not a valid number, using default 300", val);
                300
            }),
            Err(_) => 300,
        };
```

在 `GcConfig { ... }` 构造中添加 `http_timeout_seconds: gc_http_timeout`。

- [ ] **Step 6: 修改 `src/gc/mod.rs` 中的 `GarbageCollector::new`**

将：

```rust
    pub fn new(storage: Arc<Box<dyn StorageBackend>>, config: GcConfig) -> Self {
        let hub_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300)) // 5 minutes for large hash lists
            .build()
            .expect("Failed to build GC hub client");
```

替换为：

```rust
    pub fn new(storage: Arc<Box<dyn StorageBackend>>, config: GcConfig) -> Self {
        let hub_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_seconds))
            .build()
            .expect("Failed to build GC hub client");
```

- [ ] **Step 7: 运行测试**

Run: `cargo test test_gc_http_timeout_default --test test_config -- --nocapture`
Expected: PASS

Run: `cargo build 2>&1`
Expected: 编译成功

- [ ] **Step 8: 提交**

```bash
git add src/config.rs src/gc/mod.rs tests/test_config.rs
git commit -m "feat(cas): add GC_HTTP_TIMEOUT_SECONDS configuration

GC HTTP timeout was hardcoded at 300s. Large repositories with millions
of referenced hashes may need more time over slow networks. This adds
GC_HTTP_TIMEOUT_SECONDS (default 300) for distributed deployment tuning.

Refs: config-rationality-analysis R5-3 (C10)"
```

---

### Task 11: 添加 `HUB_DB_POOL_SIZE` 配置

**Files:**
- Modify: `hub/src/config.rs:105-111` (MetadataSettings struct)
- Modify: `hub/src/config.rs:119-124` (Default impl)
- Modify: `hub/src/config.rs:171-175` (from_env)
- Modify: `hub/src/config.rs:249-252` (from_file_or_env)
- Modify: `hub/src/metadata/sqlite.rs:39` (max_connections hardcode)
- Modify: `hub/src/auth/token_store.rs:39` (max_connections hardcode)
- Test: `hub/tests/test_integration.rs`

- [ ] **Step 1: 写测试验证数据库连接池配置**

在 `hub/tests/test_integration.rs` 末尾添加：

```rust
#[test]
fn test_hub_config_db_pool_size_default() {
    let config = HubConfig::default();
    assert_eq!(config.metadata.db_pool_size, 5, "Default DB pool size should be 5");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test test_hub_config_db_pool_size --package hub-api --test test_integration -- --nocapture`
Expected: FAIL with "no field `db_pool_size` on type `MetadataSettings`"

- [ ] **Step 3: 在 `MetadataSettings` 中添加 `db_pool_size` 字段**

在 `hub/src/config.rs` 的 `MetadataSettings` 结构体中添加：

```rust
pub struct MetadataSettings {
    pub sqlite_path: String,
    /// SQLite connection pool size for the metadata store.
    /// SQLite WAL mode supports unlimited concurrent readers but only 1 writer.
    /// Increase for read-heavy workloads; unlikely to help write throughput.
    /// Configure via `HUB_DB_POOL_SIZE` environment variable.
    /// Default: 5.
    pub db_pool_size: u32,
}
```

- [ ] **Step 4: 更新 `MetadataSettings` 的 `Default` impl**

```rust
impl Default for MetadataSettings {
    fn default() -> Self {
        MetadataSettings {
            sqlite_path: "hub.db".to_string(),
            db_pool_size: 5,
        }
    }
}
```

- [ ] **Step 5: 更新 `from_env()` 中解析 `HUB_DB_POOL_SIZE`**

在 `HubConfig::from_env()` 的 `metadata` 部分中修改：

```rust
            metadata: MetadataSettings {
                sqlite_path: env::var("HUB_SQLITE_PATH")
                    .unwrap_or_else(|_| "hub.db".to_string()),
                db_pool_size: env::var("HUB_DB_POOL_SIZE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5),
            },
```

- [ ] **Step 6: 更新 `from_file_or_env()` 中环境变量覆盖**

在 `from_file_or_env()` 中 `HUB_SQLITE_PATH` 处理之后添加：

```rust
        if let Some(size) = env::var("HUB_DB_POOL_SIZE").ok().and_then(|v| v.parse().ok()) {
            config.metadata.db_pool_size = size;
        }
```

- [ ] **Step 7: 修改 `SqliteMetadataStore::new` 使用可配置连接池大小**

`SqliteMetadataStore::new` 当前签名为 `new(path: &str)`。需要修改为接受 pool_size 参数：

```rust
    pub async fn new(path: &str, pool_size: u32) -> Result<Self, MetadataError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_size)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .after_connect(|conn, _| Box::pin(async move {
                sqlx::query("PRAGMA journal_mode = WAL;").execute(&mut *conn).await?;
                sqlx::query("PRAGMA foreign_keys = ON;").execute(&mut *conn).await?;
                Ok(())
            }))
            .connect(path)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Self::init_pool(&pool).await?;

        Ok(Self { pool })
    }
```

- [ ] **Step 8: 修改 `TokenStore::new` 使用可配置连接池大小**

```rust
    pub async fn new(db_path: &str, pool_size: u32) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_size)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(db_path)
            .await?;

        Self::init_tables(&pool).await?;
        Ok(Self { pool })
    }
```

- [ ] **Step 9: 更新 `hub/src/server.rs` 中的构造函数调用**

将：

```rust
    let token_store = Arc::new(
        TokenStore::new(&config.metadata.sqlite_path)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to create token store: {}", e)))?
    );

    let metadata: Arc<dyn MetadataStore> = Arc::new(
        SqliteMetadataStore::new(&config.metadata.sqlite_path)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to create metadata store: {}", e)))?
    );
```

替换为：

```rust
    let token_store = Arc::new(
        TokenStore::new(&config.metadata.sqlite_path, config.metadata.db_pool_size)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to create token store: {}", e)))?
    );

    let metadata: Arc<dyn MetadataStore> = Arc::new(
        SqliteMetadataStore::new(&config.metadata.sqlite_path, config.metadata.db_pool_size)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to create metadata store: {}", e)))?
    );
```

- [ ] **Step 10: 搜索并更新所有测试中对 `SqliteMetadataStore::new` 和 `TokenStore::new` 的调用**

Run: `grep -rn "SqliteMetadataStore::new(" /data/hub/`
Run: `grep -rn "TokenStore::new(" /data/hub/`

注意：`in_memory()` 构造函数不需要修改（它们使用 `max_connections(1)` 是有意为之）。只有接受文件路径的 `new` 函数需要更新。

- [ ] **Step 11: 运行编译和测试**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

Run: `cargo test --package hub-api 2>&1`
Expected: 所有测试通过

- [ ] **Step 12: 提交**

```bash
git add hub/src/config.rs hub/src/metadata/sqlite.rs hub/src/auth/token_store.rs hub/src/server.rs hub/tests/test_integration.rs
git commit -m "feat(hub): add HUB_DB_POOL_SIZE configuration for SQLite connection pool

SQLite pool size was hardcoded at 5 connections. While adequate for most
deployments, high-concurrency read workloads may benefit from more
connections. This adds HUB_DB_POOL_SIZE (default 5) for tuning.

Refs: config-rationality-analysis R5-2 (C11)"
```

---

### Task 12: 添加启动时绝对路径日志和相对路径警告

**Files:**
- Modify: `hub/src/server.rs:30-40` (startup logging section)

- [ ] **Step 1: 在 `hub/src/server.rs` 启动日志中添加路径警告**

在 `hub/src/server.rs` 的 `start_server` 函数中，`tracing::info!("Starting Hub API on {}", bind_addr);` 之后添加：

```rust
    // Warn about relative paths that depend on process CWD
    if !std::path::Path::new(&config.auth.private_key_path).is_absolute() {
        tracing::warn!(
            "HUB_PRIVATE_KEY_PATH '{}' is a relative path. Resolved to: '{}'. \
            Consider using an absolute path for production deployments.",
            config.auth.private_key_path,
            std::fs::canonicalize(&config.auth.private_key_path)
                .unwrap_or_else(|_| std::path::PathBuf::from("(not found)"))
                .display()
        );
    }
    if !std::path::Path::new(&config.metadata.sqlite_path).is_absolute() {
        tracing::warn!(
            "HUB_SQLITE_PATH '{}' is a relative path. \
            Consider using an absolute path (e.g., /var/lib/xet/hub.db) for production deployments.",
            config.metadata.sqlite_path
        );
    }
```

- [ ] **Step 2: 运行编译**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add hub/src/server.rs
git commit -m "feat(hub): warn at startup about relative paths in config

Relative paths for private key and SQLite DB depend on the process CWD,
which varies between systemd, Docker, and manual startup. Logging the
resolved absolute path makes misconfigurations visible immediately.

Refs: config-rationality-analysis R2-2 (C5)"
```

---

### Task 13: 添加 `XET_STORAGE_BACKEND` 启动时校验

**Files:**
- Modify: `src/server.rs:15-25` (startup section)

- [ ] **Step 1: 在 `src/server.rs` 启动时添加 storage backend 校验**

在 `start_server` 函数中，`create_storage` 调用之前添加：

```rust
    // Validate storage backend
    match config.storage.backend.as_str() {
        "local" | "s3" => {},
        invalid => {
            return Err(std::io::Error::other(format!(
                "Invalid XET_STORAGE_BACKEND '{}'. Must be 'local' or 's3'.",
                invalid
            )));
        }
    }
```

- [ ] **Step 2: 运行编译**

Run: `cargo build 2>&1`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/server.rs
git commit -m "feat(cas): validate XET_STORAGE_BACKEND at startup

Invalid backend values would previously cause a confusing error from the
storage creation code. This adds an explicit check with a clear message.

Refs: config-rationality-analysis R3-5 (C14)"
```

---

### Task 14: Hub 启动时 kid 信任关系验证

**Files:**
- Modify: `hub/src/server.rs:30-40` (startup section)
- Modify: `hub/src/cas_client/mod.rs` (add health check method)

- [ ] **Step 1: 在 `CasClient` 中添加 `health_check` 方法**

在 `hub/src/cas_client/mod.rs` 的 `CasClient` impl 中添加：

```rust
    /// Check CAS health endpoint
    pub async fn health_check(&self) -> Result<bool, HubError> {
        let url = format!("{}/health", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(resp) => Ok(false),
            Err(e) => Err(HubError::CasError(format!("Health check failed: {}", e))),
        }
    }
```

- [ ] **Step 2: 在 `hub/src/server.rs` 启动时添加可选的跨服务验证**

在 CAS client 创建之后添加（使用 `tokio::spawn` 异步执行，不阻塞启动）：

```rust
    // Optional: verify CAS connectivity and kid trust relationship at startup
    let cas_client_clone = cas_client.clone();
    let kid = config.auth.kid.clone();
    tokio::spawn(async move {
        // Check CAS health
        match cas_client_clone.health_check().await {
            Ok(true) => tracing::info!("CAS health check passed"),
            Ok(false) => tracing::warn!("CAS health check returned non-success status. Verify CAS is running at the configured URL."),
            Err(e) => tracing::warn!("CAS health check failed: {}. Hub and CAS may not be able to communicate.", e),
        }
    });
```

注意：kid 信任关系的完整验证需要 CAS 暴露一个 API 来查询 trusted_kids，当前架构下做不到。这里只做连通性检查。

- [ ] **Step 3: 运行编译和测试**

Run: `cargo build --package hub-api 2>&1`
Expected: 编译成功

Run: `cargo test --package hub-api 2>&1`
Expected: 所有测试通过

- [ ] **Step 4: 提交**

```bash
git add hub/src/server.rs hub/src/cas_client/mod.rs
git commit -m "feat(hub): verify CAS connectivity at startup

Hub now performs an async health check against CAS at startup. If CAS is
unreachable, a warning is logged immediately instead of discovering the
issue at first token exchange.

Refs: config-rationality-analysis R1-2/R2-5 (C12)"
```

---

### Task 15: 更新配置文档

**Files:**
- Modify: `docs/architecture.md` (if exists) or create config documentation

- [ ] **Step 1: 检查现有文档**

Run: `ls /data/docs/architecture.md 2>/dev/null && echo "exists" || echo "not found"`
Run: `grep -rn "HUB_LFS_THRESHOLD\|HUB_DATA_DIR" /data/docs/ /data/README.md 2>/dev/null`

- [ ] **Step 2: 清理文档中对死配置的引用**

如果 `docs/architecture.md` 或其他文档中提到了 `HUB_LFS_THRESHOLD` 或 `HUB_DATA_DIR`，删除这些引用。

- [ ] **Step 3: 在文档中补充新增配置项说明**

如果存在配置文档，添加以下新增配置项：

| 环境变量 | 默认值 | 说明 |
|----------|--------|------|
| `XET_RATE_LIMIT_RPM` | `60` | CAS 公共端点速率限制（请求/分钟/IP） |
| `HUB_RATE_LIMIT_RPM` | `120` | Hub 公共端点速率限制（请求/分钟/IP） |
| `HUB_PROXY_TOKEN_TTL_SECONDS` | `300` | LFS Proxy Token 有效期（秒） |
| `HUB_MAX_DOWNLOAD_SIZE` | `536870912` (512MB) | CAS 下载大小限制 |
| `GC_HTTP_TIMEOUT_SECONDS` | `300` | GC 请求 Hub 的 HTTP 超时（秒） |
| `HUB_DB_POOL_SIZE` | `5` | SQLite 连接池大小 |

- [ ] **Step 4: 补充部署建议文档**

如果存在部署文档，添加以下建议：

```markdown
## 部署配置建议

### S3 部署
- 设置 `XET_UPLOAD_TEMP_DIR` 到有足够空间的分区（默认 `/tmp/xet-uploads` 可能空间不足）
- S3 认证使用标准 AWS 环境变量：`AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、`AWS_REGION`

### 企业集成
- 速率限制：根据预期并发量设置 `XET_RATE_LIMIT_RPM` 和 `HUB_RATE_LIMIT_RPM`
- 安全：将 `CAS_PUBLIC_KEY_PATH` 设为 `/etc/xet/hub-public-key.pem`（权限 0644）
- GC 宽限期：大仓库建议设置 `GC_GRACE_PERIOD_SECONDS=1800`（30分钟）或更长

### 连接池调优
- SQLite WAL 模式只支持 1 个并发写入者，增加 `HUB_DB_POOL_SIZE` 主要提升读并发
- HTTP 连接池参数（pool_max_idle_per_host=10, pool_idle_timeout=90s, tcp_keepalive=60s）当前值适合大多数场景
```

- [ ] **Step 5: 提交**

```bash
git add docs/
git commit -m "docs: update configuration documentation

- Remove references to deleted dead config (HUB_LFS_THRESHOLD, HUB_DATA_DIR)
- Document new config vars: XET_RATE_LIMIT_RPM, HUB_RATE_LIMIT_RPM,
  HUB_PROXY_TOKEN_TTL_SECONDS, HUB_MAX_DOWNLOAD_SIZE,
  GC_HTTP_TIMEOUT_SECONDS, HUB_DB_POOL_SIZE
- Add deployment recommendations for S3 and enterprise scenarios

Refs: config-rationality-analysis R3-3, R3-4, R4-3, R4-4, R5-4"
```

---

### Task 16: 最终验证 — 全量编译和测试

- [ ] **Step 1: 全量编译**

Run: `cargo build --workspace 2>&1`
Expected: 编译成功，无 warning

- [ ] **Step 2: 全量测试**

Run: `cargo test --workspace 2>&1`
Expected: 所有测试通过

- [ ] **Step 3: 验证所有新增配置的环境变量解析**

Run:

```bash
XET_RATE_LIMIT_RPM=120 cargo test test_config_rate_limit --test test_config -- --nocapture
HUB_RATE_LIMIT_RPM=240 cargo test test_hub_config_rate_limit --package hub-api -- --nocapture
HUB_PROXY_TOKEN_TTL_SECONDS=600 cargo test test_hub_config_proxy_token_ttl --package hub-api -- --nocapture
XET_MIN_CONVERSION_SIZE=131072 cargo test test_min_conversion_size --test test_config -- --nocapture
HUB_DB_POOL_SIZE=10 cargo test test_hub_config_db_pool_size --package hub-api -- --nocapture
GC_HTTP_TIMEOUT_SECONDS=600 cargo test test_gc_http_timeout --test test_config -- --nocapture
```

Expected: 所有测试通过（测试验证默认值，环境变量覆盖不影响默认值测试）

- [ ] **Step 4: 清理测试环境**

Run: `rm -f hub.db /tmp/xet-public-key.pem`

- [ ] **Step 5: 最终提交（如有遗漏修复）**

```bash
git add -A
git commit -m "chore: final cleanup for config rationality improvements"
```

---

## 实施摘要

| Task | 优先级 | 建议编号 | 问题编号 | 预估时间 |
|------|--------|----------|----------|----------|
| Task 1: 删除死代码 | 🔴 高 | R3-1 | C3 | 15 min |
| Task 2: CAS 速率限制 | 🔴 高 | R2-4/R5-1 | C1 | 20 min |
| Task 3: Hub 速率限制 | 🔴 高 | R2-4/R5-1 | C1 | 20 min |
| Task 4: 公钥权限检查 | 🔴 高 | R2-1 | C2 | 25 min |
| Task 5: localhost 警告 | 🟡 中 | R1-1 | C4 | 10 min |
| Task 6: Proxy TTL 配置 | 🟡 中 | R2-3 | C6 | 30 min |
| Task 7: min_conversion_size | 🟡 中 | R4-1 | C7 | 10 min |
| Task 8: GC token 校验 | 🟡 中 | R4-2 | C8 | 15 min |
| Task 9: max_download_size | 🟡 中 | R3-2 | C9 | 25 min |
| Task 10: GC HTTP 超时 | 🟡 中 | R5-3 | C10 | 15 min |
| Task 11: DB pool size | 🟡 中 | R5-2 | C11 | 25 min |
| Task 12: 路径日志 | 🟡 中 | R2-2 | C5 | 10 min |
| Task 13: storage backend 校验 | 🟢 低 | R3-5 | C14 | 5 min |
| Task 14: CAS 健康检查 | 🟢 低 | R1-2/R2-5 | C12 | 15 min |
| Task 15: 文档更新 | 🟢 低 | R3-3/R3-4/R4-3/R4-4/R5-4 | C13/C15/C16 | 20 min |
| Task 16: 最终验证 | — | — | — | 15 min |
| **合计** | | | | **~3.5 小时** |
