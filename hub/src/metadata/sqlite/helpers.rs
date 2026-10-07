//! Shared row mapping and SQL value helpers for the sqlite store modules.

use crate::metadata::{FileEntry, MetadataError, Repo, RepoType, Revision};
use sqlx::{Row, Sqlite};

/// Check if a sqlx::Error represents a UNIQUE constraint violation.
///
/// SQLite returns extended error code 1555 (SQLITE_CONSTRAINT_UNIQUE) or
/// 2067 (SQLITE_CONSTRAINT_PRIMARYKEY) for unique constraint violations.
/// sqlx exposes these via `DatabaseError::code()` as string representations.
pub(super) fn is_unique_violation(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        if let Some(code) = db_err.code() {
            // SQLite extended codes: 1555 = CONSTRAINT_UNIQUE, 2067 = CONSTRAINT_PRIMARYKEY
            // SQLite primary code: 19 = SQLITE_CONSTRAINT (used when extended codes disabled)
            code == "1555" || code == "2067" || code == "19"
        } else {
            // Fall back to message inspection if code not available
            db_err.message().contains("UNIQUE constraint failed")
        }
    } else {
        false
    }
}

/// Map a `sqlx::Row` to a `Repo` value.
pub(super) fn row_to_repo(row: &sqlx::sqlite::SqliteRow) -> Result<Repo, MetadataError> {
    let repo_type_str: String = row.try_get(3).map_err(MetadataError::DatabaseError)?;
    let repo_type = repo_type_str
        .parse::<RepoType>()
        .map_err(|e| MetadataError::Corruption(format!("Invalid repo_type: {}", e)))?;
    Ok(Repo {
        id: row.try_get(0).map_err(MetadataError::DatabaseError)?,
        name: row.try_get(1).map_err(MetadataError::DatabaseError)?,
        namespace: row.try_get(2).map_err(MetadataError::DatabaseError)?,
        repo_type,
        sha: row.try_get(4).map_err(MetadataError::DatabaseError)?,
        private: row
            .try_get::<i64, _>(5)
            .map_err(MetadataError::DatabaseError)?
            != 0,
        created_at: row.try_get(6).map_err(MetadataError::DatabaseError)?,
        updated_at: row.try_get(7).map_err(MetadataError::DatabaseError)?,
    })
}

/// Map a `sqlx::Row` to a `Revision` value.
pub(super) fn row_to_revision(row: &sqlx::sqlite::SqliteRow) -> Result<Revision, MetadataError> {
    Ok(Revision {
        commit_id: row.try_get(0).map_err(MetadataError::DatabaseError)?,
        repo_id: row.try_get(1).map_err(MetadataError::DatabaseError)?,
        parent: row.try_get(2).map_err(MetadataError::DatabaseError)?,
        message: row.try_get(3).map_err(MetadataError::DatabaseError)?,
        author: row.try_get(4).map_err(MetadataError::DatabaseError)?,
        created_at: row.try_get(5).map_err(MetadataError::DatabaseError)?,
    })
}

/// Map a `sqlx::Row` to a `FileEntry` value.
pub(super) fn row_to_file_entry(row: &sqlx::sqlite::SqliteRow) -> Result<FileEntry, MetadataError> {
    let stored_size = row
        .try_get::<i64, _>(3)
        .map_err(MetadataError::DatabaseError)?;
    let size = u64::try_from(stored_size).map_err(|_| {
        MetadataError::Corruption(format!(
            "Corrupt file_tree row contains negative size {}",
            stored_size
        ))
    })?;

    Ok(FileEntry {
        path: row.try_get(0).map_err(MetadataError::DatabaseError)?,
        repo_id: row.try_get(1).map_err(MetadataError::DatabaseError)?,
        commit_id: row.try_get(2).map_err(MetadataError::DatabaseError)?,
        size,
        cas_hash: row.try_get(4).map_err(MetadataError::DatabaseError)?,
        is_lfs: row
            .try_get::<i64, _>(5)
            .map_err(MetadataError::DatabaseError)?
            != 0,
    })
}

pub(super) fn file_size_to_sql(size: u64) -> Result<i64, MetadataError> {
    i64::try_from(size).map_err(|_| {
        MetadataError::InvalidOperation(format!("File size {} exceeds SQLite INTEGER range", size))
    })
}

/// Insert (or replace) a file_tree row.
///
/// `repo_id`/`commit_id` are caller-supplied because the authoritative
/// source differs: the atomic commit path binds from the revision (defending
/// against entries that disagree with it), while `add_file_entries` binds
/// from the entry itself.
pub(super) async fn insert_file_entry(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    repo_id: i64,
    commit_id: &str,
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
    .bind(repo_id)
    .bind(commit_id)
    .bind(size)
    .bind(&entry.cas_hash)
    .bind(is_lfs)
    .execute(&mut **tx)
    .await
    .map_err(MetadataError::DatabaseError)?;
    Ok(())
}

pub(super) fn escape_like_pattern(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
