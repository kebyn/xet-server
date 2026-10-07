//! Atomic commit SQL for [`SqliteMetadataStore`](super::SqliteMetadataStore).
//!
//! The commit path runs inside a single BEGIN IMMEDIATE transaction guarded by
//! [`TransactionConnectionGuard`](super::TransactionConnectionGuard) so a
//! cancelled future rolls back instead of returning an open transaction to the pool.

use super::helpers::file_size_to_sql;
use super::{CommitWrite, TransactionConnectionGuard};
use crate::metadata::{FileEntry, FileTreeChange, MetadataError, Revision};
use sqlx::sqlite::SqlitePool;
use sqlx::{Connection, Row, Sqlite};

pub(super) async fn commit_atomic_write(
    pool: &SqlitePool,
    rev: &Revision,
    write: CommitWrite<'_>,
    expected_parent: Option<&str>,
) -> Result<(), MetadataError> {
    // BEGIN IMMEDIATE acquires SQLite's single-writer lock before checking HEAD.
    // If this future is cancelled, the connection guard closes the connection;
    // SQLite then rolls back before the pool can create a replacement.
    let connection = pool
        .acquire()
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    let mut connection = TransactionConnectionGuard::new(connection);
    let mut tx = connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

    let result = async {
        let current_head: Option<String> =
            sqlx::query("SELECT commit_id FROM heads WHERE repo_id = ?1")
                .bind(rev.repo_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
                .map(|row| row.try_get::<String, _>(0))
                .transpose()
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        if current_head.as_deref() != expected_parent {
            return Err(MetadataError::Conflict(current_head.unwrap_or_default()));
        }

        sqlx::query(
            "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&rev.commit_id)
        .bind(rev.repo_id)
        .bind(&rev.parent)
        .bind(&rev.message)
        .bind(&rev.author)
        .bind(rev.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        if let CommitWrite::Changes(_) = write
            && let Some(parent) = expected_parent
        {
            sqlx::query(
                "INSERT INTO file_tree \
                 (path, repo_id, commit_id, size, cas_hash, is_lfs) \
                 SELECT path, ?1, ?2, size, cas_hash, is_lfs \
                 FROM file_tree WHERE repo_id = ?1 AND commit_id = ?3",
            )
            .bind(rev.repo_id)
            .bind(&rev.commit_id)
            .bind(parent)
            .execute(&mut *tx)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        }

        match write {
            CommitWrite::Snapshot(entries) => {
                for entry in entries {
                    insert_file_entry(&mut tx, rev, entry).await?;
                }
            }
            CommitWrite::Changes(changes) => {
                for change in changes {
                    match change {
                        FileTreeChange::Upsert(entry) => {
                            insert_file_entry(&mut tx, rev, entry).await?;
                        }
                        FileTreeChange::Delete(path) => {
                            sqlx::query(
                                "DELETE FROM file_tree \
                                 WHERE path = ?1 AND repo_id = ?2 AND commit_id = ?3",
                            )
                            .bind(path)
                            .bind(rev.repo_id)
                            .bind(&rev.commit_id)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                        }
                    }
                }
            }
        }

        sqlx::query("INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)")
            .bind(rev.repo_id)
            .bind(&rev.commit_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok::<(), MetadataError>(())
    }
    .await;

    match result {
        Ok(()) => {
            tx.commit()
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
            connection.mark_resolved();
            Ok(())
        }
        Err(error) => {
            tx.rollback().await.map_err(|rollback_error| {
                MetadataError::DatabaseError(format!(
                    "Commit failed ({error}); rollback failed: {rollback_error}"
                ))
            })?;
            connection.mark_resolved();
            Err(error)
        }
    }
}

async fn insert_file_entry(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    rev: &Revision,
    entry: &FileEntry,
) -> Result<(), MetadataError> {
    let is_lfs: i64 = if entry.is_lfs { 1 } else { 0 };
    let size = file_size_to_sql(entry.size)?;
    sqlx::query(
        "INSERT OR REPLACE INTO file_tree \
     (path, repo_id, commit_id, size, cas_hash, is_lfs) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&entry.path)
    .bind(rev.repo_id)
    .bind(&rev.commit_id)
    .bind(size)
    .bind(&entry.cas_hash)
    .bind(is_lfs)
    .execute(&mut **tx)
    .await
    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    Ok(())
}
