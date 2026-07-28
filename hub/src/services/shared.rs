use crate::metadata::{MetadataError, MetadataStore, Repo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveRevisionError {
    NotFound(String),
    Internal(String),
}

pub(crate) fn can_access_repo(repo: &Repo, username: &str) -> bool {
    !repo.private || repo.namespace == username
}

pub(crate) fn can_write_repo(repo: &Repo, username: &str) -> bool {
    repo.namespace == username
}

pub(crate) async fn resolve_revision_id(
    metadata: &dyn MetadataStore,
    repo_id: i64,
    revision: &str,
) -> Result<String, ResolveRevisionError> {
    if revision.len() >= 8 && revision.chars().all(|c| c.is_ascii_hexdigit()) {
        return match metadata.get_revision(repo_id, revision).await {
            Ok(_) => Ok(revision.to_string()),
            Err(MetadataError::RevisionNotFound(_)) => Err(ResolveRevisionError::NotFound(
                format!("Revision not found: {}", revision),
            )),
            Err(error) => Err(ResolveRevisionError::Internal(error.to_string())),
        };
    }

    if revision == "main" {
        match metadata.get_head(repo_id).await {
            Ok(Some(head)) => Ok(head),
            Ok(None) => Err(ResolveRevisionError::NotFound(
                "No HEAD found for repo".to_string(),
            )),
            Err(error) => Err(ResolveRevisionError::Internal(error.to_string())),
        }
    } else {
        Err(ResolveRevisionError::NotFound(format!(
            "Revision not found: {} (only 'main' branch or commit hashes are supported)",
            revision
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{MetadataStore, RepoType, SqliteMetadataStore};
    use crate::sqlite_pool::connect_in_memory_hub_sqlite_pool;

    fn repo(namespace: &str, private: bool) -> Repo {
        Repo {
            id: 1,
            name: "repo".to_string(),
            namespace: namespace.to_string(),
            repo_type: RepoType::Model,
            sha: None,
            private,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn public_repo_can_be_read_by_any_user() {
        assert!(can_access_repo(&repo("owner", false), "owner"));
        assert!(can_access_repo(&repo("owner", false), "reader"));
    }

    #[test]
    fn private_repo_can_only_be_read_by_owner() {
        assert!(can_access_repo(&repo("owner", true), "owner"));
        assert!(!can_access_repo(&repo("owner", true), "reader"));
    }

    #[test]
    fn only_repo_owner_can_write_repo() {
        assert!(can_write_repo(&repo("owner", false), "owner"));
        assert!(!can_write_repo(&repo("owner", false), "reader"));
        assert!(can_write_repo(&repo("owner", true), "owner"));
        assert!(!can_write_repo(&repo("owner", true), "reader"));
    }

    #[tokio::test]
    async fn main_revision_propagates_head_query_failures() {
        let pool = connect_in_memory_hub_sqlite_pool().await.unwrap();
        let metadata = SqliteMetadataStore::with_pool(pool.clone()).await.unwrap();
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        sqlx::query("DROP TABLE heads")
            .execute(&pool)
            .await
            .unwrap();

        let error = resolve_revision_id(&metadata, repo.id, "main")
            .await
            .expect_err("HEAD database failures must not look like an empty repository");

        assert!(matches!(error, ResolveRevisionError::Internal(_)));
    }
}
