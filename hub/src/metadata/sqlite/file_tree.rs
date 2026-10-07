//! File-tree SQL for [`SqliteMetadataStore`](super::SqliteMetadataStore).

use super::TransactionConnectionGuard;
use super::helpers::{escape_like_pattern, insert_file_entry, row_to_file_entry};
use crate::metadata::{FileEntry, FileTreePage, MetadataError};
use sqlx::Connection;
use sqlx::sqlite::SqlitePool;

pub(super) async fn add_file_entries(
    pool: &SqlitePool,
    entries: Vec<FileEntry>,
) -> Result<(), MetadataError> {
    let connection = pool
        .acquire()
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    let mut connection = TransactionConnectionGuard::new(connection);
    let mut tx = connection
        .begin()
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

    for entry in &entries {
        insert_file_entry(&mut tx, entry.repo_id, &entry.commit_id, entry).await?;
    }

    tx.commit()
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    connection.mark_resolved();

    Ok(())
}

pub(super) async fn get_file_tree(
    pool: &SqlitePool,
    repo_id: i64,
    commit_id: &str,
) -> Result<Vec<FileEntry>, MetadataError> {
    let rows = sqlx::query(
        "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 ORDER BY path"
    )
    .bind(repo_id)
    .bind(commit_id)
    .fetch_all(pool)
    .await
    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

    rows.iter().map(row_to_file_entry).collect()
}

pub(super) async fn get_file_tree_prefix(
    pool: &SqlitePool,
    repo_id: i64,
    commit_id: &str,
    prefix: &str,
) -> Result<Vec<FileEntry>, MetadataError> {
    let normalized_prefix = prefix.trim_matches('/');

    let rows = if normalized_prefix.is_empty() {
        sqlx::query(
            "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
             FROM file_tree \
             WHERE repo_id = ?1 AND commit_id = ?2 \
             ORDER BY path",
        )
        .bind(repo_id)
        .bind(commit_id)
        .fetch_all(pool)
        .await
    } else {
        let escaped_prefix = escape_like_pattern(normalized_prefix);
        let child_pattern = format!("{}/%", escaped_prefix);
        sqlx::query(
            "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
             FROM file_tree \
             WHERE repo_id = ?1 \
               AND commit_id = ?2 \
               AND (path = ?3 OR path LIKE ?4 ESCAPE '\\') \
             ORDER BY path",
        )
        .bind(repo_id)
        .bind(commit_id)
        .bind(normalized_prefix)
        .bind(&child_pattern)
        .fetch_all(pool)
        .await
    };

    let rows = rows.map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

    rows.iter().map(row_to_file_entry).collect()
}

pub(super) async fn get_file_tree_prefix_page(
    pool: &SqlitePool,
    repo_id: i64,
    commit_id: &str,
    prefix: &str,
    after_path: Option<&str>,
    limit: usize,
) -> Result<FileTreePage, MetadataError> {
    let normalized_prefix = prefix.trim_matches('/');
    let fetch_limit = limit
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| {
            MetadataError::InvalidOperation(
                "Tree page limit exceeds the supported SQLite range".to_string(),
            )
        })?;

    let rows = match (normalized_prefix.is_empty(), after_path) {
        (true, None) => {
            sqlx::query(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                 FROM file_tree \
                 WHERE repo_id = ?1 AND commit_id = ?2 \
                 ORDER BY path LIMIT ?3",
            )
            .bind(repo_id)
            .bind(commit_id)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
        (true, Some(after_path)) => {
            sqlx::query(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                 FROM file_tree \
                 WHERE repo_id = ?1 AND commit_id = ?2 AND path > ?3 \
                 ORDER BY path LIMIT ?4",
            )
            .bind(repo_id)
            .bind(commit_id)
            .bind(after_path)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
        (false, None) => {
            let escaped_prefix = escape_like_pattern(normalized_prefix);
            let child_pattern = format!("{}/%", escaped_prefix);
            sqlx::query(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                 FROM file_tree \
                 WHERE repo_id = ?1 AND commit_id = ?2 \
                   AND (path = ?3 OR path LIKE ?4 ESCAPE '\\') \
                 ORDER BY path LIMIT ?5",
            )
            .bind(repo_id)
            .bind(commit_id)
            .bind(normalized_prefix)
            .bind(child_pattern)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
        (false, Some(after_path)) => {
            let escaped_prefix = escape_like_pattern(normalized_prefix);
            let child_pattern = format!("{}/%", escaped_prefix);
            sqlx::query(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                 FROM file_tree \
                 WHERE repo_id = ?1 AND commit_id = ?2 AND path > ?3 \
                   AND (path = ?4 OR path LIKE ?5 ESCAPE '\\') \
                 ORDER BY path LIMIT ?6",
            )
            .bind(repo_id)
            .bind(commit_id)
            .bind(after_path)
            .bind(normalized_prefix)
            .bind(child_pattern)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|error| MetadataError::DatabaseError(error.to_string()))?;

    let mut entries: Vec<FileEntry> = rows
        .iter()
        .map(row_to_file_entry)
        .collect::<Result<_, _>>()?;
    let has_more = entries.len() > limit;
    entries.truncate(limit);
    let next_after_path = if has_more {
        entries.last().map(|entry| entry.path.clone())
    } else {
        None
    };

    Ok(FileTreePage {
        entries,
        next_after_path,
    })
}

pub(super) async fn resolve_file(
    pool: &SqlitePool,
    repo_id: i64,
    commit_id: &str,
    path: &str,
) -> Result<FileEntry, MetadataError> {
    let row = sqlx::query(
        "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 AND path = ?3"
    )
    .bind(repo_id)
    .bind(commit_id)
    .bind(path)
    .fetch_optional(pool)
    .await
    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

    match row {
        Some(r) => row_to_file_entry(&r),
        None => Err(MetadataError::FileNotFound(format!(
            "{}/{}",
            commit_id, path
        ))),
    }
}
