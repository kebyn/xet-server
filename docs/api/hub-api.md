# Hub API 参考文档

Hub API 提供 HuggingFace Hub 兼容的 REST API，支持使用 `hf` CLI 工具和标准 HTTP 客户端管理仓库和文件。

**端口**：8080（默认）  
**协议**：HTTP/REST  
**认证**：Hub tokens (`hf_xxx`) - 不透明 UUID，通过 SQLite 存储和 SHA256 哈希验证

## 端点概览

### 用户认证
| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/whoami-v2` | GET | 获取当前用户信息 |

### 仓库管理
| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/repos/create` | POST | 创建仓库（统一接口） |
| `/api/models` | POST | 创建模型仓库 |
| `/api/datasets` | POST | 创建数据集仓库 |
| `/api/spaces` | POST | 创建 Space 仓库 |
| `/api/models/{ns}/{repo}` | GET/DELETE | 获取/删除模型仓库 |
| `/api/datasets/{ns}/{repo}` | GET/DELETE | 获取/删除数据集仓库 |
| `/api/spaces/{ns}/{repo}` | GET/DELETE | 获取/删除 Space 仓库 |

### 版本管理
| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/models/{ns}/{repo}/revision/{rev}` | GET | 获取模型版本信息 |
| `/api/datasets/{ns}/{repo}/revision/{rev}` | GET | 获取数据集版本信息 |
| `/api/spaces/{ns}/{repo}/revision/{rev}` | GET | 获取 Space 版本信息 |

### 文件操作
| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/models/{ns}/{repo}/commit/{rev}` | POST | 提交文件到模型 |
| `/api/datasets/{ns}/{repo}/commit/{rev}` | POST | 提交文件到数据集 |
| `/api/spaces/{ns}/{repo}/commit/{rev}` | POST | 提交文件到 Space |
| `/api/models/{ns}/{repo}/tree/{rev}` | GET | 列出模型文件树 |
| `/api/datasets/{ns}/{repo}/tree/{rev}` | GET | 列出数据集文件树 |
| `/api/spaces/{ns}/{repo}/tree/{rev}` | GET | 列出 Space 文件树 |
| `/{type}/{ns}/{repo}/resolve/{rev}/{path}` | GET | 下载文件 |

### 令牌交换
| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/models/{ns}/{repo}/xet-read-token/{rev}` | GET | 获取模型读令牌 |
| `/api/models/{ns}/{repo}/xet-write-token/{rev}` | GET | 获取模型写令牌 |
| `/api/datasets/{ns}/{repo}/xet-read-token/{rev}` | GET | 获取数据集读令牌 |
| `/api/datasets/{ns}/{repo}/xet-write-token/{rev}` | GET | 获取数据集写令牌 |
| `/api/spaces/{ns}/{repo}/xet-read-token/{rev}` | GET | 获取 Space 读令牌 |
| `/api/spaces/{ns}/{repo}/xet-write-token/{rev}` | GET | 获取 Space 写令牌 |

### 预上传
| 端点 | 方法 | 描述 |
|------|------|------|
| `/api/models/{ns}/{repo}/preupload/{rev}` | POST | 模型预上传检查 |
| `/api/datasets/{ns}/{repo}/preupload/{rev}` | POST | 数据集预上传检查 |
| `/api/spaces/{ns}/{repo}/preupload/{rev}` | POST | Space 预上传检查 |

### LFS 代理
| 端点 | 方法 | 描述 |
|------|------|------|
| `/objects/batch` | POST | LFS 批量操作代理 |
| `/lfs/objects/{oid}` | GET/PUT | LFS 对象下载/上传代理 |

---

## 用户认证 API

### 获取当前用户信息

**端点**：`GET /api/whoami-v2`

**请求头**：
```
Authorization: Bearer hf_xxx
```

**响应**：
```json
{
  "name": "admin",
  "email": "",
  "orgs": [],
  "auth": {
    "type": "access_token",
    "accessToken": {
      "name": "my-token",
      "role": "read write"
    }
  }
}
```

**字段说明**：
- `name`: 用户名
- `email`: 邮箱（当前实现为空字符串）
- `orgs`: 组织列表（当前实现为空数组）
- `auth`: 认证信息
  - `type`: 认证类型，固定为 "access_token"
  - `accessToken`: 访问令牌信息
    - `name`: 令牌名称
    - `role`: 令牌作用域（如 "read", "write", "read write"）

**示例**：
```bash
curl "http://localhost:8080/api/whoami-v2" \
  -H "Authorization: Bearer hf_xxx"
```

---

## 仓库管理 API

### 创建仓库（统一接口）

**端点**：`POST /api/repos/create`

**请求头**：
```
Authorization: Bearer hf_xxx
Content-Type: application/json
```

**请求体**：
```json
{
  "name": "my-model",
  "organization": "my-org",
  "type": "model",
  "private": false
}
```

**字段说明**：
- `name` (required): 仓库名称
- `organization` (optional): 命名空间（组织或用户名），省略时使用当前用户名
- `type` (optional): 仓库类型 - `"model"`, `"dataset"`, `"space"`，省略时默认为 "model"
- `private` (optional): 是否私有，默认 `false`

**响应**：
```json
{
  "id": "my-org/my-model",
  "name": "my-model",
  "private": false,
  "createdAt": "2026-06-12T10:00:00Z",
  "updatedAt": "2026-06-12T10:00:00Z",
  "tags": [],
  "downloads": 0,
  "likes": 0,
  "url": "/my-org/my-model"
}
```

**字段说明**：
- `id`: 仓库 ID（格式：`namespace/name`）
- `name`: 仓库名称
- `private`: 是否私有
- `createdAt`: 创建时间（ISO 8601 格式）
- `updatedAt`: 更新时间（ISO 8601 格式）
- `tags`: 标签列表（当前实现为空数组）
- `downloads`: 下载次数（当前实现为 0）
- `likes`: 点赞数（当前实现为 0）
- `url`: 仓库 URL

**示例**：
```bash
curl -X POST "http://localhost:8080/api/repos/create" \
  -H "Authorization: Bearer hf_xxx" \
  -H "Content-Type: application/json" \
  -d '{
    "type": "model",
    "name": "my-model",
    "namespace": "my-org",
    "private": false
  }'
```

### 创建模型仓库

**端点**：`POST /api/models`

**请求体**：
```json
{
  "name": "my-model",
  "namespace": "my-org",
  "private": false
}
```

**响应**：同统一接口

### 创建数据集仓库

**端点**：`POST /api/datasets`

**请求体**：
```json
{
  "name": "my-dataset",
  "namespace": "my-org",
  "private": false
}
```

### 创建 Space 仓库

**端点**：`POST /api/spaces`

**请求体**：
```json
{
  "name": "my-space",
  "namespace": "my-org",
  "private": false,
  "sdk": "gradio"
}
```

### 获取仓库信息

**端点**：`GET /api/models/{ns}/{repo}`

**路径参数**：
- `ns` (string): 命名空间
- `repo` (string): 仓库名称

**请求头**：
```
Authorization: Bearer hf_xxx
```

**响应**：
```json
{
  "id": "my-org/my-model",
  "name": "my-model",
  "private": false,
  "createdAt": "2026-06-12T10:00:00Z",
  "updatedAt": "2026-06-12T10:00:00Z",
  "tags": [],
  "downloads": 0,
  "likes": 0,
  "url": "/my-org/my-model"
}
```

**字段说明**：
- `id`: 仓库 ID（格式：`namespace/name`）
- `name`: 仓库名称
- `private`: 是否私有
- `createdAt`: 创建时间（ISO 8601 格式）
- `updatedAt`: 更新时间（ISO 8601 格式）
- `tags`: 标签列表（当前实现为空数组）
- `downloads`: 下载次数（当前实现为 0）
- `likes`: 点赞数（当前实现为 0）
- `url`: 仓库 URL

**示例**：
```bash
curl "http://localhost:8080/api/models/my-org/my-model" \
  -H "Authorization: Bearer hf_xxx"
```

### 删除仓库

**端点**：`DELETE /api/models/{ns}/{repo}`

**响应**：
- `204 No Content`: 删除成功
- `404 Not Found`: 仓库不存在

**示例**：
```bash
curl -X DELETE "http://localhost:8080/api/models/my-org/my-model" \
  -H "Authorization: Bearer hf_xxx"
```

---

## 版本管理 API

### 获取版本信息

**端点**：`GET /api/models/{ns}/{repo}/revision/{rev}`

**路径参数**：
- `ns` (string): 命名空间
- `repo` (string): 仓库名称
- `rev` (string): 修订版本（分支名、标签或 commit SHA）

**响应**：
```json
{
  "commit": {
    "id": "abc123...",
    "title": "Add model files",
    "message": "Add model files\n\nUploaded via HF CLI",
    "authors": [
      {
        "user": "admin",
        "fullname": "Admin User"
      }
    ],
    "date": "2026-06-12T10:00:00Z"
  },
  "siblings": [
    {"rfilename": "config.json", "size": 1234},
    {"rfilename": "model.safetensors", "size": 104857600}
  ]
}
```

**示例**：
```bash
curl "http://localhost:8080/api/models/my-org/my-model/revision/main" \
  -H "Authorization: Bearer hf_xxx"
```

---

## 文件操作 API

### 提交文件 (Commit API)

**端点**：`POST /api/models/{ns}/{repo}/commit/{rev}`

**请求头**：
```
Authorization: Bearer hf_xxx
Content-Type: application/x-ndjson
```

**请求体** (NDJSON 格式，每行一个 JSON 对象)：
```
{"key":"header","value":{"summary":"Add model files"}}
{"key":"file","value":{"path":"config.json","content":"eyJtb2RlbF90eXBlIjoicXdlbiJ9"}}
{"key":"lfsFile","value":{"path":"model.safetensors","oid":"sha256:abc123...","size":104857600}}
```

**NDJSON 操作类型**：

1. **Header**（必需，第一行）：
   ```json
   {
     "key": "header",
     "value": {
       "summary": "Add model files",
       "parentRevision": "abc123..."  // 可选
     }
   }
   ```
   - `summary`: 提交信息（必需）
   - `parentRevision`: 父版本（可选）

2. **File**（内联文件，原始内容最大 10MB，Base64 编码后约 13.3MB）：
   ```json
   {
     "key": "file",
     "value": {
       "path": "config.json",
       "content": "eyJtb2RlbF90eXBlIjoicXdlbiJ9"
     }
   }
   ```
   - `path`: 文件路径（必需）
   - `content`: Base64 编码的文件内容（必需）
   - 可以带 `base64:` 前缀，也可以不带

3. **LfsFile**（LFS 文件，已上传到 CAS）：
   ```json
   {
     "key": "lfsFile",
     "value": {
       "path": "model.safetensors",
       "oid": "sha256:abc123...",
       "size": 104857600
     }
   }
   ```
   - `path`: 文件路径（必需）
   - `oid`: LFS 对象 ID（SHA256 哈希，必需）
   - `size`: 文件大小（字节，必需）

4. **DeletedEntry**（删除文件）：
   ```json
   {
     "key": "deletedEntry",
     "value": {
       "path": "old-file.txt"
     }
   }
   ```
   - `path`: 要删除的文件路径（必需）

**响应**：
```json
{
  "commitOid": "abc123...",
  "commitUrl": "http://localhost:8080/models/my-org/my-model/commit/abc123...",
  "prUrl": null,
  "prNum": null
}
```

**示例**：
```bash
# 提交内联文件
cat <<EOF | curl -X POST "http://localhost:8080/api/models/my-org/my-model/commit/main" \
  -H "Authorization: Bearer hf_xxx" \
  -H "Content-Type: application/x-ndjson" \
  --data-binary @-
{"key":"header","value":{"summary":"Update config"}}
{"key":"file","value":{"path":"config.json","content":"eyJtb2RlbF90eXBlIjoicXdlbiJ9"}}
EOF

# 提交 LFS 文件（需要先上传到 CAS）
cat <<EOF | curl -X POST "http://localhost:8080/api/models/my-org/my-model/commit/main" \
  -H "Authorization: Bearer hf_xxx" \
  -H "Content-Type: application/x-ndjson" \
  --data-binary @-
{"key":"header","value":{"summary":"Add model"}}
{"key":"lfsFile","value":{"path":"model.safetensors","oid":"sha256:abc123...","size":104857600}}
EOF

# 删除文件
cat <<EOF | curl -X POST "http://localhost:8080/api/models/my-org/my-model/commit/main" \
  -H "Authorization: Bearer hf_xxx" \
  -H "Content-Type: application/x-ndjson" \
  --data-binary @-
{"key":"header","value":{"summary":"Remove old file"}}
{"key":"deletedEntry","value":{"path":"old-file.txt"}}
EOF
```

### 列出文件树

**端点**：`GET /api/models/{ns}/{repo}/tree/{rev}`

**路径参数**：
- `ns` (string): 命名空间
- `repo` (string): 仓库名称
- `rev` (string): 修订版本

**查询参数**：
- `path` (optional): 子目录路径
- `recursive` (optional): 是否递归列出，默认 `false`

**响应**：
```json
[
  {
    "type": "file",
    "oid": "abc123...",
    "size": 1234,
    "path": "config.json",
    "lastCommit": "abc123...",
    "lastModified": "2026-06-12T10:00:00Z"
  },
  {
    "type": "file",
    "oid": "def456...",
    "size": 104857600,
    "path": "model.safetensors",
    "lastCommit": "abc123...",
    "lastModified": "2026-06-12T10:00:00Z",
    "lfs": {
      "oid": "def456...",
      "size": 104857600,
      "pointerSize": 134
    }
  },
  {
    "type": "directory",
    "path": "tokenizer",
    "oid": "ghi789..."
  }
]
```

**示例**：
```bash
# 列出根目录
curl "http://localhost:8080/api/models/my-org/my-model/tree/main" \
  -H "Authorization: Bearer hf_xxx"

# 列出子目录
curl "http://localhost:8080/api/models/my-org/my-model/tree/main?path=tokenizer" \
  -H "Authorization: Bearer hf_xxx"

# 递归列出
curl "http://localhost:8080/api/models/my-org/my-model/tree/main?recursive=true" \
  -H "Authorization: Bearer hf_xxx"
```

### 下载文件 (Resolve API)

**端点**：`GET /{type}/{ns}/{repo}/resolve/{rev}/{path}`

**路径参数**：
- `type` (string): 仓库类型 - `models`, `datasets`, `spaces`
- `ns` (string): 命名空间
- `repo` (string): 仓库名称
- `rev` (string): 修订版本
- `path` (string): 文件路径

**请求头**：
```
Authorization: Bearer hf_xxx
```

**响应**：
- `200 OK`: 返回文件内容
- `302 Found`: 重定向到 CDN（大文件）
- `404 Not Found`: 文件不存在

**示例**：
```bash
# 下载模型文件
curl -o model.safetensors \
  "http://localhost:8080/models/my-org/my-model/resolve/main/model.safetensors" \
  -H "Authorization: Bearer hf_xxx"

# 下载配置文件
curl -o config.json \
  "http://localhost:8080/models/my-org/my-model/resolve/main/config.json" \
  -H "Authorization: Bearer hf_xxx"
```

---

## 令牌交换 API

令牌交换 API 用于将 Hub 令牌 (hf_xxx) 转换为 CAS 令牌 (xet_xxx)。

### 获取读令牌

**端点**：`GET /api/models/{ns}/{repo}/xet-read-token/{rev}`

**路径参数**：
- `ns` (string): 命名空间
- `repo` (string): 仓库名称
- `rev` (string): 修订版本

**请求头**：
```
Authorization: Bearer hf_xxx
```

**响应**：
```json
{
  "accessToken": "xet_eyJhbGciOiJFZDI1NTE5Iiwia2lkIjoiaHViLWtleS0xIiwidHlwIjoiSldUIn0...",
  "exp": 1718320300,
  "casUrl": "http://localhost:8081"
}
```

**字段说明**：
- `accessToken`: CAS 访问令牌（`xet_xxx` 格式）
- `exp`: 令牌过期时间（Unix 时间戳，秒）
- `casUrl`: CAS 服务器 URL

**说明**：
- 读令牌具有 `read` 作用域
- 写令牌具有 `write` 作用域
- 令牌有效期由 Hub 的 `HUB_TOKEN_TTL_SECONDS` 配置决定（默认 3600 秒）

**示例**：
```bash
curl "http://localhost:8080/api/models/my-org/my-model/xet-read-token/main" \
  -H "Authorization: Bearer hf_xxx"
```

### 获取写令牌

**端点**：`GET /api/models/{ns}/{repo}/xet-write-token/{rev}`

**响应**：同读令牌，但 `scope` 为 `"write"`

**示例**：
```bash
curl "http://localhost:8080/api/models/my-org/my-model/xet-write-token/main" \
  -H "Authorization: Bearer hf_xxx"
```

### 数据集和 Space 令牌

数据集和 Space 使用相同的端点模式：

```bash
# 数据集读令牌
GET /api/datasets/{ns}/{repo}/xet-read-token/{rev}

# 数据集写令牌
GET /api/datasets/{ns}/{repo}/xet-write-token/{rev}

# Space 读令牌
GET /api/spaces/{ns}/{repo}/xet-read-token/{rev}

# Space 写令牌
GET /api/spaces/{ns}/{repo}/xet-write-token/{rev}
```

---

## 预上传 API

预上传 API 用于在实际上传前检查文件状态（是否已存在）。

### 预上传检查

**端点**：`POST /api/models/{ns}/{repo}/preupload/{rev}`

**请求头**：
```
Authorization: Bearer hf_xxx
Content-Type: application/json
```

**请求体**：
```json
{
  "files": [
    {
      "path": "model.safetensors",
      "size": 104857600
    },
    {
      "path": "config.json",
      "size": 1234
    }
  ]
}
```

**字段说明**：
- `path`: 文件路径（必需）
- `size`: 文件大小，字节（必需）

**响应**：
```json
{
  "files": [
    {
      "path": "model.safetensors",
      "uploadMode": "lfs",
      "shouldIgnore": false
    },
    {
      "path": "config.json",
      "uploadMode": "regular",
      "shouldIgnore": false
    }
  ]
}
```

**字段说明**：
- `path`: 文件路径
- `uploadMode`: 上传模式
  - `regular`: 小文件，通过 Commit API 内联上传
  - `lfs`: 大文件，需要通过 LFS 协议上传
- `shouldIgnore`: 是否应忽略此文件（当前实现始终为 `false`）

**示例**：
```bash
curl -X POST "http://localhost:8080/api/models/my-org/my-model/preupload/main" \
  -H "Authorization: Bearer hf_xxx" \
  -H "Content-Type: application/json" \
  -d '{
    "files": [
      {"path": "model.safetensors", "size": 104857600},
      {"path": "config.json", "size": 1234}
    ]
  }'
```

---

## LFS 代理 API

LFS 代理 API 将 Git LFS 请求代理到 CAS Server。支持标准 LFS 端点和 Git-style LFS 端点。

### 标准 LFS 端点

**端点**：
- `POST /objects/batch` - 批量操作
- `POST /lfs/objects/batch` - 批量操作（别名）
- `PUT /lfs/objects/{oid}` - 上传对象
- `GET /lfs/objects/{oid}` - 下载对象

### Git-style LFS 端点（Git LFS Smart HTTP）

用于 Git LFS Smart HTTP 协议的端点，支持通过 `.git/info/lfs` 路径访问：

**模型仓库**：
- `POST /models/{ns}/{repo}.git/info/lfs/objects/batch`
- `PUT /models/{ns}/{repo}.git/info/lfs/objects/{oid}`
- `GET /models/{ns}/{repo}.git/info/lfs/objects/{oid}`

**数据集仓库**：
- `POST /datasets/{ns}/{repo}.git/info/lfs/objects/batch`
- `PUT /datasets/{ns}/{repo}.git/info/lfs/objects/{oid}`
- `GET /datasets/{ns}/{repo}.git/info/lfs/objects/{oid}`

**Space 仓库**：
- `POST /spaces/{ns}/{repo}.git/info/lfs/objects/batch`
- `PUT /spaces/{ns}/{repo}.git/info/lfs/objects/{oid}`
- `GET /spaces/{ns}/{repo}.git/info/lfs/objects/{oid}`

**通用格式**（自动检测仓库类型）：
- `POST /{ns}/{repo}.git/info/lfs/objects/batch`
- `PUT /{ns}/{repo}.git/info/lfs/objects/{oid}`
- `GET /{ns}/{repo}.git/info/lfs/objects/{oid}`

**Git LFS 配置示例**：
```bash
# 在 Git 仓库中配置 LFS 指向 Hub API
git config lfs.url http://localhost:8080/my-org/my-model.git/info/lfs

# 或使用 .lfsconfig 文件
cat > .lfsconfig << EOF
[lfs]
    url = http://localhost:8080/my-org/my-model.git/info/lfs
EOF
```

### LFS 批量操作

**端点**：`POST /objects/batch`

**请求头**：
```
Authorization: Bearer hf_xxx
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
    }
  ]
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
          },
          "expires_in": 300
        }
      }
    }
  ]
}
```

**工作原理**：
1. Hub 验证用户令牌 (hf_xxx)
2. Hub 签发短期 CAS 令牌 (xet_xxx)
3. Hub 返回带有 CAS 令牌的 action URLs
4. 客户端直接使用 CAS 令牌访问 CAS Server

### LFS 对象下载/上传

**端点**：`GET/PUT /lfs/objects/{oid}`

Hub 代理 LFS 对象请求到 CAS Server。

**请求头**：
```
Authorization: Bearer hf_xxx
```

**响应**：
- `200 OK`: 返回对象数据
- `302 Found`: 重定向到 CAS Server

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
| 401 | `authentication_error` | 认证失败 |
| 403 | `authorization_error` | 权限不足 |
| 404 | `not_found` | 资源不存在 |
| 409 | `conflict` | 资源冲突（已存在） |
| 413 | `payload_too_large` | 请求体过大 |
| 422 | `unprocessable_entity` | 无法处理的实体 |
| 500 | `internal_error` | 服务器内部错误 |

---

## 数据格式

### 仓库类型

| 类型 | 描述 | 用途 |
|------|------|------|
| `model` | 机器学习模型 | 存储模型权重、配置 |
| `dataset` | 数据集 | 存储训练/测试数据 |
| `space` | 应用空间 | 存储 Gradio/Streamlit 应用 |

### 文件模式

| 模式 | 描述 |
|------|------|
| `100644` | 普通文件 |
| `100755` | 可执行文件 |
| `120000` | 符号链接 |

### 大文件处理

**阈值**（Hub 端两分类）：
- ≤ `HUB_INLINE_THRESHOLD`（默认 1MB）: 内联在 commit 中（regular 模式）
- > `HUB_INLINE_THRESHOLD`: 通过 LFS 协议上传（lfs 模式）

> **说明**：xorb/shard 格式转换是 CAS 服务端的异步后处理步骤，由转换管道自动完成，对客户端透明。Hub 端仅负责 two-way 分类（内联 vs LFS）。

**LFS 指针文件**：
```
version https://git-lfs.github.com/spec/v1
oid sha256:abc123...
size 104857600
```

---

## 完整工作流示例

### 1. 创建仓库并上传文件

```bash
#!/bin/bash
set -e

HF_ENDPOINT="http://localhost:8080"
HF_TOKEN="hf_xxx"

# 1. 创建仓库
curl -X POST "$HF_ENDPOINT/api/repos/create" \
  -H "Authorization: Bearer $HF_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "type": "model",
    "name": "my-model",
    "namespace": "my-org"
  }'

# 2. 提交文件 (NDJSON)
cat <<EOF | curl -X POST "$HF_ENDPOINT/api/models/my-org/my-model/commit/main" \
  -H "Authorization: Bearer $HF_TOKEN" \
  -H "Content-Type: application/x-ndjson" \
  --data-binary @-
{"key":"header","value":{"summary":"Add config"}}
{"key":"file","value":{"path":"config.json","content":"eyJtb2RlbF90eXBlIjoicXdlbiJ9"}}
EOF

# 3. 列出文件
curl "$HF_ENDPOINT/api/models/my-org/my-model/tree/main" \
  -H "Authorization: Bearer $HF_TOKEN"

# 4. 下载文件
curl -o config.json \
  "$HF_ENDPOINT/models/my-org/my-model/resolve/main/config.json" \
  -H "Authorization: Bearer $HF_TOKEN"
```

### 2. 使用 hf CLI

```bash
# 配置环境变量
export HF_ENDPOINT=http://localhost:8080
export HF_TOKEN=hf_xxx

# 创建仓库
hf repo create my-org/my-model --type model

# 上传文件
hf upload my-org/my-model ./config.json config.json

# 下载文件
hf download my-org/my-model config.json --local-dir ./downloaded

# 列出文件
hf repo info my-org/my-model
```

---

## 内部 API

内部 API 用于 CAS Server 与 Hub Server 之间的通信，需要 `internal` 作用域。

### 获取引用的哈希列表

**端点**：`GET /internal/referenced-hashes`

**请求头**：
```
Authorization: Bearer hf_xxx (需要 internal scope)
```

**响应**：
```json
{
  "hashes": [
    "abc123...",
    "def456..."
  ],
  "count": 2
}
```

**字段说明**：
- `hashes`: 所有被仓库引用的 blob SHA-256 哈希列表
- `count`: 哈希数量

**用途**：
- 供 CAS Server 的垃圾回收（GC）功能使用
- GC 通过此端点获取所有被引用的 blob，然后删除未引用的孤立 blob

**示例**：
```bash
curl "http://localhost:8080/internal/referenced-hashes" \
  -H "Authorization: Bearer hf_xxx"
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

**用途**：
- 用于负载均衡器和监控系统检查服务健康状态
- 无需认证

**示例**：
```bash
curl "http://localhost:8080/health"
```

---

## 性能考虑

### 大文件上传

1. **使用 NDJSON 流式上传**：避免将整个文件加载到内存
2. **并行上传多个文件**：同时提交多个文件
3. **使用预上传检查**：避免重复上传已存在的文件

### 大文件下载

1. **使用 Resolve API**：直接下载，无需额外步骤
2. **缓存 CAS 令牌**：减少令牌交换次数
3. **并行下载**：同时下载多个文件

### 批量操作

1. **使用 LFS Batch API**：批量操作多个文件
2. **减少 API 调用**：合并多个操作到一个请求

---

## 相关文档

- [Authentication](authentication.md) - 认证机制详细说明
- [CAS API Reference](cas-api.md) - CAS 服务器 API 文档
- [Configuration Guide](../configuration.md) - 配置选项
- [Architecture](../architecture.md) - 系统架构
