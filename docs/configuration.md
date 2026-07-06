# 配置指南

本文档详细说明 Xet Server 的所有配置选项，包括 CAS Server 和 Hub API 的环境变量、默认值和最佳实践。

## 概述

Xet Server 由两个独立的服务组成，每个服务都有自己的配置：

- **CAS Server** (`xet-server`): 核心存储引擎
- **Hub API** (`hub-api`): HuggingFace 兼容 API 层

所有配置通过**环境变量**进行管理。显式设置的数值或布尔环境变量必须能被正确解析；如果设置了非法值，服务会在启动时返回配置错误并退出，不会静默回退到默认值。未设置环境变量时才使用默认值。

---

## CAS Server 配置

CAS Server 是 Xet Server 的核心存储引擎，负责内容寻址存储、文件重构和去重。

### 服务器设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `XET_HOST` | 服务器绑定地址 | `127.0.0.1` | 否 |
| `XET_PORT` | 服务器端口 | `8081` | 否 |
| `XET_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` | 否* |
| `XET_MAX_BODY_SIZE_MB` | 流式上传的最大文件大小（MB） | `2048` | 否 |
| `XET_RATE_LIMIT_RPM` | 公共端点速率限制（令牌桶算法，60秒窗口，突发容忍） | `60` | 否 |
| `XET_INDEX_REBUILD_STRICT` | 启动时 MetadataIndex 重建失败是否直接退出 | `false` | 否 |

**注意**：
- CAS Server 默认端口为 `8081`，以避免与 Hub API 默认端口 `8080` 冲突
- `XET_PUBLIC_BASE_URL` 在服务器位于反向代理、负载均衡器或 NAT 后时**必须设置**
- `XET_MAX_BODY_SIZE_MB` 控制流式上传的最大文件大小。非上传路由（JSON 请求等）的 HTTP body 限制为 10MB（硬编码）
- `XET_INDEX_REBUILD_STRICT=false` 时，CAS 可在索引重建失败后继续启动，但 `/ready` 会返回 `503`，避免流量进入未就绪实例；设置为 `true` 时重建失败会让进程启动失败

**示例**：
```bash
# 开发环境
export XET_HOST=127.0.0.1
export XET_PORT=8081

# 生产环境（反向代理后）
export XET_HOST=0.0.0.0
export XET_PORT=8081
export XET_PUBLIC_BASE_URL=https://cas.example.com
export XET_MAX_BODY_SIZE_MB=4096
export XET_INDEX_REBUILD_STRICT=true
```

### 存储设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `XET_STORAGE_BACKEND` | 存储后端类型 | `local` | 否 |
| `XET_LOCAL_PATH` | 本地存储路径 | `./data` | 是* |
| `XET_S3_BUCKET` | S3 存储桶名称 | - | 是** |
| `XET_S3_REGION` | S3 区域 | - | 否 |
| `XET_S3_ENDPOINT` | S3 端点 URL | - | 否 |
| `XET_UPLOAD_TEMP_DIR` | 流式上传临时文件目录 | 自动 | 否 |
| `XET_RECONSTRUCTION_TEMP_DIR` | 文件重构时 xorb 下载的临时目录 | `{OS_temp}/xet-reconstruction` | 否 |

**说明**：
- `XET_LOCAL_PATH` 在 `XET_STORAGE_BACKEND=local` 时必需
- `XET_S3_BUCKET` 在 `XET_STORAGE_BACKEND=s3` 时必需
- `XET_UPLOAD_TEMP_DIR` 默认值：
  - 本地存储：`{XET_LOCAL_PATH}/.tmp`（同一文件系统，支持原子重命名）
  - S3 存储：`/var/tmp/xet-uploads`（不被系统重启清理）
- `XET_RECONSTRUCTION_TEMP_DIR`：流式重构场景使用，建议使用 SSD

**示例**：
```bash
# 本地存储
export XET_STORAGE_BACKEND=local
export XET_LOCAL_PATH=/data/xet-storage
export XET_UPLOAD_TEMP_DIR=/fast-ssd/xet-uploads

# S3 存储
export XET_STORAGE_BACKEND=s3
export XET_S3_BUCKET=my-xet-bucket
export XET_S3_REGION=us-east-1
export XET_S3_ENDPOINT=https://s3.amazonaws.com
```

### 认证设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `CAS_PUBLIC_KEY_PATH` | Ed25519 公钥路径 | `/etc/xet/public-key.pem` | 是 |
| `CAS_TRUSTED_KIDS` | 受信任的密钥 ID 列表 | `hub-key-1` | 是 |
| `CAS_PRIVATE_KEY_PATH` | Ed25519 私钥路径，用于 Batch API 签发 proxy token | 空（兼容模式） | 否* |
| `CAS_SIGNING_KID` | Proxy token 签名使用的 Key ID | 空（默认用 `CAS_TRUSTED_KIDS` 第一个） | 否 |

**说明**：
- `CAS_PUBLIC_KEY_PATH` 指向 Hub 的公钥文件（PEM 格式）
- `CAS_TRUSTED_KIDS` 是逗号分隔的密钥 ID 列表，用于密钥轮换
- 默认 trusted kid 为 `hub-key-1`，应与 Hub 的 `HUB_KID` 配置保持一致
- **`CAS_PRIVATE_KEY_PATH`**：生产环境安全基线建议配置。设置后，Batch API 会签发短期 proxy token（5分钟有效期），绑定单个 OID 和 operation。未配置时，Batch API 会兼容回退到直接传递调用者的 `xet_xxx` token；该 token 的权限和 TTL 大于单个 LFS action，可能出现在客户端缓存、代理或日志中，泄露后的影响范围更大。

**示例**：
```bash
export CAS_PUBLIC_KEY_PATH=/etc/xet/hub-public-key.pem
export CAS_TRUSTED_KIDS=hub-key-1,hub-key-2
```

### 转换管道设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `XET_CONVERSION_ENABLED` | 启用自动转换 LFS blob 为 xorb/shard 格式 | `true` | 否 |
| `XET_CONVERSION_SCHEME` | 压缩方案（none/lz4/bg4lz4） | `lz4` | 否 |
| `XET_DELETE_RAW_AFTER_CONVERSION` | 转换成功后删除原始 blob（节省存储空间） | `true` | 否 |
| `XET_MIN_CONVERSION_SIZE` | 最小转换文件大小（字节），小于此值的文件保持原始格式 | `65536` (64KB) | 否 |
| `XET_MAX_CONVERSION_SIZE` | 最大转换文件大小（字节），大于此值的文件保持原始格式以防止 OOM | `536870912` (512MB) | 否 |

**说明**：
- 转换管道自动将上传的 LFS blob 转换为 xorb+shard 格式，实现全局 chunk 级去重
- `XET_CONVERSION_SCHEME` 支持三种压缩方案：
  - `none`: 不压缩
  - `lz4`: LZ4 压缩（推荐，平衡速度和压缩率）
  - `bg4lz4`: ByteGrouping4LZ4 压缩（更高压缩率，但速度较慢）
- 转换过程会加载整个文件到内存进行 CDC 分块，因此 `XET_MAX_CONVERSION_SIZE` 用于防止大文件导致 OOM
- 建议生产环境保持 `XET_DELETE_RAW_AFTER_CONVERSION=true` 以节省 50% 存储空间

**示例**：
```bash
# 启用转换管道（默认）
export XET_CONVERSION_ENABLED=true
export XET_CONVERSION_SCHEME=lz4
export XET_DELETE_RAW_AFTER_CONVERSION=true

# 调整转换大小限制
export XET_MIN_CONVERSION_SIZE=131072       # 128KB
export XET_MAX_CONVERSION_SIZE=1073741824  # 1GB
```

### 完整性验证设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `XET_VERIFY_DOWNLOAD_INTEGRITY` | 启用 LFS 下载时的 SHA-256 完整性校验 | `false` | 否 |

**说明**：
- 启用后，服务器在发送 LFS 文件前会计算 SHA-256 哈希并验证与 OID 匹配
- 可以检测存储损坏（bit rot），但会增加 CPU 开销
- 对于可信存储后端（本地文件系统、私有 S3），可以禁用以获得最佳性能
- 对于不可信存储或高安全性要求场景，建议启用

**示例**：
```bash
# 启用下载完整性验证
export XET_VERIFY_DOWNLOAD_INTEGRITY=true
```

---

## Hub API 配置

Hub API 提供 HuggingFace Hub 兼容的 REST API，负责仓库管理、提交和令牌交换。

### 服务器设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `HUB_HOST` | 服务器绑定地址 | `0.0.0.0` | 否 |
| `HUB_PORT` | 服务器端口 | `8080` | 否 |
| `HUB_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` | 否* |
| `HUB_RATE_LIMIT_RPM` | 公共端点速率限制（令牌桶算法，60秒窗口，突发容忍） | `120` | 否 |

**注意**：
- `HUB_PUBLIC_BASE_URL` 在服务器位于反向代理后时必须设置

**示例**：
```bash
# 开发环境
export HUB_HOST=127.0.0.1
export HUB_PORT=8080

# 生产环境
export HUB_HOST=0.0.0.0
export HUB_PORT=8080
export HUB_PUBLIC_BASE_URL=https://hub.example.com
```

### 认证设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `HUB_PRIVATE_KEY_PATH` | Ed25519 私钥路径 | `private_key.pem` | 是 |
| `HUB_KID` | 密钥标识符 | `hub-key-1` | 否 |
| `HUB_TOKEN_TTL_SECONDS` | CAS 令牌有效期（秒），用于签发 xet_xxx JWT | `3600` | 否 |
| `HUB_PROXY_TOKEN_TTL_SECONDS` | LFS Proxy Token 有效期（秒） | `300` (5 分钟) | 否 |
| `HUB_INTERNAL_TOKEN_TTL_SECONDS` | Hub→CAS internal endpoints 内部令牌有效期（秒），用于 `/internal/*`、GC 和内部指标类调用 | `86400` (24 小时) | 否 |

**说明**：
- `HUB_PRIVATE_KEY_PATH` 指向 Hub 的私钥文件（PEM 格式）
- `HUB_KID` 用于标识签名密钥，支持密钥轮换
- `HUB_TOKEN_TTL_SECONDS` 控制 CAS 令牌（xet_xxx）的有效期（默认 3600 秒 = 1 小时）
- Proxy 令牌（proxy_xxx）默认 5 分钟，可通过 `HUB_PROXY_TOKEN_TTL_SECONDS` 配置
- **`HUB_INTERNAL_TOKEN_TTL_SECONDS`**：内部令牌（internal_xxx）只用于 Hub→CAS internal endpoints，例如 `/internal/*`、GC 状态检查和内部指标类调用；不用于 CAS batch、public LFS object 或 inline resolve/commit 对象读写。默认 24 小时。设置小于 3600 秒会触发警告

### 安全设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `HUB_TOKEN_HASH_SALT` | Hub Token SHA256 哈希盐（defense-in-depth） | 自动生成并持久化到 SQLite | 否* |

**说明**：
- **`HUB_TOKEN_HASH_SALT`**：用于对 Hub Token（hf_xxx）进行 SHA256 哈希后再存储到数据库。首次启动时自动生成随机 salt 并持久化到 `_config` 表。**多实例部署必须通过此环境变量显式设置相同的 salt**，否则不同实例会生成不同的 hash，导致 token 验证失败

**示例**：
```bash
export HUB_PRIVATE_KEY_PATH=/etc/xet/hub-private-key.pem
export HUB_KID=hub-key-1
export HUB_TOKEN_TTL_SECONDS=7200  # 2 小时
```

### 元数据数据库

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `HUB_SQLITE_PATH` | SQLite 元数据数据库路径 | `hub.db` | 否 |
| `HUB_DB_POOL_SIZE` | SQLite 连接池大小 | `5` | 否 |

**说明**：
- 元数据数据库存储仓库、版本、文件树等信息
- Hub 启动时会运行内置 SQLite migration runner：新库会初始化当前 schema；已有 v1 schema 会补写 `schema_version`；未来版本或无法识别的已有 schema 会拒绝启动
- Hub server 会自动为连接设置 `PRAGMA journal_mode=WAL`、`PRAGMA foreign_keys=ON` 和 `PRAGMA busy_timeout=5000`
- `HUB_DB_POOL_SIZE` 是 TokenStore 和 MetadataStore 共享连接池的总连接数；SQLite 仍只有一个并发写入者，调大连接池主要提升读并发和等待写锁时的排队能力
- 当前实现只提供 SQLite backend。生产部署建议使用本地 SSD 或具备正确 SQLite 文件锁语义的持久卷；不要把普通对象存储、NFS 类弱锁语义路径或多主共享写入当作数据库后端
- 多 Hub 实例部署仍属于受限模式：所有实例必须连接同一个 SQLite 文件、使用相同 `HUB_TOKEN_HASH_SALT` 和 Hub signing key，并接受 SQLite 单写者限制；项目当前没有内置 Postgres/MySQL 等分布式数据库 backend
- 建议使用 SSD 存储以获得最佳性能

**示例**：
```bash
export HUB_SQLITE_PATH=/var/lib/xet/hub-metadata.db
export HUB_DB_POOL_SIZE=5
```

### CAS 客户端设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `CAS_BASE_URL` | CAS 服务器 URL | `http://localhost:8081` | 是 |
| `HUB_CAS_TIMEOUT_SECS` | CAS 请求超时（秒） | `30` | 否 |
| `HUB_MAX_DOWNLOAD_SIZE` | CAS 下载大小限制（字节），应 >= `HUB_MAX_UPLOAD_SIZE` | `536870912` (512MB) | 否 |
| `HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS` | Hub 启动时 CAS 健康检查超时（秒） | `10` | 否 |

**说明**：
- `CAS_BASE_URL` 指向 CAS Server 的内部 URL，默认端口为 8081（与 CAS Server 默认端口一致）
- `HUB_CAS_TIMEOUT_SECS` 控制 Hub 到 CAS 的请求超时
- `HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS`：Hub 启动时会异步检查 CAS 连通性，超过此时间未完成会记录错误日志（非阻塞）

**示例**：
```bash
export CAS_BASE_URL=http://cas-server:8081
export HUB_CAS_TIMEOUT_SECS=60
```

### 存储设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `HUB_INLINE_THRESHOLD` | 内联文件阈值（字节） | `1048576` (1MB) | 否 |
| `HUB_UPLOAD_TEMP_DIR` | 上传临时文件目录 | `./data/hub-uploads` | 否 |
| `HUB_MAX_UPLOAD_SIZE` | 最大上传文件大小（字节） | `536870912` (512MB) | 否 |

**说明**：
- `HUB_INLINE_THRESHOLD`: 小于此值的文件内联在 commit 中（regular 模式）
- `HUB_UPLOAD_TEMP_DIR`: 流式上传时的临时文件存储目录，建议使用 SSD
- `HUB_MAX_UPLOAD_SIZE`: 单个文件的最大上传大小限制

**示例**：
```bash
export HUB_INLINE_THRESHOLD=2097152  # 2MB
export HUB_UPLOAD_TEMP_DIR=/fast-ssd/hub-uploads
export HUB_MAX_UPLOAD_SIZE=1073741824  # 1GB
```

### 配置文件支持

Hub API 支持通过 TOML 文件进行配置。使用 `HUB_CONFIG_FILE` 环境变量指定配置文件路径：

```bash
export HUB_CONFIG_FILE=/etc/xet/hub-config.toml
```

配置文件示例：
```toml
[server]
host = "0.0.0.0"
port = 8080
public_base_url = "https://hub.example.com"

[auth]
private_key_path = "/etc/xet/hub-private-key.pem"
kid = "hub-key-1"
token_ttl_seconds = 3600

[metadata]
sqlite_path = "/var/lib/xet/hub-metadata.db"

[cas]
base_url = "http://localhost:8081"
internal_timeout_seconds = 30

[storage]
inline_threshold_bytes = 1048576
upload_temp_dir = "/fast-ssd/hub-uploads"
max_upload_size = 536870912
```

**优先级**：环境变量 > 配置文件 > 默认值。对数值型环境变量（如 `HUB_PORT`、`HUB_DB_POOL_SIZE`、`HUB_CAS_TIMEOUT_SECS`）和布尔型环境变量（如 `XET_INDEX_REBUILD_STRICT`），只要显式设置了非法值，启动就会失败；不会使用配置文件或默认值掩盖错误。

---

## 完整配置示例

### 开发环境

```bash
#!/bin/bash
# 开发环境配置

# CAS Server
export XET_HOST=127.0.0.1
export XET_PORT=8081
export XET_STORAGE_BACKEND=local
export XET_LOCAL_PATH=./data/cas-storage
export CAS_PUBLIC_KEY_PATH=./keys/hub-public-key.pem
export CAS_TRUSTED_KIDS=dev-key-1

# Hub API
export HUB_HOST=127.0.0.1
export HUB_PORT=8080
export HUB_PRIVATE_KEY_PATH=./keys/hub-private-key.pem
export HUB_KID=dev-key-1
export HUB_TOKEN_TTL_SECONDS=86400  # 24 小时（开发方便）
export HUB_SQLITE_PATH=./data/hub-metadata.db
export CAS_BASE_URL=http://127.0.0.1:8081
```

### 生产环境

```bash
#!/bin/bash
# 生产环境配置

# CAS Server
export XET_HOST=0.0.0.0
export XET_PORT=8081
export XET_PUBLIC_BASE_URL=https://cas.example.com
export XET_MAX_BODY_SIZE_MB=4096
export XET_STORAGE_BACKEND=local
export XET_LOCAL_PATH=/data/xet-storage
export XET_UPLOAD_TEMP_DIR=/fast-ssd/xet-uploads
export CAS_PUBLIC_KEY_PATH=/etc/xet/hub-public-key.pem
export CAS_TRUSTED_KIDS=hub-key-1,hub-key-2

# Hub API
export HUB_HOST=0.0.0.0
export HUB_PORT=8080
export HUB_PUBLIC_BASE_URL=https://hub.example.com
export HUB_PRIVATE_KEY_PATH=/etc/xet/hub-private-key.pem
export HUB_KID=hub-key-1
export HUB_TOKEN_TTL_SECONDS=3600  # 1 小时
export HUB_SQLITE_PATH=/var/lib/xet/hub-metadata.db
export CAS_BASE_URL=http://cas-server:8081
export HUB_CAS_TIMEOUT_SECS=60
export HUB_INLINE_THRESHOLD=2097152  # 2MB
```

### S3 存储后端

```bash
#!/bin/bash
# S3 存储配置

# CAS Server
export XET_HOST=0.0.0.0
export XET_PORT=8081
export XET_PUBLIC_BASE_URL=https://cas.example.com
export XET_STORAGE_BACKEND=s3
export XET_S3_BUCKET=my-xet-bucket
export XET_S3_REGION=us-east-1
export XET_S3_ENDPOINT=https://s3.amazonaws.com
export CAS_PUBLIC_KEY_PATH=/etc/xet/hub-public-key.pem
export CAS_TRUSTED_KIDS=hub-key-1

# Hub API（同上）
export HUB_HOST=0.0.0.0
export HUB_PORT=8080
# ... 其他 Hub 配置
```

> **⚠️ 重要：S3 Lifecycle Rules 配置**
>
> 使用 S3 存储后端时，**必须**配置 S3 Lifecycle Rules 来自动中止未完成的 multipart 上传。
>
> **为什么需要配置？**
> - 大文件（≥5MB）使用 multipart 上传
> - 如果进程崩溃或网络中断，multipart 上传会保持未完成状态
> - 未完成的 multipart 上传会产生持续的存储费用
> - 这些孤立的上传不会被自动清理
>
> **配置步骤：**
> 1. 在 AWS S3 控制台编辑存储桶的 Lifecycle 规则
> 2. 添加规则：中止未完成的 multipart 上传
> 3. 建议设置：7 天后中止未完成的上传
>
> **AWS CLI 示例：**
> ```bash
> aws s3api put-bucket-lifecycle-configuration \
>   --bucket my-xet-bucket \
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
> **MinIO 用户：** MinIO 也支持 lifecycle 配置，使用 `mc ilm` 命令配置。
>
> 如果不配置此规则，会导致存储费用持续增加。

---

## 密钥生成

### 生成 Ed25519 密钥对

```bash
# 生成私钥
openssl genpkey -algorithm Ed25519 -out hub-private-key.pem

# 从私钥提取公钥
openssl pkey -in hub-private-key.pem -pubout -out hub-public-key.pem
```

### 设置文件权限

```bash
# 私钥：仅所有者可读写
chmod 600 hub-private-key.pem

# 公钥：所有人可读
chmod 644 hub-public-key.pem
```

---

## 安全考虑

### 1. 私钥保护

**规则**：
- 永远不要将私钥提交到版本控制
- 使用文件权限限制访问（`chmod 600`）
- 在生产环境中使用密钥管理服务（KMS）
- 定期轮换密钥

**示例**：
```bash
# 使用 .gitignore 忽略密钥文件
echo "*.pem" >> .gitignore
echo "*.key" >> .gitignore
```

### 2. HTTPS/TLS

**生产环境必须启用 HTTPS**：

**方案 1：反向代理（推荐）**
```nginx
# Nginx 配置
server {
    listen 443 ssl;
    server_name cas.example.com;

    ssl_certificate /etc/ssl/certs/example.com.crt;
    ssl_certificate_key /etc/ssl/private/example.com.key;

    location / {
        proxy_pass http://127.0.0.1:8081;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

**注意**：Xet Server 本身不直接支持 TLS，建议在生产环境中使用反向代理（如 Nginx、Caddy）处理 TLS 终止。

### 3. 环境变量安全

**规则**：
- 不要在日志中打印环境变量
- 使用密钥管理服务存储敏感配置
- 限制环境变量文件的访问权限

**示例**：
```bash
# 使用受限的配置文件
chmod 600 .env
source .env
```

### 4. 网络安全

**规则**：
- 使用防火墙限制访问
- CAS Server 的内部 API 只应被 Hub 访问
- 使用 VPN 或私有网络连接服务

**示例**：
```bash
# UFW 防火墙规则
ufw allow 8080/tcp  # Hub API（公开）
ufw allow 8081/tcp  # CAS Server（限制访问）
ufw deny 8081/tcp from any except 10.0.0.0/8
```

---

## 性能优化

### 1. 存储性能

**建议**：
- 使用 SSD 存储数据库和临时文件
- 为本地存储使用快速文件系统（ext4, xfs）
- 考虑使用 RAID 10 提高性能

**示例**：
```bash
# 将临时文件目录放在 SSD 上
export XET_UPLOAD_TEMP_DIR=/fast-ssd/xet-uploads
export HUB_UPLOAD_TEMP_DIR=/fast-ssd/hub-uploads
```

### 2. 内存使用

**CAS Server**：
- `XET_MAX_BODY_SIZE_MB` 直接控制每个请求的内存使用
- 默认 2048MB（2GB）足够大多数用例
- 如果内存有限，可以降低此值

**Hub API**：
- 使用流式处理减少内存占用
- Commit API 的内联文件最大约 13.6MB（10MB base64 编码）

### 3. 并发连接

**建议**：
- 使用反向代理处理并发连接
- 配置合理的连接超时
- 启用 HTTP keep-alive

**Nginx 示例**：
```nginx
upstream cas_backend {
    server 127.0.0.1:8081;
    keepalive 32;
}

server {
    location / {
        proxy_pass http://cas_backend;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
    }
}
```

---

## 故障排除

### 问题 1：服务器启动失败

**症状**：服务器立即退出

**排查步骤**：
1. 检查环境变量是否正确设置
2. 验证密钥文件路径和权限
3. 检查数据库路径是否可写
4. 查看日志输出

**示例**：
```bash
# 检查密钥文件
ls -l hub-private-key.pem
# 应该显示: -rw------- (600)

# 检查目录权限
ls -ld /var/lib/xet
# 应该可写

# 查看日志
journalctl -u xet-server -f
```

### 问题 2：认证失败

**症状**：收到 401 错误

**排查步骤**：
1. 验证公钥/私钥是否匹配
2. 检查 `kid` 是否在受信任列表中
3. 确认令牌未过期
4. 检查令牌格式是否正确

**示例**：
```bash
# 验证密钥匹配
openssl pkey -in hub-private-key.pem -pubout -out test-public.pem
diff test-public.pem hub-public-key.pem
# 应该没有差异

# 检查受信任的 kids
echo $CAS_TRUSTED_KIDS
# 应该包含 Hub 的 kid
```

### 问题 3：存储错误

**症状**：上传/下载失败

**排查步骤**：
1. 检查存储路径是否存在且可写
2. 验证 S3 凭证和权限
3. 检查磁盘空间
4. 查看存储后端日志

**示例**：
```bash
# 检查磁盘空间
df -h /data/xet-storage

# 检查 S3 访问
aws s3 ls s3://my-xet-bucket --region us-east-1
```

### 问题 4：数据库锁定

**症状**：并发操作失败

**排查步骤**：
1. 检查是否有多个进程同时写入
2. 验证数据库文件权限
3. 检查数据库所在文件系统是否支持 SQLite 需要的文件锁语义
4. 降低并发写入或调整 `HUB_DB_POOL_SIZE`，但注意 SQLite 仍只有一个并发写入者

**SQLite WAL 模式**（Hub 元数据数据库）：
```bash
# Hub server 会在连接时自动启用 WAL；可用以下命令确认当前模式
sqlite3 /var/lib/xet/hub-metadata.db "PRAGMA journal_mode;"
```

---

## 监控和日志

### 日志级别

设置日志级别：
```bash
export RUST_LOG=info  # 或 debug, warn, error, trace
```

### Prometheus 指标

CAS Server 在 `/metrics` 端点暴露 Prometheus 指标（需要 `internal` scope 令牌）：

```bash
curl http://localhost:8081/metrics \
  -H "Authorization: Bearer xet_xxx"
```

> **注意**：`/metrics` 端点需要具有 `internal` scope 的 CAS 令牌，而非公开访问。监控系统应使用内部令牌。

**关键指标**：
- `http_requests_total`: HTTP 请求总数
- `http_request_duration_seconds`: 请求延迟
- `storage_operations_total`: 存储操作计数
- `storage_bytes_total`: 传输字节数

### 健康检查

```bash
# CAS Server 存活检查：进程和 HTTP server 可响应
curl http://localhost:8081/health

# CAS Server 就绪检查：存储 backend 可访问，MetadataIndex 已完成重建
curl http://localhost:8081/ready

# Hub API 存活检查
curl http://localhost:8080/health

# Hub API 就绪检查：SQLite 可查询，CAS /ready 可用
curl http://localhost:8080/ready
```

`/health` 是 liveness probe，只表示进程仍可响应 HTTP 请求；`/ready` 是 readiness probe。CAS `/ready` 会检查存储后端和索引状态，响应体包含 `checks.storage`、`checks.index`，索引就绪时还包含 `index_shard_count`。Hub `/ready` 会检查 SQLite 和 CAS `/ready`，响应体包含 `checks.database`、`checks.cas`。任一检查失败时 `/ready` 返回 `503`。

---

## 相关文档

- [Authentication](api/authentication.md) - 认证机制详细说明
- [CAS API Reference](api/cas-api.md) - CAS 服务器 API 文档
- [Hub API Reference](api/hub-api.md) - Hub API 文档
- [Architecture](architecture.md) - 系统架构说明
