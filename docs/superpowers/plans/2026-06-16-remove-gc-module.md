# GC 模块移除实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 完全移除 GC（垃圾回收）模块及其所有依赖，包括代码、配置、测试和文档

**Architecture:** 采用 5 层分层删除策略。每层完成后验证编译通过，确保渐进式安全移除。第1层解除依赖，第2层删除模块，第3层清理配置，第4层清理测试，第5层清理文档。

**Tech Stack:** Rust, Actix-web, Cargo

**设计文档:** `docs/superpowers/specs/2026-06-16-remove-gc-module-design.md`

---

## 任务概览

| 任务 | 说明 | 预计时间 |
|------|------|----------|
| Task 1 | 解除 conversion/mod.rs 中的 ReferenceTracker 依赖 | 5 分钟 |
| Task 2 | 解除 api/shard.rs 中的 ReferenceTracker 依赖 | 5 分钟 |
| Task 3 | 解除 api/lfs.rs 中的 ReferenceTracker 依赖 | 3 分钟 |
| Task 4 | 解除 server.rs 中的 GC 依赖 | 5 分钟 |
| Task 5 | 验证第1层完成 | 3 分钟 |
| Task 6 | 删除 GC 模块代码和声明 | 3 分钟 |
| Task 7 | 验证第2层完成 | 3 分钟 |
| Task 8 | 清理 config.rs 中的 GC 配置 | 10 分钟 |
| Task 9 | 验证第3层完成 | 3 分钟 |
| Task 10 | 删除 GC 测试文件和依赖 | 3 分钟 |
| Task 11 | 验证第4层完成 | 5 分钟 |
| Task 12 | 删除 GC 文档目录 | 2 分钟 |
| Task 13 | 清理文档中的 GC 引用 | 10 分钟 |
| Task 14 | 最终验证和提交 | 5 分钟 |

**总预计时间：** 约 65 分钟

---

## Task 1: 解除 conversion/mod.rs 中的 ReferenceTracker 依赖

**Files:**
- Modify: `src/conversion/mod.rs:10,100,109,113-116,339-353`

- [ ] **Step 1: 移除 ReferenceTracker import**

打开 `src/conversion/mod.rs`，删除第10行：

```rust
use crate::gc::reference_tracker::ReferenceTracker;
```

- [ ] **Step 2: 移除 ref_tracker 字段**

删除第100行的字段定义：

```rust
/// I5 fix: Optional reference tracker for proactive sidecar generation.
/// When present, the pipeline generates a `.refs.json` sidecar immediately
/// after storing the shard, so the first GC scan can use the fast Layer 1
/// path instead of parsing every shard (Layer 2).
ref_tracker: Option<Arc<dyn ReferenceTracker>>,
```

- [ ] **Step 3: 移除构造函数中的 ref_tracker 初始化**

删除第109行的初始化：

```rust
Self { storage, index, config, ref_tracker: None }
```

改为：

```rust
Self { storage, index, config }
```

- [ ] **Step 4: 移除 with_ref_tracker 方法**

删除第113-116行的方法：

```rust
/// Set the reference tracker for proactive sidecar generation.
pub fn with_ref_tracker(mut self, ref_tracker: Arc<dyn ReferenceTracker>) -> Self {
    self.ref_tracker = Some(ref_tracker);
    self
}
```

- [ ] **Step 5: 移除 convert() 中的 sidecar 生成代码**

删除第339-353行的代码块：

```rust
// 9. I5 fix: Proactively generate sidecar for GC reference tracking.
// This ensures the first GC scan uses the fast Layer 1 path (read sidecar)
// instead of Layer 2 (download + parse shard). Without this, every newly
// converted shard requires a full parse on first GC scan.
if let Some(ref tracker) = self.ref_tracker {
    let lfs_refs = vec![oid.to_string()];
    let xorb_refs = vec![xorb_hash.clone()];
    if let Err(e) = tracker.record_references(&shard_hash, &lfs_refs, &xorb_refs).await {
        // Non-fatal: GC will fall back to Layer 2 (parse shard) on next scan
        warn!("Failed to generate sidecar for shard {}: {} (non-fatal)", shard_hash, e);
    } else {
        info!("Generated sidecar for shard {} ({} lfs, {} xorb refs)",
            shard_hash, lfs_refs.len(), xorb_refs.len());
    }
}
```

- [ ] **Step 6: 提交**

```bash
git add src/conversion/mod.rs
git commit -m "refactor: remove ReferenceTracker dependency from conversion pipeline

Remove ref_tracker field, with_ref_tracker method, and sidecar generation
code from conversion pipeline. GC module will be removed in subsequent tasks."
```

---

## Task 2: 解除 api/shard.rs 中的 ReferenceTracker 依赖

**Files:**
- Modify: `src/api/shard.rs:15,37-38,191-208,299-308,333-343`

- [ ] **Step 1: 移除 ReferenceTracker import**

删除第15行：

```rust
use crate::gc::reference_tracker::ReferenceTracker;
```

- [ ] **Step 2: 移除 handler 参数**

删除第37-38行的参数：

```rust
ref_tracker: web::Data<Arc<dyn ReferenceTracker>>,
req: actix_web::HttpRequest,
```

改为：

```rust
req: actix_web::HttpRequest,
```

- [ ] **Step 3: 移除 sidecar 生成代码**

删除第191-208行的代码块：

```rust
// I5 fix: Proactively generate sidecar for GC reference tracking.
// This ensures the first GC scan uses the fast Layer 1 path (read sidecar)
// instead of Layer 2 (download + parse shard).
let xorb_refs: Vec<String> = shard
    .chunk_mappings()
    .iter()
    .map(|(_, x, _)| x.to_hex())
    .collect::<std::collections::HashSet<_>>()
    .into_iter()
    .collect();
if let Err(e) = ref_tracker.record_references(&shard_id, &file_hashes, &xorb_refs).await {
    // Non-fatal: GC will fall back to Layer 2 (parse shard) on next scan
    tracing::warn!(
        shard_id = %shard_id,
        error = %e,
        "Failed to generate sidecar for uploaded shard (non-fatal)"
    );
}
```

- [ ] **Step 4: 移除测试代码中的 ref_tracker（第一处）**

删除第299-308行的测试代码：

```rust
let ref_tracker: Arc<dyn ReferenceTracker> =
    Arc::new(crate::gc::reference_tracker::s3::SidecarReferenceTracker::new(storage_arc.clone()));

let app = test::init_service(
    App::new()
        .app_data(web::Data::from(storage_arc))
        .app_data(web::Data::new(index))
        .app_data(web::Data::new(auth))
        .app_data(web::Data::new(config))
        .app_data(web::Data::new(ref_tracker))
        .route("/v1/shards", web::post().to(upload_shard))
).await;
```

改为：

```rust
let app = test::init_service(
    App::new()
        .app_data(web::Data::from(storage_arc))
        .app_data(web::Data::new(index))
        .app_data(web::Data::new(auth))
        .app_data(web::Data::new(config))
        .route("/v1/shards", web::post().to(upload_shard))
).await;
```

- [ ] **Step 5: 移除测试代码中的 ref_tracker（第二处）**

删除第333-343行的测试代码（同上一步相同模式）。

- [ ] **Step 6: 提交**

```bash
git add src/api/shard.rs
git commit -m "refactor: remove ReferenceTracker dependency from shard upload handler

Remove ref_tracker parameter, sidecar generation code, and test setup."
```

---

## Task 3: 解除 api/lfs.rs 中的 ReferenceTracker 依赖

**Files:**
- Modify: `src/api/lfs.rs:295-296,358-359`

- [ ] **Step 1: 移除 handler 参数**

删除第295-296行的参数：

```rust
ref_tracker: web::Data<Arc<dyn crate::gc::reference_tracker::ReferenceTracker>>,
req: actix_web::HttpRequest,
```

改为：

```rust
req: actix_web::HttpRequest,
```

- [ ] **Step 2: 移除 with_ref_tracker 调用**

删除第358-359行：

```rust
// I5 fix: Pass ref_tracker for proactive sidecar generation
.with_ref_tracker(ref_tracker.get_ref().clone());
```

改为（移除这两行，保留前面的链式调用）：

```rust
```

- [ ] **Step 3: 提交**

```bash
git add src/api/lfs.rs
git commit -m "refactor: remove ReferenceTracker dependency from LFS download handler

Remove ref_tracker parameter and with_ref_tracker call from conversion pipeline."
```

---

## Task 4: 解除 server.rs 中的 GC 依赖

**Files:**
- Modify: `src/server.rs:12-13,55-84,169-171,179-181`

- [ ] **Step 1: 移除 GC 和 ReferenceTracker imports**

删除第12-13行：

```rust
use crate::gc::{IncrementalGarbageCollector, IncrementalGcStats, start_incremental_gc_background_task};
use crate::gc::reference_tracker::s3::SidecarReferenceTracker;
```

- [ ] **Step 2: 移除 GC 初始化和背景任务**

删除第55-84行的整个 GC 初始化块：

```rust
// GC: Incremental garbage collector for orphaned blobs
// Validate GC configuration
for warning in crate::config::validate_gc_config(&config) {
    tracing::warn!("{}", warning);
}

// Create sidecar reference tracker for incremental GC
let ref_tracker: Arc<dyn crate::gc::reference_tracker::ReferenceTracker> =
    Arc::new(SidecarReferenceTracker::new(storage.clone()));
// I5 fix: Share ref_tracker via web::Data so conversion pipeline and shard upload
// can proactively generate sidecars (avoids first-GC-scan Layer 2 fallback).
// Use web::Data::new() (not ::from()) to match handler's `web::Data<Arc<dyn ReferenceTracker>>` type.
let ref_tracker_data = actix_web::web::Data::new(ref_tracker.clone());

// Create incremental GC
let gc = match IncrementalGarbageCollector::new(
    storage.clone(),
    ref_tracker.clone(),
    config.gc.clone(),
) {
    Ok(gc) => Arc::new(gc),
    Err(e) => {
        tracing::error!("Failed to create incremental garbage collector: {}", e);
        return Err(std::io::Error::other(format!("Failed to create GC: {}", e)));
    }
};
let last_gc_stats = Arc::new(RwLock::new(None::<IncrementalGcStats>));

// Start background GC task (if enabled)
start_incremental_gc_background_task(gc.clone(), last_gc_stats.clone()).await;
```

- [ ] **Step 3: 移除 app_data 注入**

删除第169-171行：

```rust
.app_data(web::Data::new(gc_for_app.clone()))
.app_data(web::Data::new(stats_for_app.clone()))
.app_data(ref_tracker_data.clone())
```

注意：检查这些变量是否在前面定义（gc_for_app, stats_for_app），如果是，也需要删除它们的定义。

- [ ] **Step 4: 移除 GC 路由注册**

删除第179-181行：

```rust
// GC endpoints (CAS internal) - no rate limiting
.route("/internal/gc/run", web::post().to(crate::api::gc::trigger_gc))
.route("/internal/gc/status", web::get().to(crate::api::gc::gc_status))
```

- [ ] **Step 5: 提交**

```bash
git add src/server.rs
git commit -m "refactor: remove GC initialization and routes from server

Remove GC imports, initialization code, background task startup, app_data
injection, and route registration. GC module still exists but is unused."
```

---

## Task 5: 验证第1层完成

- [ ] **Step 1: 编译检查**

```bash
cargo check
```

Expected: 编译通过（此时 GC 模块仍存在但未被使用）

- [ ] **Step 2: 检查残留引用**

```bash
grep -rn "ReferenceTracker\|ref_tracker" src/conversion/ src/api/ src/server.rs
```

Expected: 无输出

- [ ] **Step 3: 确认第1层完成**

如果编译通过且无残留引用，第1层完成。

---

## Task 6: 删除 GC 模块代码和声明

**Files:**
- Delete: `src/gc/` (整个目录)
- Delete: `src/api/gc.rs`
- Modify: `src/lib.rs:52`
- Modify: `src/api/mod.rs:12`

- [ ] **Step 1: 删除 GC 模块目录**

```bash
rm -rf src/gc/
```

- [ ] **Step 2: 删除 GC API handler**

```bash
rm src/api/gc.rs
```

- [ ] **Step 3: 从 lib.rs 移除 gc 模块声明**

删除第52行：

```rust
pub mod gc;
```

- [ ] **Step 4: 从 api/mod.rs 移除 gc 模块声明**

删除第12行：

```rust
pub mod gc;
```

- [ ] **Step 5: 提交**

```bash
git add -A src/
git commit -m "refactor: delete GC module code

Remove src/gc/ directory (10 files, ~2,755 lines), src/api/gc.rs,
and module declarations from lib.rs and api/mod.rs."
```

---

## Task 7: 验证第2层完成

- [ ] **Step 1: 编译检查**

```bash
cargo check
```

Expected: 编译通过

- [ ] **Step 2: 库测试**

```bash
cargo test --lib
```

Expected: 所有库测试通过

- [ ] **Step 3: 检查 GC 模块引用**

```bash
grep -rn "gc::" src/
```

Expected: 无输出

- [ ] **Step 4: 确认第2层完成**

如果编译通过且测试通过，第2层完成。

---

## Task 8: 清理 config.rs 中的 GC 配置

**Files:**
- Modify: `src/config.rs:227-247,255-273,283-301,310-325,339-353,369-395,416-489,539-593,772-805`

- [ ] **Step 1: 移除 BloomConfig 结构体**

删除第227-247行的 `BloomConfig` 结构体定义及其 `Default` 实现。

- [ ] **Step 2: 移除 ScannerConfig 结构体**

删除第255-273行的 `ScannerConfig` 结构体定义及其 `Default` 实现。

- [ ] **Step 3: 移除 GraceConfig 结构体**

删除第283-301行的 `GraceConfig` 结构体定义及其 `Default` 实现。

- [ ] **Step 4: 移除 LeaseConfig 结构体**

删除第310-325行的 `LeaseConfig` 结构体定义及其 `Default` 实现。

- [ ] **Step 5: 移除 ReferenceTrackerConfig 结构体**

删除第339-353行的 `ReferenceTrackerConfig` 结构体定义及其 `Default` 实现。

- [ ] **Step 6: 移除 GcConfig 结构体**

删除第369-395行的 `GcConfig` 结构体定义及其 `Default` 实现。

- [ ] **Step 7: 移除 GcConfig::from_env 方法**

删除第416-489行的 `GcConfig::from_env()` 方法。

- [ ] **Step 8: 移除 GcConfig::validate 方法**

删除第539-593行的 `GcConfig::validate()` 方法。

- [ ] **Step 9: 移除 validate_gc_config 函数**

删除第772-805行的 `validate_gc_config()` 函数。

- [ ] **Step 10: 从 ServerConfig 移除 gc 字段**

从 `ServerConfig` 结构体定义中移除：

```rust
pub gc: GcConfig,
```

并从 `ServerConfig::from_env()` 中移除 gc 字段的初始化。

- [ ] **Step 11: 提交**

```bash
git add src/config.rs
git commit -m "refactor: remove all GC configuration from config.rs

Remove BloomConfig, ScannerConfig, GraceConfig, LeaseConfig,
ReferenceTrackerConfig, GcConfig structs and their methods.
Remove gc field from ServerConfig."
```

---

## Task 9: 验证第3层完成

- [ ] **Step 1: 编译检查**

```bash
cargo check
```

Expected: 编译通过

- [ ] **Step 2: 检查 GC 配置引用**

```bash
grep -rn "GcConfig\|GC_" src/
```

Expected: 无输出（或仅有注释中的历史引用）

- [ ] **Step 3: 确认第3层完成**

如果编译通过且无配置引用，第3层完成。

---

## Task 10: 删除 GC 测试文件和依赖

**Files:**
- Delete: `tests/gc/` (目录)
- Delete: `tests/config/gc_config_test.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: 删除 GC 测试目录**

```bash
rm -rf tests/gc/
```

- [ ] **Step 2: 删除 GC 配置测试**

```bash
rm tests/config/gc_config_test.rs
```

- [ ] **Step 3: 从 Cargo.toml 移除 bloomfilter 依赖**

删除这一行：

```toml
bloomfilter = "1.0.8"
```

- [ ] **Step 4: 从 Cargo.toml 移除 GC 测试配置**

删除以下测试配置块（如果存在）：

```toml
[[test]]
name = "errors_test"
path = "tests/gc/errors_test.rs"

[[test]]
name = "gc_config_test"
path = "tests/config/gc_config_test.rs"
```

- [ ] **Step 5: 提交**

```bash
git add -A tests/ Cargo.toml Cargo.lock
git commit -m "refactor: remove GC tests and bloomfilter dependency

Delete tests/gc/ directory and gc_config_test.rs.
Remove bloomfilter crate from Cargo.toml."
```

---

## Task 11: 验证第4层完成

- [ ] **Step 1: 完整测试**

```bash
cargo test
```

Expected: 所有测试通过

- [ ] **Step 2: 检查代码中所有 GC 引用**

```bash
grep -rn "gc::\|GcConfig\|GC_\|bloom\|ref_tracker\|ReferenceTracker" src/ tests/
```

Expected: 无输出

- [ ] **Step 3: 确认第4层完成**

如果所有测试通过且代码中无 GC 引用，第4层完成。

---

## Task 12: 删除 GC 文档目录

**Files:**
- Delete: `docs/gc/` (目录)
- Delete: `docs/superpowers/specs/2026-06-14-incremental-gc-bloom-filter-design.md`

- [ ] **Step 1: 删除 GC 文档目录**

```bash
rm -rf docs/gc/
```

- [ ] **Step 2: 删除 GC 设计规格文档**

```bash
rm docs/superpowers/specs/2026-06-14-incremental-gc-bloom-filter-design.md
```

- [ ] **Step 3: 提交**

```bash
git add -A docs/
git commit -m "docs: delete GC documentation

Remove docs/gc/ directory (architecture, configuration, migration guides)
and GC design spec document."
```

---

## Task 13: 清理文档中的 GC 引用

**Files:**
- Modify: `docs/README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/configuration.md`
- Modify: `docs/api/cas-api.md`
- Modify: `README.md` (根目录)

- [ ] **Step 1: 清理 docs/README.md**

移除文档状态表中 GC 相关条目（gc/migration.md, gc/configuration.md, gc/architecture.md）。
移除目录结构中 `docs/gc/` 部分。

- [ ] **Step 2: 清理 docs/architecture.md**

移除以下内容：
- gc/ 模块树（第167-177行左右）
- GC 工作流程描述（第404-431行左右）
- 宽限期描述（第432-435行左右）
- 核心特性中的 GC 条目（第454-459行左右）
- 配置表格中所有 GC_* 行（第465-482行左右）

- [ ] **Step 3: 清理 docs/configuration.md**

移除整个"增量 GC v2 设置"章节（约第135-222行）。

- [ ] **Step 4: 清理 docs/api/cas-api.md**

移除 `/internal/gc/run` 和 `/internal/gc/status` 端点文档。
移除 API 端点表格中的 GC 行。

- [ ] **Step 5: 清理 README.md（根目录）**

移除 GC 相关的功能描述、环境变量列表、架构说明。

- [ ] **Step 6: 提交**

```bash
git add docs/ README.md
git commit -m "docs: remove GC references from documentation

Clean up GC mentions in README, architecture, configuration, and API docs."
```

---

## Task 14: 最终验证和提交

- [ ] **Step 1: 检查文档中所有 GC 引用**

```bash
grep -rn "GC_\|/gc/\|garbage collection\|垃圾回收" docs/ README.md | grep -v "superpowers/specs/2026-06-16"
```

Expected: 无输出（或仅有设计文档中的历史记录）

- [ ] **Step 2: 最终编译和测试**

```bash
cargo build --release
cargo test
```

Expected: 编译和测试全部通过

- [ ] **Step 3: 生成移除报告**

```bash
echo "=== GC 模块移除完成 ===" && \
echo "删除的代码行数: $(git log --oneline --diff-filter=D --summary | grep -c 'delete mode')" && \
echo "剩余的 GC 引用: $(grep -r 'GC_\|gc::' src/ docs/ 2>/dev/null | wc -l)" && \
echo "编译状态: $(cargo check 2>&1 | grep -q 'Finished' && echo '通过' || echo '失败')"
```

- [ ] **Step 4: 创建总结提交**

```bash
git add -A
git commit -m "refactor: complete GC module removal

Successfully removed all GC-related code, configuration, tests, and documentation.
All tests pass. See docs/superpowers/specs/2026-06-16-remove-gc-module-design.md for details.

Removed:
- GC module code (~2,755 lines)
- ReferenceTracker dependency
- GC configuration (18 environment variables)
- GC tests and bloomfilter dependency
- GC documentation (~2,466 lines)

Affected functionality: None (all non-GC features work normally)"
```

- [ ] **Step 5: 验证完成**

确认：
- ✅ `cargo build --release` 通过
- ✅ `cargo test` 通过
- ✅ 代码中无 GC 引用
- ✅ 文档中无 GC 引用（除设计文档外）
- ✅ 所有任务已提交

---

## 恢复指南

如需恢复 GC 功能，可从 git 历史恢复：

```bash
# 查看 GC 移除前的最后一次提交
git log --oneline | grep -B1 "complete GC module removal"

# 恢复特定文件
git checkout <commit-hash>^ -- src/gc/ src/api/gc.rs src/config.rs

# 或完全回滚
git revert <commit-hash>
```

---

## 检查清单

完成后确认：

- [ ] 所有代码中的 GC 引用已移除
- [ ] 所有配置中的 GC 字段已移除
- [ ] 所有测试中的 GC 测试已移除
- [ ] 所有文档中的 GC 引用已移除
- [ ] bloomfilter 依赖已从 Cargo.toml 移除
- [ ] cargo build --release 通过
- [ ] cargo test 通过
- [ ] 所有更改已提交
- [ ] 设计文档已保留作为历史记录
