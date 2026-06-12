# Xet Server

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
- **两层认证** - Hub tokens (`hf_xxx`) + CAS tokens (`xet_xxx`)
- **作用域控制** - read、write、internal 三种权限级别
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
             │ (HTTP :8080)                 │ (HTTP :8080)
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
                              │                        │
                              │  + SQLite (元数据)      │
                              └────────────────────────┘
```

### 组件说明

**Hub API Server** (`hub-api`)
- 端口：8080（默认）
- 功能：提供 HuggingFace Hub 兼容的 REST API
- 职责：仓库管理、提交 API、令牌交换、LFS 代理
- 数据库：SQLite（元数据存储）

**CAS Server** (`xet-server`)
- 端口：8080（默认，生产环境建议设置为 8081 避免冲突）
- 功能：内容寻址存储引擎
- 职责：xorb/shard 存储、文件重构、去重、LFS 对象管理
- 数据库：SQLite（状态跟踪）

## 🚀 快速开始

### 环境要求

- **Rust** 1.85+ (Edition 2024)
- **SQLite** 3.35+
- **可选**：S3/MinIO 存储后端

### 编译安装

```bash
# 克隆仓库
git clone https://github.com/your-org/xet-server.git
cd xet-server

# 编译（release 模式）
cargo build --release

# 二进制文件位置
# CAS Server: target/release/xet-server
# Hub API:    target/release/hub-api
```

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
# 服务器设置（注意：默认端口 8080 会与 Hub 冲突，建议改为 8081）
export XET_HOST=0.0.0.0
export XET_PORT=8081
export XET_PUBLIC_BASE_URL=http://localhost:8081
export XET_MAX_BODY_SIZE_MB=2048

# 存储设置
export XET_STORAGE_BACKEND=local
export XET_LOCAL_PATH=/data/xet-storage

# 认证设置
export CAS_PUBLIC_KEY_PATH=/path/to/public_key.pem
export CAS_TRUSTED_KIDS=hub-key-1

# 状态数据库
export CAS_STATE_DB_PATH=/data/xet-state.db
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
export CAS_BASE_URL=http://localhost:8081

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
| `/v1/reconstructions/{file_id}` | GET | 获取文件重构信息 |
| `/v1/chunks/{prefix}/{hash}` | GET | 全局去重查询 |
| `/objects/batch` | POST | Git LFS 批量 API |
| `/health` | GET | 健康检查 |
| `/metrics` | GET | Prometheus 指标 |

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

详细文档：[Hub API Reference](docs/api/hub-api.md)

## ⚙️ 配置参考

### CAS Server 环境变量

| 变量名 | 描述 | 默认值 |
|--------|------|--------|
| `XET_HOST` | 服务器绑定地址 | `127.0.0.1` |
| `XET_PORT` | 服务器端口 | `8080` |
| `XET_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` |
| `XET_MAX_BODY_SIZE_MB` | 最大请求体大小（MB） | `2048` |
| `XET_STORAGE_BACKEND` | 存储后端类型 | `local` |
| `XET_LOCAL_PATH` | 本地存储路径 | `./data` |
| `XET_S3_BUCKET` | S3 存储桶名称 | - |
| `XET_S3_REGION` | S3 区域 | - |
| `XET_S3_ENDPOINT` | S3 端点 URL | - |
| `CAS_PUBLIC_KEY_PATH` | Ed25519 公钥路径 | `/tmp/xet-public-key.pem` |
| `CAS_TRUSTED_KIDS` | 受信任的密钥 ID 列表 | `test-kid` |
| `CAS_STATE_DB_PATH` | 状态数据库路径 | `/tmp/xet-state.db` |

### Hub API 环境变量

| 变量名 | 描述 | 默认值 |
|--------|------|--------|
| `HUB_HOST` | 服务器绑定地址 | `0.0.0.0` |
| `HUB_PORT` | 服务器端口 | `8080` |
| `HUB_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` |
| `HUB_PRIVATE_KEY_PATH` | Ed25519 私钥路径 | `private_key.pem` |
| `HUB_KID` | 密钥标识符 | `hub-key-1` |
| `HUB_TOKEN_TTL_SECONDS` | 令牌有效期（秒） | `3600` |
| `HUB_SQLITE_PATH` | 元数据数据库路径 | `hub.db` |
| `CAS_BASE_URL` | CAS 服务器 URL | `http://localhost:3000` |

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
- [Actix Web](https://actix.rs/) - 高性能 Web 框架
- [HuggingFace](https://huggingface.co/) - Hub API 设计参考
- [Git LFS](https://git-lfs.github.com/) - 大文件存储协议

## 📞 支持

- 📧 Email: support@example.com
- 💬 Issues: [GitHub Issues](https://github.com/your-org/xet-server/issues)
- 📚 Docs: [完整文档](docs/)
