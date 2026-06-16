# 移除 GC 模块设计文档

**日期**：2026-06-16  
**状态**：待实施  
**决策**：完全删除 GC 功能，包括所有代码、配置、测试和文档

---

## 背景

GC（垃圾回收）模块存在问题，需要暂时移除。本文档描述了完全删除 GC 功能的范围和策略。

## 移除范围

### 代码移除

**GC 核心模块**（约 2,755 行）：

| 文件 | 行数 | 说明 |
|------|------|------|
| `src/gc/mod.rs` | 676 | GC 主逻辑和 5 阶段循环 |
| `src/gc/bloom.rs` | 408 | Bloom filter 实现 |
| `src/gc/scanner.rs` | 462 | 增量扫描器 |
| `src/gc/coordinator.rs` | 436 | 多节点租约协调 |
| `src/gc/checkpoint.rs` | 297 | 崩溃恢复 |
| `src/gc/grace.rs` | 116 | 宽限期管理 |
| `src/gc/errors.rs` | 49 | 错误类型 |
| `src/gc/reference_tracker/mod.rs` | 88 | 引用追踪 trait |
| `src/gc/reference_tracker/s3.rs` | 223 | S3 sidecar 实现 |
| `src/api/gc.rs` | 114 | GC API 端点 |

**依赖模块清理**：

| 文件 | 移除内容 |
|------|----------|
| `src/server.rs` | GC 导入、初始化、背景任务、路由注册、app_data 注入 |
| `src/api/shard.rs` | ReferenceTracker 导入、handler 参数、sidecar 生成代码、测试代码 |
| `src/api/lfs.rs` | ReferenceTracker handler 参数、with_ref_tracker 调用 |
| `src/conversion/mod.rs` | ReferenceTracker 导入、ref_tracker 字段、with_ref_tracker 方法、sidecar 生成代码 |
| `src/lib.rs` | `pub mod gc;` 声明 |
| `src/api/mod.rs` | `pub mod gc;` 声明 |

**配置清理**（`src/config.rs`）：

移除以下结构体和函数：
- `BloomConfig`（第227-247行）
- `ScannerConfig`（第255-273行）
- `GraceConfig`（第283-301行）
- `LeaseConfig`（第310-325行）
- `ReferenceTrackerConfig`（第339-353行）
- `GcConfig`（第369-395行）
- `GcConfig::from_env()`（第416-489行）
- `GcConfig::validate()`（第539-593行）
- `validate_gc_config()`（第772-805行）
- `ServerConfig` 中的 `gc: GcConfig` 字段

**测试清理**：

- 删除 `tests/gc/errors_test.rs`
- 删除 `tests/config/gc_config_test.rs`
- 移除 `Cargo.toml` 中的 GC 测试配置

**依赖清理**：

- 从 `Cargo.toml` 移除 `bloomfilter = "1.0.8"`

### 文档移除

**删除的文档文件**（约 2,466 行）：

| 文件 | 行数 | 说明 |
|------|------|------|
| `docs/gc/architecture.md` | 614 | GC v2 架构设计 |
| `docs/gc/configuration.md` | 351 | GC 配置参考 |
| `docs/gc/migration.md` | 477 | 旧 GC 迁移指南 |
| `docs/superpowers/specs/2026-06-14-incremental-gc-bloom-filter-design.md` | 1024 | GC 设计规格 |

**需要清理 GC 引用的文档**：

| 文件 | 移除内容 |
|------|----------|
| `docs/README.md` | GC 文档条目、目录结构中 docs/gc/ 部分 |
| `docs/architecture.md` | gc/ 模块树、GC 工作流程、宽限期、核心特性、GC 配置表格 |
| `docs/configuration.md` | 整个"增量 GC v2 设置"章节 |
| `docs/api/cas-api.md` | `/internal/gc/run` 和 `/internal/gc/status` 端点文档 |
| `README.md`（根目录） | GC 功能描述、环境变量、架构说明 |

---

## 实施策略：分层删除

### 第1层：解除 ReferenceTracker 依赖

修改以下文件，移除 ReferenceTracker 相关代码：

**`src/conversion/mod.rs`**：
- 移除 `use crate::gc::reference_tracker::ReferenceTracker;`（第10行）
- 移除 `ref_tracker: Option<Arc<dyn ReferenceTracker>>` 字段（第100行）
- 移除构造函数中 `ref_tracker: None` 初始化（第109行）
- 移除 `with_ref_tracker()` 方法（第113-116行）
- 移除 convert() 中主动生成 sidecar 的代码块（第339-353行）

**`src/api/shard.rs`**：
- 移除 `use crate::gc::reference_tracker::ReferenceTracker;`（第15行）
- 移除 handler 参数 `ref_tracker: web::Data<Arc<dyn ReferenceTracker>>`（第37-38行）
- 移除上传时主动生成 sidecar 的代码块（第191-208行）
- 移除测试代码中的 ref_tracker 创建和注入（第299-308行、第333-343行）

**`src/api/lfs.rs`**：
- 移除 handler 参数 `ref_tracker: web::Data<Arc<dyn ReferenceTracker>>`（第295-296行）
- 移除 `.with_ref_tracker(ref_tracker.get_ref().clone())`（第358-359行）

**`src/server.rs`**：
- 移除 GC 和 ReferenceTracker 的 use 导入（第12-13行）
- 移除 GC 配置验证、ref_tracker 创建、GC 实例化、背景任务启动（第55-84行）
- 移除 `.app_data()` 注入 gc、stats、ref_tracker（第169-171行）
- 移除 GC 路由注册（第179-181行）

**验证**：`cargo check` 应通过（此时 GC 模块仍存在但未被使用）

### 第2层：删除 GC 模块代码

- 删除整个 `src/gc/` 目录（10个文件）
- 删除 `src/api/gc.rs`
- 从 `src/lib.rs` 移除 `pub mod gc;`
- 从 `src/api/mod.rs` 移除 `pub mod gc;`

**验证**：`cargo check` 和 `cargo test --lib` 应通过

### 第3层：清理配置

从 `src/config.rs` 移除所有 GC 相关结构体和函数（见上文列表）。

**验证**：`cargo check` 应通过

### 第4层：清理测试和依赖

- 删除 `tests/gc/` 目录
- 删除 `tests/config/gc_config_test.rs`
- 从 `Cargo.toml` 移除 `bloomfilter` 依赖
- 从 `Cargo.toml` 移除 GC 测试配置

**验证**：`cargo test` 应通过

### 第5层：清理文档

- 删除 `docs/gc/` 目录
- 删除 `docs/superpowers/specs/2026-06-14-incremental-gc-bloom-filter-design.md`
- 清理其他文档中的 GC 引用（见上文列表）

**验证**：`grep -rn "GC_\|/gc/\|gc/" docs/` 应无结果（或仅有历史记录引用）

---

## 边界情况处理

### 存储中的 GC 数据

**不删除**以下已有数据（数据层面不清理）：

- `shard_refs/*.refs.json` — sidecar 文件（ReferenceTracker 写入）
- `.gc/checkpoint.json` — GC checkpoint
- `.gc/bloom.bin` — Bloom filter 状态
- `.gc/lease.json` — GC 租约

移除代码后，不再有代码读写这些文件。运维人员可手动清理：

```bash
# 可选：清理 GC 相关数据（如果不再需要恢复 GC）
rm -rf {storage_path}/shard_refs/
rm -rf {storage_path}/.gc/
rm -rf {GC_DATA_DIR}/
```

### 不需要修改的文件

- `src/storage/` — 无 GC 引用
- `src/index.rs` — 无 GC 引用
- `hub/` — Hub 服务不依赖 GC
- `xet-auth-types/` — 认证类型不依赖 GC

---

## 验证清单

每层完成后执行以下验证：

```bash
# 1. 编译检查
cargo check

# 2. 库测试
cargo test --lib

# 3. 代码残留检查
grep -rn "gc::\|GcConfig\|GC_\|bloom\|ref_tracker\|ReferenceTracker" src/

# 4. 文档残留检查
grep -rn "GC_\|/gc/\|gc/" docs/

# 5. 完整测试（第4层完成后）
cargo test
```

最终验证应满足：
- ✅ `cargo check` 通过
- ✅ `cargo test` 通过
- ✅ 代码中无 GC 相关引用
- ✅ 文档中无 GC 相关引用（除历史记录外）

---

## 影响评估

### 移除的功能

- 增量垃圾回收（5阶段循环）
- Bloom filter 引用追踪
- 多节点 GC 协调（租约机制）
- 崩溃恢复（checkpoint）
- 宽限期保护
- Sidecar 引用追踪（.refs.json）
- GC API 端点（/internal/gc/run, /internal/gc/status）
- Prometheus GC 指标

### 不受影响的功能

- LFS 对象上传/下载
- Xorb 上传/下载
- Shard 上传
- 文件重建（reconstruction）
- 转换管道（conversion pipeline）— 移除 ref_tracker 后仍正常工作
- Hub API
- 认证系统
- 存储后端（local, S3）

### 潜在风险

1. **存储膨胀**：无 GC 后，未引用的 blob 会持续积累
   - 缓解：手动清理或恢复 GC 功能
   
2. **sidecar 文件残留**：存储中已有的 .refs.json 文件不会被清理
   - 缓解：手动删除 shard_refs/ 目录

3. **恢复 GC 的成本**：需要恢复所有删除的代码
   - 缓解：代码在 git 历史中，可通过 revert 恢复

---

## 决策记录

### 为什么选择"完全删除"而非"保留代码但禁用"？

- 代码存在但禁用会增加维护负担
- GC 模块与其他模块有交叉依赖（ReferenceTracker），保留会增加复杂度
- git 历史保留完整代码，恢复成本低

### 为什么一并删除 ReferenceTracker？

- ReferenceTracker 是为 GC 服务的（记录 shard 引用关系）
- 无 GC 时，sidecar 文件无用途
- 保留会增加 shard 上传的 I/O 开销（写入 .refs.json）

### 为什么不保留 GC 配置作为 deprecated？

- 配置结构体在代码中被引用，保留需要条件编译
- 增加代码复杂度，收益有限
- git 历史保留完整配置，恢复成本低

---

## 总结

**移除范围**：
- 代码：约 2,755 行 GC 模块 + 约 100 行依赖清理
- 配置：6 个结构体，18 个环境变量
- API：2 个端点
- 文档：约 2,466 行
- 测试：约 66 行 + 内联测试

**实施策略**：5 层分层删除，每层验证编译通过

**预计工时**：
- 代码移除：30 分钟
- 文档清理：20 分钟
- 验证和调试：20 分钟
- 总计：约 70 分钟

**恢复方案**：`git revert` 或从 git 历史恢复删除的文件
