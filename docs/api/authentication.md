# 认证文档

Xet Server 使用基于 **Ed25519** 的 JWT（JSON Web Token）认证系统，提供安全、灵活的访问控制。

## 概述

### 两层认证架构

```
┌─────────────────────────────────────────────────────────┐
│                      客户端                              │
│              (hf CLI, git lfs, custom)                  │
└──────────┬──────────────────────────────────┬───────────┘
           │                                  │
           │ 1. Hub Token (hf_xxx)           │ 4. CAS Token (xet_xxx)
           │    长期令牌                       │    短期令牌
           │                                  │
           ▼                                  ▼
┌──────────────────────┐          ┌──────────────────────┐
│    Hub API Server    │          │    CAS Server        │
│    (端口 8080)        │          │    (端口 8081)        │
│                      │ 2. 签发   │                      │
│  验证 Hub Token      │───────▶  │  验证 CAS Token      │
│  管理仓库/用户       │          │  管理存储/数据        │
│                      │◀───────  │                      │
└──────────────────────┘ 3. 返回   └──────────────────────┘
                        CAS Token
```

## 令牌类型

### Hub Tokens (`hf_xxx`)

**用途**：访问 Hub API 的长期令牌

**格式**：`hf_{header}.{payload}.{signature}`

**特点**：
- 长期有效（默认 1 小时，可配置）
- 用于用户身份认证
- 管理仓库、提交文件等操作

**示例**：
```
hf_eyJhbGciOiJFZDI1NTE5Iiwia2lkIjoiaHViLWtleS0xIiwidHlwIjoiSldUIn0.eyJleHAiOjE3MTgzMjAwMDAsImlhdCI6MTcxODMxNjQwMCwic3ViIjoiYWRtaW4iLCJzY29wZSI6InJlYWQgd3JpdGUiLCJyZXBvX2lkIjoibXktb3JnL215LW1vZGVsIiwicmVwb190eXBlIjoibW9kZWwiLCJyZXZpc2lvbiI6Im1haW4iLCJraWQiOiJodWIta2V5LTEifQ.signature
```

### CAS Tokens (`xet_xxx`)

**用途**：访问 CAS 服务器的短期令牌

**格式**：`xet_{header}.{payload}.{signature}`

**特点**：
- 短期有效（默认 5 分钟）
- 由 Hub 签发，CAS 验证
- 绑定到特定仓库和修订版本

**示例**：
```
xet_eyJhbGciOiJFZDI1NTE5Iiwia2lkIjoiaHViLWtleS0xIiwidHlwIjoiSldUIn0.eyJleHAiOjE3MTgzMTY3MDAsImlhdCI6MTcxODMxNjQwMCwic3ViIjoiYWRtaW4iLCJzY29wZSI6InJlYWQiLCJyZXBvX2lkIjoibXktb3JnL215LW1vZGVsIiwicmVwb190eXBlIjoibW9kZWwiLCJyZXZpc2lvbiI6Im1haW4iLCJraWQiOiJodWIta2V5LTEiLCJ0b2tlbl90eXBlIjoiY2FzIn0.signature
```

### Proxy Tokens (特殊 CAS Token)

**用途**：LFS 代理操作的超短期令牌

**特点**：
- 超短期有效（5 分钟）
- 绑定到特定 LFS 对象 ID
- 绑定到特定操作（upload/download）

**JWT Claims**：
```json
{
  "sub": "admin",
  "scope": "write",
  "repo_id": "my-org/my-model",
  "repo_type": "model",
  "revision": "main",
  "exp": 1718316700,
  "iat": 1718316400,
  "kid": "hub-key-1",
  "token_type": "proxy",
  "oid": "abc123...",
  "operation": "upload"
}
```

## 令牌作用域

| 作用域 | 描述 | 权限 |
|--------|------|------|
| `read` | 读取权限 | 下载文件、列出仓库、获取元数据 |
| `write` | 写入权限 | 上传文件、创建仓库、提交更改 |
| `internal` | 内部权限 | Hub → CAS 通信（超级权限） |

**注意**：`internal` 作用域自动包含 `read` 和 `write` 权限。

## JWT 结构

### Header

```json
{
  "alg": "EdDSA",
  "typ": "JWT",
  "kid": "hub-key-1"
}
```

**字段说明**：
- `alg`: 签名算法（固定为 `EdDSA`）
- `typ`: 令牌类型（固定为 `JWT`）
- `kid`: 密钥标识符（用于密钥轮换）

### Payload (Claims)

```json
{
  "sub": "admin",
  "scope": "read write",
  "repo_id": "my-org/my-model",
  "repo_type": "model",
  "revision": "main",
  "exp": 1718320000,
  "iat": 1718316400,
  "kid": "hub-key-1",
  "token_type": "user"
}
```

**字段说明**：
- `sub`: 用户标识（主题）
- `scope`: 授权作用域（空格分隔）
- `repo_id`: 仓库 ID（`namespace/repo` 格式）
- `repo_type`: 仓库类型（`model`、`dataset`、`space`）
- `revision`: Git 修订版本（分支/标签）
- `exp`: 过期时间（Unix 时间戳）
- `iat`: 签发时间（Unix 时间戳）
- `kid`: 密钥标识符
- `token_type`: 令牌类型（`user`、`cas`、`proxy`）

## 认证流程

### 流程 1：Hub API 认证

```
1. 客户端 → Hub API
   GET /api/models/my-org/my-model/tree/main
   Authorization: Bearer hf_xxx

2. Hub API 验证令牌
   - 解析 JWT
   - 验证签名（使用私钥）
   - 检查过期时间
   - 检查作用域

3. Hub API 处理请求
   - 查询元数据数据库
   - 返回文件树
```

### 流程 2：CAS 令牌交换

```
1. 客户端 → Hub API
   GET /api/models/my-org/my-model/xet-read-token/main
   Authorization: Bearer hf_xxx

2. Hub API 签发 CAS 令牌
   - 验证 Hub 令牌
   - 创建 CAS claims
   - 使用私钥签名
   - 返回 xet_xxx 令牌

3. 客户端 → CAS Server
   GET /v1/reconstructions/file123
   Authorization: Bearer xet_xxx

4. CAS Server 验证令牌
   - 解析 JWT
   - 验证签名（使用 Hub 公钥）
   - 检查过期时间
   - 检查作用域
   - 检查 kid 是否受信任

5. CAS Server 处理请求
   - 返回文件重构信息
```

### 流程 3：LFS 代理认证

```
1. 客户端 → Hub API
   POST /objects/batch (LFS Batch API)
   Authorization: Bearer hf_xxx

2. Hub API 签发代理令牌
   - 验证 Hub 令牌
   - 创建 proxy claims（绑定到 oid 和 operation）
   - 返回带有代理令牌的 action URLs

3. 客户端 → CAS Server
   PUT /lfs/objects/{oid}
   Authorization: Bearer xet_xxx (proxy token)

4. CAS Server 验证令牌
   - 验证签名
   - 检查 oid 是否匹配
   - 检查 operation 是否匹配
   - 处理上传
```

## 密钥管理

### 生成密钥对

```bash
# 生成 Ed25519 私钥
openssl genpkey -algorithm Ed25519 -out private_key.pem

# 从私钥提取公钥
openssl pkey -in private_key.pem -pubout -out public_key.pem
```

### 密钥配置

**Hub API（签名端）**：
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
4. 等待旧令牌过期
5. 从 CAS 中移除旧公钥

## 安全考虑

### 1. 令牌存储

**客户端**：
- 使用安全的密钥存储（如 `keyring`）
- 不要将令牌硬编码到代码中
- 使用环境变量或配置文件

**服务器**：
- 私钥文件权限：`chmod 600 private_key.pem`
- 不要将私钥提交到版本控制
- 定期轮换密钥

### 2. 令牌有效期

**建议配置**：
- Hub Tokens：1-24 小时（根据使用场景）
- CAS Tokens：5-15 分钟
- Proxy Tokens：5 分钟（固定）

**权衡**：
- 较短的有效期 → 更高的安全性，更频繁的令牌交换
- 较长的有效期 → 更好的性能，更大的泄露风险

### 3. 作用域最小化

**原则**：只授予必要的最小权限

**示例**：
```bash
# 只读用户
scope="read"

# 上传用户
scope="write"

# 管理员（谨慎使用）
scope="internal"
```

### 4. HTTPS/TLS

**生产环境必须启用 HTTPS**：

```bash
# 使用反向代理（推荐）
nginx/caddy → Hub API (HTTP)
nginx/caddy → CAS Server (HTTP)

# 或直接配置 TLS
export HUB_TLS_CERT_PATH=/path/to/cert.pem
export HUB_TLS_KEY_PATH=/path/to/key.pem
```

## 错误处理

### 常见错误

| HTTP 状态码 | 错误类型 | 描述 |
|------------|----------|------|
| 401 | `InvalidToken` | 令牌格式无效 |
| 401 | `Expired` | 令牌已过期 |
| 401 | `InvalidSignature` | 签名验证失败 |
| 401 | `UnknownKid` | 密钥 ID 不受信任 |
| 403 | `InsufficientScope` | 权限不足 |

### 错误响应示例

```json
{
  "error": {
    "type": "authentication_error",
    "message": "Token has expired",
    "code": "expired"
  }
}
```

### 重试策略

**令牌过期**：
1. 检测到 401 错误（`expired`）
2. 请求新的 CAS 令牌
3. 使用新令牌重试请求

**签名错误**：
1. 检查密钥配置
2. 验证 kid 是否匹配
3. 不要重试（配置问题）

## API 参考

### Hub Token 创建

```bash
# 使用 CLI 工具
./target/release/hub-api create-token \
  --name "admin" \
  --scope "read write" \
  --repo "my-org/my-model" \
  --private-key private_key.pem \
  --kid "hub-key-1"
```

### CAS 令牌交换

```bash
# 获取读令牌
curl "$HF_ENDPOINT/api/models/my-org/my-model/xet-read-token/main" \
  -H "Authorization: Bearer hf_xxx"

# 获取写令牌
curl "$HF_ENDPOINT/api/models/my-org/my-model/xet-write-token/main" \
  -H "Authorization: Bearer hf_xxx"
```

## 故障排除

### 问题 1：令牌验证失败

**症状**：收到 401 错误

**排查步骤**：
1. 检查令牌格式是否正确
2. 验证令牌是否过期
3. 确认 kid 是否在受信任列表中
4. 检查公钥/私钥是否匹配

### 问题 2：权限不足

**症状**：收到 403 错误

**排查步骤**：
1. 检查令牌的 scope 是否包含所需权限
2. 验证 repo_id 和 repo_type 是否匹配
3. 确认 revision 是否正确

### 问题 3：密钥加载失败

**症状**：服务器启动失败

**排查步骤**：
1. 检查密钥文件路径是否正确
2. 验证文件格式（PEM）
3. 确认文件权限（600）
4. 检查密钥是否损坏

## 相关文档

- [CAS API Reference](cas-api.md) - CAS 服务器 API 详细文档
- [Hub API Reference](hub-api.md) - Hub API 详细文档
- [Configuration Guide](../configuration.md) - 配置选项说明
