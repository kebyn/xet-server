# 安全与数据完整性修复 — 设计文档

**日期**: 2026-06-15
**范围**: 全项目审计发现的 Critical + Important + Minor 问题修复
**明确排除**: `src/gc/*`(GC 子系统本次完全不动 —— C1 软周期架构问题、C2 删除下限均不在本次范围)

---

## 背景

对 ~14k LOC 的 Rust CAS/Hub 系统做了五路并行审计(鉴权、CAS 格式/存储、chunking/config/metadata、API 层、GC)。
本文档记录**除 GC 外**所有已确认问题及修复方案。每个 Critical/Important 项以「先写复现测试 → 再修复」(TDD)方式实施;每项修复以一个失败测试开始,证明问题真实存在。

基线状态(已验证):`cargo build --workspace --all-targets` 通过;`cargo clippy` 有 11 个警告(均为 Minor)。

---

## Critical 问题(已逐一读码确认)

### C-AUTH-1 — 私有 repo 读取越权(IDOR)

**位置**: `hub/src/api/resolve.rs:8-154`(`handle_resolve`)
**问题**: 加载了 `repo` 并可读到 `repo.private` / `repo.namespace`,但从不与调用者身份比对。任何持有合法 `read` token 的用户都能下载**任意**私有 repo 的文件内容(inline 或 302 重定向)。
**根因**: 所有权/私有性校验目前只在 `repo.rs:238`(delete 路径)存在,read 路径缺失。
**修复**: 在 `handle_resolve` 加载 repo 后,若 `repo.private` 为真且 `repo.namespace != auth.username()`,返回 **404**(而非 403,避免泄露私有 repo 的存在性)。
**数据模型已确认可行**: `Repo { namespace, private }` 存在;`AuthUser::username()` 可用。

### C-AUTH-2 — token 交换越权(读 & 写)

**位置**: `hub/src/api/token_exchange.rs:19-84`(`do_exchange`),被 6 个 `exchange_{model,dataset,space}_{read,write}` 调用
**问题**: `do_exchange` 加载 repo 后直接签发 xet read/write token,无任何 namespace 校验。`exchange_*_write` 仅要求**某个** write token —— 用户 A 可为用户 B 的 repo 取得 write 范围的 xet token 并推送。
**修复**: 在 `do_exchange` 内、签名前加同样的所有权门控:私有 repo 要求 `repo.namespace == info.username`,否则 404。读写路径都加。

### C-DATA-1 — LZ4 解压炸弹

**位置**: `src/format/compression.rs:175-189`(`decompress`),触发于 `src/api/lfs.rs:1082`
**问题**: LZ4 分支调用 `decompress_size_prepended`,信任压缩数据内嵌的 4 字节长度前缀进行预分配,**完全忽略**传入的 `original_size` 参数。上传校验(`verify_xorb`)只 hash 压缩字节、从不解压,因此一个前缀声称 ~4GB 的 chunk 能通过上传校验并存入;下载时每个 chunk 预分配 ~4GB → OOM/DoS。
**修复**:
1. 解压前:读取前缀声明长度,与 `original_size` 及一个硬上限(匹配 header `uncompressed_length` 字段宽度)比对,超限直接返回 `ParseError`,不分配。
2. 解压后:断言 `out.len() == original_size`,不符则报错。
3. `None` 分支同样不应被信任的下游长度影响(当前 `None`/`LZ4` 都忽略 original_size,统一加校验)。

### C-DATA-2 — 读路径缺失 chunk hash 校验

**位置**: `src/api/lfs.rs:1051-1088`(重建流)对比上传期校验 `src/api/xorb.rs:179`
**问题**: 重建时按 offset 切片、解压、直接 yield 给客户端,**从不**用 `compute_data_hash` 重算并与 shard 记录的 `chunk_entry.chunk_hash` 比对。本地磁盘 bit-rot、S3 损坏、存储被串改均无法被发现 —— 违背内容寻址存储的核心保证。
**字段已确认**: `shard.rs:284 pub chunk_hash: MerkleHash`。
**修复**: 对每个解压出的 chunk,重算 hash 与 `chunk_entry.chunk_hash` 比对,不符则中断流并 yield 错误。
**已知限制(写入 spec)**: 大文件经 `SizedStream` 流式响应,一旦发出 200 + Content-Length 头就无法回退状态码;校验失败只能中断流,客户端需自行重试/校验。此限制与现有整体下载完整性限制一致,不在本次扩大范围。

---

## Important 问题

> 以下多数来自子代理审计、尚未逐行独立确认。实施时每项**先写复现测试**;若测试证明问题已被处理,则该项转为「补测试」而非改代码,并在实施记录中说明。

### I-STOR-1 — 重建临时文件名冲突

**位置**: `src/api/lfs.rs:1013`(`xorb-{hash}-{pid}.tmp`),`src/storage/s3.rs:482`(`.part`)
**问题**: 同进程内两个并发请求重建同一 xorb 会推导出相同临时路径,交错写入 → 文件损坏 + TOCTOU 删除。
**修复**: 临时文件名加入 UUID/随机分量(上传路径已用 `tempfile::Builder`,与之对齐)。

### I-STOR-2 — 本地跨文件系统 fallback 非原子

**位置**: `src/storage/local.rs:105-114`
**问题**: 跨文件系统时 `fs::copy(source, dest)` 直接写最终 key,中断会留下截断文件。
**修复**: 先 copy 到 `dest.tmp` 再 rename。

### I-STOR-3 — 本地 `put()` 对 shard 非原子

**位置**: `src/storage/local.rs:83`(`fs::write`)
**问题**: `put_atomic` 已存在,但 `put()` 原地写;崩溃会留下截断的 shard,叠加 C-DATA-2 则被静默当作部分映射解析。
**修复**: 持久化写统一走 temp+rename 的 `put_atomic`。

### I-STOR-4 — S3 NotFound 用字符串匹配判定

**位置**: `src/storage/s3.rs:438,473,533,637` 等
**问题**: 靠 `e.to_string().contains("NoSuchKey"/"NotFound")` 判定;AWS SDK 错误展示格式非契约稳定,格式变更会误分类 → 破坏幂等/去重逻辑。
**修复**: 改为匹配类型化 SDK 错误(`SdkError::ServiceError` / `is_no_such_key()`)。

### I-API-1 — 代理 href 重写失败泄露内部 CAS URL

**位置**: `hub/src/api/lfs_proxy.rs:194-216`(`rewrite_action_url`)
**问题**: `url::Url::parse` 失败时保留原始 CAS href,仅替换 Authorization 头 → 客户端拿到内部主机名 + 不可达 URL。
**修复**: 重写失败时丢弃该 action(与签名失败路径一致),不透传原始 URL。

### I-API-2 — tree 目录推断 strip_prefix 缺路径边界

**位置**: `hub/src/api/tree.rs:27,144`
**问题**: 前缀 `model` 对条目 `models/x.bin` 被 strip 成 `s/x.bin`,推断出错误目录。递归分支(`:111`)正确用了 `format!("{}/", tree_path)`。
**修复**: 非递归/`infer_directories` 路径也要求尾部 `/` 边界。

### I-API-3 — 首次提交并发不变量需确认 + 补测试

**位置**: `hub/src/api/commit.rs:457`、`hub/src/metadata/sqlite.rs:558-578`(`commit_atomic`,`BEGIN IMMEDIATE`)
**问题**: 两个并发**首次**提交(parent=None)的正确性完全依赖 `commit_atomic` 在 HEAD≠NULL 时拒绝 parent=None 的写入。已有 mismatched-parent 测试,但缺 None/None 并发测试。
**修复**: 确认 `commit_atomic` 对 (parent=None, HEAD≠NULL) 拒绝;补并发首次提交回归测试。

### I-API-4 — 异步 handler 中阻塞式 remove_file

**位置**: `src/api/xorb.rs:224`(`std::fs::remove_file`)
**问题**: 存储错误清理路径阻塞 tokio worker;LFS 路径已修为 `tokio::fs::remove_file`。
**修复**: xorb.rs 对齐为 `tokio::fs::remove_file`。

---

## 行为变更项(已确认决策)

### B-CONF-1 — 代理 token 透传(已确认:保留现状,不改)

**位置**: `src/api/batch.rs:84-91,206-207`(透传逻辑),`src/config.rs:516`(默认 `private_key_path: None`)
**读码确认的真实风险**:
- 触发条件:未设 `CAS_PRIVATE_KEY_PATH`(默认)时,`POST /objects/batch` 对每个 object 走 `else` 分支 `auth_token_for_action = token.clone()`,把调用方自己发来的 **xet token** 原样放进响应 action 的 `Authorization` 头,而非签发 5 分钟单 oid 的 proxy token。
- 透传的**不是** HF 长期 PAT(那个仅 Hub 见过、哈希存储),而是调用方已持有的 xet token(用户范围、TTL 可配)。**不是泄露给客户端**(客户端已有)。
- 真实风险是 **blast-radius 放大**:proxy token 本是「会随 action URL/header 流经中间代理/CDN 日志/LFS 客户端缓存的低权短期凭证」,被全量 xet token 替代后,此类泄露暴露的范围/TTL 更大。
- 已有 I5/I6 修复已堵住「签名失败静默回退」(`batch.rs:184-205` 返回 500),只剩「根本没配私钥」这一条 warn + 透传。
**风险等级**: Important 级 defense-in-depth 弱点,**非越权**(token 仍只授予其本有权限)。
**决策**: **保留现状,不改**。已有 warning 日志。记录为已接受风险。

### B-CONF-2 — Hub 默认绑定 0.0.0.0(已确认:保留默认值,加启动日志)

**位置**: `hub/src/config.rs:46,209`
**现状**: Hub 默认监听所有网卡(CAS 正确默认 loopback);鉴权中间件为 default-deny,风险有限。
**决策**: **不改默认值**(改 127.0.0.1 会破坏现有对外部署)。**修复内容**:在 Hub 启动时打印一条 info/warn 日志,显式输出实际绑定地址 + 鉴权是否启用,便于运维察觉「对外暴露 + 鉴权状态」。补一句配置文档说明。

---

## Minor 问题

- **M-1 clippy collapsible-if**: `src/api/lfs.rs:91,100,328,337`、`hub/src/cas_client/mod.rs:224`
- **M-2 死代码**: `hub/src/api/commit.rs:545`(`with_existing_oid`、`with_upload_failure` 未使用)
- **M-3 非 snake_case 测试名**: `commit.rs:709 test_commitAtomic_rejects_mismatched_parent`
- **M-4 CDC mask 仅对 2 的幂正确**: `src/chunking/cdc.rs:67,173`。当前 `target=65536`(2 的幂)正确,但 mask 公式对非 2 的幂会得到散乱位。修复:按 `target.trailing_zeros()` 推导 mask,或 `assert!(target.is_power_of_two())`。
- **M-5 conversion 文档与实现不符**: `src/conversion/mod.rs:88-89` 声称 chunk 级去重,实际 `:207-213` 只计数不应用,去重仅在整 xorb 粒度。修复:修正文档/指标命名(不实现跨 xorb chunk 复用,避免扩大范围)。
- **M-6 hash salt 与 hash 同库存储**: `hub/src/auth/token_store.rs:152-207`。影响低(token 为 122-bit UUID)。修复:生产要求 env 提供 salt,或修正注释中夸大的防护描述。
- **M-7 私钥文件权限未检查**: `src/api/auth.rs:336-357`、`hub/src/auth/xet_signer.rs:60-78`。修复:启动时检查 PEM 文件非 world-readable,过宽则警告。

> 注:子代理提到的限流按 peer IP / 无 XFF 感知(`server.rs`)是部署相关行为,本次仅在文档中说明「Hub 须直接终止客户端连接或配置可信头提取器」,不改代码。

---

## 实施与验证策略

1. **分组 TDD**: 按上面分组逐项「先写失败测试 → 修复 → 测试转绿」。
2. **行为变更项**(B-CONF-1/2)实施前单独确认。
3. **全量验证**: 全部完成后运行
   - `cargo build --workspace --all-targets`
   - `cargo clippy --workspace --all-targets`(目标:0 警告)
   - `cargo test --workspace`
4. **重新 review**(用户原始需求的「重新进行 review」): 全部修复完成后,用 `requesting-code-review` 对完整 diff 做一次复审。
5. 清理验证过程中产生的临时文件。

---

## 范围外(明确不做)

- `src/gc/*` 全部 —— 包括 C1(重复引用旧对象误删 / soft-cycle 软周期架构)与 C2(删除下限兜底)。GC 默认关闭。
- 跨 xorb chunk 级去重的实现(仅修文档)。
- 限流器 XFF 改造(仅文档)。
- 改 Hub 默认绑定地址(默认值不动,除非另行确认)。
- **LFS 对象字节路径的 repo 级鉴权**(`lfs_batch` / `lfs_download`,仅加代码注释记录)。复审发现这两个端点只按 token scope + OID-绑定 proxy token 鉴权,不校验 OID 是否属于 URL 中的 repo。但单加 URL-repo 校验对该威胁净收益为零:body 的 OID 与 URL 的 repo 解耦(攻击者可用自己的公开 repo 作 URL、body 塞受害者私有 OID),且裸路由(`/objects/batch`、`/lfs/objects/{oid}`)按构造无 repo 上下文可直接旁路。这是内容寻址系统(LFS/Xet)固有的能力模型:知道 64 位内容哈希即拥有字节访问能力。**现实威胁已被缓解** —— 唯一能把私有 repo 映射到其 cas_hash/OID 的途径(tree / resolve / repo 元数据端点)已在本次做了 repo 所有权 gating,无 OID 则此路不可达。彻底修复属架构改动:新增 `MetadataStore` 的 `file_entries(repo_id, cas_hash)` 反查 + 索引、逐 download OID 校验、移除/repo-scope 裸路由、处理去重语义(同一 OID 可合法属于公开与私有 repo)。详见 `hub/src/api/lfs_proxy.rs` 中 `lfs_batch` 上方注释。

