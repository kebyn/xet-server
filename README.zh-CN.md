# Xet Server

[English](README.md) | 简体中文

Xet Server 是一个高性能的 **内容寻址存储（Content-Addressable Storage, CAS）** 服务器，专为大规模机器学习模型和数据集的管理而设计。它同时支持 **Git LFS 协议** 和 **HuggingFace Hub API**，提供跨协议的智能去重能力。

## ✨ 核心特性

### 存储引擎
- **内容寻址存储（CAS）** - 基于内容哈希的去重存储，自动消除重复数据
- **内容定义分块（CDC）** - 使用 GearHash 算法进行可变大小分块（8KB-128KB）
- **BLAKE3 哈希** - 高速加密哈希，支持 Merkle 树聚合验证
- **LZ4 压缩** - 快速压缩，平衡性能和存储效率
- **多存储后端** - 支持本地文件系统和 S3/MinIO 对象存储

### 协议支持
- **Git LFS 兼容** - 完整的 Git Large File Storage 协议支持
- **HuggingFace Hub API** - 兼容 HuggingFace Hub REST API，支持 `hf` CLI 工具
- **Xet 原生协议** - 高性能原生协议，支持 xorbs 和 shards
- **跨协议去重** - Git LFS 上传的文件可通过 HF API 去重下载

### 安全特性
- **Ed25519 认证** - 基于 Ed25519 的 JWT 非对称密钥签名
- **分层认证** - Hub tokens (`hf_xxx`) + CAS user tokens (`xet_xxx`) + LFS proxy tokens (`proxy_xxx`) + internal service tokens (`internal_xxx`)
- **作用域控制** - `read`、`write`、`internal`、`lfs-upload`、`lfs-download`；`internal` 仅用于 Hub -> CAS 内部端点，不包含 `read`/`write`
- **密钥轮换** - 支持 key ID (`kid`) 的多密钥管理

## 🏗️ 架构概览

Xet Server 采用**双进程架构**，由两个独立的服务组成：

```
┌─────────────────────────────────────────────────────────────┐
│                        客户端                                │
│  (git lfs, hf CLI, xet-tools, custom clients)              │
└────────────┬──────────────────────────────┬─────────────────┘
             │                              │
             │ Git LFS / HF Hub API         │ Xet 原生协议
             │ (HTTP :8080)                 │ (HTTP :8081)
             ▼                              ▼
┌────────────────────────┐      ┌────────────────────────┐
│     Hub API Server     │      │    CAS Server (xet)    │
│     (HuggingFace       │─────▶│   (Content Addressable │
│      Compatible)       │      │        Storage)        │
│                        │      │                        │
│  • Repository CRUD     │      │  • Xorb 存储           │
│  • Commit API          │      │  • Shard 存储          │
│  • Token Exchange      │      │  • 文件重构            │
│  • Tree Listing        │      │  • 全局去重            │
│  • File Resolve        │      │  • LFS 对象存储        │
│  • LFS Proxy           │      │  • 状态管理            │
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

### 组件说明

**Hub API Server** (`hub-api`)
- 端口：8080（默认）
- 功能：提供 HuggingFace Hub 兼容的 REST API
- 职责：仓库管理、提交 API、令牌交换、LFS 代理
- 数据库：SQLite（元数据存储，启动时运行内置 migration runner）

**CAS Server** (`xet-server`)
- 端口：8081（默认，避免与 Hub API 端口 8080 冲突）
- 功能：内容寻址存储引擎
- 职责：xorb/shard 存储、文件重构、去重、LFS 对象管理
- 无数据库（纯内存索引，启动时从存储后端重建）

## 🚀 快速开始

### 环境要求

- **Rust** 1.94.1+ (Edition 2024)
- **SQLite** 3.35+（仅 Hub API 需要）
- **可选**：S3/MinIO 存储后端

### 编译安装

```bash
# 克隆仓库
git clone https://github.com/kebyn/xet-server.git
cd xet-server

# 编译（release 模式；同时构建两个二进制）
cargo build --release --workspace --bins

# 二进制文件位置
# CAS Server: target/release/xet-server
# Hub API:    target/release/hub-api
```

Release tag 产物为可重现构建——字节级复现步骤见任一
[release 页面](https://github.com/kebyn/xet-server/releases)。

### 生成认证密钥

```bash
# 生成 Hub 用户令牌（用于 Hub API 认证）
./target/release/hub-api create-token \
  --username admin \
  --name "admin-token" \
  --scope "read write" \
  --db hub.db

# 生成 Ed25519 密钥对（用于 CAS 令牌签名）
openssl genpkey -algorithm Ed25519 -out private_key.pem
openssl pkey -in private_key.pem -pubout -out public_key.pem
```

### 配置环境变量

**CAS Server 配置**：
```bash
# 服务器设置（默认端口 8081，避免与 Hub API 端口 8080 冲突）
export XET_HOST=0.0.0.0
export XET_PORT=8081
export XET_PUBLIC_BASE_URL=http://localhost:8081
export XET_MAX_BODY_SIZE_MB=2048
export XET_INDEX_REBUILD_STRICT=false

# 存储设置
export XET_STORAGE_BACKEND=local
export XET_LOCAL_PATH=/data/xet-storage

# 认证设置
export CAS_PUBLIC_KEYS=hub-key-1=/path/to/public_key.pem
export CAS_TRUSTED_KIDS=hub-key-1
```

**Hub API 配置**：
```bash
# 服务器设置
export HUB_HOST=0.0.0.0
export HUB_PORT=8080
export HUB_PUBLIC_BASE_URL=http://localhost:8080

# 认证设置
export HUB_PRIVATE_KEY_PATH=/path/to/private_key.pem
export HUB_KID=hub-key-1
export HUB_TOKEN_TTL_SECONDS=3600

# CAS 客户端设置
export HUB_CAS_BASE_URL=http://localhost:8081

# 元数据数据库
export HUB_SQLITE_PATH=/data/hub-metadata.db
```

### 启动服务

```bash
# 终端 1：启动 CAS Server
./target/release/xet-server

# 终端 2：启动 Hub API
./target/release/hub-api
```

## 💡 使用示例

### 方式 1：Git LFS 工作流

使用标准 Git LFS 命令与 Xet Server 交互：

```bash
# 初始化仓库
mkdir my-model && cd my-model
git init
git lfs install

# 配置 LFS 指向 Xet Server
cat > .lfsconfig << EOF
[lfs]
    url = http://localhost:8081/lfs
EOF

# 添加大文件
echo "*.safetensors filter=lfs diff=lfs merge=lfs -text" > .gitattributes
cp /path/to/model.safetensors .

# 提交并推送
git add .
git commit -m "Add model"
git remote add origin http://localhost:8081/repo.git
git push origin master
```

### 方式 2：HuggingFace CLI 工作流

使用 `hf` CLI 工具与 Hub API 交互：

**注意**：`hf` 是 [huggingface-cli](https://huggingface.co/docs/huggingface_hub/main/en/guides/cli) 的简写形式。
也可以使用标准的 `huggingface-cli` 命令，两者功能相同。Xet Server 实现了与 HuggingFace Hub 兼容的 REST API，
因此支持标准的 HuggingFace 工具链。

```bash
# 设置环境变量
export HF_ENDPOINT=http://localhost:8080
export HF_TOKEN=hf_your_token_here

# 创建仓库
hf repo create my-model --type model

# 上传文件
hf upload my-model ./model.safetensors model.safetensors

# 下载文件
hf download my-org/my-model model.safetensors --local-dir ./downloaded
```

### 方式 3：混合工作流（跨协议去重）

结合 Git LFS 和 HF API，实现跨协议去重：

```bash
# 步骤 1：通过 Git LFS 上传大文件
git lfs track "*.bin"
git add model.bin
git commit -m "Add model"
git push origin master

# 步骤 2：通过 HF API 下载（自动去重）
export HF_ENDPOINT=http://localhost:8080
hf download my-org/my-repo model.bin --local-dir ./downloaded
# 文件从 CAS 直接返回，无需重复存储
```

## 📚 API 参考

### CAS Server API (端口 8081)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/v1/xorbs/{prefix}/{hash}` | POST/PUT | 上传 Xorb 对象 |
| `/v1/xorbs/{prefix}/{hash}/download` | GET | 下载 Xorb 对象 |
| `/lfs/objects/{oid}` | PUT | 上传 LFS 对象 |
| `/lfs/objects/{oid}` | GET | 下载 LFS 对象 |
| `/v1/shards` | POST | 上传 Shard 元数据 |
| `/v2/reconstructions/{file_id}` | GET | 获取文件重构信息（V2） |
| `/v1/chunks/{prefix}/{hash}` | GET | 全局去重查询 |
| `/objects/batch` | POST | Git LFS 批量 API |
| `/lfs/objects/batch` | POST | Git LFS 批量 API（LFS 路径） |
| `/health` | GET | 存活检查（liveness） |
| `/ready` | GET | 就绪检查（storage + MetadataIndex） |
| `/metrics` | GET | Prometheus 指标 |

CAS 对象访问是 content-capability based：持有有效 CAS token 的客户端按 token scope 访问内容能力，不强制 repository-scoped object isolation。LFS batch action 优先返回短期 `proxy_xxx` token；如果未配置 `CAS_PRIVATE_KEY_PATH`，会兼容回退为调用者的 `xet_xxx` token，生产环境应避免此模式。

详细文档：[CAS API Reference](docs/api/cas-api.md)

### Hub API (端口 8080)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/whoami-v2` | GET | 用户身份信息 |
| `/api/repos/create` | POST | 创建仓库 |
| `/api/models` | POST | 创建模型仓库 |
| `/api/datasets` | POST | 创建数据集仓库 |
| `/api/spaces` | POST | 创建 Space 仓库 |
| `/api/{type}/{ns}/{repo}/commit/{rev}` | POST | 提交文件（NDJSON） |
| `/api/{type}/{ns}/{repo}/tree/{rev}` | GET | 列出文件树 |
| `/{type}/{ns}/{repo}/resolve/{rev}/{path}` | GET | 下载文件 |
| `/api/{type}/{ns}/{repo}/xet-read-token/{rev}` | GET | 获取读令牌 |
| `/api/{type}/{ns}/{repo}/xet-write-token/{rev}` | GET | 获取写令牌 |
| `/health` | GET | 存活检查 |
| `/ready` | GET | 就绪检查（SQLite + CAS） |

详细文档：[Hub API Reference](docs/api/hub-api.md)

### 协议与运行保证

- Commit API 的 `header` 必须是第一个非空 NDJSON operation 且只能出现一次；后续 file/LFS/delete operation 保持请求顺序。同一路径以后出现的 operation 为准。
- Commit 写入端点只接受精确的 `main` revision。读取 tree/resolve 仍可使用 `main` 或已有 commit ID；其他分支名、commit ID 和大小写变体会由写入端点返回 `400 ValidationError`。
- 每个 commit 是完整文件树 snapshot：SQLite 在同一写事务中复制父 commit 未修改条目、按顺序应用本次 delta 并更新 HEAD，不在 Hub 内存中加载完整父树；非首个 commit 必须提交与当前 HEAD 一致的 `parentRevision`。
- Commit API 的 LFS `oid` 是不带 `sha256:` 前缀的 64 字符十六进制值。Hub 通过 CAS `HEAD /internal/blob/{oid}` 的必需 `X-Blob-Size` 校验声明大小；对象不存在或大小不一致返回 422。
- Shard 从本地文件或远端临时文件有界解析，不保留整份原始字节副本；启动重建流式枚举 key，最多 10 个 shard 一批。S3 列表固定 1000 项一页并校验 continuation token 进度。S3 multipart 以唯一 upload ID 跟踪并在错误、取消和 shutdown 时 best-effort abort，bucket lifecycle rule 仍是进程崩溃时的最终兜底。
- Hub inline resolve 在返回小文件前验证 snapshot 大小和 SHA-256 OID；损坏的 CAS 响应不会降级重定向。内部 SQL、路径、S3 配置和 CAS upstream body 只写服务端日志。Hub 对客户端返回稳定通用的 500/502 文案，CAS 的普通 500 JSON 为 `{"error":"Internal server error"}`。
- Hub tree listing 使用每页 1000 个 snapshot 文件条目的 SQLite keyset pagination，并通过标准 `Link rel="next"` 与 `huggingface_hub` 透明分页。

## ⚙️ 配置参考

### CAS Server 环境变量

| 变量名 | 描述 | 默认值 |
|--------|------|--------|
| `XET_HOST` | 服务器绑定地址 | `127.0.0.1` |
| `XET_PORT` | 服务器端口 | `8081` |
| `XET_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` |
| `XET_MAX_BODY_SIZE_MB` | 最大请求体大小（MB） | `2048` |
| `XET_INDEX_REBUILD_STRICT` | MetadataIndex 重建失败时是否启动失败 | `false` |
| `XET_STORAGE_BACKEND` | 存储后端类型 | `local` |
| `XET_LOCAL_PATH` | 本地存储路径（使用 local 后端时必需） | `./data` |
| `XET_S3_BUCKET` | S3 存储桶名称（使用 s3 后端时必需） | - |
| `XET_S3_REGION` | S3 区域 | - |
| `XET_S3_ENDPOINT` | S3 端点 URL | - |
| `XET_UPLOAD_TEMP_DIR` | 上传临时文件目录 | 自动 |
| `XET_RECONSTRUCTION_TEMP_DIR` | 文件重构及远端 xorb/shard/LFS 有界下载的临时目录 | 自动 |
| `XET_VERIFY_DOWNLOAD_INTEGRITY` | 启用下载完整性校验 | `false` |

> **⚠️ 重要：S3 Lifecycle Rules 配置**
>
> 使用 S3 存储后端时，**必须**配置 S3 Lifecycle Rules 来自动中止未完成的 multipart 上传，否则会产生持续的存储费用。
>
> **配置步骤：**
> 1. 在 AWS S3 控制台或使用 AWS CLI 编辑存储桶的 Lifecycle 规则
> 2. 添加规则：中止未完成的 multipart 上传
> 3. 建议设置：7 天后中止未完成的上传
>
> **AWS CLI 示例：**
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
> 进程会按唯一 upload ID 跟踪并在错误、请求取消或正常 shutdown 时 best-effort abort；如果进程崩溃、runtime 已退出或 abort 自身失败，仍可能遗留 multipart。因此 lifecycle rule 不可省略。

| 变量名 | 描述 | 默认值 |
|--------|------|--------|
| `CAS_PUBLIC_KEYS` | 有序 `kid=/path/to/public.pem` keyring | 空（使用单公钥兼容模式） |
| `CAS_PUBLIC_KEY_PATH` | 兼容模式的单 Ed25519 公钥路径 | `/etc/xet/public-key.pem` |
| `CAS_TRUSTED_KIDS` | key ID allowlist | keyring 全部 kid；兼容模式 `hub-key-1` |
| `CAS_PRIVATE_KEY_PATH` | Ed25519 私钥路径（生成 LFS proxy token） | 空（兼容模式；生产环境应配置） |
| `CAS_SIGNING_KID` | Proxy token 签名使用的 Key ID | 空（keyring 中首个受信任映射） |

`CAS_PUBLIC_KEYS=kid1=/path/old-public.pem,kid2=/path/new-public.pem` 建立真正的 `kid` 到公钥映射；`CAS_TRUSTED_KIDS` 只是 allowlist。未设置 keyring 时才使用旧 `CAS_PUBLIC_KEY_PATH`，并将同一公钥映射给所有 trusted kids。配置 `CAS_PRIVATE_KEY_PATH` 后，私钥必须与 `CAS_SIGNING_KID` 对应公钥匹配，否则 CAS 启动失败。

`CAS_PRIVATE_KEY_PATH` 未配置时，CAS Batch API 会把调用者的 `xet_xxx` token 放入 LFS action header，而不是签发短期、单 OID、单 operation 的 `proxy_xxx` token。这会扩大 action token 泄露后的影响范围；生产环境应配置该私钥。

`/health` 只表示 HTTP server 存活；`/ready` 用于负载均衡和编排系统的 readiness probe。CAS `/ready` 会检查存储后端和 MetadataIndex 重建状态，Hub `/ready` 会检查 SQLite 和 CAS `/ready`。显式设置的数值或布尔环境变量如果解析失败，服务会启动失败，不会静默回退默认值；布尔值只接受 `true`/`false`/`1`/`0`，生效的 URL 只接受带 host 的 HTTP(S)，上传/下载上限、TTL、rate、pool 等零值与不一致的跨字段限制会被拒绝。

### Hub API 环境变量

| 变量名 | 描述 | 默认值 |
|--------|------|--------|
| `HUB_HOST` | 服务器绑定地址 | `0.0.0.0` |
| `HUB_PORT` | 服务器端口 | `8080` |
| `HUB_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` |
| `HUB_PRIVATE_KEY_PATH` | Ed25519 私钥路径 | `private_key.pem` |
| `HUB_KID` | 密钥标识符 | `hub-key-1` |
| `HUB_TOKEN_TTL_SECONDS` | 令牌有效期（秒，范围 `1..=604800`） | `3600` |
| `HUB_PROXY_TOKEN_TTL_SECONDS` | Proxy Token 有效期（秒，范围 `1..=604800`） | `300` (5分钟) |
| `HUB_INTERNAL_TOKEN_TTL_SECONDS` | 内部令牌有效期（秒，用于 Hub→CAS internal endpoints，范围 `1..=604800`） | `86400` (24小时) |
| `HUB_SQLITE_PATH` | 元数据数据库路径 | `hub.db` |
| `HUB_DB_POOL_SIZE` | SQLite 连接池大小 | `5` |
| `HUB_CAS_BASE_URL` | CAS 服务器 URL | `http://localhost:8081` |
| `HUB_CAS_TIMEOUT_SECS` | CAS 请求超时（秒） | `30` |
| `HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS` | 启动时CAS健康检查超时（秒） | `10` |
| `HUB_INLINE_THRESHOLD` | 内联文件阈值（字节） | `1048576` (1MB) |
| `HUB_UPLOAD_TEMP_DIR` | 上传临时文件目录 | `./data/hub-uploads` |
| `HUB_MAX_UPLOAD_SIZE` | 最大上传文件大小（字节） | `536870912` (512MB) |
| `HUB_MAX_DOWNLOAD_SIZE` | CAS 下载大小限制（字节） | `536870912` (512MB) |

**安全相关**：
| 变量名 | 描述 | 默认值 |
|--------|------|--------|
| `HUB_TOKEN_HASH_SALT` | Token 哈希盐（多实例部署必须一致） | 自动生成 |

Hub API 当前只提供 SQLite 元数据 backend。启动时会自动初始化或校验 schema，并为 SQLite 连接启用 WAL、外键和 busy timeout。多 Hub 实例部署必须共享同一个 SQLite 文件、使用相同 `HUB_TOKEN_HASH_SALT` 和 Hub signing key，并接受 SQLite 单写者限制；项目当前没有内置 Postgres/MySQL 等分布式数据库 backend。

详细文档：[Configuration Guide](docs/configuration.md)

## 🧪 测试

```bash
# 运行所有测试
cargo test

# 运行集成测试
cargo test --test '*'

# 运行基准测试
cargo bench

# 运行特定测试
cargo test test_name
```

测试覆盖：
- 单元测试：哈希、分块、格式、存储
- 集成测试：API 端点、认证、工作流
- 端到端测试：完整上传/下载流程

## 📖 文档

- [API 文档](docs/api/) - CAS 和 Hub API 详细参考
- [配置指南](docs/configuration.md) - 完整配置选项说明
- [架构说明](docs/architecture.md) - 系统架构和数据流
- [集成指南](HF_XET_INTEGRATION_GUIDE.md) - HuggingFace 集成工作流

## 🤝 贡献

欢迎贡献！请参阅以下步骤：

1. Fork 本仓库
2. 创建特性分支 (`git checkout -b feature/amazing-feature`)
3. 提交更改 (`git commit -m 'Add amazing feature'`)
4. 推送到分支 (`git push origin feature/amazing-feature`)
5. 开启 Pull Request

### 开发指南

```bash
# 开发模式运行
cargo run --bin xet-server
cargo run --bin hub-api

# 代码检查
cargo clippy

# 格式化
cargo fmt
```

## 📄 许可证

本项目采用 MIT 许可证 - 详见 [LICENSE](LICENSE) 文件

## 🙏 致谢

- [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) - 高速加密哈希
- [Actix Web](https://github.com/actix/actix-web) - 高性能 Web 框架
- [HuggingFace Xet Core](https://github.com/huggingface/xet-core) - Hub API 设计参考
- [Git LFS](https://git-lfs.github.com/) - 大文件存储协议

## 📞 支持

- 💬 Issues: [GitHub Issues](https://github.com/kebyn/xet-server/issues)
- 📚 Docs: [完整文档](docs/)
