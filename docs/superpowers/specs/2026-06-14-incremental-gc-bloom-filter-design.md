# 增量GC + Bloom Filter保护集设计规范

**日期**: 2026-06-14
**状态**: 已批准
**作者**: GC重构项目组

---

## 1. 概述

### 1.1 问题陈述

当前Xet Server的GC实现存在三个核心问题：

1. **Hub接口耦合** - GC依赖Hub提供`/internal/referenced-hashes`接口，增加运维复杂度和单点故障风险
2. **多节点竞态** - 多个CAS节点同时运行GC时可能产生竞态条件，导致重复删除或遗漏
3. **性能瓶颈** - 全量扫描方式在数据量增长后性能急剧下降，GC周期时间不可控

### 1.2 设计目标

- ✅ 完全去除Hub接口依赖，CAS独立运行GC
- ✅ 支持去中心化多节点GC，无竞态问题
- ✅ 增量扫描，性能随数据量线性增长
- ✅ 最终一致性模型，接受临时孤立数据换取性能
- ✅ 灵活支持S3和本地存储部署

### 1.3 关键决策

| 决策项 | 选择方案 | 理由 |
|--------|---------|------|
| GC算法 | 增量GC + Bloom Filter保护集 | 避免全量扫描，内存效率高 |
| S3引用追踪 | Sidecar `.refs.json`文件 | S3元数据2KB限制不可行 |
| 本地存储引用追踪 | 内联解析 + SQLite缓存 | 简单可靠，无外部依赖 |
| 多节点协调 | S3-based Lease | 无Redis/etcd外部依赖 |
| Grace Period | 双层保护（绝对 + 软保护期） | 3层安全防御 |

---

## 2. 架构设计

### 2.1 整体架构

```
┌─────────────────────────────────────────────────────────────────┐
│                    增量GC系统架构                                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │   Bloom      │  │  Incremental │  │  Reference   │          │
│  │   Filter     │  │  Scanner     │  │  Tracker     │          │
│  │  Protected   │←─┤  + Checkpoint│←─┤  (S3/Local)  │          │
│  │  Set         │  │              │  │              │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
│         ↑                 ↑                  ↑                   │
│         │                 │                  │                   │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │   Grace      │  │   Coordinator│  │   Metrics    │          │
│  │   Period     │  │  (Lease Mgmt)│  │  (Prometheus)│          │
│  │  (2-tier)    │  │              │  │              │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 5阶段GC流程

```
Phase 1: 获取GC Lease（多节点协调）
  - 使用S3条件PUT创建lease文件
  - Lease有效期：1小时
  - 失败则跳过本次GC
  
Phase 2: 加载Checkpoint + Bloom Filter
  - 从s3://bucket/.gc/checkpoint.json加载上次扫描位置
  - 从s3://bucket/.gc/bloom.bin加载Bloom Filter
  - 验证CRC32完整性
  
Phase 3: 增量扫描（分页列出新对象）
  - 从上次checkpoint位置开始列出对象
  - 每批1000个对象，流式处理
  - 对每个shard：
    * S3: 读取s3://bucket/shard_refs/{hash}.refs.json
    * Local: 解析shard文件 + 更新本地缓存
  - 提取的引用加入Bloom Filter
  - 每10000个对象保存一次checkpoint
  
Phase 4: 计算候选删除集
  - 列出存储中的所有对象
  - 候选 = 存储对象 - Bloom Filter保护集
  - 应用双层grace period:
    * 绝对保护期（1h）：创建时间 < 1h的对象永不删除
    * 软保护期（24h）：必须连续2次GC都未被引用才删除
  
Phase 5: 删除 + 清理
  - 删除候选对象（带重试）
  - 保存更新的Bloom Filter和checkpoint
  - 释放lease
  - 报告统计信息
```

### 2.3 核心组件

| 组件 | 文件路径 | 职责 |
|------|----------|------|
| Bloom Filter | `src/gc/bloom.rs` | 保护集，避免删除引用数据 |
| 增量扫描器 | `src/gc/scanner.rs` | 分页扫描，checkpoint支持 |
| Checkpoint | `src/gc/checkpoint.rs` | 断点续传，崩溃恢复 |
| 协调器 | `src/gc/coordinator.rs` | 多节点lease管理 |
| 引用追踪器 | `src/gc/reference_tracker/*.rs` | Sidecar/本地缓存实现 |
| Grace Period | `src/gc/grace.rs` | 双层保护期 |
| 指标 | `src/gc/metrics.rs` | Prometheus指标 |
| 错误处理 | `src/gc/errors.rs` | 错误类型和恢复策略 |

---

## 3. 详细设计

### 3.1 Bloom Filter Protected Set

#### 技术选型

- **Crate**: `bloomfilter = "1.0.8"`（纯Rust，serde支持，无FFI）
- **校验**: `crc32fast = "1.3"`（CRC32完整性校验）
- **序列化**: `bincode = "1.3"`（二进制序列化）

#### 数据结构

```rust
pub struct BloomFilterProtectedSet {
    /// 当前活跃的Bloom Filter（用于查询）
    active: Bloom<[u8]>,
    
    /// 正在刷新的Bloom Filter（后台重建）
    refreshing: Option<Bloom<[u8]>>,
    
    /// 配置
    config: BloomConfig,
    
    /// 统计信息
    stats: BloomStats,
}

pub struct BloomConfig {
    /// 预期元素数量（决定Bloom Filter大小）
    pub expected_items: u64,      // 默认：10,000,000
    
    /// 误判率（False Positive Rate）
    pub false_positive_rate: f64, // 默认：0.001 (0.1%)
    
    /// 触发重建的阈值（占容量的百分比）
    pub rebuild_threshold: f64,   // 默认：0.8 (80%)
}
```

#### 内存占用估算

```
公式：m = -n * ln(p) / (ln(2))^2
其中：
  n = expected_items = 10,000,000
  p = false_positive_rate = 0.001
  m = number of bits

计算：
  m = -10,000,000 * ln(0.001) / 0.4804
    = 143,775,000 bits
    = 17,971,875 bytes
    ≈ 17.1 MB

实际内存占用：~17-20 MB（可接受）
```

#### 持久化格式

```
[CRC32: 4 bytes][Bloom Filter data: variable]
```

- CRC32用于检测损坏
- 使用原子写入（write-tmp → rename）
- 损坏时自动重建新的Bloom Filter

#### 双缓冲重建策略

```rust
// 当active达到80%容量时触发重建
fn should_rebuild(&self) -> bool {
    let capacity = self.active.number_of_bits() as f64 * 0.693;
    let usage_ratio = self.stats.items_inserted as f64 / capacity;
    usage_ratio >= self.config.rebuild_threshold
}

// 重建期间：active继续服务查询，refreshing后台构建
fn start_rebuild(&mut self) {
    let new_bloom = Bloom::new_for_fp_rate(
        self.config.expected_items as usize,
        self.config.false_positive_rate,
    );
    self.refreshing = Some(new_bloom);
}

// 重建完成后交换
fn complete_rebuild(&mut self) {
    if let Some(new_bloom) = self.refreshing.take() {
        self.active = new_bloom;
        self.stats.items_inserted = 0;
    }
}
```

#### 关键API

```rust
impl BloomFilterProtectedSet {
    pub fn new(config: BloomConfig) -> Self;
    pub fn insert(&mut self, hash: &str);
    pub fn insert_all(&mut self, hashes: &[String]);
    pub fn contains(&self, hash: &str) -> bool;
    pub fn save<W: Write>(&self, writer: &mut W) -> Result<(), GcError>;
    pub fn load<R: Read>(reader: &mut R, config: BloomConfig) -> Result<Self, GcError>;
}
```

---

### 3.2 增量扫描器 + Checkpoint

#### Checkpoint数据结构

```rust
pub struct GcCheckpoint {
    /// Checkpoint版本（用于向后兼容）
    pub version: u32,
    
    /// 上次扫描的时间戳
    pub last_scan_at: DateTime<Utc>,
    
    /// S3分页cursor（用于增量列出对象）
    pub s3_cursor: Option<String>,
    
    /// 已扫描的shard数量
    pub shards_scanned: u64,
    
    /// 已扫描的xorb数量
    pub xorbs_scanned: u64,
    
    /// 已扫描的LFS blob数量
    pub lfs_blobs_scanned: u64,
    
    /// 当前GC周期开始时间
    pub cycle_started_at: DateTime<Utc>,
    
    /// 完成状态
    pub status: CheckpointStatus,
    
    /// CRC32校验
    pub crc32: u32,
}

pub enum CheckpointStatus {
    InProgress,
    Completed,
    Failed(String),
}
```

#### 扫描流程

```rust
impl IncrementalScanner {
    pub async fn scan(
        &self,
        bloom: &mut BloomFilterProtectedSet,
        checkpoint: &mut GcCheckpoint,
    ) -> Result<ScanResult, GcError> {
        let start_time = Instant::now();
        let mut result = ScanResult::default();
        
        // 1. 增量列出shards（从上次cursor开始）
        let mut shard_stream = self.storage
            .list_objects_paged("shards", checkpoint.s3_cursor.as_deref(), 
                              self.config.page_size)
            .await?;
        
        let mut scanned_since_checkpoint = 0u64;
        
        while let Some(page) = shard_stream.next_page().await? {
            // 检查超时
            if start_time.elapsed() > self.config.max_scan_duration {
                warn!("扫描超时，保存checkpoint并退出");
                break;
            }
            
            // 2. 处理每个shard
            for shard_meta in &page.objects {
                let shard_hash = shard_meta.key.strip_prefix("shards/").unwrap();
                
                // 读取sidecar引用
                let refs = self.load_shard_references(shard_hash).await?;
                
                // 3. 插入Bloom Filter
                bloom.insert_all(&refs.xorb_refs);
                bloom.insert_all(&refs.lfs_refs);
                
                result.shards_scanned += 1;
                checkpoint.shards_scanned += 1;
                scanned_since_checkpoint += 1;
                
                // 4. 定期保存checkpoint
                if scanned_since_checkpoint >= self.config.checkpoint_interval {
                    checkpoint.s3_cursor = page.next_cursor.clone();
                    checkpoint.last_scan_at = Utc::now();
                    checkpoint.save(&*self.storage).await?;
                    scanned_since_checkpoint = 0;
                }
            }
            
            checkpoint.s3_cursor = page.next_cursor.clone();
            
            if !page.has_more {
                checkpoint.status = CheckpointStatus::Completed;
                checkpoint.save(&*self.storage).await?;
                break;
            }
        }
        
        result.duration = start_time.elapsed();
        Ok(result)
    }
}
```

#### Sidecar读取（三层防御）

```rust
async fn load_shard_references(&self, shard_hash: &str) -> Result<ReferenceSet, GcError> {
    let sidecar_key = format!("{}.refs.json", shard_hash);
    
    // Layer 1: 尝试读取sidecar
    match self.storage.get("shard_refs", &sidecar_key).await {
        Ok(data) => {
            let refs: ReferenceSet = serde_json::from_slice(&data)?;
            
            // 验证引用数量（安全检查）
            if let Ok(shard_data) = self.storage.get("shards", shard_hash).await {
                let shard = parse_shard_headers(&shard_data)?;
                if refs.xorb_refs.len() != shard.xorb_entries.len() {
                    warn!("Sidecar引用数量不匹配！使用shard解析");
                    return self.parse_shard_references(shard_hash, &shard_data).await;
                }
            }
            
            Ok(refs)
        }
        Err(_) => {
            // Layer 2: Sidecar不存在，解析shard
            warn!("Sidecar缺失，解析shard: {}", shard_hash);
            let shard_data = self.storage.get("shards", shard_hash).await?;
            self.parse_shard_references(shard_hash, &shard_data).await
            
            // Layer 3: 解析失败 → 保守处理（不删除相关xorb）
            // 在调用方处理：返回空引用集 → Bloom Filter不包含 → 不删除
        }
    }
}
```

---

### 3.3 引用追踪器

#### Trait接口

```rust
pub trait ReferenceTracker: Send + Sync {
    /// 上传shard时记录引用关系
    async fn record_references(
        &self,
        shard_hash: &str,
        lfs_refs: &[String],
        xorb_refs: &[String],
    ) -> Result<(), GcError>;
    
    /// 删除shard时清理引用关系
    async fn remove_references(&self, shard_hash: &str) -> Result<(), GcError>;
    
    /// 获取所有引用关系（流式，避免内存溢出）
    async fn stream_all_references(
        &self,
    ) -> Result<BoxStream<'_, Result<ReferenceSet, GcError>>, GcError>;
    
    /// 健康检查
    async fn health_check(&self) -> Result<(), GcError>;
}

pub struct ReferenceSet {
    pub shard_hash: String,
    pub lfs_refs: Vec<String>,
    pub xorb_refs: Vec<String>,
}
```

#### S3实现：Sidecar文件

**存储结构**：
```
s3://bucket/
├── shards/
│   └── abc123...
├── shard_refs/
│   └── abc123....refs.json    ← ReferenceManifest JSON
├── xorbs/
│   └── ...
└── .gc/
    ├── checkpoint.json
    ├── bloom.bin
    └── lease.json
```

**Sidecar文件格式**：
```json
{
  "version": 1,
  "shard_hash": "abc123...",
  "lfs_refs": ["hash1", "hash2"],
  "xorb_refs": ["xorb1", "xorb2", "xorb3"],
  "created_at": "2026-06-14T10:00:00Z"
}
```

**上传钩子集成**：
```rust
// src/conversion/mod.rs
pub async fn upload_shard_with_refs(
    storage: Arc<dyn StorageBackend>,
    ref_tracker: Arc<dyn ReferenceTracker>,
    shard_hash: &str,
    shard_data: &[u8],
) -> Result<(), GcError> {
    // 1. 解析shard提取引用
    let refs = extract_references_from_shard(shard_data)?;
    
    // 2. 记录引用关系（同步，确保一致性）
    ref_tracker.record_references(
        shard_hash,
        &refs.lfs_refs,
        &refs.xorb_refs,
    ).await?;
    
    // 3. 存储shard
    storage.put("shards", shard_hash, shard_data).await?;
    
    Ok(())
}
```

#### 本地存储实现：SQLite缓存

```rust
pub struct LocalReferenceTracker {
    storage_root: PathBuf,
    cache_db: Arc<rusqlite::Connection>,
}

// SQLite表结构
// CREATE TABLE shard_references (
//     shard_hash TEXT PRIMARY KEY,
//     lfs_refs TEXT,      -- JSON array
//     xorb_refs TEXT,     -- JSON array
//     scanned_at INTEGER  -- Unix timestamp
// );
```

---

### 3.4 多节点协调（S3-based Lease）

#### Lease数据结构

```rust
pub struct GcLease {
    pub holder_node_id: String,
    pub expires_at: DateTime<Utc>,
    pub acquired_at: DateTime<Utc>,
    pub current_checkpoint: Option<GcCheckpoint>,
}
```

#### Lease获取流程

```rust
impl GcCoordinator {
    pub async fn try_acquire_lease(&self) -> Result<Option<GcLeaseGuard>, GcError> {
        // 1. 检查是否已有lease
        let existing_lease = match self.storage.get(".gc", "lease.json").await {
            Ok(data) => Some(serde_json::from_slice(&data)?),
            Err(_) => None,
        };
        
        // 2. 检查lease是否过期
        if let Some(ref lease) = existing_lease {
            if lease.expires_at > Utc::now() && lease.holder_node_id != self.node_id {
                return Ok(None); // Lease未过期，且不是我们持有
            }
        }
        
        // 3. 尝试获取lease（条件PUT）
        let new_lease = GcLease {
            holder_node_id: self.node_id.clone(),
            expires_at: Utc::now() + self.config.lease_ttl,
            acquired_at: Utc::now(),
            current_checkpoint: None,
        };
        
        match self.storage.put_if_absent_or_expired(
            ".gc", "lease.json",
            &serde_json::to_vec(&new_lease)?,
            existing_lease.as_ref(),
        ).await {
            Ok(_) => Ok(Some(GcLeaseGuard { coordinator: self, lease: new_lease })),
            Err(StorageError::ConditionFailed) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
```

#### Lease刷新（后台任务）

```rust
impl GcLeaseGuard {
    pub fn start_renewal_task(&mut self) -> tokio::task::JoinHandle<()> {
        let coordinator = self.coordinator;
        let mut lease = self.lease.clone();
        let renew_interval = coordinator.config.lease_renew_interval;
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(renew_interval);
            loop {
                interval.tick().await;
                if let Err(e) = coordinator.renew_lease(&mut lease).await {
                    error!("刷新lease失败: {}", e);
                    break;
                }
            }
        })
    }
}
```

---

### 3.5 双层Grace Period

```rust
pub struct GracePeriod {
    /// 绝对保护期：创建时间 < absolute_grace的对象永不删除
    pub absolute_grace: Duration,     // 默认：1小时
    
    /// 软保护期：必须连续soft_grace_cycles次GC都未被引用才删除
    pub soft_grace_cycles: u32,       // 默认：2次
    
    /// 跟踪未被引用的次数
    unreferenced_tracker: RwLock<HashMap<String, u32>>,
}

impl GracePeriod {
    pub async fn can_delete(&self, obj_meta: &ObjectMetadata) -> Result<bool, GcError> {
        let now = Utc::now();
        let age = now - obj_meta.last_modified;
        
        // 1. 绝对保护期检查
        if age.to_std()? < self.absolute_grace {
            return Ok(false);
        }
        
        // 2. 软保护期检查
        let mut tracker = self.unreferenced_tracker.write().await;
        let count = tracker.entry(obj_meta.key.clone()).or_insert(0);
        *count += 1;
        
        if *count < self.soft_grace_cycles {
            return Ok(false);
        }
        
        tracker.remove(&obj_meta.key);
        Ok(true)
    }
}
```

---

## 4. 配置系统

### 4.1 新增环境变量

| 变量 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `GC_ENABLED` | bool | `false` | 启用GC |
| `GC_INTERVAL_SECONDS` | u64 | `3600` | 运行间隔 |
| `GC_DATA_DIR` | string | `/var/lib/cas/gc` | 数据目录 |
| `GC_BLOOM_EXPECTED_ITEMS` | u64 | `10000000` | Bloom Filter容量 |
| `GC_BLOOM_FALSE_POSITIVE_RATE` | f64 | `0.001` | 误判率 |
| `GC_BLOOM_REBUILD_THRESHOLD` | f64 | `0.8` | 重建阈值 |
| `GC_SCANNER_PAGE_SIZE` | usize | `1000` | 分页大小 |
| `GC_SCANNER_CHECKPOINT_INTERVAL` | u64 | `10000` | Checkpoint间隔 |
| `GC_SCANNER_MAX_DURATION_SECONDS` | u64 | `1800` | 最大扫描时间 |
| `GC_GRACE_ABSOLUTE_SECONDS` | u64 | `3600` | 绝对保护期 |
| `GC_GRACE_SOFT_CYCLES` | u32 | `2` | 软保护期次数 |
| `GC_LEASE_TTL_SECONDS` | u64 | `3600` | Lease有效期 |
| `GC_LEASE_RENEW_INTERVAL_SECONDS` | u64 | `600` | Lease刷新间隔 |
| `GC_REFERENCE_TRACKER_MODE` | string | `sidecar` | 引用追踪模式 |
| `GC_LOCAL_CACHE_DB_PATH` | string | `/var/lib/cas/gc/refs.db` | 本地缓存路径 |
| `GC_DRY_RUN` | bool | `true` | Dry-run模式 |
| `GC_DELETE_BATCH_SIZE` | usize | `100` | 删除批大小 |
| `GC_DELETE_MAX_RETRIES` | u32 | `3` | 删除重试次数 |

### 4.2 已废弃配置

- `GC_HUB_BASE_URL` - 不再需要
- `GC_HUB_INTERNAL_TOKEN` - 不再需要
- `GC_HUB_TIMEOUT_SECONDS` - 不再需要

---

## 5. 监控与指标

### 5.1 Prometheus指标

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `gc_cycles_total` | Counter | GC周期总数 |
| `gc_cycles_success_total` | Counter | 成功数 |
| `gc_cycles_failed_total` | Counter | 失败数 |
| `gc_cycles_skipped_total` | Counter | 跳过数（未获取lease） |
| `gc_shards_scanned_total` | Counter | 扫描shard总数 |
| `gc_xorbs_scanned_total` | Counter | 扫描xorb总数 |
| `gc_blobs_deleted_total` | Counter | 删除blob总数 |
| `gc_bytes_freed_total` | Counter | 释放字节数 |
| `gc_delete_errors_total` | Counter | 删除失败数 |
| `gc_bloom_queries_total` | Counter | Bloom查询总数 |
| `gc_bloom_hits_total` | Counter | Bloom命中数 |
| `gc_bloom_rebuilds_total` | Counter | Bloom重建次数 |
| `gc_bloom_items_current` | Gauge | 当前Bloom元素数 |
| `gc_bloom_memory_bytes` | Gauge | Bloom内存占用 |
| `gc_sidecar_missing_total` | Counter | Sidecar缺失次数 |
| `gc_lease_acquired_total` | Counter | Lease获取成功数 |
| `gc_lease_failed_total` | Counter | Lease获取失败数 |
| `gc_lease_renewals_total` | Counter | Lease刷新次数 |
| `gc_cycle_duration_seconds` | Histogram | GC周期耗时 |
| `gc_scan_speed_shards_per_second` | Histogram | 扫描速度 |

### 5.2 健康检查端点

```
GET /gc/health

Response:
{
  "status": "healthy",
  "bloom_filter": {
    "items": 5000000,
    "memory_bytes": 18000000,
    "false_positive_rate": 0.001,
    "rebuild_count": 2
  },
  "last_gc_cycle": {
    "completed_at": "2026-06-14T10:00:00Z",
    "duration_seconds": 120,
    "shards_scanned": 10000,
    "blobs_deleted": 50,
    "bytes_freed": 52428800
  },
  "lease": {
    "held_by": "node-1",
    "expires_at": "2026-06-14T11:00:00Z",
    "acquired_at": "2026-06-14T10:00:00Z"
  },
  "sidecar_coverage": {
    "total_shards": 100000,
    "with_sidecar": 95000,
    "coverage_percent": 95.0
  }
}
```

---

## 6. 边缘情况处理

### 6.1 Bloom Filter损坏

**检测**：CRC32校验失败

**恢复**：
1. 创建新的Bloom Filter
2. 重置checkpoint，从头扫描
3. 记录warning日志
4. 不中断服务，下次GC周期自动恢复

### 6.2 Checkpoint损坏

**检测**：CRC32校验失败

**恢复**：
1. 创建新checkpoint
2. 从头开始新的GC周期
3. 记录warning日志

### 6.3 Sidecar缺失

**检测**：读取sidecar文件失败

**恢复**：
1. 自动fallback到解析shard文件
2. 异步生成sidecar（不阻塞GC）
3. 递增`gc_sidecar_missing_total`指标

### 6.4 S3最终一致性

**问题**：刚删除的对象可能仍然可见

**处理**：
1. 删除后等待100ms再验证
2. 不返回错误，继续处理下一个
3. 下次GC周期会再次检查

### 6.5 节点崩溃

**检测**：新leader获取lease后发现旧lease未过期

**恢复**：
1. 检查旧checkpoint
2. 从crashed leader的cursor位置继续
3. 释放旧lease，获取新lease

### 6.6 引用数据不完整

**检测**：sidecar中的引用数量与shard实际引用数量不匹配

**处理**：
1. 记录error日志
2. 使用shard解析结果（权威来源）
3. 重新生成sidecar

---

## 7. 测试策略

### 7.1 单元测试

**Bloom Filter**:
- 插入/查询正确性
- 持久化/反序列化
- CRC32校验
- 容量阈值触发重建
- 双缓冲重建不中断服务

**Scanner**:
- 分页列出对象
- Checkpoint保存/恢复
- 增量扫描（从cursor继续）
- 超时保护
- Sidecar缺失fallback

**ReferenceTracker**:
- S3: sidecar读写
- Local: SQLite缓存命中/未命中
- Shard解析正确性
- 引用数量验证

**Coordinator**:
- Lease获取（成功/失败）
- Lease刷新
- Lease释放
- Lease过期检测
- 故障恢复

**GracePeriod**:
- 绝对保护期
- 软保护期计数器
- 达到阈值后允许删除

### 7.2 集成测试

**完整GC周期**：
1. 上传shard和xorb
2. 运行GC
3. 验证孤立xorb被删除
4. 验证引用xorb保留

**Sidecar缺失恢复**：
1. 上传shard但不创建sidecar
2. 运行GC
3. 验证GC fallback到解析shard
4. 验证sidecar被自动创建

**多节点协调**：
1. 启动2个GC实例（不同node_id）
2. 同时触发GC
3. 验证只有1个获取lease
4. 另一个跳过本次GC

**崩溃恢复**：
1. 运行GC到一半，模拟崩溃
2. 重启GC
3. 验证从checkpoint继续
4. 验证最终结果正确

**大规模数据**：
1. 生成100万shards
2. 运行GC
3. 验证扫描速度 > 1000 shards/second
4. 验证内存占用 < 50MB

### 7.3 性能测试

| 指标 | 目标值 | 测试方法 |
|------|--------|----------|
| 扫描速度 | > 1000 shards/second | 100万shards |
| Bloom查询延迟 | < 1μs | 基准测试 |
| 内存占用 | < 50MB (10M items) | 内存profiling |
| GC周期耗时 | < 30分钟 (100万shards) | 端到端测试 |
| S3 API调用 | < 10000/cycle | 监控 |

---

## 8. 迁移计划

### 8.1 4阶段迁移

**阶段1：部署新代码（禁用）- 1-2周**
- 部署新GC代码，`GC_ENABLED=false`
- 验证代码无bug，不影响现有系统
- 回滚：删除新代码

**阶段2：Dry-run模式 - 1-2周**
- `GC_DRY_RUN=true`
- 观察metrics：候选删除集、sidecar缺失率、Bloom误判率
- 验证逻辑正确性
- 关键指标：
  * `gc_candidates_found_total`（应该合理）
  * `gc_sidecar_missing_total`（历史数据迁移进度）
  * `gc_bloom_queries_total` vs `gc_bloom_hits_total`（误判率）

**阶段3：实际删除 + 旧GC并行 - 2-4周**
- `GC_DRY_RUN=false`
- 新旧GC并行运行，对比删除结果
- 监控数据完整性
- 关键验证：新旧GC删除结果一致

**阶段4：完全切换 - 永久**
- 禁用旧GC（`GC_ENABLED=false` on old GC）
- 批量迁移脚本生成sidecar
- 移除旧GC代码和Hub接口
- 更新文档

### 8.2 数据迁移脚本

```rust
// tools/migrate_gc_sidecars.rs
/// 为历史shard批量生成sidecar文件
async fn migrate_sidecars(storage: Arc<dyn StorageBackend>) -> Result<()> {
    info!("开始批量生成sidecar文件");
    
    let shards = storage.list_objects("shards").await?;
    let total = shards.len();
    let mut migrated = 0;
    let mut skipped = 0;
    let mut errors = 0;
    
    for (i, shard_meta) in shards.iter().enumerate() {
        let shard_hash = shard_meta.key.strip_prefix("shards/").unwrap();
        let sidecar_key = format!("{}.refs.json", shard_hash);
        
        // 跳过已有sidecar的
        if storage.exists("shard_refs", &sidecar_key).await? {
            skipped += 1;
            continue;
        }
        
        // 解析shard，生成sidecar
        match storage.get("shards", shard_hash).await {
            Ok(shard_data) => {
                match parse_shard_references(&shard_data) {
                    Ok(refs) => {
                        let refs_json = serde_json::to_vec_pretty(&refs)?;
                        storage.put("shard_refs", &sidecar_key, &refs_json).await?;
                        migrated += 1;
                        
                        if i % 100 == 0 {
                            info!("进度: {}/{} (migrated={}, skipped={}, errors={})", 
                                  i, total, migrated, skipped, errors);
                        }
                    }
                    Err(e) => {
                        error!("解析shard失败: {} - {}", shard_hash, e);
                        errors += 1;
                    }
                }
            }
            Err(e) => {
                error!("读取shard失败: {} - {}", shard_hash, e);
                errors += 1;
            }
        }
    }
    
    info!("迁移完成: total={}, migrated={}, skipped={}, errors={}", 
          total, migrated, skipped, errors);
    
    Ok(())
}
```

---

## 9. 新增依赖

```toml
[dependencies]
bloomfilter = "1.0.8"     # Bloom Filter实现
crc32fast = "1.3"         # CRC32校验
bincode = "1.3"           # 二进制序列化
rusqlite = { version = "0.31", features = ["bundled"] }  # 本地缓存（可选）
```

---

## 10. 风险与缓解

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|----------|
| Bloom Filter误判 | 保留孤立数据（不删除） | 低（0.1%） | 可接受；定期重建 |
| Sidecar丢失 | GC需解析shard（慢） | 中 | 自动生成sidecar，监控缺失率 |
| Lease竞态 | 多节点同时GC | 低 | S3条件PUT + 3层安全检查 |
| Checkpoint损坏 | 重头扫描 | 低 | CRC32校验 + 自动重建 |
| S3最终一致性 | 删除后仍可见 | 中 | 延迟验证 + 容忍 |
| 数据迁移不完整 | 部分shard无sidecar | 高（初期） | fallback到解析 + 迁移脚本 |
| 内存占用过高 | OOM | 低 | 监控 + 自动重建 |
| GC周期过长 | 超时 | 中 | 30分钟超时保护 |

---

## 11. 关键文件清单

| 文件路径 | 说明 | 状态 | 优先级 |
|----------|------|------|--------|
| `src/gc/errors.rs` | 错误类型定义 | 新建 | P0 |
| `src/gc/bloom.rs` | Bloom Filter实现 | 新建 | P0 |
| `src/gc/checkpoint.rs` | Checkpoint管理 | 新建 | P0 |
| `src/gc/grace.rs` | Grace Period | 新建 | P0 |
| `src/gc/reference_tracker/mod.rs` | Trait定义 | 新建 | P0 |
| `src/gc/reference_tracker/s3.rs` | S3 sidecar实现 | 新建 | P0 |
| `src/gc/scanner.rs` | 增量扫描器 | 新建 | P0 |
| `src/gc/coordinator.rs` | 多节点协调 | 新建 | P1 |
| `src/gc/reference_tracker/local.rs` | 本地缓存实现 | 新建 | P1 |
| `src/gc/metrics.rs` | Prometheus指标 | 新建 | P1 |
| `src/gc/mod.rs` | GC主模块，重写 | 修改 | P0 |
| `src/config.rs` | GC配置 | 修改 | P0 |
| `src/storage/mod.rs` | Storage trait扩展 | 修改 | P0 |
| `src/storage/s3.rs` | S3分页列出 | 修改 | P0 |
| `src/storage/local.rs` | 本地分页列出 | 修改 | P1 |
| `src/conversion/mod.rs` | 上传钩子 | 修改 | P0 |
| `src/api/gc.rs` | GC API端点 | 修改 | P1 |
| `hub/src/api/internal.rs` | Hub内部接口 | 废弃 | P2 |
| `tools/migrate_gc_sidecars.rs` | 迁移脚本 | 新建 | P2 |

---

## 12. 验收标准

### 12.1 功能验收

- [ ] 单元测试覆盖率 > 80%
- [ ] 集成测试通过所有场景
- [ ] 多节点测试验证lease协调
- [ ] 崩溃恢复测试通过
- [ ] Sidecar缺失恢复测试通过

### 12.2 性能验收

- [ ] 扫描速度 > 1000 shards/second
- [ ] Bloom Filter查询延迟 < 1μs
- [ ] 内存占用 < 50MB（10M items）
- [ ] GC周期耗时 < 30分钟（100万shards）

### 12.3 可靠性验收

- [ ] 连续运行7天无数据丢失
- [ ] 注入故障（网络断开、S3超时）验证恢复
- [ ] 对比新旧GC删除结果一致性 > 99.9%

### 12.4 迁移验收

- [ ] Dry-run模式下0误删
- [ ] Sidecar迁移完整性 > 99.9%
- [ ] 旧GC并行期无冲突

---

## 13. 总结

### 13.1 核心优势

1. ✅ **完全去除Hub依赖** - CAS独立运行GC
2. ✅ **增量扫描** - 性能随数据量线性增长
3. ✅ **Bloom Filter保护** - 内存效率高（~20MB），误判率低（0.1%）
4. ✅ **多节点协调** - S3-based lease，无外部依赖
5. ✅ **崩溃恢复** - Checkpoint + 自动恢复
6. ✅ **统一架构** - S3和本地存储使用相同的sidecar格式
7. ✅ **平滑迁移** - 4阶段渐进式切换

### 13.2 已解决的原问题

1. ✅ **Hub接口耦合** → 完全去除，CAS侧引用追踪
2. ✅ **多节点竞态** → S3-based lease协调
3. ✅ **性能瓶颈** → 增量扫描 + Bloom Filter

### 13.3 下一步行动

1. 创建详细实现计划（使用writing-plans技能）
2. 按优先级P0 → P1 → P2实现各组件
3. 编写单元测试和集成测试
4. 执行4阶段迁移计划
5. 监控生产环境指标

---

**文档结束**
