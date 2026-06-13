# 配置合理性全面分析设计文档

**日期**：2026-06-13  
**状态**：设计审批中  
**方法**：分层系统分析 + 场景驱动验证

## 概述

对 Xet Server 双进程架构（CAS Server + Hub API）的全部配置项进行合理性分析。采用 5 层系统分析作为主线，每层用 4 个部署场景进行验证，最终输出跨层问题汇总和优先级排序的改进建议。

### 分析范围

- **CAS Server**（`xet-server`）：24 个配置项 + 6 个硬编码值
- **Hub API**（`hub-api`）：15 个配置项 + 11 个硬编码值
- **总计**：39 个配置项 + 17 个硬编码值

### 部署场景

| 场景 | 描述 | 关键约束 |
|------|------|----------|
| 本地开发 | 单机 localhost，最小配置 | 安全性可放松，便捷性优先 |
| 单机生产 | 单节点，本地存储或 S3，生产流量 | 安全性、可靠性、数据完整性 |
| 分布式 S3 | 多实例，S3 后端，跨机器通信 | 网络可达性、跨服务认证、可扩展性 |
| 企业集成 | 反向代理后，多租户，高并发 | 可定制性、SLA、安全合规 |

---

## 第 1 层：网络与部署层

### 配置项清单

| # | 环境变量 | 结构体字段 | 默认值 | 服务 | 用途 |
|---|----------|------------|--------|------|------|
| 1 | `XET_HOST` | `server.host` | `127.0.0.1` | CAS | TCP 绑定地址 |
| 2 | `XET_PORT` | `server.port` | `8081` | CAS | TCP 端口 |
| 3 | `XET_PUBLIC_BASE_URL` | `server.public_base_url` | `None` | CAS | 公共 URL |
| 4 | `XET_MAX_BODY_SIZE_MB` | `server.max_body_size_mb` | `2048` | CAS | 最大请求体 (MB) |
| 5 | `HUB_HOST` | `server.host` | `0.0.0.0` | Hub | TCP 绑定地址 |
| 6 | `HUB_PORT` | `server.port` | `8080` | Hub | TCP 端口 |
| 7 | `HUB_PUBLIC_BASE_URL` | `server.public_base_url` | `None` | Hub | 公共 URL |

### 分析

**L1-1: `XET_HOST` 默认 `127.0.0.1` vs `HUB_HOST` 默认 `0.0.0.0` — 不一致**

两个服务的绑定地址默认值不同。本地开发时 Hub 通过 `localhost:8081` 连接 CAS 没有问题。但在分布式部署中，Hub 和 CAS 运行在不同机器上，CAS 默认只监听 localhost 导致 Hub 无法连接。

这不是一个需要修改默认值的问题（改 `0.0.0.0` 会降低开发环境安全性），而是需要在文档和启动日志中明确提醒。

**L1-2: `XET_PUBLIC_BASE_URL` — 合理**

默认回退 `http://{host}:{port}` 对开发环境合理。URL 格式验证 + panic 行为正确（快速失败）。无问题。

**L1-3: `XET_MAX_BODY_SIZE_MB` = 2048 — 合理**

2GB 上限覆盖 xorb 上传（最大 512MB + 开销）。upload 路由通过流式字节计数手动限制，非 upload 路由的 `PayloadConfig` 为 10MB。设计合理。

**L1-4: 端口 8080/8081 — 合理**

避免冲突，代码与文档一致。

### 场景验证

| 场景 | 结果 | 说明 |
|------|------|------|
| 本地开发 | ✅ | CAS localhost 安全隔离，Hub 0.0.0.0 方便外部访问 |
| 单机生产 | ✅ | 同机部署，localhost 可达 |
| 分布式 S3 | ⚠️ | CAS 默认 localhost，Hub 在不同机器无法连接，必须显式设置 `XET_HOST` |
| 企业集成 | ⚠️ | 同上，且 `PUBLIC_BASE_URL` 必须设置以匹配反向代理 |

### 建议

| 编号 | 严重度 | 建议 |
|------|--------|------|
| R1-1 | 🟡 中 | 启动时检测 `XET_HOST=127.0.0.1` 输出警告日志：`CAS server bound to localhost only. Set XET_HOST=0.0.0.0 for remote access.` |
| R1-2 | 🟢 低 | Hub 启动时尝试连接 CAS `/health` 端点验证跨服务连通性，失败时输出警告 |

---

## 第 2 层：认证与安全层

### 配置项清单

| # | 环境变量 | 默认值 | 服务 | 用途 |
|---|----------|--------|------|------|
| 12 | `CAS_PUBLIC_KEY_PATH` | `/tmp/xet-public-key.pem` | CAS | 验证 Hub 签发的 Token |
| 13 | `CAS_TRUSTED_KIDS` | `hub-key-1` | CAS | 信任的密钥 ID 列表 |
| 28 | `HUB_PRIVATE_KEY_PATH` | `private_key.pem` | Hub | 签名 Token 的私钥 |
| 29 | `HUB_KID` | `hub-key-1` | Hub | 密钥标识符 |
| 30 | `HUB_TOKEN_TTL_SECONDS` | `3600` | Hub | CAS Token 有效期 |
| 31 | `HUB_SQLITE_PATH` | `hub.db` | Hub | SQLite 数据库路径 |

### 硬编码值

| # | 位置 | 值 | 用途 |
|---|------|-----|------|
| H8 | `xet_signer.rs:126` | 300s (5 min) | Proxy Token TTL |
| H9 | `xet_signer.rs:153` | 60s | Internal Token TTL |
| H1 | `server.rs:62-65` | 60 req/min | CAS 速率限制 |
| H2 | `hub/server.rs:45-48` | 120 req/min | Hub 速率限制 |
| H3 | `server.rs:78` | 10MB | CAS PayloadConfig |
| H4 | `hub/server.rs:60` | 50MB | Hub PayloadConfig |

### 分析

| L2-1: `CAS_PUBLIC_KEY_PATH` 默认 `/tmp/xet-public-key.pem` — 严重安全风险 |

`/tmp` 在 Linux 上权限为 `1777`（所有用户可读写执行），任何本地进程可：
- 读取公钥文件（本身风险低）
- **替换公钥文件**（高风险：CAS 会信任攻击者签发的 Token）
- 重启后文件丢失

这仅适合开发环境。生产环境必须将公钥放在安全路径（如 `/etc/xet/hub-public-key.pem`）。

**L2-2: `HUB_PRIVATE_KEY_PATH` 和 `HUB_SQLITE_PATH` 使用相对路径**

`private_key.pem` 和 `hub.db` 默认使用相对路径，依赖进程 CWD。不同部署方式（systemd、Docker、手动启动）的 CWD 不同，可能导致文件找不到或写入错误位置。

此外，如果 CAS 和 Hub 使用相同 CWD，`./data`（CAS 存储）和 `hub.db`（Hub 数据库）会在同一目录，造成混乱。

**L2-3: `CAS_TRUSTED_KIDS` / `HUB_KID` — 合理但无交叉验证**

默认值 `hub-key-1` 两端一致，开发环境方便。但：
- 无启动时交叉验证：Hub 的 kid 不在 CAS trusted_kids 中时，要到运行时才发现 Token 被拒
- 建议 Hub 启动时用配置的 kid 连接 CAS 验证信任关系

**L2-4: Proxy Token TTL = 5 分钟硬编码 — 大文件上传可能超时**

Proxy Token 绑定到特定 LFS OID 和操作（upload/download）。5 分钟对下载通常足够，但上传 512MB 文件在慢速网络（如跨地域 S3）可能需要超过 5 分钟。

**L2-5: 速率限制硬编码 — 运维灵活性关键缺失**

60/120 RPM 不可通过任何方式调整。这是最常需要根据部署环境修改的参数，但被硬编码。`docs/architecture.md` 第 462-465 行已提到计划添加 `CAS_RATE_LIMIT_RPM` / `HUB_RATE_LIMIT_RPM`，但从未实现。

### 场景验证

| 场景 | 结果 | 关键问题 |
|------|------|----------|
| 本地开发 | ✅ | `/tmp` 密钥方便，速率限制足够 |
| 单机生产 | ⚠️ | `/tmp` 公钥不安全，需改路径 |
| 分布式 S3 | ⚠️ | kid 匹配必须保证，速率限制可能不足 |
| 企业集成 | ❌ | `/tmp` 公钥不可接受，速率限制不可调，Proxy TTL 可能不够 |

### 建议

| 编号 | 严重度 | 建议 |
|------|--------|------|
| R2-1 | 🔴 高 | 启动时检查 `CAS_PUBLIC_KEY_PATH` 文件权限，如果对其他用户可写则输出安全警告。文档强调生产环境必须使用安全路径（如 `/etc/xet/`） |
| R2-2 | 🟡 中 | `HUB_PRIVATE_KEY_PATH` 和 `HUB_SQLITE_PATH` 默认值改为绝对路径（如 `/etc/xet/private_key.pem`、`/var/lib/xet/hub.db`），或在启动时解析为绝对路径并日志输出 |
| R2-3 | 🟡 中 | 添加 `HUB_PROXY_TOKEN_TTL_SECONDS` 配置项（默认 300），允许企业环境按需调整 |
| R2-4 | 🔴 高 | 添加 `XET_RATE_LIMIT_RPM` / `HUB_RATE_LIMIT_RPM` 环境变量（默认 60/120） |
| R2-5 | 🟢 低 | Hub 启动时验证 kid 在 CAS trusted_kids 中（可选的健康检查） |

---

## 第 3 层：存储引擎层

### 配置项清单

| # | 环境变量 | 默认值 | 服务 | 用途 |
|---|----------|--------|------|------|
| 5 | `XET_STORAGE_BACKEND` | `local` | CAS | 后端类型 |
| 6 | `XET_S3_BUCKET` | `None` | CAS | S3 桶名 |
| 7 | `XET_S3_REGION` | `None` | CAS | S3 区域 |
| 8 | `XET_S3_ENDPOINT` | `None` | CAS | S3 自定义端点 |
| 9 | `XET_LOCAL_PATH` | `./data` | CAS | 本地存储路径 |
| 10 | `XET_UPLOAD_TEMP_DIR` | 自动推导 | CAS | 上传临时目录 |
| 11 | `XET_VERIFY_DOWNLOAD_INTEGRITY` | `false` | CAS | 下载校验 |

### 未使用配置

| # | 环境变量 | 默认值 | 问题 |
|---|----------|--------|------|
| U1 | `HUB_LFS_THRESHOLD` | `10485760` (10MB) | 在 `hub/src/config.rs` 定义，但代码库中从未读取或使用 |
| U2 | `HUB_DATA_DIR` | `./data` | 在 `hub/src/config.rs` 定义，但代码库中从未读取或使用 |

### 硬编码值

| # | 位置 | 值 | 用途 |
|---|------|-----|------|
| H5 | `storage/s3.rs:15` | 5MB | S3 multipart 阈值 |
| H6 | `storage/s3.rs:19` | 8MB | S3 part 大小 |
| H7 | `conversion/mod.rs:19` | 1MB | 转换读取块大小 |
| H17 | `cas_client/mod.rs:226,262` | 512MB | CAS Client 下载大小限制 |

### 分析

**L3-1: 未使用配置 `HUB_LFS_THRESHOLD` 和 `HUB_DATA_DIR` — 死代码**

`HUB_LFS_THRESHOLD` 的设计意图可能是区分"小 LFS"和"大 LFS"文件，但实际文件分类只使用 `HUB_INLINE_THRESHOLD`（1MB）作为唯一阈值。`HUB_LFS_THRESHOLD` 从未在代码中被读取。

`HUB_DATA_DIR` 可能是预留的数据目录配置，但从未被使用。Hub 的 LFS 代理上传直接使用 `HUB_UPLOAD_TEMP_DIR`，不涉及 `data_dir`。

这两个配置项的存在会误导用户以为它们有效。在 TOML 配置文件中也可以设置它们，进一步强化了错误印象。

**L3-2: `XET_LOCAL_PATH` = `./data` — 相对路径**

与第 2 层的 `HUB_PRIVATE_KEY_PATH` 同类问题。另外，如果 CAS 和 Hub 使用相同 CWD，`./data`（CAS 存储 xorbs/shards/lfs）和 `hub.db`（Hub SQLite 数据库）会混在同一目录。

**L3-3: `XET_UPLOAD_TEMP_DIR` 自动推导 — 设计合理但 S3 场景需注意**

- 本地存储推导为 `{local_path}/.tmp`（同一文件系统，支持原子 rename）
- S3 场景推导为 `/tmp/xet-uploads`
- 大文件（512MB）上传时 `/tmp` 空间可能不足

**L3-4: H17 `MAX_DOWNLOAD_SIZE` = 512MB — 重复硬编码**

`hub/src/cas_client/mod.rs` 中硬编码了 512MB 下载限制，与 `HUB_MAX_UPLOAD_SIZE`（512MB）和 `XET_MAX_CONVERSION_SIZE`（512MB）形成三处对齐要求。用户如果调整上传限制，下载限制不会跟随变化。

### 场景验证

| 场景 | 结果 | 关键问题 |
|------|------|----------|
| 本地开发 | ✅ | 本地存储方便，相对路径可接受 |
| 单机生产 | ⚠️ | 相对路径需改为绝对路径 |
| 分布式 S3 | ⚠️ | `/tmp` 临时目录空间可能不足 |
| 企业集成 | ⚠️ | 未使用配置造成困惑，下载限制不可调 |

### 建议

| 编号 | 严重度 | 建议 |
|------|--------|------|
| R3-1 | 🔴 高 | 删除 `HUB_LFS_THRESHOLD` 和 `HUB_DATA_DIR` 死代码：移除 `hub/src/config.rs` 中的定义、`from_env()` 中的解析、`from_file_or_env()` 中的覆盖、文档中所有提及 |
| R3-2 | 🟡 中 | `hub/src/cas_client/mod.rs` 中的 `MAX_DOWNLOAD_SIZE` 改为从 `HUB_MAX_UPLOAD_SIZE` 配置读取 |
| R3-3 | 🟡 中 | 文档中补充 S3 场景的 `XET_UPLOAD_TEMP_DIR` 配置建议 |
| R3-4 | 🟢 低 | 补充 S3 认证配置文档（`AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY` 等标准 AWS 环境变量） |
| R3-5 | 🟢 低 | `XET_STORAGE_BACKEND` 添加启动时校验：非 `local`/`s3` 时输出清晰错误信息并退出 |

---

## 第 4 层：应用逻辑层

### 配置项清单

| # | 环境变量 | 默认值 | 服务 | 用途 |
|---|----------|--------|------|------|
| 14 | `XET_CONVERSION_ENABLED` | `true` | CAS | 启用转换管道 |
| 15 | `XET_CONVERSION_SCHEME` | `lz4` | CAS | 压缩方案 |
| 16 | `XET_DELETE_RAW_AFTER_CONVERSION` | `true` | CAS | 转换后删除原始 |
| 17 | `XET_MIN_CONVERSION_SIZE` | `1024` (1KB) | CAS | 最小转换大小 |
| 18 | `XET_MAX_CONVERSION_SIZE` | `536870912` (512MB) | CAS | 最大转换大小 |
| 19 | `GC_ENABLED` | `false` | CAS | 启用 GC |
| 20 | `GC_INTERVAL_SECONDS` | `3600` | CAS | GC 间隔 |
| 21 | `GC_GRACE_PERIOD_SECONDS` | `600` | CAS | GC 宽限期 |
| 22 | `GC_DRY_RUN` | `true` | CAS | GC 试运行 |
| 23 | `GC_HUB_BASE_URL` | `http://localhost:8080` | CAS | GC→Hub URL |
| 24 | `GC_HUB_INTERNAL_TOKEN` | `""` | CAS | GC→Hub 认证 Token |
| 35 | `HUB_INLINE_THRESHOLD` | `1048576` (1MB) | Hub | 内联/LFS 分界 |
| 38 | `HUB_MAX_UPLOAD_SIZE` | `536870912` (512MB) | Hub | 最大上传大小 |

### 分析

**L4-1: `XET_MIN_CONVERSION_SIZE` = 1KB — 可能过低**

转换过程会加载整个文件到内存，进行 CDC 分块、压缩、构建 xorb/shard。对 1KB 文件：
- GearHash CDC 最小 chunk 为 8KB，1KB 文件只产生 1 个 chunk
- xorb header + footer + chunk metadata ≈ 200-500 bytes 开销
- global dedup 价值极小（1KB 重复概率极低）
- 但每次转换有 CPU、I/O、S3 写入开销

建议默认值提升到 64KB（`65536`），减少小文件无效转换。

**L4-2: `GC_HUB_INTERNAL_TOKEN` = "" — 空值无启动验证**

GC 启用时必须提供 internal token 才能访问 Hub `/internal/referenced-hashes` 端点。但代码中不检查 token 是否为空。后果：GC 启用后第一次运行（最多 1 小时后）才会发现 token 无效，浪费一个 GC 周期。

**L4-3: `max_conversion_size` / `max_upload_size` / `MAX_DOWNLOAD_SIZE` 三处需同步**

三个值当前都是 512MB，但分布在三个独立的配置/硬编码中：
- `XET_MAX_CONVERSION_SIZE` = 512MB（CAS 配置）
- `HUB_MAX_UPLOAD_SIZE` = 512MB（Hub 配置）
- `MAX_DOWNLOAD_SIZE` = 512MB（Hub cas_client 硬编码）

如果用户只调整其中一个，会导致文件可以上传但不能转换、或可以转换但不能下载的边界情况。

**L4-4: 文件分类逻辑 — 单阈值设计**

`HUB_INLINE_THRESHOLD` = 1MB 是唯一有效的分类阈值：
- 文件 ≤ 1MB → inline（存储在 commit NDJSON 中，base64 编码约 1.36MB）
- 文件 > 1MB → LFS（走 CAS 存储路径）

删除 `HUB_LFS_THRESHOLD`（R3-1）后，分类逻辑是清晰的单阈值设计，无灰色地带。

### 场景验证

| 场景 | 结果 | 关键问题 |
|------|------|----------|
| 本地开发 | ✅ | 默认配置合理，GC 关闭 |
| 单机生产 | ⚠️ | GC 需手动启用；min_conversion_size 1KB 产生不必要开销 |
| 分布式 S3 | ⚠️ | GC 需配置 Hub URL + Token；S3 读取大文件转换慢 |
| 企业集成 | ⚠️ | GC 跨服务认证必须配置；阈值可能需要根据业务调整 |

### 建议

| 编号 | 严重度 | 建议 |
|------|--------|------|
| R4-1 | 🟡 中 | `XET_MIN_CONVERSION_SIZE` 默认值从 `1024` 提升到 `65536` (64KB) |
| R4-2 | 🟡 中 | GC 启用时启动阶段验证 `GC_HUB_INTERNAL_TOKEN` 非空 |
| R4-3 | 🟡 中 | 文档中强调 `max_conversion_size` ≥ `max_upload_size`，或统一配置源 |
| R4-4 | 🟢 低 | 文档补充大仓库 GC 宽限期建议（30-60 分钟） |

---

## 第 5 层：运维与性能层

### 硬编码运维参数

| # | 位置 | 值 | 用途 | 企业影响 |
|---|------|-----|------|----------|
| H1 | `src/server.rs:62-65` | `per_second=60, burst=60` | CAS 速率限制 | 高并发场景不够 |
| H2 | `hub/src/server.rs:45-48` | `per_second=60, burst=120` | Hub 速率限制 | 高并发场景不够 |
| H10 | `src/gc/mod.rs:48` | 300s | GC HTTP 超时 | 大仓库可能不够 |
| H11 | `hub/src/metadata/sqlite.rs:39` | 5 | SQLite max_connections | 高并发写入可能不够 |
| H12 | `hub/src/metadata/sqlite.rs:41` | 5s | SQLite acquire_timeout | 合理 |
| H13 | `hub/src/auth/token_store.rs:39,41` | 5/5s | TokenStore 连接池/超时 | 同上 |
| H14 | `hub/src/cas_client/mod.rs:108` | 10 | HTTP pool_max_idle_per_host | 高并发可能不够 |
| H15 | `hub/src/cas_client/mod.rs:109` | 90s | HTTP pool_idle_timeout | 合理 |
| H16 | `hub/src/cas_client/mod.rs:110` | 60s | HTTP tcp_keepalive | 合理 |
| H17 | `hub/src/cas_client/mod.rs:226,262` | 512MB | MAX_DOWNLOAD_SIZE | 应与 max_upload_size 同步 |

### 分析

**L5-1: 速率限制硬编码 — 最重要的硬编码缺失**

速率限制是最常需要根据部署环境调整的参数。当前无任何配置选项：
- 本地开发：60/120 RPM 足够
- 企业集成：可能对接多个客户端系统，需要 1000+ RPM
- 无环境变量、无配置文件、无任何调整方式

`docs/architecture.md` 第 462-465 行已提到计划添加此配置但未实现。

**L5-2: SQLite 连接池硬编码**

TokenStore 和 MetadataStore 各 5 个连接，都使用同一个 SQLite 数据库。SQLite WAL 模式下：
- 并发写入者只有 1 个
- 读取不受限
- 5 个连接对读多写少场景足够
- 高并发写入（大量并发 commit）可能成为瓶颈

**L5-3: GC HTTP 超时 300 秒**

`/internal/referenced-hashes` 返回所有被引用哈希列表。大仓库可能有数百万哈希，响应体可达几百 MB。300 秒通常足够，但网络慢的场景可能不够。

### 场景验证

| 场景 | 结果 | 关键问题 |
|------|------|----------|
| 本地开发 | ✅ | 硬编码值完全足够 |
| 单机生产 | ⚠️ | 速率限制通常足够，SQLite 连接数可能成为瓶颈 |
| 分布式 S3 | ⚠️ | 速率限制硬编码无法满足高并发需求 |
| 企业集成 | ❌ | 速率限制、连接池都不可调，无法满足定制化 SLA |

### 建议

| 编号 | 严重度 | 建议 |
|------|--------|------|
| R5-1 | 🔴 高 | 添加 `XET_RATE_LIMIT_RPM` / `HUB_RATE_LIMIT_RPM` 环境变量（默认 60/120） |
| R5-2 | 🟡 中 | 添加 `HUB_DB_POOL_SIZE` 环境变量（默认 5），或至少在文档中说明当前限制 |
| R5-3 | 🟡 中 | 添加 `GC_HTTP_TIMEOUT_SECONDS` 环境变量（默认 300） |
| R5-4 | 🟢 低 | HTTP 连接池参数当前值合理，记录在文档中供调优参考 |

---

## 跨层问题汇总

### 按优先级排序

#### 🔴 高优先级（必须修复）

| 编号 | 问题 | 涉及层 | 影响场景 | 建议 |
|------|------|--------|----------|------|
| C1 | 速率限制硬编码，无任何配置选项 | L2/L5 | 企业集成 ❌ | 添加 `XET_RATE_LIMIT_RPM` / `HUB_RATE_LIMIT_RPM` |
| C2 | `CAS_PUBLIC_KEY_PATH` 默认 `/tmp`，任意进程可替换 | L2 | 企业集成 ❌ | 启动时权限检查 + 文档强调安全路径 |
| C3 | 3 个死配置项：`HUB_LFS_THRESHOLD`、`HUB_DATA_DIR` 从未使用 | L3 | 所有场景 | 删除代码和文档 |

#### 🟡 中优先级（建议修复）

| 编号 | 问题 | 涉及层 | 影响场景 | 建议 |
|------|------|--------|----------|------|
| C4 | `XET_HOST` 默认 `127.0.0.1` 不适合分布式 | L1 | 分布式、企业 | 启动时日志警告 |
| C5 | 3 个相对路径配置（`private_key.pem`、`hub.db`、`./data`）依赖 CWD | L2/L3 | 单机生产+ | 文档建议绝对路径 |
| C6 | Proxy Token TTL 5 分钟硬编码，大文件上传可能超时 | L2 | 企业集成 | 添加 `HUB_PROXY_TOKEN_TTL_SECONDS` |
| C7 | `XET_MIN_CONVERSION_SIZE` = 1KB 过低 | L4 | 单机生产+ | 提升到 64KB |
| C8 | `GC_HUB_INTERNAL_TOKEN` 空值无启动验证 | L4 | 分布式、企业 | 启动时检查非空 |
| C9 | `MAX_DOWNLOAD_SIZE` 硬编码 512MB 与 `MAX_UPLOAD_SIZE` 重复 | L3/L5 | 所有场景 | 复用 `HUB_MAX_UPLOAD_SIZE` |
| C10 | `GC_HTTP_TIMEOUT` 硬编码 300s | L5 | 分布式 | 添加 `GC_HTTP_TIMEOUT_SECONDS` |
| C11 | SQLite 连接池大小硬编码 5 | L5 | 单机生产+ | 添加 `HUB_DB_POOL_SIZE` |

#### 🟢 低优先级（可延后）

| 编号 | 问题 | 涉及层 | 建议 |
|------|------|--------|------|
| C12 | 无跨服务启动验证 | L1/L2 | Hub 启动时 ping CAS `/health` |
| C13 | S3 凭证未在配置文档中说明 | L3 | 补充 AWS 环境变量文档 |
| C14 | `XET_STORAGE_BACKEND` 无效值无明确错误 | L3 | 添加校验 |
| C15 | GC 宽限期文档不够详细 | L4 | 补充大仓库建议 |
| C16 | HTTP 连接池参数硬编码 | L5 | 记录在文档中供调优参考 |

### 场景-问题矩阵

| 场景 | 🔴 影响 | 🟡 影响 | 🟢 影响 | 总评 |
|------|---------|---------|---------|------|
| 本地开发 | 无 | 无 | 无 | ✅ 全部默认值可直接使用 |
| 单机生产 | C2 | C5, C7, C8 | C12 | ⚠️ 需调整密钥路径，建议启用 GC |
| 分布式 S3 | C1 | C4, C5, C6, C9, C10 | C13 | ⚠️ 速率限制和跨服务配置必须调整 |
| 企业集成 | C1, C2, C3 | C4-C11 | C12-C16 | ❌ 多个硬编码限制需要定制化 |

### 实施工作量估算

| 优先级 | 建议数 | 预估工作量 | 风险 |
|--------|--------|------------|------|
| 🔴 高 | 3 项 | 2-3 小时 | 低（添加配置项、删除死代码） |
| 🟡 中 | 8 项 | 4-6 小时 | 中（涉及默认值变更、启动验证逻辑） |
| 🟢 低 | 5 项 | 2-3 小时 | 低（文档补充、校验增强） |
| **合计** | **16 项** | **8-12 小时** | — |

---

## 附录：完整配置清单

### CAS Server 环境变量（24 项）

| 环境变量 | 默认值 | 状态 |
|----------|--------|------|
| `XET_HOST` | `127.0.0.1` | ⚠️ 见 L1-1 |
| `XET_PORT` | `8081` | ✅ 合理 |
| `XET_PUBLIC_BASE_URL` | `None` | ✅ 合理 |
| `XET_MAX_BODY_SIZE_MB` | `2048` | ✅ 合理 |
| `XET_STORAGE_BACKEND` | `local` | ✅ 合理 |
| `XET_S3_BUCKET` | `None` | ✅ 合理 |
| `XET_S3_REGION` | `None` | ✅ 合理 |
| `XET_S3_ENDPOINT` | `None` | ✅ 合理 |
| `XET_LOCAL_PATH` | `./data` | ⚠️ 见 L3-2 |
| `XET_UPLOAD_TEMP_DIR` | 自动 | ✅ 合理 |
| `XET_VERIFY_DOWNLOAD_INTEGRITY` | `false` | ✅ 合理 |
| `CAS_PUBLIC_KEY_PATH` | `/tmp/xet-public-key.pem` | 🔴 见 L2-1 |
| `CAS_TRUSTED_KIDS` | `hub-key-1` | ✅ 合理 |
| `XET_CONVERSION_ENABLED` | `true` | ✅ 合理 |
| `XET_CONVERSION_SCHEME` | `lz4` | ✅ 合理 |
| `XET_DELETE_RAW_AFTER_CONVERSION` | `true` | ✅ 合理 |
| `XET_MIN_CONVERSION_SIZE` | `1024` | ⚠️ 见 L4-1 |
| `XET_MAX_CONVERSION_SIZE` | `536870912` | ✅ 合理 |
| `GC_ENABLED` | `false` | ✅ 合理 |
| `GC_INTERVAL_SECONDS` | `3600` | ✅ 合理 |
| `GC_GRACE_PERIOD_SECONDS` | `600` | ✅ 合理 |
| `GC_DRY_RUN` | `true` | ✅ 合理 |
| `GC_HUB_BASE_URL` | `http://localhost:8080` | ✅ 合理 |
| `GC_HUB_INTERNAL_TOKEN` | `""` | ⚠️ 见 L4-2 |

### Hub API 环境变量（15 项 + 1 文件配置）

| 环境变量 | 默认值 | 状态 |
|----------|--------|------|
| `HUB_HOST` | `0.0.0.0` | ✅ 合理 |
| `HUB_PORT` | `8080` | ✅ 合理 |
| `HUB_PUBLIC_BASE_URL` | `None` | ✅ 合理 |
| `HUB_PRIVATE_KEY_PATH` | `private_key.pem` | ⚠️ 见 L2-2 |
| `HUB_KID` | `hub-key-1` | ✅ 合理 |
| `HUB_TOKEN_TTL_SECONDS` | `3600` | ✅ 合理 |
| `HUB_SQLITE_PATH` | `hub.db` | ⚠️ 见 L2-2 |
| `CAS_BASE_URL` | `http://localhost:8081` | ✅ 合理 |
| `HUB_CAS_TIMEOUT_SECS` | `30` | ✅ 合理 |
| `HUB_DATA_DIR` | `./data` | 🔴 死代码 (U2) |
| `HUB_INLINE_THRESHOLD` | `1048576` | ✅ 合理 |
| `HUB_LFS_THRESHOLD` | `10485760` | 🔴 死代码 (U1) |
| `HUB_UPLOAD_TEMP_DIR` | `/tmp/hub-uploads` | ✅ 合理 |
| `HUB_MAX_UPLOAD_SIZE` | `536870912` | ✅ 合理 |
| `HUB_CONFIG_FILE` | `None` | ✅ 合理 |

### 关键硬编码值（17 项）

| # | 值 | 位置 | 状态 |
|---|-----|------|------|
| H1 | 60 RPM | CAS 速率限制 | 🔴 需配置化 |
| H2 | 120 RPM | Hub 速率限制 | 🔴 需配置化 |
| H3 | 10MB | CAS PayloadConfig | ✅ 合理 |
| H4 | 50MB | Hub PayloadConfig | ✅ 合理 |
| H5 | 5MB | S3 multipart 阈值 | ✅ 合理 |
| H6 | 8MB | S3 part 大小 | ✅ 合理 |
| H7 | 1MB | 转换读取块大小 | ✅ 合理 |
| H8 | 300s | Proxy Token TTL | ⚠️ 需配置化 |
| H9 | 60s | Internal Token TTL | ✅ 合理 |
| H10 | 300s | GC HTTP 超时 | ⚠️ 需配置化 |
| H11 | 5 | SQLite max_connections | ⚠️ 需配置化 |
| H12 | 5s | SQLite acquire_timeout | ✅ 合理 |
| H13 | 5/5s | TokenStore 池 | ✅ 合理 |
| H14 | 10 | HTTP idle connections | ✅ 合理 |
| H15 | 90s | HTTP idle timeout | ✅ 合理 |
| H16 | 60s | TCP keepalive | ✅ 合理 |
| H17 | 512MB | MAX_DOWNLOAD_SIZE | ⚠️ 需同步 |
