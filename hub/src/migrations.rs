use std::collections::BTreeSet;

use sqlx::Row;
use sqlx::sqlite::SqlitePool;

pub const CURRENT_SCHEMA_VERSION: i64 = 1;

const V1_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    user_id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tokens (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL,
    scope TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    revoked_at INTEGER,
    FOREIGN KEY (user_id) REFERENCES users(user_id)
);

CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id);

CREATE TABLE IF NOT EXISTS _config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL,
    repo_type TEXT NOT NULL,
    sha TEXT,
    private INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(namespace, name, repo_type)
);

CREATE TABLE IF NOT EXISTS revisions (
    commit_id TEXT PRIMARY KEY,
    repo_id INTEGER NOT NULL,
    parent TEXT,
    message TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (repo_id) REFERENCES repos(id)
);

CREATE TABLE IF NOT EXISTS heads (
    repo_id INTEGER PRIMARY KEY,
    commit_id TEXT NOT NULL,
    FOREIGN KEY (repo_id) REFERENCES repos(id),
    FOREIGN KEY (commit_id) REFERENCES revisions(commit_id)
);

CREATE TABLE IF NOT EXISTS file_tree (
    path TEXT NOT NULL,
    repo_id INTEGER NOT NULL,
    commit_id TEXT NOT NULL,
    size INTEGER NOT NULL,
    cas_hash TEXT NOT NULL,
    is_lfs INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (path, repo_id, commit_id),
    FOREIGN KEY (repo_id) REFERENCES repos(id),
    FOREIGN KEY (commit_id) REFERENCES revisions(commit_id)
);

CREATE INDEX IF NOT EXISTS idx_file_tree_prefix ON file_tree(repo_id, commit_id, path);
"#;

const REQUIRED_V1_COLUMNS: &[(&str, &[&str])] = &[
    ("schema_version", &["version"]),
    ("users", &["user_id", "username", "created_at"]),
    (
        "tokens",
        &[
            "token_hash",
            "user_id",
            "name",
            "scope",
            "created_at",
            "expires_at",
            "revoked_at",
        ],
    ),
    ("_config", &["key", "value", "created_at"]),
    (
        "repos",
        &[
            "id",
            "name",
            "namespace",
            "repo_type",
            "sha",
            "private",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "revisions",
        &[
            "commit_id",
            "repo_id",
            "parent",
            "message",
            "author",
            "created_at",
        ],
    ),
    ("heads", &["repo_id", "commit_id"]),
    (
        "file_tree",
        &["path", "repo_id", "commit_id", "size", "cas_hash", "is_lfs"],
    ),
];

pub async fn run_hub_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let has_version_table = table_exists(pool, "schema_version").await?;

    if !has_version_table {
        if has_any_hub_table(pool).await? {
            validate_existing_v1_schema_without_version(pool).await?;
            create_schema_version(pool).await?;
            set_schema_version_if_empty(pool).await?;
        } else {
            apply_v1_schema(pool).await?;
        }
        return Ok(());
    }

    let versions: Vec<i64> = sqlx::query_scalar("SELECT version FROM schema_version")
        .fetch_all(pool)
        .await?;

    if versions.is_empty() {
        if has_any_hub_table(pool).await? {
            validate_existing_v1_schema(pool).await?;
            set_schema_version_if_empty(pool).await?;
        } else {
            apply_v1_schema(pool).await?;
        }
        return Ok(());
    }

    if versions.len() != 1 {
        return Err(protocol_error(format!(
            "Hub schema_version must contain exactly one row, found {}",
            versions.len()
        )));
    }

    let version = versions[0];
    if version > CURRENT_SCHEMA_VERSION {
        return Err(protocol_error(format!(
            "Hub database schema version {} is newer than supported version {}",
            version, CURRENT_SCHEMA_VERSION
        )));
    }
    if version < CURRENT_SCHEMA_VERSION {
        return Err(protocol_error(format!(
            "Hub database schema version {} is older than supported version {} and no migration path is available",
            version, CURRENT_SCHEMA_VERSION
        )));
    }

    validate_existing_v1_schema(pool).await
}

async fn apply_v1_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(V1_SCHEMA).execute(pool).await?;
    set_schema_version_if_empty(pool).await
}

async fn create_schema_version(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")
        .execute(pool)
        .await?;
    Ok(())
}

async fn set_schema_version_if_empty(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO schema_version (version)
         SELECT ?1
         WHERE NOT EXISTS (SELECT 1 FROM schema_version)",
    )
    .bind(CURRENT_SCHEMA_VERSION)
    .execute(pool)
    .await?;
    Ok(())
}

async fn has_any_hub_table(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    for (table, _) in REQUIRED_V1_COLUMNS {
        if *table != "schema_version" && table_exists(pool, table).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool, sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(table)
            .fetch_one(pool)
            .await?;
    Ok(count == 1)
}

async fn validate_existing_v1_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    validate_required_columns(pool, REQUIRED_V1_COLUMNS).await
}

async fn validate_existing_v1_schema_without_version(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let required: Vec<_> = REQUIRED_V1_COLUMNS
        .iter()
        .copied()
        .filter(|(table, _)| *table != "schema_version")
        .collect();
    validate_required_columns(pool, &required).await
}

async fn validate_required_columns(
    pool: &SqlitePool,
    required_tables: &[(&str, &[&str])],
) -> Result<(), sqlx::Error> {
    for (table, required_columns) in required_tables {
        if !table_exists(pool, table).await? {
            return Err(unrecognized_schema(format!("missing table '{table}'")));
        }

        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(pool)
            .await?;
        let actual: BTreeSet<String> = rows
            .into_iter()
            .map(|row| row.get::<String, _>("name"))
            .collect();

        for column in *required_columns {
            if !actual.contains(*column) {
                return Err(unrecognized_schema(format!(
                    "table '{table}' is missing column '{column}'"
                )));
            }
        }
    }
    Ok(())
}

fn unrecognized_schema(detail: String) -> sqlx::Error {
    protocol_error(format!("Unrecognized existing Hub schema: {detail}"))
}

fn protocol_error(message: String) -> sqlx::Error {
    sqlx::Error::Protocol(message)
}
