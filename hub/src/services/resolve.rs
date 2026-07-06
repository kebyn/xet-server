use std::sync::Arc;

use crate::metadata::{FileEntry, MetadataError, MetadataStore, Repo, RepoType};
use crate::services::shared::{can_access_repo, resolve_revision_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveServiceError {
    NotFound(String),
    Internal(String),
}

pub(crate) struct ResolveFileRequest<'a> {
    pub(crate) username: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) repo_name: &'a str,
    pub(crate) repo_type: RepoType,
    pub(crate) revision: &'a str,
    pub(crate) file_path: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedFile {
    pub(crate) commit_id: String,
    pub(crate) file_entry: FileEntry,
}

pub(crate) struct ResolveService {
    metadata: Arc<dyn MetadataStore>,
}

impl ResolveService {
    pub(crate) fn new(metadata: Arc<dyn MetadataStore>) -> Self {
        Self { metadata }
    }

    pub(crate) async fn resolve_file(
        &self,
        request: ResolveFileRequest<'_>,
    ) -> Result<ResolvedFile, ResolveServiceError> {
        let repo = self
            .load_repo(request.namespace, request.repo_name, request.repo_type)
            .await?;

        if !can_access_repo(&repo, request.username) {
            return Err(ResolveServiceError::NotFound(
                "Repository not found".to_string(),
            ));
        }

        let commit_id = resolve_revision_id(self.metadata.as_ref(), repo.id, request.revision)
            .await
            .map_err(ResolveServiceError::NotFound)?;

        let file_entry = self
            .metadata
            .resolve_file(repo.id, &commit_id, request.file_path)
            .await
            .map_err(|err| match err {
                MetadataError::FileNotFound(_) => ResolveServiceError::NotFound(err.to_string()),
                _ => ResolveServiceError::Internal(err.to_string()),
            })?;

        Ok(ResolvedFile {
            commit_id,
            file_entry,
        })
    }

    async fn load_repo(
        &self,
        namespace: &str,
        repo_name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, ResolveServiceError> {
        self.metadata
            .get_repo(namespace, repo_name, repo_type)
            .await
            .map_err(|err| match err {
                MetadataError::RepoNotFound(_) => ResolveServiceError::NotFound(err.to_string()),
                _ => ResolveServiceError::Internal(err.to_string()),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::metadata::{FileEntry, MetadataStore, RepoType, Revision, SqliteMetadataStore};

    use super::{ResolveFileRequest, ResolveService, ResolveServiceError};

    async fn metadata() -> Arc<dyn MetadataStore> {
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap())
    }

    async fn add_revision(metadata: &dyn MetadataStore, repo_id: i64, commit_id: &str) {
        metadata
            .add_revision(Revision {
                commit_id: commit_id.to_string(),
                repo_id,
                parent: None,
                message: "initial".to_string(),
                author: "owner".to_string(),
                created_at: 1000,
            })
            .await
            .unwrap();
        metadata.set_head(repo_id, commit_id).await.unwrap();
    }

    fn file(repo_id: i64, commit_id: &str, path: &str, size: u64, hash: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            repo_id,
            commit_id: commit_id.to_string(),
            size,
            cas_hash: hash.to_string(),
            is_lfs: true,
        }
    }

    #[tokio::test]
    async fn resolves_file_metadata_and_commit_id() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123ef").await;
        metadata
            .add_file_entries(vec![file(
                repo.id,
                "abc123ef",
                "model.bin",
                1024,
                "hash123",
            )])
            .await
            .unwrap();

        let service = ResolveService::new(metadata);
        let resolved = service
            .resolve_file(ResolveFileRequest {
                username: "reader",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                file_path: "model.bin",
            })
            .await
            .unwrap();

        assert_eq!(resolved.commit_id, "abc123ef");
        assert_eq!(resolved.file_entry.path, "model.bin");
        assert_eq!(resolved.file_entry.cas_hash, "hash123");
        assert_eq!(resolved.file_entry.size, 1024);
    }

    #[tokio::test]
    async fn private_repo_read_by_non_owner_is_not_found() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "secret", RepoType::Model, true)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123ef").await;

        let service = ResolveService::new(metadata);
        let err = service
            .resolve_file(ResolveFileRequest {
                username: "attacker",
                namespace: "owner",
                repo_name: "secret",
                repo_type: RepoType::Model,
                revision: "main",
                file_path: "model.bin",
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            ResolveServiceError::NotFound("Repository not found".to_string())
        );
    }

    #[tokio::test]
    async fn missing_file_returns_not_found() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123ef").await;

        let service = ResolveService::new(metadata);
        let err = service
            .resolve_file(ResolveFileRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                file_path: "missing.bin",
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            ResolveServiceError::NotFound("File not found: abc123ef/missing.bin".to_string())
        );
    }

    #[tokio::test]
    async fn missing_revision_returns_not_found() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();

        let service = ResolveService::new(metadata);
        let err = service
            .resolve_file(ResolveFileRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                file_path: "model.bin",
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            ResolveServiceError::NotFound("No HEAD found for repo".to_string())
        );
    }
}
