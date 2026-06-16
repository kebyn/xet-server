# CAS API 参考文档

CAS (Content Addressable Storage) Server 是 Xet Server 的核心存储引擎，提供高性能的内容寻址存储服务。

**端口**：8081（默认）  
**协议**：HTTP/REST  
**认证**：Ed25519 JWT (xet_xxx tokens)

## 端点概览

| 端点 | 方法 | 描述 | 认证 |
|------|------|------|------|
| `/v1/xorbs/{prefix}/{hash}` | POST/PUT | 上传 Xorb 对象 | 需要 write |
| `/v1/xorbs/{prefix}/{hash}/download` | GET | 下载 Xorb 对象 | 需要 read |
| `/lfs/objects/{oid}` | PUT | 上传 LFS 对象 | 需要 write |
| `/lfs/objects/{oid}` | GET | 下载 LFS 对象 | 需要 read |
| `/v1/shards` | POST | 上传 Shard 元数据 | 需要 write |
| `/v1/reconstructions/{file_id}` | GET | 获取文件重构信息 (V1) | 需要 read |
| `/v2/reconstructions/{file_id}` | GET | 获取文件重构信息 (V2) | 需要 read |
| `/v1/chunks/{prefix}/{hash}` | GET | 全局去重查询 | 需要 read |
| `/objects/batch` | POST | Git LFS 批量 API | 需要 read/write |
| `/lfs/objects/batch` | POST | Git LFS 批量 API (别名) | 需要 read/write |
| `/internal/state/{oid}` | GET | 查询 blob 状态 | 需要 internal |
| `/internal/blob/{oid}` | HEAD | 检查 blob 存在 | 需要 internal |
| `/health` | GET | 健康检查 | 无需认证 |
| `/metrics` | GET | Prometheus 指标 | 需要 internal |

---

## Xorb 对象 API

Xorb (Xet Object) 是内容寻址存储的核心对象，包含分块后的文件数据和元数据。

### 上传 Xorb

**端点**：`POST /v1/xorbs/{prefix}/{hash}` 或 `PUT /v1/xorbs/{prefix}/{hash}`

**路径参数**：
- `prefix` (string): Xorb 哈希的前 2 个字符（用于分片）
- `hash` (string): Xorb 的完整 BLAKE3 哈希（64 个十六进制字符）

**请求头**：
```
Authorization: Bearer xet_xxx
Content-Type: application/octet-stream
```

**请求体**：Xorb 二进制数据

**响应**：
- `200 OK`: 上传成功
- `400 Bad Request`: 哈希验证失败
- `401 Unauthorized`: 认证失败
- `500 Internal Server Error`: 服务器错误

**示例**：
```bash
curl -X POST "http://localhost:8081/v1/xorbs/ab/abc123...def" \
  -H "Authorization: Bearer xet_xxx" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @xorb.bin
```

### 下载 Xorb

**端点**：`GET /v1/xorbs/{prefix}/{hash}/download`

**路径参数**：
- `prefix` (string): Xorb 哈希的前 2 个字符
- `hash` (string): Xorb 的完整哈希

**请求头**：
```
Authorization: Bearer xet_xxx
```

**响应**：
- `200 OK`: 返回 Xorb 二进制数据
- `404 Not Found`: Xorb 不存在
- `401 Unauthorized`: 认证失败

**示例**：
```bash
curl -o xorb.bin "http://localhost:8081/v1/xorbs/ab/abc123...def/download" \
  -H "Authorization: Bearer xet_xxx"
```

---

## LFS 对象 API

Git LFS (Large File Storage) 对象存储，提供与 Git LFS 协议兼容的原始文件存储。

### 上传 LFS 对象

**端点**：`PUT /lfs/objects/{oid}`

**路径参数**：
- `oid` (string): LFS 对象的 SHA-256 哈希（64 个十六进制字符）

**请求头**：
```
Authorization: Bearer xet_xxx
Content-Type: application/octet-stream
```

**请求体**：原始文件数据

**响应**：
- `200 OK`: 上传成功
- `400 Bad Request`: 哈希验证失败
- `401 Unauthorized`: 认证失败

**示例**：
```bash
curl -X PUT "http://localhost:8081/lfs/objects/abc123...def" \
  -H "Authorization: Bearer xet_xxx" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @model.bin
```

### 下载 LFS 对象

**端点**：`GET /lfs/objects/{oid}`

**路径参数**：
- `oid` (string): LFS 对象的 SHA-256 哈希

**请求头**：
```
Authorization: Bearer xet_xxx
```

**响应**：
- `200 OK`: 返回原始文件数据
- `404 Not Found`: 对象不存在
- `401 Unauthorized`: 认证失败

**示例**：
```bash
curl -o model.bin "http://localhost:8081/lfs/objects/abc123...def" \
  -H "Authorization: Bearer xet_xxx"
```

---

## Shard 元数据 API

Shard 是 Merkle DB 分片文件，包含文件到 chunk/xorb 的映射元数据。

### 上传 Shard

**端点**：`POST /v1/shards`

**请求头**：
```
Authorization: Bearer xet_xxx
Content-Type: application/octet-stream
```

**请求体**：Shard 二进制数据

**响应**：
- `200 OK`: 上传成功
- `400 Bad Request`: Shard 格式无效
- `401 Unauthorized`: 认证失败

**示例**：
```bash
curl -X POST "http://localhost:8081/v1/shards" \
  -H "Authorization: Bearer xet_xxx" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @shard.mdb
```

---

## 文件重构 API

文件重构 API 返回从 chunks 和 xorbs 重构完整文件所需的元数据。

### 获取重构信息 (V1)

**端点**：`GET /v1/reconstructions/{file_id}`

**路径参数**：
- `file_id` (string): 文件标识符（通常是文件哈希）

**请求头**：
```
Authorization: Bearer xet_xxx
```

**响应** (V1)：
```json
{
  "file_id": "abc123...",
  "xorbs": [
    {
      "xorb_hash": "xorb1_hash",
      "size": 65536,
      "chunks": [
        {
          "chunk_hash": "chunk1_hash",
          "offset": 0,
          "length": 65536
        },
        {
          "chunk_hash": "chunk2_hash",
          "offset": 65536,
          "length": 65536
        }
      ]
    }
  ]
}
```

**字段说明**：
- `file_id`: 文件标识符
- `xorbs`: Xorb 对象列表
  - `xorb_hash`: Xorb 的 BLAKE3 哈希
  - `size`: Xorb 大小（字节）
  - `chunks`: 该 Xorb 中包含的 chunk 列表
    - `chunk_hash`: Chunk 的 BLAKE3 哈希
    - `offset`: Chunk 在 Xorb 中的偏移量
    - `length`: Chunk 长度（字节）

**响应码**：
- `200 OK`: 返回重构信息
- `404 Not Found`: 文件不存在
- `401 Unauthorized`: 认证失败

### 获取重构信息 (V2)

**端点**：`GET /v2/reconstructions/{file_id}`

**与 V1 的区别**：
- 分离 Xorb 元数据和获取信息
- `fetch_info` 提供存储路径和大小，便于客户端直接下载
- 减少响应体积（xorbs 数组只包含哈希和大小）

**响应** (V2)：
```json
{
  "file_id": "abc123...",
  "xorbs": [
    {
      "xorb_hash": "xorb1_hash",
      "size": 65536
    },
    {
      "xorb_hash": "xorb2_hash",
      "size": 131072
    }
  ],
  "fetch_info": {
    "xorb1_hash": {
      "storage_path": "xorbs/ab/xorb1_hash",
      "size": 65536
    },
    "xorb2_hash": {
      "storage_path": "xorbs/cd/xorb2_hash",
      "size": 131072
    }
  }
}
```

**字段说明**：
- `file_id`: 文件标识符
- `xorbs`: Xorb 对象列表（仅包含哈希和大小）
- `fetch_info`: Xorb 获取信息映射表
  - key: Xorb 哈希
  - `storage_path`: 存储路径（用于构造下载 URL）
  - `size`: Xorb 大小（字节）

**示例**：
```bash
curl "http://localhost:8081/v2/reconstructions/abc123..." \
  -H "Authorization: Bearer xet_xxx"
```

---

## 全局去重 API

查询 chunk 是否已存在于存储中，用于全局去重优化。

### 查询 Chunk

**端点**：`GET /v1/chunks/{prefix}/{hash}`

**路径参数**：
- `prefix` (string): Chunk 哈希的前 2 个字符
- `hash` (string): Chunk 的完整 BLAKE3 哈希

**请求头**：
```
Authorization: Bearer xet_xxx
```

**响应**：
- `200 OK`: 查询成功（无论 chunk 是否存在）
  ```json
  {
    "hash": "abc123...",
    "found": true,
    "xorb_hash": "xorb_hash_value",
    "chunk_index": 0
  }
  ```
  
  **字段说明**：
  - `hash`: 查询的 chunk 哈希
  - `found`: chunk 是否存在
  - `xorb_hash`: chunk 所在的 Xorb 哈希（仅当 `found=true` 时存在）
  - `chunk_index`: chunk 在 Xorb 中的索引（仅当 `found=true` 时存在）

- `400 Bad Request`: 参数错误（prefix 不是 "default" 或 hash 格式不正确）
- `401 Unauthorized`: 认证失败

**示例**：
```bash
curl "http://localhost:8081/v1/chunks/ab/abc123...def" \
  -H "Authorization: Bearer xet_xxx"
```

---

## Git LFS 批量 API

Git LFS 批量 API 提供标准的 LFS 协议支持，用于客户端批量操作。

### 批量操作

**端点**：`POST /objects/batch` 或 `POST /lfs/objects/batch`

**请求头**：
```
Authorization: Bearer xet_xxx
Content-Type: application/vnd.git-lfs+json
```

**请求体**：
```json
{
  "operation": "upload",
  "transfers": ["basic"],
  "objects": [
    {
      "oid": "abc123...",
      "size": 104857600
    },
    {
      "oid": "def456...",
      "size": 52428800
    }
  ],
  "ref": {
    "name": "refs/heads/main"
  }
}
```

**响应**：
```json
{
  "transfer": "basic",
  "objects": [
    {
      "oid": "abc123...",
      "size": 104857600,
      "authenticated": true,
      "actions": {
        "upload": {
          "href": "http://localhost:8081/lfs/objects/abc123...",
          "header": {
            "Authorization": "Bearer xet_xxx"
          }
        }
      }
    },
    {
      "oid": "def456...",
      "size": 52428800,
      "authenticated": true,
      "actions": {
        "download": {
          "href": "http://localhost:8081/lfs/objects/def456...",
          "header": {
            "Authorization": "Bearer xet_xxx"
          }
        }
      }
    }
  ]
}
```

**操作类型**：
- `upload`: 上传对象
- `download`: 下载对象

**示例**：
```bash
curl -X POST "http://localhost:8081/objects/batch" \
  -H "Authorization: Bearer xet_xxx" \
  -H "Content-Type: application/vnd.git-lfs+json" \
  -d '{
    "operation": "download",
    "objects": [{"oid": "abc123...", "size": 104857600}]
  }'
```

---

## 内部 API

内部 API 用于 Hub Server 与 CAS Server 之间的通信，需要 `internal` 作用域。

### 查询 Blob 状态

**端点**：`GET /internal/state/{oid}`

**路径参数**：
- `oid` (string): Blob 的对象 ID

**请求头**：
```
Authorization: Bearer xet_xxx (需要 internal token (sub=hub-service, scope=internal, token_type=internal))
```

**响应**：

当 blob 已转换为 Xet 格式（`xet_only`）：
```json
{
  "state": "xet_only",
  "xet_file_id": "abc123...",
  "size": 104857600,
  "sha256": "abc123...",
  "converted_at": null
}
```

当 blob 仅存储为原始格式（`raw_only`）：
```json
{
  "state": "raw_only",
  "xet_file_id": null,
  "size": 104857600,
  "sha256": "abc123...",
  "converted_at": null
}
```

**字段说明**：
- `state`: Blob 存储状态
  - `xet_only`: 仅存储 Xet 格式（已分块/去重）
  - `raw_only`: 仅存储原始字节（来自 Git LFS）
- `xet_file_id`: Xet 文件 ID（仅当 `state=xet_only` 时有值）
- `size`: Blob 大小（字节）
- `sha256`: Blob 的 SHA-256 哈希
- `converted_at`: 转换时间戳（当前实现中为 `null`）

**响应码**：
- `200 OK`: 查询成功
- `404 Not Found`: Blob 不存在
- `401 Unauthorized`: 认证失败
- `403 Forbidden`: 权限不足（需要 `internal` scope）

### 检查 Blob 存在

**端点**：`HEAD /internal/blob/{oid}`

**路径参数**：
- `oid` (string): Blob 的对象 ID

**请求头**：
```
Authorization: Bearer xet_xxx (需要 internal token (sub=hub-service, scope=internal, token_type=internal))
```

**响应**：
- `200 OK`: Blob 存在
- `404 Not Found`: Blob 不存在

**示例**：
```bash
curl -I "http://localhost:8081/internal/state/abc123..." \
  -H "Authorization: Bearer xet_xxx"
```

---

## 系统 API

### 健康检查

**端点**：`GET /health`

**响应**：
```json
{
  "status": "ok"
}
```

**示例**：
```bash
curl "http://localhost:8081/health"
```

### Prometheus 指标

**端点**：`GET /metrics`

**响应** (Prometheus 格式)：
```
# HELP http_requests_total Total number of HTTP requests
# TYPE http_requests_total counter
http_requests_total 1234

# HELP http_requests_by_status HTTP requests by status code range
# TYPE http_requests_by_status counter
http_requests_by_status{status="2xx"} 1100
http_requests_by_status{status="3xx"} 50
http_requests_by_status{status="4xx"} 80
http_requests_by_status{status="5xx"} 4
http_requests_by_status{status="other"} 0

# HELP storage_operations_total Total number of storage operations
# TYPE storage_operations_total counter
storage_operations_total 300

# HELP upload_bytes_total Total bytes uploaded
# TYPE upload_bytes_total counter
upload_bytes_total 1073741824

# HELP download_bytes_total Total bytes downloaded
# TYPE download_bytes_total counter
download_bytes_total 2147483648

# HELP errors_total Total number of errors
# TYPE errors_total counter
errors_total 42

# HELP active_connections Current number of active connections
# TYPE active_connections gauge
active_connections 15

# HELP request_latency_us_total Total request latency in microseconds
# TYPE request_latency_us_total counter
request_latency_us_total 5000000

# HELP request_latency_count Total number of latency measurements
# TYPE request_latency_count counter
request_latency_count 1234
```

**示例**：
```bash
curl "http://localhost:8081/metrics"
```

---

## 错误响应

### 错误格式

```json
{
  "error": {
    "type": "error_type",
    "message": "Human-readable error message",
    "code": "error_code"
  }
}
```

### 常见错误

| HTTP 状态码 | 错误类型 | 描述 |
|------------|----------|------|
| 400 | `validation_error` | 请求参数无效 |
| 400 | `hash_mismatch` | 哈希验证失败 |
| 401 | `authentication_error` | 认证失败 |
| 401 | `expired` | 令牌已过期 |
| 403 | `insufficient_scope` | 权限不足 |
| 404 | `not_found` | 资源不存在 |
| 413 | `payload_too_large` | 请求体过大 |
| 500 | `internal_error` | 服务器内部错误 |
| 503 | `service_unavailable` | 服务暂不可用 |

---

## 数据格式

### Xorb 格式

Xorb 是 Xet Server 的核心存储格式，包含分块后的文件数据：

```
┌─────────────────────────────────────┐
│         Chunk 1 Data                │
├─────────────────────────────────────┤
│         Chunk 2 Data                │
├─────────────────────────────────────┤
│         ...                         │
├─────────────────────────────────────┤
│         Chunk N Data                │
├─────────────────────────────────────┤
│         Footer                      │
│  • Chunk count                      │
│  • Chunk hashes                     │
│  • Chunk boundaries                 │
│  • Compression info                 │
└─────────────────────────────────────┘
```

### Shard 格式

Shard 是 Merkle DB 分片文件，包含文件到 chunk/xorb 的映射：

```
┌─────────────────────────────────────┐
│         Header                      │
│  • Magic number                     │
│  • Version                          │
├─────────────────────────────────────┤
│         File Entries                │
│  • File hash                        │
│  • Chunk list                       │
│  • Xorb mappings                    │
├─────────────────────────────────────┤
│         Xorb Entries                │
│  • Xorb hash                        │
│  • Chunk hashes                     │
├─────────────────────────────────────┤
│         Footer                      │
│  • Index offsets                    │
│  • Checksums                        │
└─────────────────────────────────────┘
```

### 压缩方案

支持的压缩算法：

| 方案 | 描述 | 使用场景 |
|------|------|----------|
| `None` | 无压缩 | 小文件、已压缩数据 |
| `LZ4` | LZ4 快速压缩 | 默认方案，平衡速度和压缩率 |
| `ByteGrouping4LZ4` | 字节分组 + LZ4 | 特定数据类型优化 |

---

## 性能考虑

### 上传优化

1. **批量上传**：使用 LFS Batch API 批量操作多个文件
2. **并行上传**：同时上传多个 xorbs/chunks
3. **全局去重**：先查询 `/v1/chunks` 避免重复上传

### 下载优化

1. **并行下载**：同时下载多个 xorbs
2. **缓存**：客户端缓存已下载的 xorbs
3. **重构缓存**：缓存文件重构信息

### 存储优化

1. **内容寻址**：自动去重，节省存储空间
2. **压缩**：LZ4 压缩减少存储占用
3. **分片**：Xorb 前缀分片优化查找性能

---

## 相关文档

- [Authentication](authentication.md) - 认证机制详细说明
- [Hub API Reference](hub-api.md) - Hub API 文档
- [Configuration Guide](../configuration.md) - 配置选项
- [Architecture](../architecture.md) - 系统架构
