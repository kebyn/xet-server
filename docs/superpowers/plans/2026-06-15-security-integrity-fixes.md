# 安全与数据完整性修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 /data 这个 Rust CAS/Hub 系统中已确认的 4 个 Critical + 8 个 Important + 7 个 Minor 问题(明确排除 `src/gc/*` 全部、B-CONF-1 保留现状)。

**Architecture:** 行为类修复走 TDD(先写失败测试再改代码);lint/文档/日志类由 `cargo build` + `cargo clippy` 验证。每个任务自包含、可独立提交。

**Tech Stack:** Rust (edition 2024)、actix-web 4.5、tokio、aws-sdk-s3 1.15、lz4_flex 0.11、blake3、ed25519-dalek、sqlx/SQLite、uuid v4。两个 crate:`xet-server`(根)与 `hub-api`(`hub/`)。

**Spec:** `docs/superpowers/specs/2026-06-15-security-integrity-fixes-design.md`

**基线(已验证):** `cargo build --workspace --all-targets` 通过;`cargo clippy` 11 个 Minor 警告。

---

## 文件改动总览

| 文件 | 改动 | 任务 |
|---|---|---|
| `hub/src/api/resolve.rs` | 私有 repo 所有权校验 | T1 |
| `hub/src/api/token_exchange.rs` | 私有 repo 所有权校验 | T2 |
| `src/format/compression.rs` | LZ4/BG4 解压设界 | T3 |
| `src/api/lfs.rs` | 重建路径 chunk hash 校验 + 唯一临时名 | T4, T5 |
| `src/storage/local.rs` | 跨 fs 原子拷贝 + put() 原子化 | T6, T7 |
| `src/storage/s3.rs` | 类型化错误码判定 | T8 |
| `hub/src/api/lfs_proxy.rs` | 重写失败丢弃 action | T9 |
| `hub/src/api/tree.rs` | strip_prefix 路径边界 | T10 |
| `hub/tests/test_metadata.rs` | commit_atomic 首提交冲突测试 | T11 |
| `src/api/xorb.rs` | 阻塞 remove_file → tokio | T12 |
| `src/api/auth.rs` | CAS 私钥文件权限检查 | T13 |
| `src/api/lfs.rs`, `hub/src/cas_client/mod.rs`, `hub/src/api/commit.rs`, `src/chunking/cdc.rs`, `src/conversion/mod.rs`, `hub/src/auth/token_store.rs`, `hub/src/server.rs` | clippy/文档/日志 | T14 |

---

## Task 1: C-AUTH-1 — 私有 repo 读取越权(resolve.rs)

**Files:**
- Modify: `hub/src/api/resolve.rs`(`handle_resolve`,在 line 35 之后)
- Test: `hub/src/api/resolve.rs`(`#[cfg(test)] mod tests`)

- [ ] **Step 1: 写失败测试 — 非 owner 访问私有 repo 应 404**

在 `hub/src/api/resolve.rs` 的 `mod tests` 内新增:

```rust
    #[actix_web::test]
    async fn test_resolve_private_repo_denies_non_owner() {
        let (token_store, metadata, config) = setup_test_env_with_files().await;
        // attacker 的 read token
        let token = token_store.create_token("attacker", "t", "read").await.unwrap();
        // 私有 repo,owner 是别人
        let repo = metadata.create_repo("owner", "secret-model", RepoType::Model, true).await.unwrap();
        let commit_id = "abc123";
        metadata.add_revision(Revision {
            commit_id: commit_id.to_string(), repo_id: repo.id, parent: None,
            message: "i".to_string(), author: "owner".to_string(), created_at: 1000,
        }).await.unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();
        metadata.add_file_entries(vec![FileEntry {
            path: "model.bin".to_string(), repo_id: repo.id, commit_id: commit_id.to_string(),
            size: 10, cas_hash: "h".to_string(), is_lfs: true,
        }]).await.unwrap();

        let app = actix_test::init_service(App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(config.clone()))
            .route("/{ns}/{repo}/resolve/{revision}/{path}", web::get().to(resolve_model))
        ).await;
        let req = actix_test::TestRequest::get()
            .uri("/owner/secret-model/resolve/main/model.bin")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test -p hub-api --lib resolve::tests::test_resolve_private_repo_denies_non_owner`
Expected: FAIL(当前会返回 302/200,而非 404)

- [ ] **Step 3: 实现 — 在 handle_resolve 加所有权门控**

在 `hub/src/api/resolve.rs` 的 `let repo = match ... };` 块(line 19-35)之后、`// Resolve revision`(line 37)之前插入:

```rust
    // C-AUTH-1: 私有 repo 仅 owner 可访问。返回 404 而非 403,避免泄露私有 repo 的存在性。
    if repo.private && repo.namespace != auth.info.username {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": "Repository not found",
            "error_type": "NotFoundError"
        }));
    }
```

- [ ] **Step 4: 运行测试,确认通过(并确认既有测试不回归)**

Run: `cargo test -p hub-api --lib resolve::tests`
Expected: PASS(新测试 + 既有 `test_resolve_existing_file`/`test_resolve_missing_file` 全绿。既有测试用的是 `private=false`,不受影响。)

- [ ] **Step 5: 提交**

```bash
git add hub/src/api/resolve.rs
git commit -m "fix(auth): enforce private-repo ownership on file resolve (C-AUTH-1)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 2: C-AUTH-2 — token 交换越权(token_exchange.rs)

**Files:**
- Modify: `hub/src/api/token_exchange.rs`(`do_exchange`,line 47 之后)
- Test: `hub/src/api/token_exchange.rs`(`mod tests`)

- [ ] **Step 1: 写失败测试 — 非 owner 交换私有 repo token 应 404**

在 `hub/src/api/token_exchange.rs` 的 `mod tests` 内新增:

```rust
    #[actix_web::test]
    async fn test_exchange_private_repo_denies_non_owner() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;
        let token = token_store.create_token("attacker", "t", "read").await.unwrap();
        metadata.create_repo("owner", "repo", RepoType::Model, true).await.unwrap();
        let app = test::init_service(App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(xet_signer.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(config.clone()))
            .route("/api/models/{namespace}/{repo}/read/{revision}", web::post().to(exchange_model_read))
        ).await;
        let req = test::TestRequest::post()
            .uri("/api/models/owner/repo/read/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test -p hub-api --lib token_exchange::tests::test_exchange_private_repo_denies_non_owner`
Expected: FAIL(当前返回 200 并签发 token)

- [ ] **Step 3: 实现 — 在 do_exchange 加所有权门控**

在 `hub/src/api/token_exchange.rs` 的 `let repo = match ... };` 块(line 31-47)之后、`// Determine revision`(line 49)之前插入:

```rust
    // C-AUTH-2: 私有 repo 仅 owner 可换取 token(读写皆然)。404 不泄露存在性。
    if repo.private && repo.namespace != info.username {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": "Repository not found",
            "error_type": "NotFoundError"
        }));
    }
```

- [ ] **Step 4: 运行测试,确认通过(不回归)**

Run: `cargo test -p hub-api --lib token_exchange::tests`
Expected: PASS(新测试 + 既有 4 个 exchange 测试全绿;既有用 `private=false`,不受影响)

- [ ] **Step 5: 提交**

```bash
git add hub/src/api/token_exchange.rs
git commit -m "fix(auth): enforce private-repo ownership on token exchange (C-AUTH-2)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 3: C-DATA-1 — LZ4/BG4 解压炸弹设界(compression.rs)

**Files:**
- Modify: `src/format/compression.rs`(`decompress`,line 175-189;新增 `check_lz4_prefix`)
- Test: `src/format/compression.rs`(`mod tests`)

- [ ] **Step 1: 写失败测试 — 超大长度前缀应被拒绝**

在 `src/format/compression.rs` 的 `mod tests` 内新增:

```rust
    #[test]
    fn test_decompress_lz4_rejects_oversized_prefix() {
        // 前缀谎称 ~4GB,实际数据极小 → 必须在分配前拒绝。
        let mut data = vec![0xFFu8, 0xFF, 0xFF, 0xFF]; // LE u32 = 4294967295
        data.extend_from_slice(&[0u8; 4]);
        let result = decompress(CompressionScheme::LZ4, &data, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_decompress_lz4_roundtrip_ok() {
        let original = b"hello world, this is some test data".repeat(10);
        let compressed = compress(CompressionScheme::LZ4, &original).unwrap();
        let out = decompress(CompressionScheme::LZ4, &compressed, original.len()).unwrap();
        assert_eq!(out, original);
    }
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test -p xet-server --lib format::compression::tests::test_decompress_lz4_rejects_oversized_prefix`
Expected: FAIL(当前 `decompress_size_prepended` 会尝试分配 ~4GB 或 panic/err 不可控)

- [ ] **Step 3: 实现 — 解压前比对前缀,解压后断言长度**

将 `src/format/compression.rs` 的 `decompress`(line 175-189)整体替换为:

```rust
pub fn decompress(scheme: CompressionScheme, data: &[u8], original_size: usize) -> Result<Vec<u8>> {
    match scheme {
        CompressionScheme::None => {
            if data.len() != original_size {
                return Err(XetError::ParseError(format!(
                    "Uncompressed data size {} != expected {}", data.len(), original_size
                )));
            }
            Ok(data.to_vec())
        }
        CompressionScheme::LZ4 => {
            check_lz4_prefix(data, original_size)?;
            let out = decompress_size_prepended(data)
                .map_err(|e| XetError::ParseError(format!("LZ4 decompression failed: {}", e)))?;
            if out.len() != original_size {
                return Err(XetError::ParseError(format!(
                    "LZ4 output size {} != expected {}", out.len(), original_size
                )));
            }
            Ok(out)
        }
        CompressionScheme::ByteGrouping4LZ4 => {
            check_lz4_prefix(data, original_size)?;
            let decompressed = decompress_size_prepended(data)
                .map_err(|e| XetError::ParseError(format!("BG4-LZ4 decompression failed: {}", e)))?;
            bg4_regroup(&decompressed, original_size)
        }
    }
}

/// lz4_flex 在压缩数据头部写入 4 字节小端 u32 表示解压后长度。
/// 解压前用可信的 `original_size`(来自 chunk header 的 uncompressed_length)
/// 比对该前缀,防止恶意/损坏前缀触发巨量内存分配(解压炸弹)。
fn check_lz4_prefix(data: &[u8], original_size: usize) -> Result<()> {
    if data.len() < 4 {
        return Err(XetError::ParseError(
            "LZ4 data too short for size prefix".to_string(),
        ));
    }
    let prefix = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if prefix != original_size {
        return Err(XetError::ParseError(format!(
            "LZ4 size prefix {} does not match expected uncompressed size {}",
            prefix, original_size
        )));
    }
    Ok(())
}
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test -p xet-server --lib format::compression`
Expected: PASS(新增 2 个 + 既有 compression 测试全绿)

- [ ] **Step 5: 提交**

```bash
git add src/format/compression.rs
git commit -m "fix(format): bound LZ4/BG4 decompression by trusted size to prevent decompression bomb (C-DATA-1)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 4: C-DATA-2 — 重建路径 chunk hash 校验(lfs.rs)

> chunk_hash 覆盖的是 `XorbChunkHeader(8B) + 压缩数据`(见 `xorb_builder.rs:83-89`),所以在**解压前**对该区域校验,既保证完整性又先于 LZ4 解压挡住污染数据。把每 chunk 的「定位+校验+解压」抽成可单测的 `extract_chunk_verified`。

**Files:**
- Modify: `src/api/lfs.rs`(新增 `extract_chunk_verified`;替换重建循环 line 1051-1089 的循环体)
- Test: `src/api/lfs.rs`(`#[cfg(test)] mod tests`)

- [ ] **Step 1: 写失败测试 — 损坏字节应被检出**

在 `src/api/lfs.rs` 的 `mod tests`(若无则新建 `#[cfg(test)] mod tests { use super::*; ... }`)内新增:

```rust
    #[test]
    fn test_extract_chunk_verified_detects_corruption() {
        use crate::format::xorb::XorbChunkHeader;
        use crate::format::compression::{compress, CompressionScheme};
        use crate::hash::compute_data_hash;

        let raw = b"some chunk payload data for verification".to_vec();
        let compressed = compress(CompressionScheme::LZ4, &raw).unwrap();
        let header = XorbChunkHeader {
            version: 1,
            compressed_length: compressed.len() as u32,
            compression_scheme: CompressionScheme::LZ4,
            uncompressed_length: raw.len() as u32,
        };
        let mut chunk_bytes = Vec::new();
        header.serialize(&mut chunk_bytes).unwrap();
        chunk_bytes.extend_from_slice(&compressed);
        let hash = compute_data_hash(&chunk_bytes);

        // 正常路径:解压结果等于原文
        let ok = extract_chunk_verified(&chunk_bytes, 0, &hash).unwrap();
        assert_eq!(&ok[..], &raw[..]);

        // 翻转压缩区最后一字节 → hash 不匹配 → 报错
        let mut corrupted = chunk_bytes.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        assert!(extract_chunk_verified(&corrupted, 0, &hash).is_err());
    }
```

- [ ] **Step 2: 运行测试,确认失败(函数不存在)**

Run: `cargo test -p xet-server --lib api::lfs::tests::test_extract_chunk_verified_detects_corruption`
Expected: FAIL(`extract_chunk_verified` 未定义,编译错误)

- [ ] **Step 3: 实现 — 新增校验型提取函数**

在 `src/api/lfs.rs` 模块级(`create_reconstruction_stream` 函数之外,文件顶部 `use` 之后)新增。确认顶部已 `use` 到 `XorbChunkHeader`、`decompress`、`MerkleHash`(若缺,补 `use crate::format::xorb::XorbChunkHeader; use crate::format::compression::decompress; use crate::types::merkle_hash::MerkleHash;`):

```rust
/// 从 xorb 原始字节中按偏移定位单个 chunk,校验其完整性后解压。
///
/// C-DATA-2: chunk_hash 覆盖 header(8B)+压缩数据。在解压前对该区域重算 hash
/// 并与 shard 记录值比对,检出磁盘 bit-rot / 存储损坏 / 串改,并先于解压挡住污染数据。
fn extract_chunk_verified(
    xorb_data: &[u8],
    chunk_offset_bytes: usize,
    expected_hash: &MerkleHash,
) -> Result<bytes::Bytes, String> {
    if chunk_offset_bytes + XorbChunkHeader::SIZE > xorb_data.len() {
        return Err("Chunk offset out of bounds".to_string());
    }
    let mut chunk_cursor = std::io::Cursor::new(&xorb_data[chunk_offset_bytes..]);
    let chunk_header = XorbChunkHeader::deserialize(&mut chunk_cursor)
        .map_err(|e| format!("Failed to parse chunk header: {}", e))?;

    let data_start = chunk_offset_bytes + XorbChunkHeader::SIZE;
    let data_end = data_start + chunk_header.compressed_length as usize;
    if data_end > xorb_data.len() {
        return Err("Chunk data out of bounds".to_string());
    }

    // 解压前校验 header+压缩数据 区域的完整性。
    let chunk_region = &xorb_data[chunk_offset_bytes..data_end];
    let actual_hash = crate::hash::compute_data_hash(chunk_region);
    if actual_hash != *expected_hash {
        return Err(format!(
            "Chunk hash mismatch at offset {}: stored data is corrupted",
            chunk_offset_bytes
        ));
    }

    let compressed_data = &xorb_data[data_start..data_end];
    let decompressed = decompress(
        chunk_header.compression_scheme,
        compressed_data,
        chunk_header.uncompressed_length as usize,
    )
    .map_err(|e| format!("Failed to decompress chunk: {}", e))?;
    Ok(bytes::Bytes::from(decompressed))
}
```

- [ ] **Step 4: 替换重建循环体调用该函数**

将 `src/api/lfs.rs` 重建循环(line 1052-1089)替换为:

```rust
                for chunk_idx in 0..*num_entries {
                    let global_chunk_idx = xorb_chunk_offset + chunk_idx;
                    if global_chunk_idx >= shard.xorb_chunk_entries.len() {
                        break;
                    }
                    let chunk_entry = &shard.xorb_chunk_entries[global_chunk_idx];
                    let chunk_offset_bytes = chunk_entry.chunk_byte_range_start as usize;

                    match extract_chunk_verified(&xorb_data, chunk_offset_bytes, &chunk_entry.chunk_hash) {
                        Ok(bytes) => yield Ok(bytes),
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }
```

- [ ] **Step 5: 运行测试,确认通过(并跑重建相关集成测试不回归)**

Run: `cargo test -p xet-server --lib api::lfs && cargo test -p xet-server --test test_e2e`
Expected: PASS(新单测通过;端到端重建/下载测试仍绿)

- [ ] **Step 6: 提交**

```bash
git add src/api/lfs.rs
git commit -m "fix(cas): verify chunk hash on reconstruction read path (C-DATA-2)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 5: I-STOR-1 — 重建临时文件名唯一化(lfs.rs)

> `xorb-{hash}-{pid}.tmp` 在同进程并发重建同一 xorb 时冲突。改用 UUID,与上传路径 `TempFile`(用 `uuid::Uuid::new_v4()`)一致。机械改动,由 build + 既有重建测试验证。

**Files:**
- Modify: `src/api/lfs.rs`(line 982 与 1013 的临时路径构造)

- [ ] **Step 1: 替换两处临时路径构造**

将 `src/api/lfs.rs:982` 与 `:1013` 两处:

```rust
let temp_path = temp_dir.join(format!("xorb-{}-{}.tmp", xorb_hash, std::process::id()));
```

改为:

```rust
let temp_path = temp_dir.join(format!("xorb-{}-{}.tmp", xorb_hash, uuid::Uuid::new_v4()));
```

(line 982 处变量可能名为别的临时名;按上下文把 `std::process::id()` 替换为 `uuid::Uuid::new_v4()` 即可。`uuid` 已是依赖。)

- [ ] **Step 2: build + 重建测试不回归**

Run: `cargo build -p xet-server && cargo test -p xet-server --test test_e2e`
Expected: 编译通过,端到端测试全绿。

- [ ] **Step 3: 提交**

```bash
git add src/api/lfs.rs
git commit -m "fix(cas): use per-request UUID for reconstruction temp files to avoid concurrent collision (I-STOR-1)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 6: I-STOR-2 — 本地跨文件系统拷贝原子化(local.rs)

> `put_from_path` 的跨 fs `fs::copy` 直接写最终 key,中断会留下截断文件。抽出 `copy_then_rename` 辅助函数(copy 到 `.tmp` 再 rename),可单测。

**Files:**
- Modify: `src/storage/local.rs`(`put_from_path` line 105-114 的 fallback;新增 `copy_then_rename`)
- Test: `src/storage/local.rs`(`#[cfg(test)] mod tests`)

- [ ] **Step 1: 写失败测试 — 辅助函数写入后无 .tmp 残留且内容正确**

在 `src/storage/local.rs` 的 `mod tests`(若无则新建)内新增:

```rust
    #[tokio::test]
    async fn test_copy_then_rename_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("sub/dest.bin");
        tokio::fs::create_dir_all(dest.parent().unwrap()).await.unwrap();
        tokio::fs::write(&src, b"payload").await.unwrap();

        copy_then_rename(&src, &dest).await.unwrap();

        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"payload");
        // 不留下 .tmp 中间文件
        assert!(!dest.with_extension("tmp").exists());
    }
```

- [ ] **Step 2: 运行测试,确认失败(函数不存在)**

Run: `cargo test -p xet-server --lib storage::local::tests::test_copy_then_rename_atomic`
Expected: FAIL(`copy_then_rename` 未定义)

- [ ] **Step 3: 实现辅助函数并在 fallback 中调用**

在 `src/storage/local.rs` 模块级(`impl` 之外)新增:

```rust
/// 跨文件系统安全拷贝:先 copy 到临时文件,再原子 rename 到最终路径。
/// 避免中断时在最终 key 留下截断文件。
async fn copy_then_rename(source: &Path, dest: &Path) -> StorageResult<()> {
    let temp_dest = dest.with_extension("tmp");
    fs::copy(source, &temp_dest).await.map_err(|e| {
        StorageError::Internal(format!(
            "Failed to copy {} → {}: {}", source.display(), temp_dest.display(), e
        ))
    })?;
    fs::rename(&temp_dest, dest).await.map_err(|e| {
        let _ = std::fs::remove_file(&temp_dest);
        StorageError::Internal(format!(
            "Failed to rename {} → {}: {}", temp_dest.display(), dest.display(), e
        ))
    })?;
    Ok(())
}
```

把 `put_from_path`(line 101-116)的 `Err(_)` fallback 分支改为:

```rust
            Err(_) => {
                copy_then_rename(source, &dest).await?;
                let _ = fs::remove_file(source).await;
                Ok(())
            }
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test -p xet-server --lib storage::local`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/storage/local.rs
git commit -m "fix(storage): atomic cross-filesystem copy in local put_from_path (I-STOR-2)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 7: I-STOR-3 — 本地 put() 原子化(local.rs)

> `put()` 用 `fs::write` 原地写,崩溃留截断文件。改为委托已有的原子 `put_atomic`(local 已覆盖 `put_atomic`,不会递归)。

**Files:**
- Modify: `src/storage/local.rs`(`put` line 73-87)
- Test: `src/storage/local.rs`(`mod tests`)

- [ ] **Step 1: 写失败测试 — put 后无 .tmp 残留**

在 `mod tests` 内新增:

```rust
    #[tokio::test]
    async fn test_put_is_atomic_no_temp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().to_path_buf());
        store.put("xorbs/abc", Bytes::from_static(b"data")).await.unwrap();
        assert_eq!(store.get("xorbs/abc").await.unwrap(), Bytes::from_static(b"data"));
        // 原子写不应残留 .tmp
        assert!(!dir.path().join("xorbs/abc.tmp").exists());
    }
```

> 注:`LocalStorage::new` 的构造签名以本文件实际为准(若为 `LocalStorage::new(path)` 接受 `PathBuf`)。

- [ ] **Step 2: 运行测试,确认当前行为**

Run: `cargo test -p xet-server --lib storage::local::tests::test_put_is_atomic_no_temp_leftover`
Expected: 当前 `put` 用 `fs::write` 直接写最终路径,理论上也不残留 `.tmp`,故此测试可能已通过 —— 它的真实目的是**锁定**「put 经由原子路径」这一不变量,防回归。若已通过,仍按 Step 3 切换实现以获得 crash-safety,测试保持绿即可。

- [ ] **Step 3: 实现 — put 委托 put_atomic**

将 `src/storage/local.rs` 的 `put`(line 73-87)整体替换为:

```rust
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        // 经由原子写(temp + rename),避免崩溃时留下截断文件。
        self.put_atomic(key, data).await
    }
```

- [ ] **Step 4: 运行测试,确认通过(并跑 storage 集成测试)**

Run: `cargo test -p xet-server --lib storage::local && cargo test -p xet-server --test test_storage`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/storage/local.rs
git commit -m "fix(storage): route local put() through atomic temp+rename (I-STOR-3)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 8: I-STOR-4 — S3 错误用结构化错误码判定(s3.rs)

> 用 `ProvideErrorMetadata::code()`(返回 S3 契约性错误码字符串)替代 `e.to_string().contains(...)`。比 Display 文本稳定。无 S3 mock 设施,由 `cargo build` + `cargo clippy` 验证编译与正确性。

**Files:**
- Modify: `src/storage/s3.rs`(line 29-37 imports;以及 line 438/473/533/637/662/758/777/805/807 的字符串匹配)

- [ ] **Step 1: 加 import**

在 `src/storage/s3.rs` 的 use 区(line 29-37)新增:

```rust
use aws_sdk_s3::error::ProvideErrorMetadata;
```

- [ ] **Step 2: 替换所有 `e.to_string().contains(...)` 判定**

把形如:

```rust
if e.to_string().contains("NoSuchKey") { StorageError::NotFound(key.to_string()) } else { ... }
```

改为基于错误码:

```rust
if e.code() == Some("NoSuchKey") || e.code() == Some("NotFound") {
    StorageError::NotFound(key.to_string())
} else {
    StorageError::Internal(format!("S3 get failed: {}", e))
}
```

逐站点对应替换(保留各自原有的 Internal 文案):
- **line 438**(`get`)与 **line 473**(`download_to_path`):`e.code() == Some("NoSuchKey") || e.code() == Some("NotFound")` → `NotFound`。
- **line 533**(`exists`):`Err(e) if e.code() == Some("NotFound") || e.code() == Some("NoSuchKey") => Ok(false)`。
- **line 637**(`get_mtime`)、**line 662**(`get_size`)、**line 777**(`get_etag`):`e.code() == Some("NotFound") || e.code() == Some("NoSuchKey")` → `NotFound`。
- **line 758 / 805**(条件 put / delete 的 precondition):`e.code() == Some("PreconditionFailed")` → `ConditionFailed`。
- **line 807**(`delete_if_match`):`e.code() == Some("NoSuchKey") || e.code() == Some("NotFound")`。

- [ ] **Step 3: 验证编译与 clippy**

Run: `cargo build -p xet-server && cargo clippy -p xet-server`
Expected: 编译通过,无新增警告。

- [ ] **Step 4: 提交**

```bash
git add src/storage/s3.rs
git commit -m "fix(storage): classify S3 errors via structured error code instead of Display string (I-STOR-4)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 9: I-API-1 — 代理重写失败丢弃 action(lfs_proxy.rs)

> `rewrite_action_url` 在 href 解析失败时保留原始内部 CAS URL。改为返回 bool,调用方在失败时删除该 action(与既有「签名失败删除」一致)。

**Files:**
- Modify: `hub/src/api/lfs_proxy.rs`(`rewrite_action_url` 192-227;调用方 `rewrite_batch_urls` 163-185)
- Test: `hub/src/api/lfs_proxy.rs`(`#[cfg(test)] mod tests`)

- [ ] **Step 1: 写失败测试**

在 `hub/src/api/lfs_proxy.rs` 的 `mod tests`(若无则新建 `#[cfg(test)] mod tests { use super::*; ... }`)内新增:

```rust
    #[test]
    fn test_rewrite_action_url_drops_on_parse_failure() {
        let hub = url::Url::parse("https://hub.example.com").unwrap();
        let mut action = serde_json::json!({"href": "not a valid url at all"});
        let ok = rewrite_action_url(&mut action, &hub, "proxy_tok");
        assert!(!ok, "无法解析的 href 应返回 false 以便调用方丢弃 action");
    }

    #[test]
    fn test_rewrite_action_url_rewrites_valid() {
        let hub = url::Url::parse("https://hub.example.com:9000").unwrap();
        let mut action = serde_json::json!({"href": "http://cas-internal:5000/lfs/objects/abc"});
        let ok = rewrite_action_url(&mut action, &hub, "proxy_tok");
        assert!(ok);
        let href = action.get("href").unwrap().as_str().unwrap();
        assert!(href.contains("hub.example.com"));
        assert!(href.contains("token=proxy_tok"));
        assert!(!href.contains("cas-internal"));
    }
```

- [ ] **Step 2: 运行测试,确认失败(签名不匹配:返回类型仍是 `()`)**

Run: `cargo test -p hub-api --lib api::lfs_proxy::tests`
Expected: FAIL(编译错误:`rewrite_action_url` 返回 `()`,不能用作 bool)

- [ ] **Step 3: 改 rewrite_action_url 返回 bool**

将 `hub/src/api/lfs_proxy.rs:192-227` 的 `rewrite_action_url` 替换为:

```rust
/// Rewrite a single action's URL and auth header with proxy token.
/// 返回 true 表示成功重写;false 表示 href 无法解析,调用方应丢弃该 action
/// 以免把内部 CAS URL 泄露给客户端。
fn rewrite_action_url(action: &mut serde_json::Value, hub_url: &url::Url, proxy_token: &str) -> bool {
    let new_href = action.get("href")
        .and_then(|h| h.as_str())
        .and_then(|h| url::Url::parse(h).ok())
        .map(|mut url| {
            url.set_scheme(hub_url.scheme()).ok();
            url.set_host(hub_url.host_str()).ok();
            if let Some(port) = hub_url.port() {
                url.set_port(Some(port)).ok();
            } else {
                url.set_port(None).ok();
            }
            url.query_pairs_mut().append_pair("token", proxy_token);
            url.to_string()
        });

    let Some(href) = new_href else {
        // 无法解析 href:不透传原始(内部 CAS)URL,交由调用方丢弃 action。
        return false;
    };
    if let Some(action_obj) = action.as_object_mut() {
        action_obj.insert("href".to_string(), serde_json::Value::String(href));
    }

    if action.get("header").and_then(|h| h.get("Authorization")).is_some()
        && let Some(header_obj) = action.get_mut("header").and_then(|h| h.as_object_mut()) {
            header_obj.insert(
                "Authorization".to_string(),
                serde_json::Value::String(format!("Bearer {}", proxy_token)),
            );
        }
    true
}
```

- [ ] **Step 4: 调用方在失败时删除 action**

将 `rewrite_batch_urls` 中 upload 分支(line 163-174)的 `Ok` 臂改为:

```rust
                    if let Some(upload_action) = actions.get_mut("upload") {
                        match signer.sign_proxy(username, &oid, "upload", "", "") {
                            Ok((proxy_token, _)) => {
                                if !rewrite_action_url(upload_action, &hub_url, &proxy_token)
                                    && let Some(actions_obj) = actions.as_object_mut() {
                                        actions_obj.remove("upload");
                                    }
                            }
                            Err(e) => {
                                tracing::error!("Failed to sign proxy token for upload {}: {}", oid, e);
                                if let Some(actions_obj) = actions.as_object_mut() {
                                    actions_obj.remove("upload");
                                }
                            }
                        }
                    }
```

download 分支(line 175-185)同样改为:

```rust
                    if let Some(download_action) = actions.get_mut("download") {
                        match signer.sign_proxy(username, &oid, "download", "", "") {
                            Ok((proxy_token, _)) => {
                                if !rewrite_action_url(download_action, &hub_url, &proxy_token)
                                    && let Some(actions_obj) = actions.as_object_mut() {
                                        actions_obj.remove("download");
                                    }
                            }
                            Err(e) => {
                                tracing::error!("Failed to sign proxy token for download {}: {}", oid, e);
                                if let Some(actions_obj) = actions.as_object_mut() {
                                    actions_obj.remove("download");
                                }
                            }
                        }
                    }
```

- [ ] **Step 5: 运行测试,确认通过(不回归)**

Run: `cargo test -p hub-api --lib api::lfs_proxy`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add hub/src/api/lfs_proxy.rs
git commit -m "fix(hub): drop LFS action when CAS href rewrite fails instead of leaking internal URL (I-API-1)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 10: I-API-2 — tree 目录推断加路径边界(tree.rs)

> 非递归路径 `strip_prefix(tree_path)`(无尾 `/`)会把前缀 `model` 从 `model/sub/a.bin` 误剥成 `/sub/a.bin` → 空目录;递归路径(line 112-114)已正确加尾 `/`。对齐两处。

**Files:**
- Modify: `hub/src/api/tree.rs`(`infer_directories` line 27;非递归 file loop line 144)
- Test: `hub/src/api/tree.rs`(`#[cfg(test)] mod tests`)

- [ ] **Step 1: 写失败测试**

在 `hub/src/api/tree.rs` 的 `mod tests`(若无则新建)内新增:

```rust
    #[test]
    fn test_infer_directories_respects_path_boundary() {
        let entries = vec![FileEntry {
            path: "model/sub/a.bin".to_string(), repo_id: 1, commit_id: "c".to_string(),
            size: 1, cas_hash: "h".to_string(), is_lfs: true,
        }];
        let dirs = infer_directories(&entries, "model");
        assert_eq!(dirs, vec!["sub".to_string()]);
    }
```

(当前实现 strip "model"(无 `/`)→ "/sub/a.bin" → 取 `[..find('/')]` = 空串 → dirs = [""],与期望 ["sub"] 不符。)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test -p hub-api --lib api::tree::tests::test_infer_directories_respects_path_boundary`
Expected: FAIL(得到 `[""]` 而非 `["sub"]`)

- [ ] **Step 3: 修 infer_directories(line 24-28)**

将:

```rust
        let rel_path = if prefix.is_empty() {
            entry.path.clone()
        } else {
            entry.path.strip_prefix(prefix).unwrap_or(&entry.path).to_string()
        };
```

改为:

```rust
        let rel_path = if prefix.is_empty() {
            entry.path.clone()
        } else {
            let prefix_with_slash = format!("{}/", prefix);
            entry.path.strip_prefix(&prefix_with_slash).unwrap_or(&entry.path).to_string()
        };
```

- [ ] **Step 4: 修非递归 file loop(line 141-145)**

将:

```rust
            let rel_path = if tree_path.is_empty() {
                entry.path.clone()
            } else {
                entry.path.strip_prefix(&tree_path).unwrap_or(&entry.path).to_string()
            };
```

改为:

```rust
            let rel_path = if tree_path.is_empty() {
                entry.path.clone()
            } else {
                let prefix_with_slash = format!("{}/", tree_path);
                entry.path.strip_prefix(&prefix_with_slash).unwrap_or(&entry.path).to_string()
            };
```

- [ ] **Step 5: 运行测试,确认通过(不回归)**

Run: `cargo test -p hub-api --lib api::tree`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add hub/src/api/tree.rs
git commit -m "fix(hub): require path boundary in tree prefix stripping (I-API-2)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 11: I-API-3 — commit_atomic 首提交冲突测试(补测试)

> `commit_atomic`(`sqlite.rs:578`)**已正确**在 parent=None 且 HEAD≠NULL 时返回 Conflict。这是缺确定性测试,不是代码 bug。补一个 metadata 层测试锁定该不变量(它保护并发首次提交)。

**Files:**
- Test: `hub/tests/test_metadata.rs`(新增测试)

- [ ] **Step 1: 写测试 — 已有 HEAD 时 parent=None 的首提交应 Conflict**

在 `hub/tests/test_metadata.rs` 末尾新增(确认顶部已 `use` 到 `SqliteMetadataStore, MetadataStore, RepoType, Revision, MetadataError, FileEntry`;按文件现有 use 调整):

```rust
#[tokio::test]
async fn test_commit_atomic_rejects_first_commit_when_head_exists() {
    let metadata = SqliteMetadataStore::in_memory().await.unwrap();
    let repo = metadata.create_repo("u", "r", RepoType::Model, false).await.unwrap();

    let empty: Vec<FileEntry> = Vec::new();

    // 第一个首提交(parent=None)在 HEAD 为空时成功。
    let first = Revision {
        commit_id: "c1".to_string(), repo_id: repo.id, parent: None,
        message: "init".to_string(), author: "u".to_string(), created_at: 1,
    };
    metadata.commit_atomic(&first, &empty, None).await.unwrap();

    // 第二个「首提交」(parent=None)必须被拒绝,因为 HEAD 现在是 c1。
    // 这正是并发首次提交场景下保护数据不被覆盖的不变量。
    let second = Revision {
        commit_id: "c2".to_string(), repo_id: repo.id, parent: None,
        message: "init2".to_string(), author: "u".to_string(), created_at: 2,
    };
    let result = metadata.commit_atomic(&second, &empty, None).await;
    assert!(matches!(result, Err(MetadataError::Conflict(_))));
}
```

- [ ] **Step 2: 运行测试,确认通过(代码已正确)**

Run: `cargo test -p hub-api --test test_metadata test_commit_atomic_rejects_first_commit_when_head_exists`
Expected: PASS(若意外 FAIL,说明 commit_atomic 实际未拒绝 —— 此时升级为代码 bug,在 `hub/src/metadata/sqlite.rs:578` 修正 HEAD 检查后再绿)

- [ ] **Step 3: 提交**

```bash
git add hub/tests/test_metadata.rs
git commit -m "test(hub): lock first-commit conflict invariant in commit_atomic (I-API-3)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 12: I-API-4 — xorb 清理改非阻塞(xorb.rs)

**Files:**
- Modify: `src/api/xorb.rs`(line 224)

- [ ] **Step 1: 改阻塞 remove_file 为 tokio 版**

将 `src/api/xorb.rs:224`:

```rust
        let _ = std::fs::remove_file(&temp_path);
```

改为:

```rust
        let _ = tokio::fs::remove_file(&temp_path).await;
```

- [ ] **Step 2: build 验证**

Run: `cargo build -p xet-server`
Expected: 编译通过(已在 async handler 内)。

- [ ] **Step 3: 提交**

```bash
git add src/api/xorb.rs
git commit -m "fix(cas): use non-blocking remove_file in xorb error cleanup (I-API-4)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 13: M-7 — CAS 私钥文件权限检查(auth.rs)

> Hub 端已有 `mode & 0o044` 检查(`server.rs:54-70`),CAS 端加载私钥无检查。加一个可单测的权限检测函数,加载私钥时若 group/other 可访问则告警。

**Files:**
- Modify: `src/api/auth.rs`(line 343-348 加载私钥处;新增 `key_permissions_too_open`)
- Test: `src/api/auth.rs`(`#[cfg(test)] mod tests`)

- [ ] **Step 1: 写失败测试(Unix)**

在 `src/api/auth.rs` 的 `mod tests`(若无则新建)内新增:

```rust
    #[cfg(unix)]
    #[test]
    fn test_key_permissions_detection() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("key.pem");
        std::fs::write(&p, b"x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!key_permissions_too_open(&p));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(key_permissions_too_open(&p));
    }
```

- [ ] **Step 2: 运行测试,确认失败(函数不存在)**

Run: `cargo test -p xet-server --lib api::auth::tests::test_key_permissions_detection`
Expected: FAIL(`key_permissions_too_open` 未定义)

- [ ] **Step 3: 实现检测函数并在私钥加载处调用**

在 `src/api/auth.rs` 模块级新增:

```rust
/// 私钥文件若对 group/other 可读/可写/可执行(mode & 0o077 != 0)则视为权限过宽。
#[cfg(unix)]
fn key_permissions_too_open(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o077 != 0)
        .unwrap_or(false)
}
```

在 `src/api/auth.rs` 私钥加载处(line 343 `std::fs::read_to_string(pk_path)` 之前)插入告警:

```rust
            #[cfg(unix)]
            if key_permissions_too_open(std::path::Path::new(pk_path)) {
                tracing::warn!(
                    "CAS private key {} is group/other-accessible; recommend chmod 0600",
                    pk_path
                );
            }
```

(`pk_path` 为该作用域内私钥路径变量;按实际类型,若为 `&String` 则 `std::path::Path::new(pk_path.as_str())`。)

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test -p xet-server --lib api::auth::tests::test_key_permissions_detection`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/api/auth.rs
git commit -m "fix(cas): warn on group/other-accessible private key file (M-7)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 14: Minor 清理 + B-CONF-2 启动日志

> 纯 lint/文档/日志改动,由 `cargo build` + `cargo clippy`(目标 0 警告)验证。

**Files:**
- Modify: `src/api/lfs.rs`、`hub/src/cas_client/mod.rs`、`hub/src/api/commit.rs`、`src/chunking/cdc.rs`、`src/conversion/mod.rs`、`hub/src/auth/token_store.rs`、`hub/src/server.rs`

- [ ] **Step 1: M-1 折叠 collapsible-if**

用 clippy 自动修复折叠嵌套 if(edition 2024 支持 let-chains):

Run: `cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged`

随后人工核对受影响位置:`src/api/lfs.rs:90-93,100-103,327-330,337-340`(形如 `if let Some(ref bound_oid) = claims.oid { if bound_oid != &oid {` → `if let Some(ref bound_oid) = claims.oid && bound_oid != &oid {`),`hub/src/cas_client/mod.rs:224-225`。

- [ ] **Step 2: M-2 删除死代码**

删除 `hub/src/api/commit.rs:545-553` 的两个未用方法 `with_existing_oid` 与 `with_upload_failure`(连同其上方注释)。

- [ ] **Step 3: M-3 重命名测试**

`hub/src/api/commit.rs:709`:`test_commitAtomic_rejects_mismatched_parent` → `test_commit_atomic_rejects_mismatched_parent`。

- [ ] **Step 4: M-4 CDC mask 加 2 次幂断言**

在 `src/chunking/cdc.rs` 的 `Chunker::new`(line 64 计算 mask 之前)与 `StreamingChunker::new`(line 172 之前)各加:

```rust
        assert!(target.is_power_of_two(), "chunk target size must be a power of two, got {}", target);
```

(默认 target=65536 为 2 的幂,现有 mask 公式对 2 的幂正确;断言锁定该前提,不改变行为。)

- [ ] **Step 5: M-5 修正 conversion 文档**

`src/conversion/mod.rs:88-89` 文档:

```rust
/// Pipeline for converting raw LFS blobs into xorb+shard format
/// with global chunk-level deduplication.
```

改为:

```rust
/// Pipeline for converting raw LFS blobs into xorb+shard format.
/// Deduplication is performed at whole-xorb granularity (an identical xorb is
/// stored once). `num_deduped` counts chunks observed to already exist as a
/// statistic only — chunks are still packed into the new xorb.
```

- [ ] **Step 6: M-6 修正 salt 注释**

`hub/src/auth/token_store.rs:32-34` 与 `:385-386` 关于「prevents offline dictionary attacks if database is compromised」的注释,改为如实描述:

```rust
    /// M4: Server-side salt for token hashing. When provided out-of-band via
    /// HUB_TOKEN_HASH_SALT, it mitigates offline attacks against a leaked DB.
    /// If auto-generated, the salt is persisted in this same DB, so a full DB
    /// compromise exposes both — set HUB_TOKEN_HASH_SALT in production.
```

(line 385-386 处的重复声明同样改为「out-of-band salt 才提供该防护」。)

- [ ] **Step 7: B-CONF-2 Hub 启动日志**

在 `hub/src/server.rs:106`(`tracing::info!("CAS: ...")` 之后)插入:

```rust
    if config.server.host == "0.0.0.0" {
        tracing::warn!(
            "Hub is binding to 0.0.0.0 (all interfaces) on port {}. Ensure it sits behind a trusted proxy/firewall; authentication is always enforced.",
            config.server.port
        );
    }
    tracing::info!("Authentication: enforced — all public endpoints require a valid bearer token");
```

- [ ] **Step 8: 全量 build + clippy(0 警告)**

Run: `cargo build --workspace --all-targets && cargo clippy --workspace --all-targets`
Expected: 编译通过;clippy 无警告(原 11 个 Minor 警告清零)。

- [ ] **Step 9: 提交**

```bash
git add src/api/lfs.rs hub/src/cas_client/mod.rs hub/src/api/commit.rs src/chunking/cdc.rs src/conversion/mod.rs hub/src/auth/token_store.rs hub/src/server.rs
git commit -m "chore: clippy fixes, dead-code removal, doc corrections, hub startup logging (M-1..M-6, B-CONF-2)

Co-Authored-By: Claude Opus 4 <noreply@anthropic.com>"
```

---

## Task 15: 全量验证 + 重新 Review

- [ ] **Step 1: 全量 build**

Run: `cargo build --workspace --all-targets`
Expected: 通过。

- [ ] **Step 2: clippy 零警告**

Run: `cargo clippy --workspace --all-targets`
Expected: 无警告。

- [ ] **Step 3: 全量测试**

Run: `cargo test --workspace`
Expected: 全绿。

- [ ] **Step 4: 重新 code review(用户原始需求的「重新进行 review」)**

用 `superpowers:requesting-code-review` 对完整 diff(从本计划起点到当前)做复审,重点核验 4 个 Critical 的修复正确性与无回归。

- [ ] **Step 5: 清理验证期临时文件**

确认无遗留 `*.tmp` / tempdir 残留(测试用 `tempfile` 自动清理)。

---

## 范围外(明确不做)

- `src/gc/*` 全部 —— C1(soft-cycle 软周期)与 C2(删除下限兜底)均不动。GC 默认关闭。
- B-CONF-1(未配私钥时 batch 透传 xet token)—— 保留现状,已有 warn 日志,记为已接受风险。
- 跨 xorb chunk 级去重的实现(仅修文档,见 M-5)。
- 限流器 XFF 改造、改 Hub 默认绑定地址(默认值不动,仅加日志)。

---

## Self-Review 记录

- **Spec 覆盖:** C-AUTH-1→T1,C-AUTH-2→T2,C-DATA-1→T3,C-DATA-2→T4,I-STOR-1→T5,I-STOR-2→T6,I-STOR-3→T7,I-STOR-4→T8,I-API-1→T9,I-API-2→T10,I-API-3→T11,I-API-4→T12,M-7→T13,M-1..M-6+B-CONF-2→T14。全覆盖。
- **对 spec 的修正(研究后):** ① C-DATA-2 校验区域是 header+压缩数据(非解压后),在解压前校验;② C-DATA-1 可信上界是 chunk header 的 `uncompressed_length`,实现为「LZ4 前缀必须 == original_size」;③ I-API-3 经核实 `commit_atomic` 已正确拒绝,降级为补测试(T11 含「若意外失败则升级为代码 bug」的指引)。
- **类型一致性:** `extract_chunk_verified(&[u8], usize, &MerkleHash) -> Result<Bytes, String>`、`rewrite_action_url(...) -> bool`、`copy_then_rename(&Path, &Path)`、`key_permissions_too_open(&Path) -> bool`、`check_lz4_prefix(&[u8], usize) -> Result<()>` 在定义与调用处一致。
