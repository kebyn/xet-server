use hub_api::auth::token_store::TokenStore;
use hub_api::metadata::SqliteMetadataStore;
use hub_api::migrations::{CURRENT_SCHEMA_VERSION, run_hub_migrations};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

async fn memory_pool() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("create in-memory sqlite pool")
}

async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(name)
            .fetch_one(pool)
            .await
            .expect("query sqlite_master");
    count == 1
}

async fn schema_version(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT version FROM schema_version")
        .fetch_one(pool)
        .await
        .expect("read schema version")
}

#[tokio::test]
async fn migrations_create_all_hub_tables_and_version() {
    let pool = memory_pool().await;

    run_hub_migrations(&pool).await.expect("run migrations");

    for table in [
        "schema_version",
        "users",
        "tokens",
        "_config",
        "repos",
        "revisions",
        "heads",
        "file_tree",
    ] {
        assert!(table_exists(&pool, table).await, "{table} should exist");
    }
    assert_eq!(schema_version(&pool).await, CURRENT_SCHEMA_VERSION);
}

#[tokio::test]
async fn token_and_metadata_stores_share_migration_runner() {
    let pool = memory_pool().await;

    let _tokens = TokenStore::with_pool(pool.clone())
        .await
        .expect("token store init");
    let _metadata = SqliteMetadataStore::with_pool(pool.clone())
        .await
        .expect("metadata store init");

    assert!(table_exists(&pool, "tokens").await);
    assert!(table_exists(&pool, "file_tree").await);
    assert_eq!(schema_version(&pool).await, CURRENT_SCHEMA_VERSION);
}

#[tokio::test]
async fn migrations_backfill_missing_version_for_existing_v1_schema() {
    let pool = memory_pool().await;

    sqlx::query(
        "CREATE TABLE users (
            user_id TEXT PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE tokens (
            token_hash TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            name TEXT NOT NULL,
            scope TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER,
            revoked_at INTEGER,
            FOREIGN KEY (user_id) REFERENCES users(user_id)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_tokens_user ON tokens(user_id)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE _config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE repos (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            namespace TEXT NOT NULL,
            repo_type TEXT NOT NULL,
            sha TEXT,
            private INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(namespace, name, repo_type)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE revisions (
            commit_id TEXT PRIMARY KEY,
            repo_id INTEGER NOT NULL,
            parent TEXT,
            message TEXT NOT NULL,
            author TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (repo_id) REFERENCES repos(id)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE heads (
            repo_id INTEGER PRIMARY KEY,
            commit_id TEXT NOT NULL,
            FOREIGN KEY (repo_id) REFERENCES repos(id),
            FOREIGN KEY (commit_id) REFERENCES revisions(commit_id)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE file_tree (
            path TEXT NOT NULL,
            repo_id INTEGER NOT NULL,
            commit_id TEXT NOT NULL,
            size INTEGER NOT NULL,
            cas_hash TEXT NOT NULL,
            is_lfs INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (path, repo_id, commit_id),
            FOREIGN KEY (repo_id) REFERENCES repos(id),
            FOREIGN KEY (commit_id) REFERENCES revisions(commit_id)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_file_tree_prefix ON file_tree(repo_id, commit_id, path)")
        .execute(&pool)
        .await
        .unwrap();

    run_hub_migrations(&pool)
        .await
        .expect("backfill version for recognized v1 schema");

    assert_eq!(schema_version(&pool).await, CURRENT_SCHEMA_VERSION);
}

#[tokio::test]
async fn migrations_reject_future_schema_version() {
    let pool = memory_pool().await;
    sqlx::query("CREATE TABLE schema_version (version INTEGER NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO schema_version (version) VALUES (?1)")
        .bind(CURRENT_SCHEMA_VERSION + 1)
        .execute(&pool)
        .await
        .unwrap();

    let err = run_hub_migrations(&pool)
        .await
        .expect_err("future schema version should fail");

    assert!(err.to_string().contains("newer than supported"));
}

#[tokio::test]
async fn migrations_reject_unrecognized_existing_schema() {
    let pool = memory_pool().await;
    sqlx::query("CREATE TABLE repos (id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();

    let err = run_hub_migrations(&pool)
        .await
        .expect_err("partial existing schema should fail");

    assert!(err.to_string().contains("existing Hub schema"));

    let columns: Vec<String> = sqlx::query("PRAGMA table_info(repos)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
    assert_eq!(columns, vec!["id"]);
}
