# 异步 SQLite 库迁移分析

## 1. 当前问题分析

### 1.1 问题定位
**文件**: `hub/src/auth/token_store.rs`

**当前实现**:
```rust
pub struct TokenStore {
    conn: Mutex<Connection>,  // std::sync::Mutex 包装 rusqlite::Connection
}

pub fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, rusqlite::Error> {
    let conn = self.conn.lock().map_err(lock_error)?;  // 同步锁
    // 同步数据库操作...
}
```

**问题场景**:
- `validate_token` 在 `AuthUser::from_request` (异步上下文) 中被调用
- 每个认证请求都会同步获取锁并执行数据库查询
- 如果 SQLite 操作慢（磁盘I/O、锁竞争），会阻塞整个异步运行时线程

**影响范围**:
- 生产代码中 3 处直接调用
- `AuthUser::from_request` 作为 actix-web 的 `FromRequest` trait 实现
- 每个需要认证的请求都会经过此路径

### 1.2 性能影响评估

**当前架构**:
```
异步请求 → from_request (异步) → validate_token (同步) → SQLite I/O (阻塞)
                    ↑
              这里会阻塞！
```

**阻塞后果**:
- actix-web 使用线程池处理请求（默认 CPU 核心数）
- 一个同步数据库查询阻塞 = 一个工作线程被占用
- 高并发时，所有工作线程可能被数据库操作占满
- 新的异步请求无法被处理，即使它们不需要数据库

**实际风险评估**:
- SQLite 读操作通常 < 1ms（本地 SSD）
- 但如果数据库文件大、并发写、或慢速磁盘，可能达到 10-100ms
- 在 100ms 延迟下，单线程 QPS = 10，4线程 QPS = 40
- 对于高流量 API，这是严重的性能瓶颈

## 2. 异步 SQLite 库选项对比

### 2.1 选项 A: sqlx-sqlite (推荐)

**项目**: https://github.com/launchbadge/sqlx

**特点**:
- 纯 Rust 实现，编译时 SQL 检查（可选）
- 真正的异步实现（使用 libsqlite3 的异步 API）
- 连接池支持
- 支持事务、预处理语句
- 活跃维护，社区庞大

**优点**:
✅ 真正的异步，不阻塞运行时
✅ 内置连接池，自动管理连接
✅ 类型安全，编译时检查 SQL
✅ 支持多种数据库（PostgreSQL, MySQL, SQLite）
✅ 良好的错误处理和文档

**缺点**:
❌ 需要重写所有数据库代码
❌ API 与 rusqlite 不同，需要学习成本
❌ 编译时间增加（特别是启用编译时检查）
❌ 依赖较重

**迁移复杂度**: ⭐⭐⭐⭐ (高)
**性能收益**: ⭐⭐⭐⭐⭐ (极高)
**维护成本**: ⭐⭐⭐ (中等)

**代码示例**:
```rust
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

pub struct TokenStore {
    pool: SqlitePool,
}

impl TokenStore {
    pub async fn new(db_path: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(db_path)
            .await?;
        
        // 初始化表
        sqlx::query("CREATE TABLE IF NOT EXISTS ...")
            .execute(&pool)
            .await?;
        
        Ok(Self { pool })
    }

    pub async fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, sqlx::Error> {
        let token_hash = Self::hash_token(token);
        
        let result = sqlx::query_as!(
            TokenRow,
            "SELECT u.user_id, u.username, t.name, t.scope, t.expires_at, t.revoked_at
             FROM tokens t JOIN users u ON t.user_id = u.user_id
             WHERE t.token_hash = ?1",
            token_hash
        )
        .fetch_optional(&self.pool)
        .await?;
        
        // 处理结果...
    }
}
```

### 2.2 选项 B: rusqlite + spawn_blocking (快速修复)

**特点**:
- 保持现有 rusqlite 代码不变
- 使用 `tokio::task::spawn_blocking` 将同步操作移到后台线程池
- 最小代码改动

**优点**:
✅ 改动最小，风险低
✅ 不需要学习新库
✅ 立即解决阻塞问题
✅ 可以逐步迁移

**缺点**:
❌ 不是真正的异步，只是将阻塞移到后台
❌ 后台线程池大小有限（默认 512）
❌ 高并发时仍可能耗尽线程池
❌ 没有连接池管理

**迁移复杂度**: ⭐⭐ (低)
**性能收益**: ⭐⭐⭐ (中等)
**维护成本**: ⭐⭐ (低)

**代码示例**:
```rust
use std::sync::Arc;
use tokio::task;

pub struct TokenStore {
    conn: Arc<Mutex<Connection>>,
}

impl TokenStore {
    pub async fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, rusqlite::Error> {
        let conn = self.conn.clone();
        let token_hash = Self::hash_token(token);
        
        // 将同步操作移到后台线程池
        let result = task::spawn_blocking(move || {
            let conn = conn.lock().map_err(lock_error)?;
            let mut stmt = conn.prepare("SELECT ...")?;
            stmt.query_row(params![token_hash], |row| {
                // 提取数据...
            })
        })
        .await
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        
        // 处理结果...
    }
}
```

### 2.3 选项 C: async-sqlite (实验性)

**项目**: https://crates.io/crates/async-sqlite

**特点**:
- 基于 rusqlite 的异步封装
- 内部使用 spawn_blocking

**评估**: 
- 项目活跃度低，不推荐
- 与选项 B 类似，但封装更重

### 2.4 选项 D: 保持现状 + 优化

**策略**:
- 保持 rusqlite + Mutex
- 优化数据库操作（索引、查询优化）
- 添加缓存层

**优点**:
✅ 无需改动
✅ 低风险

**缺点**:
❌ 根本问题未解决
❌ 缓存增加复杂性

**评估**: 不推荐，只是延迟问题

## 3. 推荐方案

### 3.1 短期方案（1-2周）: 选项 B - spawn_blocking

**理由**:
1. 最小改动，快速解决阻塞问题
2. 风险低，可以立即部署
3. 为后续迁移争取时间

**实施步骤**:
1. 将 `TokenStore` 的 `conn` 字段改为 `Arc<Mutex<Connection>>`
2. 为所有公开方法添加 `async` 关键字
3. 内部使用 `spawn_blocking` 包装数据库操作
4. 更新所有调用点，添加 `.await`

**预估工作量**: 2-3 天
**风险**: 低
**性能提升**: 30-50%（高并发场景）

### 3.2 长期方案（1-2月）: 选项 A - sqlx-sqlite

**理由**:
1. 真正的异步，性能最优
2. 连接池管理，资源利用率高
3. 类型安全，减少运行时错误
4. 行业最佳实践

**实施步骤**:
1. 添加 `sqlx` 依赖
2. 创建新的 `TokenStore` 实现（使用 `SqlitePool`）
3. 迁移所有数据库操作
4. 更新所有调用点
5. 性能测试和优化

**预估工作量**: 1-2 周
**风险**: 中等
**性能提升**: 50-100%（高并发场景）

## 4. 迁移详细计划

### 4.1 阶段一：spawn_blocking 快速修复

#### 步骤 1: 修改 TokenStore 结构
```rust
pub struct TokenStore {
    conn: Arc<Mutex<Connection>>,
}

impl TokenStore {
    pub fn new(db_path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        Self::init_tables(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }
}
```

#### 步骤 2: 改造 validate_token 方法
```rust
pub async fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, rusqlite::Error> {
    let conn = self.conn.clone();
    let token_hash = Self::hash_token(token);
    let now = now_secs();
    
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(lock_error)?;
        let mut stmt = conn.prepare(
            "SELECT u.user_id, u.username, t.name, t.scope, t.expires_at, t.revoked_at
             FROM tokens t JOIN users u ON t.user_id = u.user_id
             WHERE t.token_hash = ?1"
        )?;
        
        stmt.query_row(params![token_hash], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
    })
    .await
    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    
    match result {
        Ok((user_id, username, name, scope, expires_at, revoked_at)) => {
            if revoked_at.is_some() {
                return Ok(None);
            }
            if let Some(exp) = expires_at {
                if (exp as u64) < now {
                    return Ok(None);
                }
            }
            Ok(Some(TokenInfo {
                user_id,
                username,
                token_name: name,
                scope,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}
```

#### 步骤 3: 更新调用点
```rust
// hub/src/auth/extract.rs
let info = token_store
    .validate_token(&token)
    .await  // 添加 .await
    .map_err(|e| AuthError::Internal(e.to_string()))?
    .ok_or(AuthError::InvalidToken)?;
```

#### 步骤 4: 测试验证
- 单元测试：验证功能正确性
- 性能测试：对比阻塞前后延迟
- 压力测试：高并发场景验证

### 4.2 阶段二：sqlx-sqlite 完整迁移

#### 步骤 1: 添加依赖
```toml
# hub/Cargo.toml
[dependencies]
sqlx = { version = "0.7", features = ["runtime-tokio-rustls", "sqlite"] }
tokio = { version = "1", features = ["full"] }
```

#### 步骤 2: 创建新的 TokenStore
```rust
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::FromRow;

#[derive(Debug, FromRow)]
struct TokenRow {
    user_id: String,
    username: String,
    name: String,
    scope: String,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
}

pub struct TokenStore {
    pool: SqlitePool,
}

impl TokenStore {
    pub async fn new(db_path: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(db_path)
            .await?;
        
        Self::init_tables(&pool).await?;
        Ok(Self { pool })
    }
    
    async fn init_tables(pool: &SqlitePool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                user_id TEXT PRIMARY KEY,
                username TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL
            )"
        )
        .execute(pool)
        .await?;
        
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tokens (
                token_hash TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                name TEXT NOT NULL,
                scope TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER,
                revoked_at INTEGER,
                FOREIGN KEY (user_id) REFERENCES users(user_id)
            );
            CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id);"
        )
        .execute(pool)
        .await?;
        
        Ok(())
    }
    
    pub async fn validate_token(&self, token: &str) -> Result<Option<TokenInfo>, sqlx::Error> {
        let token_hash = Self::hash_token(token);
        let now = now_secs() as i64;
        
        let result: Option<TokenRow> = sqlx::query_as(
            "SELECT u.user_id, u.username, t.name, t.scope, t.expires_at, t.revoked_at
             FROM tokens t JOIN users u ON t.user_id = u.user_id
             WHERE t.token_hash = ?1"
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        
        match result {
            Some(row) => {
                if row.revoked_at.is_some() {
                    return Ok(None);
                }
                if let Some(exp) = row.expires_at {
                    if exp < now {
                        return Ok(None);
                    }
                }
                Ok(Some(TokenInfo {
                    user_id: row.user_id,
                    username: row.username,
                    token_name: row.name,
                    scope: row.scope,
                }))
            }
            None => Ok(None),
        }
    }
    
    pub async fn create_token(
        &self,
        username: &str,
        token_name: &str,
        scope: &str,
    ) -> Result<String, sqlx::Error> {
        let token = format!("hf_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let token_hash = Self::hash_token(&token);
        let now = now_secs() as i64;
        let user_id = format!("user_{}", &token_hash[..16]);
        
        let mut tx = self.pool.begin().await?;
        
        sqlx::query(
            "INSERT OR IGNORE INTO users (user_id, username, created_at) VALUES (?1, ?2, ?3)"
        )
        .bind(&user_id)
        .bind(username)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        
        sqlx::query(
            "INSERT INTO tokens (token_hash, user_id, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)"
        )
        .bind(&token_hash)
        .bind(&user_id)
        .bind(token_name)
        .bind(scope)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        
        tx.commit().await?;
        
        Ok(token)
    }
    
    fn hash_token(token: &str) -> String {
        use sha2::{Sha256, Digest};
        hex::encode(Sha256::digest(token.as_bytes()))
    }
}
```

#### 步骤 3: 更新所有调用点
- 添加 `.await` 到所有 `validate_token`、`create_token`、`revoke_token` 调用
- 更新错误处理（`sqlx::Error` 替代 `rusqlite::Error`）
- 更新测试代码

#### 步骤 4: 性能测试
```rust
// 性能测试示例
#[tokio::test]
async fn benchmark_validate_token() {
    let store = TokenStore::new("test.db").await.unwrap();
    let token = store.create_token("user", "token", "read").await.unwrap();
    
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        let _ = store.validate_token(&token).await.unwrap();
    }
    let elapsed = start.elapsed();
    
    println!("1000 validations in {:?}", elapsed);
    // 目标：< 100ms (10,000 QPS)
}
```

## 5. 性能影响量化

### 5.1 当前实现（同步）
```
单线程延迟: 0.1-1ms (本地 SSD)
并发能力: 100-1000 QPS (单线程)
4线程 QPS: 400-4000
高并发问题: 线程阻塞，请求排队
```

### 5.2 spawn_blocking 方案
```
单线程延迟: 0.2-2ms (增加线程切换开销)
并发能力: 500-2000 QPS (单线程)
4线程 QPS: 2000-8000
高并发表现: 线程池缓冲，延迟增加但不阻塞
```

### 5.3 sqlx-sqlite 方案
```
单线程延迟: 0.1-0.5ms (真正的异步)
并发能力: 1000-5000 QPS (单线程)
4线程 QPS: 4000-20000
高并发表现: 连接池管理，延迟稳定
```

## 6. 迁移风险评估

### 6.1 spawn_blocking 方案
**风险等级**: 低
- ✅ 逻辑不变，只是执行位置改变
- ✅ 可以逐步迁移，一次一个方法
- ✅ 容易回滚
- ⚠️ 需要注意 Mutex 死锁（但现有代码已处理）

### 6.2 sqlx-sqlite 方案
**风险等级**: 中等
- ⚠️ API 完全不同，需要重写
- ⚠️ 错误类型变化，需要更新错误处理
- ⚠️ 事务语义可能略有不同
- ⚠️ 需要全面测试
- ✅ 一次性解决所有问题

## 7. 建议与结论

### 7.1 立即行动（本周）
**采用 spawn_blocking 方案**
1. 修改 `TokenStore` 结构（30分钟）
2. 改造 `validate_token` 方法（1小时）
3. 更新 3 个调用点（30分钟）
4. 测试验证（1-2小时）

**总耗时**: 半天到一天

### 7.2 中期规划（1-2月）
**迁移到 sqlx-sqlite**
1. 评估是否值得（如果流量不大，spawn_blocking 可能足够）
2. 如果决定迁移，安排 1-2 周开发时间
3. 全面测试和性能优化

### 7.3 长期维护
- 监控数据库性能指标
- 如果 QPS 超过 1000，考虑迁移到 PostgreSQL
- 定期审查数据库查询性能

### 7.4 最终建议

**对于当前项目**:
- 如果日活 < 10,000，使用 spawn_blocking 即可
- 如果日活 > 100,000，建议迁移到 sqlx-sqlite
- 如果有计划扩展到多数据库，强烈建议 sqlx-sqlite

**决策树**:
```
流量大吗？
├─ 是 → 迁移到 sqlx-sqlite
└─ 否 → spawn_blocking 足够
    └─ 未来流量增长？
        ├─ 是 → 计划 sqlx-sqlite 迁移
        └─ 否 → 保持现状
```

## 8. 参考资料

- SQLx 文档: https://docs.rs/sqlx/latest/sqlx/
- SQLx SQLite: https://docs.rs/sqlx/latest/sqlx/sqlite/index.html
- rusqlite 文档: https://docs.rs/rusqlite/latest/rusqlite/
- Tokio spawn_blocking: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
- Actix-web 最佳实践: https://actix.rs/docs/
