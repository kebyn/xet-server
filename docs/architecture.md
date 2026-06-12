# 系统架构文档

本文档详细说明 Xet Server 的系统架构，包括组件设计、数据流、存储格式和安全模型。

## 概述

Xet Server 是一个高性能的**内容寻址存储（CAS）**系统，专为大规模机器学习模型和数据集的管理而设计。它采用**双进程架构**，同时支持 **Git LFS** 和 **HuggingFace Hub API** 两种协议。

### 设计原则

1. **内容寻址**：所有数据通过内容哈希寻址，自动实现去重
2. **协议兼容**：同时支持 Git LFS 和 HuggingFace Hub API
3. **跨协议去重**：通过不同协议上传的文件可以相互去重
4. **高性能**：优化的存储格式和并发处理
5. **可扩展**：支持本地存储和 S3 对象存储

---

## 系统架构

### 高层架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                           客户端                                 │
│         (git lfs, hf CLI, xet-tools, custom clients)           │
└──────────────┬──────────────────────────────────┬────────────────┘
               │                                  │
               │ Git LFS / HF Hub API             │ Xet 原生协议
               │ (HTTP :8080)                     │ (HTTP :8081)
               ▼                                  ▼
┌──────────────────────────┐        ┌──────────────────────────┐
│    Hub API Server        │        │    CAS Server            │
│    (HuggingFace 兼容)     │        │    (Content Addressable  │
│                          │        │        Storage)          │
│  • Repository CRUD       │        │                          │
│  • Commit API (NDJSON)   │        │  • Xorb 存储             │
│  • Token Exchange        │        │  • Shard 存储            │
│  • Tree Listing          │        │  • 文件重构              │
│  • File Resolve          │        │  • 全局去重              │
│  • LFS Proxy             │        │  • LFS 对象存储          │
│                          │        │  • 状态管理              │
│  端口: 8080              │        │  端口: 8081              │
└────────────┬─────────────┘        └────────────┬─────────────┘
             │                                   │
             │ Internal API                      │
             │ (HTTP, 需要 internal scope)       │
             └─────────────┬─────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │    Storage Backend     │
              │                        │
              │  • Local Filesystem    │
              │  • S3 / MinIO          │
              │                        │
              │  + SQLite (元数据)      │
              └────────────────────────┘
```

### 组件交互

**Hub API Server**：
- 接收用户请求（HF CLI、REST API）
- 管理仓库、版本、文件树元数据
- 签发 CAS 令牌（xet_xxx）
- 代理 LFS 请求到 CAS

**CAS Server**：
- 核心存储引擎
- 管理 xorbs、shards、LFS 对象
- 提供文件重构信息
- 实现全局去重

**Storage Backend**：
- 实际数据存储（本地或 S3）
- SQLite 数据库（元数据）

---

## 核心组件

### 1. Hub API Server

**职责**：
- 提供 HuggingFace Hub 兼容的 REST API
- 管理仓库、版本、文件树
- 用户认证和令牌签发
- 代理 LFS 操作到 CAS

**关键模块**：

```
hub/src/
├── api/              # API 端点处理器
│   ├── commit.rs     # Commit API (NDJSON)
│   ├── repo.rs       # 仓库 CRUD
│   ├── tree.rs       # 文件树列出
│   ├── resolve.rs    # 文件下载
│   ├── token_exchange.rs  # 令牌交换
│   └── lfs_proxy.rs  # LFS 代理
├── auth/             # 认证
│   ├── xet_signer.rs # JWT 签名
│   ├── token_store.rs # 令牌存储
│   └── extract.rs    # 令牌提取
├── metadata/         # 元数据管理
│   └── sqlite.rs     # SQLite 元数据存储
└── cas_client/       # CAS 客户端
    └── mod.rs        # 与 CAS 通信
```

**数据流**：

1. **仓库创建**：
   ```
   客户端 → Hub API → 元数据数据库
   ```

2. **文件提交**：
   ```
   客户端 → Hub API (NDJSON) → CAS Server → 存储后端
                    ↓
              元数据数据库
   ```

3. **令牌交换**：
   ```
   客户端 → Hub API → 签发 CAS 令牌 → 客户端
   ```

### 2. CAS Server

**职责**：
- 内容寻址存储引擎
- Xorb/Shard 存储和管理
- 文件重构（从 chunks 重建文件）
- 全局去重查询
- Git LFS 对象存储

**关键模块**：

```
src/
├── api/              # API 端点处理器
│   ├── xorb.rs       # Xorb 上传/下载
│   ├── shard.rs      # Shard 上传
│   ├── lfs.rs        # LFS 对象操作
│   ├── reconstruction.rs  # 文件重构
│   ├── global_dedup.rs    # 全局去重
│   ├── batch.rs      # Git LFS 批量 API
│   └── internal.rs   # 内部 API（Hub 使用）
├── hash/             # 哈希算法
│   ├── blake3_hash.rs # BLAKE3 哈希
│   └── merkle_tree.rs # Merkle 树
├── chunking/         # 分块
│   └── cdc.rs        # 内容定义分块
├── format/           # 文件格式
│   ├── xorb.rs       # Xorb 格式
│   ├── shard.rs      # Shard 格式
│   └── compression.rs # 压缩
├── storage/          # 存储后端
│   ├── local.rs      # 本地文件系统
│   └── s3.rs         # S3 存储
└── state/            # 状态管理
    └── sqlite.rs     # SQLite 状态跟踪
```

**数据流**：

1. **Xorb 上传**：
   ```
   客户端 → CAS API → 验证哈希 → 存储后端 → 状态数据库
   ```

2. **文件重构**：
   ```
   客户端 → CAS API → 状态数据库 → Shard 查询 → 返回重构信息
   ```

3. **全局去重**：
   ```
   客户端 → CAS API → 查询 chunk 存在 → 返回结果
   ```

### 3. 存储后端

**本地存储** (`storage/local.rs`)：
- 使用文件系统存储 xorbs 和 shards
- 支持原子重命名（同一文件系统）
- 优化的目录结构（前缀分片）

**目录结构**：
```
{XET_LOCAL_PATH}/
├── xorbs/
│   ├── ab/
│   │   ├── abc123...def.xorb
│   │   └── ...
│   ├── cd/
│   │   └── ...
│   └── ...
├── shards/
│   └── ...
└── lfs/
    └── objects/
        ├── abc123...
        └── ...
```

**S3 存储** (`storage/s3.rs`)：
- 使用 S3/MinIO 对象存储
- 支持 multipart 上传（大文件）
- 优化的传输策略

**配置**：
```bash
export XET_STORAGE_BACKEND=s3
export XET_S3_BUCKET=my-xet-bucket
export XET_S3_REGION=us-east-1
export XET_S3_ENDPOINT=https://s3.amazonaws.com
```

### 4. 认证系统

**Ed25519 JWT**：
- 使用 EdDSA 签名的 JWT 令牌
- 两层认证：Hub tokens (hf_xxx) + CAS tokens (xet_xxx)
- 支持密钥轮换（kid）

**令牌类型**：

1. **Hub Tokens** (`hf_xxx`)：
   - 长期有效（可配置 TTL）
   - 用于用户身份认证
   - 管理仓库、提交文件

2. **CAS Tokens** (`xet_xxx`)：
   - 短期有效（5 分钟）
   - 绑定到特定仓库和修订版本
   - 由 Hub 签发，CAS 验证

3. **Proxy Tokens** (特殊 CAS token)：
   - 超短期（5 分钟）
   - 绑定到特定 LFS 对象和操作
   - 用于 LFS 代理

**认证流程**：
```
1. 客户端 → Hub: 请求 CAS 令牌
2. Hub: 验证用户令牌，签发 CAS 令牌
3. 客户端 → CAS: 使用 CAS 令牌访问
4. CAS: 验证令牌签名（使用 Hub 公钥）
```

---

## 数据流

### 上传流程

#### Git LFS 上传

```
┌──────────┐
│ Git + LFS│
└────┬─────┘
     │
     │ 1. POST /objects/batch (批量请求)
     ▼
┌──────────────────┐
│ CAS Server       │
│ (Batch API)      │
└────┬─────────────┘
     │
     │ 2. 返回 action URLs + CAS 令牌
     ▼
┌──────────┐
│ Git + LFS│
└────┬─────┘
     │
     │ 3. PUT /lfs/objects/{oid} (上传原始文件)
     ▼
┌──────────────────┐
│ CAS Server       │
│ (LFS Upload)     │
│                  │
│ • 计算 SHA-256   │
│ • 验证哈希       │
│ • 存储到后端     │
│ • 更新状态       │
└────┬─────────────┘
     │
     │ 4. 状态: RawOnly
     ▼
┌──────────────────┐
│ Storage Backend  │
│ (原始文件)       │
└──────────────────┘
```

#### HuggingFace Hub API 上传

```
┌──────────┐
│ HF CLI   │
└────┬─────┘
     │
     │ 1. POST /api/repos/create (创建仓库)
     ▼
┌──────────────────┐
│ Hub API          │
│                  │
│ • 验证用户令牌   │
│ • 创建元数据     │
└──────────────────┘

┌──────────┐
│ HF CLI   │
└────┬─────┘
     │
     │ 2. POST /api/models/{ns}/{repo}/commit/{rev} (NDJSON)
     │    {"key": "header"}
     │    {"key": "file", "path": "model.bin", "size": ...}
     │    <base64-content>
     │    {"key": "status"}
     ▼
┌──────────────────┐
│ Hub API          │
│                  │
│ • 解析 NDJSON   │
│ • 提取文件       │
│ • 分类（inline/LFS/Xet）
└────┬─────────────┘
     │
     │ 3a. 小文件 (≤1MB): 内联存储
     │ 3b. 中文件 (1-10MB): LFS 路径
     │ 3c. 大文件 (>10MB): Xet 路径
     ▼
┌──────────────────┐
│ CAS Server       │
│                  │
│ • 存储文件       │
│ • 更新元数据     │
└────┬─────────────┘
     │
     │ 4. 返回 commit 信息
     ▼
┌──────────┐
│ HF CLI   │
└──────────┘
```

### 下载流程

#### Git LFS 下载

```
┌──────────┐
│ Git + LFS│
└────┬─────┘
     │
     │ 1. POST /objects/batch (批量请求)
     ▼
┌──────────────────┐
│ CAS Server       │
│ (Batch API)      │
│                  │
│ • 查询对象状态   │
│ • 返回 action URLs
└────┬─────────────┘
     │
     │ 2. 返回 action URLs + CAS 令牌
     ▼
┌──────────┐
│ Git + LFS│
└────┬─────┘
     │
     │ 3. GET /lfs/objects/{oid} (下载原始文件)
     ▼
┌──────────────────┐
│ CAS Server       │
│ (LFS Download)   │
│                  │
│ • 从存储读取     │
│ • 返回文件数据   │
└──────────────────┘
```

#### HuggingFace Hub API 下载

```
┌──────────┐
│ HF CLI   │
└────┬─────┘
     │
     │ 1. GET /api/models/{ns}/{repo}/tree/{rev} (列出文件)
     ▼
┌──────────────────┐
│ Hub API          │
│                  │
│ • 查询元数据     │
│ • 返回文件列表   │
└──────────────────┘

┌──────────┐
│ HF CLI   │
└────┬─────┘
     │
     │ 2. GET /api/models/{ns}/{repo}/xet-read-token/{rev} (获取令牌)
     ▼
┌──────────────────┐
│ Hub API          │
│                  │
│ • 验证用户令牌   │
│ • 签发 CAS 令牌  │
└──────────────────┘

┌──────────┐
│ HF CLI   │
└────┬─────┘
     │
     │ 3. GET /{type}/{ns}/{repo}/resolve/{rev}/{path} (下载文件)
     ▼
┌──────────────────┐
│ Hub API          │
│                  │
│ • 查询文件位置   │
│ • 代理到 CAS     │
└────┬─────────────┘
     │
     │ 4. CAS Server 返回文件数据
     │    • 如果是 Xet 格式：重构文件
     │    • 如果是 LFS 格式：直接返回
     │    • 如果是 inline：从元数据返回
     ▼
┌──────────┐
│ HF CLI   │
└──────────┘
```

### 跨协议去重

**场景**：文件通过 Git LFS 上传，然后通过 HF API 下载

```
1. 上传（Git LFS）:
   客户端 → CAS → 存储为 RawOnly

2. 下载（HF API）:
   客户端 → Hub → CAS
   CAS 检查状态: RawOnly
   CAS 直接返回原始文件
   （无需转换，自动去重）
```

**优势**：
- 无需重复存储
- 无需格式转换
- 透明的跨协议访问

---

## 存储格式

### 1. Xorb 格式

Xorb (Xet Object) 是内容寻址存储的核心格式，包含分块后的文件数据和元数据。

**结构**：
```
┌─────────────────────────────────────┐
│         Chunk 1 Data                │
│  (可变大小: 8KB - 128KB)            │
├─────────────────────────────────────┤
│         Chunk 2 Data                │
├─────────────────────────────────────┤
│         ...                         │
├─────────────────────────────────────┤
│         Chunk N Data                │
├─────────────────────────────────────┤
│         Footer                      │
│                                     │
│  • Magic: "XORB"                    │
│  • Version: u32                     │
│  • Chunk count: u32                 │
│  • Chunk hashes: [BLAKE3; N]        │
│  • Chunk boundaries: [u64; N]       │
│  • Compression info                 │
│  • Footer hash (BLAKE3)             │
└─────────────────────────────────────┘
```

**特点**：
- 内容寻址：整个 Xorb 通过 BLAKE3 哈希标识
- 分块存储：文件被分成可变大小的 chunks
- 压缩支持：每个 chunk 可独立压缩（LZ4）
- 完整性验证：Footer 包含所有 chunk 的哈希

**使用场景**：
- 大文件（> 10MB）
- 需要去重的文件
- 需要高效重构的文件

### 2. Shard 格式

Shard 是 Merkle DB 分片文件，包含文件到 chunk/xorb 的映射元数据。

**结构**：
```
┌─────────────────────────────────────┐
│         Header                      │
│                                     │
│  • Magic: "MDBS"                    │
│  • Version: u32                     │
│  • Flags: u32                       │
├─────────────────────────────────────┤
│         File Entries                │
│                                     │
│  • File hash (BLAKE3)               │
│  • File size: u64                   │
│  • Chunk count: u32                 │
│  • Chunk list:                      │
│    - Chunk hash (BLAKE3)            │
│    - Xorb hash (BLAKE3)             │
│    - Chunk offset in xorb: u64      │
│    - Chunk length: u64              │
├─────────────────────────────────────┤
│         Xorb Entries                │
│                                     │
│  • Xorb hash (BLAKE3)               │
│  • Xorb size: u64                   │
│  • Chunk count: u32                 │
│  • Chunk hashes: [BLAKE3; N]        │
├─────────────────────────────────────┤
│         Footer                      │
│                                     │
│  • File index offset: u64           │
│  • Xorb index offset: u64           │
│  • Footer hash (BLAKE3)             │
└─────────────────────────────────────┘
```

**特点**：
- 元数据索引：快速查找文件对应的 chunks 和 xorbs
- 压缩索引：使用紧凑的二进制格式
- 支持多个文件：一个 shard 可以包含多个文件的映射

**使用场景**：
- 文件重构：从 chunks/xorbs 重建文件
- 去重查询：检查 chunk 是否已存在
- 元数据管理：跟踪文件结构

### 3. LFS 对象格式

LFS 对象是原始文件的直接存储，使用 SHA-256 哈希标识。

**结构**：
```
┌─────────────────────────────────────┐
│         Raw File Data               │
│                                     │
│  (未修改的原始文件内容)              │
└─────────────────────────────────────┘
```

**命名**：
- 文件名：SHA-256 哈希（64 个十六进制字符）
- 路径：`{XET_LOCAL_PATH}/lfs/objects/{oid}`

**特点**：
- 简单直接：存储原始文件字节
- 哈希验证：使用 SHA-256（Git LFS 标准）
- 无需转换：直接存储和检索

**使用场景**：
- Git LFS 上传的文件
- 中等大小文件（1-10MB）
- 需要与 Git LFS 完全兼容的场景

### 4. 压缩方案

**支持的压缩算法**：

| 方案 | 描述 | 压缩率 | 速度 | 使用场景 |
|------|------|--------|------|----------|
| `None` | 无压缩 | 1x | 最快 | 小文件、已压缩数据 |
| `LZ4` | LZ4 压缩 | ~2x | 快 | 默认方案 |
| `ByteGrouping4LZ4` | 字节分组 + LZ4 | ~2.5x | 中等 | 特定数据类型 |

**LZ4 压缩**：
- 压缩速度：~500 MB/s
- 解压速度：~1500 MB/s
- 压缩率：约 2x（取决于数据类型）
- 适用：通用场景，平衡速度和压缩率

**ByteGrouping4LZ4**：
- 将数据分成 4 字节组
- 对每组独立压缩
- 适用：结构化数据（如浮点数数组）

---

## 安全模型

### 1. 认证

**Ed25519 JWT**：
- 使用 EdDSA 签名的 JWT 令牌
- 非对称密钥：Hub 持有私钥，CAS 持有公钥
- 防篡改：签名验证确保令牌未被修改

**两层认证**：
1. **Hub 层**：用户令牌 (hf_xxx)
   - 长期有效
   - 用于身份认证
   - 管理仓库和文件

2. **CAS 层**：CAS 令牌 (xet_xxx)
   - 短期有效（5 分钟）
   - 绑定到特定仓库和修订版本
   - 由 Hub 签发，CAS 验证

### 2. 授权

**作用域（Scopes）**：

| 作用域 | 权限 | 使用场景 |
|--------|------|----------|
| `read` | 读取 | 下载文件、列出仓库 |
| `write` | 写入 | 上传文件、创建仓库 |
| `internal` | 内部 | Hub → CAS 通信（超级权限） |

**权限检查**：
- 每个 API 端点检查所需作用域
- `internal` 自动包含 `read` 和 `write`
- 令牌绑定到特定 `repo_id` 和 `repo_type`

### 3. 数据安全

**哈希验证**：
- 上传时验证内容哈希
- 防止数据损坏
- 确保内容寻址的正确性

**传输安全**：
- 生产环境使用 HTTPS/TLS
- 防止中间人攻击
- 保护令牌和数据

**存储安全**：
- 文件权限控制（`chmod 600`）
- 私钥安全存储
- 定期密钥轮换

### 4. 网络安全

**防火墙规则**：
- Hub API (8080): 公开访问
- CAS Server (8081): 限制访问（仅 Hub 和授权客户端）
- Internal API: 仅 Hub 可访问

**反向代理**：
- 使用 Nginx/Caddy 处理 TLS
- 负载均衡
- 速率限制

---

## 可扩展性考虑

### 1. 水平扩展

**Hub API**：
- 有状态设计（元数据存储在 SQLite）
- 可以通过共享 SQLite 数据库或使用分布式数据库运行多个实例
- 使用负载均衡器分发请求（需要会话粘性或共享数据库）

**CAS Server**：
- 存储后端可共享（S3）
- 状态数据库需要同步
- 可以使用分布式数据库（未来）

### 2. 存储扩展

**本地存储**：
- 使用 RAID 提高容量和性能
- 定期备份
- 考虑使用分布式文件系统（Ceph, GlusterFS）

**S3 存储**：
- 无限容量
- 自动复制
- 跨区域访问

### 3. 性能优化

**缓存**：
- 客户端缓存 CAS 令牌
- 客户端缓存已下载的 xorbs
- CDN 缓存热门文件

**并发**：
- 异步 I/O（Tokio）
- 并行上传/下载
- 批量操作

**压缩**：
- LZ4 快速压缩
- 减少存储占用
- 减少网络传输

### 4. 未来改进

**分布式数据库**：
- 替换 SQLite
- 支持多写入节点
- 提高并发性能

**CDN 集成**：
- 缓存热门文件
- 减少源站负载
- 提高下载速度

**智能分层**：
- 热数据：SSD
- 温数据：HDD
- 冷数据：归档存储

---

## 相关文档

- [Configuration Guide](configuration.md) - 配置选项详细说明
- [CAS API Reference](api/cas-api.md) - CAS 服务器 API 文档
- [Hub API Reference](api/hub-api.md) - Hub API 文档
- [Authentication](api/authentication.md) - 认证机制详细说明
