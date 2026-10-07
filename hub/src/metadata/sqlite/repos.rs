//! Repo CRUD SQL for [`SqliteMetadataStore`](super::SqliteMetadataStore).

use super::helpers::{is_unique_violation, row_to_repo};
use crate::metadata::{MetadataError, Repo, RepoType};
use sqlx::sqlite::SqlitePool;

pub(super) async fn create_repo(
    pool: &SqlitePool,
    namespace: &str,
    name: &str,
    repo_type: RepoType,
    private: bool,
) -> Result<Repo, MetadataError> {
    let repo_type_str = repo_type.to_string();
    let private_int: i64 = if private { 1 } else { 0 };
    let now = crate::util::unix_now_secs() as i64;

    let result = sqlx::query(
        "INSERT INTO repos (name, namespace, repo_type, private, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    )
    .bind(name)
    .bind(namespace)
    .bind(&repo_type_str)
    .bind(private_int)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await;

    match result {
        Ok(result) => {
            let id = result.last_insert_rowid();
            Ok(Repo {
                id,
                name: name.to_string(),
                namespace: namespace.to_string(),
                repo_type,
                sha: None,
                private,
                created_at: now,
                updated_at: now,
            })
        }
        Err(e) if is_unique_violation(&e) => Err(MetadataError::RepoAlreadyExists(format!(
            "{}/{}/{}",
            namespace, name, repo_type_str
        ))),
        Err(e) => Err(MetadataError::DatabaseError(e)),
    }
}

pub(super) async fn get_repo(
    pool: &SqlitePool,
    namespace: &str,
    name: &str,
    repo_type: RepoType,
) -> Result<Repo, MetadataError> {
    let repo_type_str = repo_type.to_string();

    let row = sqlx::query(
        "SELECT id, name, namespace, repo_type, sha, private, created_at, updated_at FROM repos WHERE namespace = ?1 AND name = ?2 AND repo_type = ?3"
    )
    .bind(namespace)
    .bind(name)
    .bind(&repo_type_str)
    .fetch_optional(pool)
    .await
    .map_err(MetadataError::DatabaseError)?;

    match row {
        Some(row) => row_to_repo(&row),
        None => Err(MetadataError::RepoNotFound(format!(
            "{}/{}/{}",
            namespace, name, repo_type_str
        ))),
    }
}

/// Delete a repository and all its metadata (file_tree, heads, revisions, repo record).
/// Wrapped in a single transaction for atomicity.
pub(super) async fn delete_repo(pool: &SqlitePool, repo_id: i64) -> Result<(), MetadataError> {
    // Wrap deletion in a transaction for atomicity
    let mut tx = pool.begin().await.map_err(MetadataError::DatabaseError)?;

    let result = async {
        // Delete in order (foreign key constraints)
        sqlx::query("DELETE FROM file_tree WHERE repo_id = ?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await
            .map_err(MetadataError::DatabaseError)?;

        sqlx::query("DELETE FROM heads WHERE repo_id = ?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await
            .map_err(MetadataError::DatabaseError)?;

        sqlx::query("DELETE FROM revisions WHERE repo_id = ?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await
            .map_err(MetadataError::DatabaseError)?;

        let rows = sqlx::query("DELETE FROM repos WHERE id = ?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await
            .map_err(MetadataError::DatabaseError)?;

        if rows.rows_affected() == 0 {
            Err(MetadataError::RepoNotFound(format!("id={}", repo_id)))
        } else {
            Ok(())
        }
    }
    .await;

    match result {
        Ok(()) => {
            tx.commit().await.map_err(MetadataError::DatabaseError)?;
            Ok(())
        }
        Err(e) => {
            // Rollback is implicit when tx is dropped without commit.
            // Removed meaningless SELECT 1 statement.
            drop(tx);
            Err(e)
        }
    }
}
