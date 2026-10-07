//! Revision and head SQL for [`SqliteMetadataStore`](super::SqliteMetadataStore).

use super::helpers::row_to_revision;
use crate::metadata::{MetadataError, Revision};
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

pub(super) async fn add_revision(
    pool: &SqlitePool,
    revision: Revision,
) -> Result<(), MetadataError> {
    sqlx::query(
        "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    )
    .bind(&revision.commit_id)
    .bind(revision.repo_id)
    .bind(&revision.parent)
    .bind(&revision.message)
    .bind(&revision.author)
    .bind(revision.created_at)
    .execute(pool)
    .await
    .map_err(MetadataError::DatabaseError)?;

    Ok(())
}

pub(super) async fn get_revision(
    pool: &SqlitePool,
    repo_id: i64,
    commit_id: &str,
) -> Result<Revision, MetadataError> {
    let row = sqlx::query(
        "SELECT commit_id, repo_id, parent, message, author, created_at FROM revisions WHERE repo_id = ?1 AND commit_id = ?2"
    )
    .bind(repo_id)
    .bind(commit_id)
    .fetch_optional(pool)
    .await
    .map_err(MetadataError::DatabaseError)?;

    match row {
        Some(row) => row_to_revision(&row),
        None => Err(MetadataError::RevisionNotFound(commit_id.to_string())),
    }
}

pub(super) async fn get_head(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Option<String>, MetadataError> {
    let row = sqlx::query("SELECT commit_id FROM heads WHERE repo_id = ?1")
        .bind(repo_id)
        .fetch_optional(pool)
        .await
        .map_err(MetadataError::DatabaseError)?;

    match row {
        Some(r) => Ok(Some(r.try_get(0).map_err(MetadataError::DatabaseError)?)),
        None => Ok(None),
    }
}

pub(super) async fn set_head(
    pool: &SqlitePool,
    repo_id: i64,
    commit_id: &str,
) -> Result<(), MetadataError> {
    sqlx::query("INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)")
        .bind(repo_id)
        .bind(commit_id)
        .execute(pool)
        .await
        .map_err(MetadataError::DatabaseError)?;

    Ok(())
}

pub(super) async fn get_commit_log(
    pool: &SqlitePool,
    repo_id: i64,
    limit: Option<usize>,
) -> Result<Vec<Revision>, MetadataError> {
    // Use recursive CTE instead of N+1 queries.
    // Single SQL query walks the entire parent chain from HEAD.
    // Use i64::MAX - 1 to avoid overflow when casting from usize.
    let effective_limit = limit.map(|l| l as i64).unwrap_or(i64::MAX - 1);

    // The recursive step condition uses (ch.depth + 1 < ?2) so that
    // the base case (depth=0) plus (limit-1) recursive steps yields
    // exactly `limit` rows total.
    let rows = sqlx::query(
        "WITH RECURSIVE commit_history AS (
            SELECT r.commit_id, r.repo_id, r.parent, r.message, r.author, r.created_at, 0 AS depth
            FROM revisions r
            INNER JOIN heads h ON r.commit_id = h.commit_id
            WHERE h.repo_id = ?1
            UNION ALL
            SELECT r.commit_id, r.repo_id, r.parent, r.message, r.author, r.created_at, ch.depth + 1
            FROM revisions r
            INNER JOIN commit_history ch ON r.commit_id = ch.parent
            WHERE ch.depth + 1 < ?2
        )
        SELECT commit_id, repo_id, parent, message, author, created_at
        FROM commit_history
        ORDER BY depth",
    )
    .bind(repo_id)
    .bind(effective_limit)
    .fetch_all(pool)
    .await
    .map_err(MetadataError::DatabaseError)?;

    rows.iter().map(row_to_revision).collect()
}
