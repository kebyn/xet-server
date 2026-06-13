# CAS_STATE_DB_PATH 回滚报告

> **历史记录** — CAS_STATE_DB_PATH 配置已被回滚移除，CAS 不使用 SQLite。本文档保留作为决策记录。

## 回滚原因

经过深入代码分析，发现 **CAS_STATE_DB_PATH 配置实际上不需要**。

---

## 分析过程

### 1. 文档描述 vs 代码实现

**文档声称**：
- `CAS_STATE_DB_PATH` 用于配置 SQLite 状态数据库
- 跟踪 blob 的存储状态（RawOnly/XetOnly）
- 默认路径 `/tmp/xet-state.db`

**代码实际**：
- ❌ 没有任何代码读取 `config.state_db_path`
- ❌ 没有独立的状态数据库文件
- ❌ 状态跟踪通过其他机制实现

### 2. 实际的状态跟踪机制

通过分析 `src/api/internal.rs` 的 `get_blob_state` 函数，发现状态跟踪实际通过以下方式实现：

```rust
// 检查 xet_only 状态 - 通过 MetadataIndex
if index.get_shards_for_file(&oid).is_some() {
    return HttpResponse::Ok().json(serde_json::json!({
        "state": "xet_only",
        ...
    }));
}

// 检查 raw_only 状态 - 通过 StorageBackend
let object_key = format!("lfs/objects/{}", oid);
match storage.exists(&object_key).await {
    Ok(true) => {
        return HttpResponse::Ok().json(serde_json::json!({
            "state": "raw_only",
            ...
        }));
    }
}
```

**结论**：
- ✅ **MetadataIndex** (metadata_index.db) 跟踪已转换的 xet 文件
- ✅ **StorageBackend** 检查原始 blob 是否存在
- ❌ **不需要独立的状态数据库**

### 3. 架构真相

```
实际架构：
┌─────────────────────────────────────┐
│         CAS Server                  │
│                                     │
│  MetadataIndex (metadata_index.db) │ ← 跟踪 xet_only
│  StorageBackend (文件系统/S3)       │ ← 跟踪 raw_only
│                                     │
│  状态查询：                         │
│  1. 查 MetadataIndex → xet_only    │
│  2. 查 StorageBackend → raw_only   │
│  3. 都没找到 → 404                 │
└─────────────────────────────────────┘

文档描述的架构（错误）：
┌─────────────────────────────────────┐
│         CAS Server                  │
│                                     │
│  State Database (xet-state.db)     │ ← 不存在
│  - blob 状态（RawOnly/XetOnly）     │
└─────────────────────────────────────┘
```

---

## 回滚操作

### 代码修改

**已移除**：
1. `src/config.rs` - ServerConfig 结构体中的 `state_db_path` 字段
2. `src/config.rs` - Default 实现中的 `state_db_path` 初始化
3. `src/config.rs` - from_env() 中的 `CAS_STATE_DB_PATH` 环境变量读取

**验证**：
```bash
cargo check
# 结果：编译通过 ✅
```

### 文档修改

**README.md**：
- ✅ 移除第 134 行：`export CAS_STATE_DB_PATH=/data/xet-state.db`
- ✅ 移除第 317 行：CAS_STATE_DB_PATH 配置表格行

**docs/configuration.md**：
- ✅ 移除第 95-108 行：整个"状态数据库"章节
- ✅ 移除示例中的 3 处 `export CAS_STATE_DB_PATH=...` 行

**docs/architecture.md**：
- ✅ 第 42 行：将"状态管理"改为"元数据索引管理"
- ✅ 第 164-165 行：将 `state/` 目录改为 `index.rs`
- ✅ 第 172 行：将"状态数据库"改为"元数据索引"
- ✅ 第 177 行：将"状态数据库"改为"元数据索引"
- ✅ 第 679 行：将"状态数据库需要同步"改为"元数据索引需要同步"

---

## 影响评估

### 正面影响
1. **消除混淆** - 用户不再配置无用的环境变量
2. **文档准确** - 架构文档与实际代码一致
3. **简化部署** - 减少一个不必要的配置项

### 无负面影响
- ✅ 无破坏性变更（配置从未被使用）
- ✅ 无功能损失（状态跟踪正常工作）
- ✅ 无性能影响

---

## 正确的状态跟踪说明

### 状态查询流程

```
客户端请求：GET /internal/state/{oid}

1. 检查 MetadataIndex
   └─ index.get_shards_for_file(&oid)
      ├─ 找到 → 返回 "xet_only"
      └─ 未找到 → 继续

2. 检查 StorageBackend
   └─ storage.exists("lfs/objects/{oid}")
      ├─ 存在 → 返回 "raw_only"
      └─ 不存在 → 返回 404

3. 返回结果
   {
     "state": "xet_only" | "raw_only",
     "size": <bytes>,
     "sha256": "<oid>"
   }
```

### 相关数据库

| 数据库 | 路径 | 用途 |
|--------|------|------|
| MetadataIndex | `./data/metadata_index.db` | 跟踪文件→分片映射，支持 xet_only 查询 |
| Hub Metadata | `hub.db` | 仓库、版本、文件树元数据 |

**注意**：没有独立的"状态数据库"。

---

## 后续建议

### 1. 配置完整性分析更新

在阶段 2 的生产就绪配置中，**不需要**添加 state_db_path 相关配置。

### 2. 文档审查

建议全面审查文档，确保所有描述与代码实现一致：
- ✅ 已完成：CAS_STATE_DB_PATH 相关文档
- ⚠️ 待检查：其他配置项的文档准确性

### 3. 架构文档澄清

已在 architecture.md 中明确：
- "状态管理"实际是"元数据索引管理"
- 状态查询通过 MetadataIndex + StorageBackend 实现
- 没有独立的状态数据库

---

## 总结

**回滚完成** ✅

- 代码：已移除未使用的 state_db_path 配置
- 文档：已清理所有 CAS_STATE_DB_PATH 引用
- 架构：已澄清状态跟踪机制
- 编译：通过验证

**关键发现**：
文档中提到的"状态数据库"实际是指 MetadataIndex，而非独立的数据库文件。这是一个文档错误，已在本次回滚中修正。

---

**报告时间**：2026-06-12  
**回滚操作**：已完成  
**验证状态**：✅ 编译通过，文档已更新
