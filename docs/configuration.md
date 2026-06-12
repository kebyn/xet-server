# 配置指南

本文档详细说明 Xet Server 的所有配置选项，包括 CAS Server 和 Hub API 的环境变量、默认值和最佳实践。

## 概述

Xet Server 由两个独立的服务组成，每个服务都有自己的配置：

- **CAS Server** (`xet-server`): 核心存储引擎
- **Hub API** (`hub-api`): HuggingFace 兼容 API 层

所有配置通过**环境变量**进行管理。

---

## CAS Server 配置

CAS Server 是 Xet Server 的核心存储引擎，负责内容寻址存储、文件重构和去重。

### 服务器设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `XET_HOST` | 服务器绑定地址 | `127.0.0.1` | 否 |
| `XET_PORT` | 服务器端口 | `8080` | 否 |
| `XET_PUBLIC_BASE_URL` | 公共访问 URL | `http://{host}:{port}` | 否* |
| `XET_MAX_BODY_SIZE_MB` | 最大请求体大小（MB） | `2048` | 否 |

**注意**：
- `XET_PUBLIC_BASE_URL` 在服务器位于反向代理、负载均衡器或 NAT 后时**必须设置**
- `XET_MAX_BODY_SIZE_MB` 控制单个请求的最大内存使用

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
```

### 存储设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `XET_STORAGE_BACKEND` | 存储后端类型 | `local` | 否 |
| `XET_LOCAL_PATH` | 本地存储路径 | `./data` | 是* |
| `XET_S3_BUCKET` | S3 存储桶名称 | - | 是** |
| `XET_S3_REGION` | S3 区域 | - | 否 |
| `XET_S3_ENDPOINT` | S3 端点 URL | - | 否 |
| `XET_UPLOAD_TEMP_DIR` | 上传临时文件目录 | 自动 | 否 |

**说明**：
- `XET_LOCAL_PATH` 在 `XET_STORAGE_BACKEND=local` 时必需
- `XET_S3_BUCKET` 在 `XET_STORAGE_BACKEND=s3` 时必需
- `XET_UPLOAD_TEMP_DIR` 默认值：
  - 本地存储：`{XET_LOCAL_PATH}/.tmp`（同一文件系统，支持原子重命名）
  - S3 存储：`/tmp/xet-uploads`

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
| `CAS_PUBLIC_KEY_PATH` | Ed25519 公钥路径 | `/tmp/xet-public-key.pem` | 是 |
| `CAS_TRUSTED_KIDS` | 受信任的密钥 ID 列表 | `test-kid` | 是 |

**说明**：
- `CAS_PUBLIC_KEY_PATH` 指向 Hub 的公钥文件（PEM 格式）
- `CAS_TRUSTED_KIDS` 是逗号分隔的密钥 ID 列表，用于密钥轮换

**示例**：
```bash
export CAS_PUBLIC_KEY_PATH=/etc/xet/hub-public-key.pem
export CAS_TRUSTED_KIDS=hub-key-1,hub-key-2
```

### 状态数据库

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `CAS_STATE_DB_PATH` | SQLite 状态数据库路径 | `/tmp/xet-state.db` | 否 |

**说明**：
- 状态数据库跟踪 blob 的存储状态（RawOnly/XetOnly）
- 建议使用 SSD 存储以获得最佳性能

**示例**：
```bash
export CAS_STATE_DB_PATH=/var/lib/xet/state.db
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
| `HUB_TOKEN_TTL_SECONDS` | 用户令牌有效期（秒） | `3600` | 否 |

**说明**：
- `HUB_PRIVATE_KEY_PATH` 指向 Hub 的私钥文件（PEM 格式）
- `HUB_KID` 用于标识签名密钥，支持密钥轮换
- `HUB_TOKEN_TTL_SECONDS` 控制 CAS 令牌（xet_xxx）的有效期（默认 3600 秒 = 1 小时）
- Proxy 令牌（proxy_xxx）固定为 5 分钟（不可配置）

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

**说明**：
- 元数据数据库存储仓库、版本、文件树等信息
- 建议使用 SSD 存储以获得最佳性能

**示例**：
```bash
export HUB_SQLITE_PATH=/var/lib/xet/hub-metadata.db
```

### CAS 客户端设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `CAS_BASE_URL` | CAS 服务器 URL | `http://localhost:3000` | 是 |
| `HUB_CAS_TIMEOUT_SECS` | CAS 请求超时（秒） | `30` | 否 |

**说明**：
- `CAS_BASE_URL` 指向 CAS Server 的内部 URL
- `HUB_CAS_TIMEOUT_SECS` 控制 Hub 到 CAS 的请求超时

**示例**：
```bash
export CAS_BASE_URL=http://cas-server:8081
export HUB_CAS_TIMEOUT_SECS=60
```

### 存储设置

| 环境变量 | 描述 | 默认值 | 必需 |
|---------|------|--------|------|
| `HUB_DATA_DIR` | Hub 数据目录 | `./data` | 否 |
| `HUB_INLINE_THRESHOLD` | 内联文件阈值（字节） | `1048576` (1MB) | 否 |
| `HUB_LFS_THRESHOLD` | LFS 文件阈值（字节） | `10485760` (10MB) | 否 |

**说明**：
- `HUB_INLINE_THRESHOLD`: 小于此值的文件内联在 commit 中
- `HUB_LFS_THRESHOLD`: 大于此值的文件使用 Xet 路径（分块/去重）
- 介于两者之间的文件使用 LFS 路径（原始字节存储）

**示例**：
```bash
export HUB_DATA_DIR=/var/lib/xet/hub-data
export HUB_INLINE_THRESHOLD=2097152  # 2MB
export HUB_LFS_THRESHOLD=20971520    # 20MB
```

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
export CAS_STATE_DB_PATH=./data/cas-state.db

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
export CAS_STATE_DB_PATH=/var/lib/xet/state.db

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
export HUB_LFS_THRESHOLD=20971520    # 20MB
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
export CAS_STATE_DB_PATH=/var/lib/xet/state.db

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
export HUB_DATA_DIR=/fast-ssd/hub-data
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
3. 考虑使用 WAL 模式

**SQLite WAL 模式**：
```bash
# 启用 WAL 模式（提高并发性能）
sqlite3 /var/lib/xet/state.db "PRAGMA journal_mode=WAL;"
```

---

## 监控和日志

### 日志级别

设置日志级别：
```bash
export RUST_LOG=info  # 或 debug, warn, error, trace
```

### Prometheus 指标

CAS Server 在 `/metrics` 端点暴露 Prometheus 指标：

```bash
curl http://localhost:8081/metrics
```

**关键指标**：
- `http_requests_total`: HTTP 请求总数
- `http_request_duration_seconds`: 请求延迟
- `storage_operations_total`: 存储操作计数
- `storage_bytes_total`: 传输字节数

### 健康检查

```bash
# CAS Server 健康检查
curl http://localhost:8081/health

# Hub API 健康检查
curl http://localhost:8080/health
```

---

## 相关文档

- [Authentication](api/authentication.md) - 认证机制详细说明
- [CAS API Reference](api/cas-api.md) - CAS 服务器 API 文档
- [Hub API Reference](api/hub-api.md) - Hub API 文档
- [Architecture](architecture.md) - 系统架构说明
