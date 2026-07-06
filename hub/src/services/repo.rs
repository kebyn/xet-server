use std::sync::Arc;

use crate::metadata::{MetadataError, MetadataStore, Repo, RepoType, Revision};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoServiceError {
    Validation(String),
    NotFound(String),
    Conflict(String),
    Forbidden(String),
    RevisionNotFound(String),
    Internal(String),
}

pub(crate) enum RepoServiceResult {
    Revision {
        repo: Repo,
        revision: Revision,
    },
    EmptyMainRevision {
        repo: Repo,
        head_sha: Option<String>,
    },
}

pub(crate) struct RepoService {
    metadata: Arc<dyn MetadataStore>,
}

impl RepoService {
    pub(crate) fn new(metadata: Arc<dyn MetadataStore>) -> Self {
        Self { metadata }
    }

    pub(crate) async fn create_typed_repo(
        &self,
        username: &str,
        name: &str,
        repo_type: RepoType,
        private: bool,
    ) -> Result<Repo, RepoServiceError> {
        validate_repo_name(name)?;

        self.metadata
            .create_repo(username, name, repo_type, private)
            .await
            .map_err(|err| match err {
                MetadataError::RepoAlreadyExists(_) => RepoServiceError::Conflict(err.to_string()),
                _ => RepoServiceError::Internal(err.to_string()),
            })
    }

    pub(crate) async fn create_unified_repo(
        &self,
        username: &str,
        organization: Option<&str>,
        name: &str,
        repo_type: Option<&str>,
        private: bool,
    ) -> Result<Repo, RepoServiceError> {
        let namespace = organization.unwrap_or(username);
        if namespace != username {
            return Err(RepoServiceError::Forbidden(format!(
                "Cannot create repo in namespace '{}': not a member",
                namespace
            )));
        }

        validate_repo_name(name)?;

        let repo_type = parse_unified_repo_type(repo_type);
        match self
            .metadata
            .create_repo(namespace, name, repo_type, private)
            .await
        {
            Ok(repo) => Ok(repo),
            Err(MetadataError::RepoAlreadyExists(_)) => self
                .metadata
                .get_repo(namespace, name, repo_type)
                .await
                .map_err(|_| RepoServiceError::Conflict("Repo already exists".to_string())),
            Err(err) => Err(RepoServiceError::Internal(err.to_string())),
        }
    }

    pub(crate) async fn get_repo(
        &self,
        username: &str,
        namespace: &str,
        repo_name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, RepoServiceError> {
        let repo = self.load_repo(namespace, repo_name, repo_type).await?;
        if !can_access_repo(&repo, username) {
            return Err(RepoServiceError::NotFound(
                "Repository not found".to_string(),
            ));
        }
        Ok(repo)
    }

    pub(crate) async fn delete_repo(
        &self,
        username: &str,
        namespace: &str,
        repo_name: &str,
        repo_type: RepoType,
    ) -> Result<(), RepoServiceError> {
        let repo = self.load_repo(namespace, repo_name, repo_type).await?;
        if repo.namespace != username {
            return Err(RepoServiceError::Forbidden(
                "You do not have permission to delete this repository".to_string(),
            ));
        }

        self.metadata
            .delete_repo(repo.id)
            .await
            .map_err(|err| RepoServiceError::Internal(err.to_string()))
    }

    pub(crate) async fn get_revision(
        &self,
        username: &str,
        namespace: &str,
        repo_name: &str,
        revision: &str,
        repo_type: RepoType,
    ) -> Result<RepoServiceResult, RepoServiceError> {
        let repo = self
            .metadata
            .get_repo(namespace, repo_name, repo_type)
            .await
            .map_err(|_| RepoServiceError::NotFound("Repository not found".to_string()))?;

        if !can_access_repo(&repo, username) {
            return Err(RepoServiceError::NotFound(
                "Repository not found".to_string(),
            ));
        }

        match self.metadata.get_revision(repo.id, revision).await {
            Ok(rev) => Ok(RepoServiceResult::Revision {
                repo,
                revision: rev,
            }),
            Err(_) if revision == "main" => {
                let head_sha = self.metadata.get_head(repo.id).await.ok().flatten();
                Ok(RepoServiceResult::EmptyMainRevision { repo, head_sha })
            }
            Err(_) => Err(RepoServiceError::RevisionNotFound(format!(
                "Revision not found: {}",
                revision
            ))),
        }
    }

    async fn load_repo(
        &self,
        namespace: &str,
        repo_name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, RepoServiceError> {
        self.metadata
            .get_repo(namespace, repo_name, repo_type)
            .await
            .map_err(|err| match err {
                MetadataError::RepoNotFound(_) => RepoServiceError::NotFound(err.to_string()),
                _ => RepoServiceError::Internal(err.to_string()),
            })
    }
}

fn validate_repo_name(name: &str) -> Result<(), RepoServiceError> {
    if name.is_empty() {
        return Err(RepoServiceError::Validation(
            "Repository name cannot be empty".to_string(),
        ));
    }
    if name.len() > 96 {
        return Err(RepoServiceError::Validation(format!(
            "Repository name too long ({} chars, max 96)",
            name.len()
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(RepoServiceError::Validation(format!(
            "Repository name '{}' contains invalid characters. Only alphanumeric, '.', '_', '-' are allowed",
            name
        )));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(RepoServiceError::Validation(format!(
            "Repository name '{}' cannot start with '.' or '-'",
            name
        )));
    }
    if name.ends_with('.') {
        return Err(RepoServiceError::Validation(format!(
            "Repository name '{}' cannot end with '.'",
            name
        )));
    }
    if name.contains("..") {
        return Err(RepoServiceError::Validation(format!(
            "Repository name '{}' cannot contain '..'",
            name
        )));
    }
    Ok(())
}

fn parse_unified_repo_type(repo_type: Option<&str>) -> RepoType {
    match repo_type {
        Some("dataset") => RepoType::Dataset,
        Some("space") => RepoType::Space,
        _ => RepoType::Model,
    }
}

fn can_access_repo(repo: &Repo, username: &str) -> bool {
    !repo.private || repo.namespace == username
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::metadata::{MetadataStore, RepoType, SqliteMetadataStore};

    use super::{RepoService, RepoServiceError, RepoServiceResult};

    async fn metadata() -> Arc<dyn MetadataStore> {
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn unified_create_returns_existing_repo_for_duplicate() {
        let metadata = metadata().await;
        let service = RepoService::new(metadata);

        let created = service
            .create_unified_repo("owner", None, "repo", Some("model"), false)
            .await
            .unwrap();
        let duplicate = service
            .create_unified_repo("owner", None, "repo", Some("model"), true)
            .await
            .unwrap();

        assert_eq!(duplicate.id, created.id);
        assert!(!duplicate.private);
    }

    #[tokio::test]
    async fn unified_create_rejects_other_namespace() {
        let metadata = metadata().await;
        let service = RepoService::new(metadata);

        let err = service
            .create_unified_repo("user", Some("org"), "repo", Some("model"), false)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            RepoServiceError::Forbidden(
                "Cannot create repo in namespace 'org': not a member".into()
            )
        );
    }

    #[tokio::test]
    async fn private_repo_read_by_non_owner_is_not_found() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "secret", RepoType::Model, true)
            .await
            .unwrap();
        let service = RepoService::new(metadata);

        let err = service
            .get_repo("attacker", "owner", "secret", RepoType::Model)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            RepoServiceError::NotFound("Repository not found".into())
        );
    }

    #[tokio::test]
    async fn delete_repo_rejects_non_owner() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let service = RepoService::new(metadata);

        let err = service
            .delete_repo("other", "owner", "repo", RepoType::Model)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            RepoServiceError::Forbidden(
                "You do not have permission to delete this repository".into()
            )
        );
    }

    #[tokio::test]
    async fn main_revision_for_empty_repo_returns_empty_result() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let service = RepoService::new(metadata);

        let RepoServiceResult::EmptyMainRevision { repo, head_sha } = service
            .get_revision("owner", "owner", "repo", "main", RepoType::Model)
            .await
            .unwrap()
        else {
            panic!("expected empty main revision result");
        };

        assert_eq!(repo.namespace, "owner");
        assert_eq!(repo.name, "repo");
        assert_eq!(head_sha, None);
    }
}
