use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

const SQLITE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn connect_hub_sqlite_pool(
    path: &str,
    max_connections: u32,
) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(path)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(SQLITE_BUSY_TIMEOUT);

    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .min_connections(1)
        .acquire_timeout(SQLITE_ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
}

pub(crate) async fn connect_in_memory_hub_sqlite_pool() -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?
        .foreign_keys(true)
        .busy_timeout(SQLITE_BUSY_TIMEOUT);

    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
}
