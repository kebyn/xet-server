# 认证文档

Xet Server 使用**双层认证系统**：Hub API 使用基于 SQLite 的令牌存储，CAS 使用 Ed25519 签名的 JWT。

## 概述

### 双层认证架构

```
┌─────────────────────────────────────────────────────────┐
│                      客户端                              │
│              (hf CLI, git lfs, custom)                  │
└──────────┬──────────────────────────────────┬───────────┘
           │                                  │
           │ 1. Hub Token (hf_xxx)           │ 4. CAS Token (xet_xxx)
           │    不透明令牌（UUID）             │    Ed25519 JWT
           │                                  │
           ▼                                  ▼
┌──────────────────────┐          ┌──────────────────────┐
│    Hub API Server    │          │    CAS Server        │
│    (端口 8080)        │          │    (端口 8080/8081)   │
│                      │ 2. 签发   │                      │
│  验证 Hub Token      │───────▶  │  验证 CAS Token      │
│  (SQLite 查询)       │          │  (Ed25519 签名验证)   │
│  管理仓库/用户       │◀───────  │  管理存储/数据        │
└──────────────────────┘ 3. 返回   └──────────────────────┘
                        CAS Token
```

## 令牌类型

### Hub Tokens (`hf_xxx`)

**用途**：访问 Hub API 的用户令牌

**格式**：`hf_{uuid_without_dashes}`

**特点**：
- 不透明令牌（不是 JWT）
- 永久有效（无过期时间）
- 存储在 SQLite 数据库中
- 通过 SHA256 哈希验证
- 由管理员通过 `create-token` 命令创建

**生成方式**：
```bash
./target/release/hub-api create-token \
  --username admin \
  --name "admin-token" \
  --scope "read write" \
  --db hub.db
```

**输出示例**：
```
Token created successfully!
Username: admin
Scope: read write
Token name: admin-token
Token (keep this secret): hf_a1b2c3d4e5f678901234567890123456
```

**验证机制**：
1. 客户端发送 `Authorization: Bearer hf_xxx`
2. Hub 计算 token 的 SHA256 哈希
3. 在 SQLite `tokens` 表中查找匹配的 `token_hash`
4. 验证 scope 是否匹配所需权限

**示例**：
```
hf_a1b2c3d4e5f678901234567890123456
```

### CAS Tokens (`xet_xxx`)

**用途**：访问 CAS 服务器的短期令牌

**格式**：`xet_{base64url(header)}.{base64url(payload)}.{base64url(signature)}`

**特点**：
- Ed25519 签名的 JWT
- 短期有效（默认 1 小时）
- 由 Hub 使用私钥签发
- CAS 使用公钥验证签名
- 绑定到特定仓库和修订版本

**JWT Header**：
```json
{
  "alg": "EdDSA",
  "typ": "JWT",
  "kid": "hub-key-1"
}
```

**JWT Payload**：
```json
{
  "sub": "admin",
  "scope": "read",
  "repo_id": "my-org/my-model",
  "repo_type": "model",
  "revision": "main",
  "exp": 1718320000,
  "iat": 1718316400,
  "kid": "hub-key-1",
  "token_type": "user"
}
```

**签发流程**：
1. 客户端请求 CAS 令牌：`GET /api/models/{ns}/{repo}/xet-read-token/{rev}`
2. Hub 验证用户的 Hub Token
3. Hub 使用私钥签发 CAS JWT
4. 返回 `xet_xxx` 令牌给客户端

**示例**：
```
xet_eyJhbGciOiJFZDI1NTE5Iiwia2lkIjoiaHViLWtleS0xIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhZG1pbiIsInNjb3BlIjoicmVhZCIsInJlcG9faWQiOiJteS1vcmcvbXktbW9kZWwiLCJyZXBvX3R5cGUiOiJtb2RlbCIsInJldmlzaW9uIjoibWFpbiIsImV4cCI6MTcxODMyMDAwMCwiaWF0IjoxNzE4MzE2NDAwLCJraWQiOiJodWIta2V5LTEiLCJ0b2tlbl90eXBlIjoidXNlciJ9.signature
```

### Proxy Tokens (`proxy_xxx`)

**用途**：LFS 代理操作的超短期令牌

**格式**：`proxy_{base64url(header)}.{base64url(payload)}.{base64url(signature)}`

**特点**：
- Ed25519 签名的 JWT
- 超短期有效（固定 5 分钟）
- 绑定到特定 LFS 对象 ID (`oid`)
- 绑定到特定操作 (`upload` 或 `download`)
- scope 为 `lfs-upload` 或 `lfs-download`

**JWT Payload**：
```json
{
  "sub": "admin",
  "scope": "lfs-upload",
  "repo_id": "my-org/my-model",
  "repo_type": "model",
  "revision": "",
  "exp": 1718316700,
  "iat": 1718316400,
  "kid": "hub-key-1",
  "token_type": "proxy",
  "oid": "abc123...",
  "operation": "upload"
}
```

**使用场景**：
- Hub API 代理 LFS 请求到 CAS 时
- 确保令牌只能用于特定的 LFS 对象和操作

## 令牌作用域

### Hub Token 作用域

| 作用域 | 描述 | 权限 |
|--------|------|------|
| `read` | 读取权限 | 下载文件、列出仓库、获取元数据 |
| `write` | 写入权限 | 上传文件、创建仓库、提交更改 |
| `read write` | 读写权限 | 同时具有 read 和 write 权限 |

### CAS Token 作用域

| 作用域 | 描述 | 权限 |
|--------|------|------|
| `read` | 读取权限 | 下载 xorbs、shards、重构信息 |
| `write` | 写入权限 | 上传 xorbs、shards |
| `internal` | 内部权限 | Hub → CAS 通信（超级权限） |
| `lfs-upload` | LFS 上传 | 上传 LFS 对象（proxy token） |
| `lfs-download` | LFS 下载 | 下载 LFS 对象（proxy token） |

**注意**：`internal` 作用域自动包含 `read` 和 `write` 权限。

## 认证流程

### 流程 1：Hub API 认证

```
1. 客户端 → Hub API
   GET /api/models/my-org/my-model/tree/main
   Authorization: Bearer hf_a1b2c3d4...

2. Hub API 验证令牌
   - 计算 token_hash = SHA256("hf_a1b2c3d4...")
   - 查询 SQLite: SELECT * FROM tokens WHERE token_hash = ?
   - 检查 scope 是否包含所需权限
   - 检查令牌是否被撤销

3. Hub API 处理请求
   - 查询元数据数据库
   - 返回文件树
```

### 流程 2：CAS 令牌交换

```
1. 客户端 → Hub API
   GET /api/models/my-org/my-model/xet-read-token/main
   Authorization: Bearer hf_a1b2c3d4...

2. Hub API 验证 Hub Token
   - SHA256 哈希查询 SQLite
   - 验证 scope

3. Hub API 签发 CAS Token
   - 创建 JWT claims (sub, scope, repo_id, repo_type, revision, exp, iat)
   - 使用 Ed25519 私钥签名
   - 返回 xet_xxx 令牌

4. 客户端 → CAS Server
   GET /v1/reconstructions/file123
   Authorization: Bearer xet_eyJhbGci...

5. CAS Server 验证令牌
   - 解析 JWT
   - 使用 Hub 公钥验证 Ed25519 签名
   - 检查 exp 是否过期
   - 检查 kid 是否在受信任列表中
   - 检查 scope 是否匹配

6. CAS Server 处理请求
   - 返回文件重构信息
```

### 流程 3：LFS 代理认证

```
1. 客户端 → Hub API
   POST /objects/batch (LFS Batch API)
   Authorization: Bearer hf_a1b2c3d4...

2. Hub API 验证 Hub Token

3. Hub API 签发 Proxy Token
   - 创建 proxy JWT claims (绑定到 oid 和 operation)
   - 使用 Ed25519 私钥签名
   - 返回 proxy_xxx 令牌

4. 客户端 → CAS Server
   PUT /lfs/objects/{oid}
   Authorization: Bearer proxy_eyJhbGci...

5. CAS Server 验证 Proxy Token
   - 验证 Ed25519 签名
   - 检查 oid 是否匹配
   - 检查 operation 是否匹配
   - 检查 scope 是否为 lfs-upload

6. CAS Server 处理上传
```

## 密钥管理

### 生成 Ed25519 密钥对

Hub 需要 Ed25519 密钥对来签发和验证 CAS tokens：

```bash
# 生成私钥（Hub 使用）
openssl genpkey -algorithm Ed25519 -out private_key.pem

# 从私钥提取公钥（CAS 使用）
openssl pkey -in private_key.pem -pubout -out public_key.pem
```

### 配置密钥

**Hub API（签发端）**：
```bash
export HUB_PRIVATE_KEY_PATH=/path/to/private_key.pem
export HUB_KID=hub-key-1
```

**CAS Server（验证端）**：
```bash
export CAS_PUBLIC_KEY_PATH=/path/to/public_key.pem
export CAS_TRUSTED_KIDS=hub-key-1,backup-key-1
```

### 密钥轮换

1. 生成新密钥对
2. 在 CAS 中添加新公钥：`CAS_TRUSTED_KIDS=old-key,new-key`
3. 在 Hub 中切换到新密钥：`HUB_KID=new-key`
4. 等待所有旧令牌过期（默认 1 小时）
5. 从 CAS 中移除旧公钥

## 数据库结构

### Hub Token 存储

```sql
-- 用户表
CREATE TABLE users (
    user_id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

-- 令牌表
CREATE TABLE tokens (
    token_hash TEXT PRIMARY KEY,  -- SHA256(hf_xxx)
    user_id TEXT NOT NULL,
    name TEXT NOT NULL,
    scope TEXT NOT NULL,          -- "read", "write", "read write"
    created_at INTEGER NOT NULL,
    expires_at INTEGER,           -- NULL = 永不过期
    revoked_at INTEGER,           -- NULL = 未撤销
    FOREIGN KEY (user_id) REFERENCES users(user_id)
);
```

### 令牌创建流程

```rust
// hub/src/auth/token_store.rs:60-76
pub fn create_token(&self, username: &str, token_name: &str, scope: &str) -> Result<String> {
    // 生成 hf_ + UUID（不带连字符）
    let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    
    // 计算 SHA256 哈希
    let token_hash = Self::hash_token(&token);
    
    // 插入数据库
    conn.execute(
        "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![token_hash, user_id, token_name, scope, now],
    )?;
    
    Ok(token)  // 返回明文令牌（仅显示一次）
}
```

## 安全考虑

### 1. Hub Token 安全

**存储**：
- 客户端：使用安全的密钥存储（如 `keyring`）
- 服务器：SQLite 文件权限设置为 600
- 不要将令牌硬编码到代码中

**传输**：
- 生产环境必须使用 HTTPS
- 防止令牌在网络中被截获

**撤销**：
- 可以通过设置 `revoked_at` 字段撤销令牌
- 撤销立即生效

### 2. CAS Token 安全

**私钥保护**：
- 私钥文件权限：`chmod 600 private_key.pem`
- 不要将私钥提交到版本控制
- 定期轮换密钥

**短期有效**：
- CAS tokens 默认 1 小时有效
- Proxy tokens 固定 5 分钟有效
- 减少令牌泄露的风险

**签名验证**：
- CAS 使用 Ed25519 公钥验证签名
- 防止令牌篡改
- 确保令牌由受信任的 Hub 签发

### 3. HTTPS/TLS

**生产环境必须启用 HTTPS**：

```bash
# 使用反向代理（推荐）
nginx/caddy → Hub API (HTTP :8080)
nginx/caddy → CAS Server (HTTP :8080/8081)
```

### 4. 网络安全

**防火墙规则**：
- Hub API (8080): 公开访问
- CAS Server (8080/8081): 限制访问（仅 Hub 和授权客户端）
- Internal API: 仅 Hub 可访问

## 错误处理

### 常见错误

| HTTP 状态码 | 错误类型 | 描述 |
|------------|----------|------|
| 401 | `missing_auth` | 缺少 Authorization header |
| 401 | `invalid_token` | Hub token 哈希未找到 |
| 401 | `expired_token` | CAS token 已过期 |
| 401 | `invalid_signature` | CAS token 签名验证失败 |
| 401 | `unknown_kid` | CAS token 的 kid 不受信任 |
| 403 | `insufficient_scope` | 令牌 scope 不足 |

### 错误响应示例

```json
{
  "error": {
    "type": "authentication_error",
    "message": "Invalid token",
    "code": "invalid_token"
  }
}
```

## API 参考

### Hub Token 创建

```bash
./target/release/hub-api create-token \
  --username admin \
  --name "my-token" \
  --scope "read write" \
  --db hub.db
```

**参数**：
- `--username` / `-u`: 用户名（默认: admin）
- `--scope` / `-s`: 作用域（默认: write）
- `--name` / `-n`: 令牌名称（默认: default-token）
- `--db` / `-d`: 数据库路径（默认: hub.db）

### CAS 令牌交换

```bash
# 获取读令牌
curl "$HF_ENDPOINT/api/models/my-org/my-model/xet-read-token/main" \
  -H "Authorization: Bearer hf_a1b2c3d4..."

# 获取写令牌
curl "$HF_ENDPOINT/api/models/my-org/my-model/xet-write-token/main" \
  -H "Authorization: Bearer hf_a1b2c3d4..."
```

## 故障排除

### 问题 1：Hub Token 验证失败

**症状**：收到 401 错误（`invalid_token`）

**排查步骤**：
1. 检查令牌格式是否正确（`hf_` 前缀 + 32 个十六进制字符）
2. 确认令牌未被撤销
3. 验证数据库路径是否正确
4. 检查 SQLite 文件权限

### 问题 2：CAS Token 签名验证失败

**症状**：收到 401 错误（`invalid_signature`）

**排查步骤**：
1. 检查 CAS 的公钥是否与 Hub 的私钥匹配
2. 验证 `kid` 是否在 `CAS_TRUSTED_KIDS` 列表中
3. 检查密钥文件路径是否正确
4. 确认密钥文件格式（PEM）

### 问题 3：权限不足

**症状**：收到 403 错误（`insufficient_scope`）

**排查步骤**：
1. 检查 Hub Token 的 scope 是否包含所需权限
2. 检查 CAS Token 的 scope 是否匹配操作
3. 对于 proxy token，检查 oid 和 operation 是否匹配

## 相关文档

- [CAS API Reference](cas-api.md) - CAS 服务器 API 详细文档
- [Hub API Reference](hub-api.md) - Hub API 详细文档
- [Configuration Guide](../configuration.md) - 配置选项说明
