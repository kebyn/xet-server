//! Atomic commit SQL for [`SqliteMetadataStore`](super::SqliteMetadataStore).
//!
//! The commit path runs inside a single BEGIN IMMEDIATE transaction guarded by
//! [`TransactionConnectionGuard`](super::TransactionConnectionGuard) so a
//! cancelled future rolls back instead of returning an open transaction to the pool.

use super::helpers::insert_file_entry;
use super::{CommitWrite, TransactionConnectionGuard};
use crate::metadata::{FileTreeChange, MetadataError, Revision};
use sqlx::sqlite::SqlitePool;
use sqlx::{Connection, Row};

pub(super) async fn commit_atomic_write(
    pool: &SqlitePool,
    rev: &Revision,
    write: CommitWrite<'_>,
    expected_parent: Option<&str>,
) -> Result<(), MetadataError> {
    // BEGIN IMMEDIATE acquires SQLite's single-writer lock before checking HEAD.
    // If this future is cancelled, the connection guard closes the connection;
    // SQLite then rolls back before the pool can create a replacement.
    let connection = pool.acquire().await.map_err(MetadataError::DatabaseError)?;
    let mut connection = TransactionConnectionGuard::new(connection);
    let mut tx = connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(MetadataError::DatabaseError)?;

    let result = async {
        let current_head: Option<String> =
            sqlx::query("SELECT commit_id FROM heads WHERE repo_id = ?1")
                .bind(rev.repo_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(MetadataError::DatabaseError)?
                .map(|row| row.try_get::<String, _>(0))
                .transpose()
                .map_err(MetadataError::DatabaseError)?;

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
        .map_err(MetadataError::DatabaseError)?;

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
            .map_err(MetadataError::DatabaseError)?;
        }

        match write {
            CommitWrite::Snapshot(entries) => {
                for entry in entries {
                    insert_file_entry(&mut tx, rev.repo_id, &rev.commit_id, entry).await?;
                }
            }
            CommitWrite::Changes(changes) => {
                for change in changes {
                    match change {
                        FileTreeChange::Upsert(entry) => {
                            insert_file_entry(&mut tx, rev.repo_id, &rev.commit_id, entry).await?;
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
                            .map_err(MetadataError::DatabaseError)?;
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
            .map_err(MetadataError::DatabaseError)?;

        Ok::<(), MetadataError>(())
    }
    .await;

    match result {
        Ok(()) => {
            tx.commit().await.map_err(MetadataError::DatabaseError)?;
            connection.mark_resolved();
            Ok(())
        }
        Err(error) => {
            // The rollback failure (a genuine sqlx error) becomes the returned
            // error so its source chain survives; the original commit error is
            // preserved in the log. The unresolved connection guard still drops
            // closed, so SQLite rolls back either way.
            tx.rollback().await.map_err(|rollback_error| {
                tracing::error!("Commit failed ({error}); rollback failed: {rollback_error}");
                MetadataError::DatabaseError(rollback_error)
            })?;
            connection.mark_resolved();
            Err(error)
        }
    }
}
