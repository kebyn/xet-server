use std::sync::Arc;

use crate::auth::xet_signer::XetSigner;
use crate::metadata::{MetadataError, MetadataStore, Repo, RepoType};
use crate::services::shared::{can_access_repo, can_write_repo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenExchangeServiceError {
    NotFound(String),
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExchangeScope {
    Read,
    Write,
}

impl ExchangeScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ExchangeScope::Read => "read",
            ExchangeScope::Write => "write",
        }
    }
}

pub(crate) struct TokenExchangeRequest<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) username: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) repo_name: &'a str,
    pub(crate) revision: &'a str,
    pub(crate) required_scope: ExchangeScope,
    pub(crate) repo_type: RepoType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenExchangeResult {
    pub(crate) access_token: String,
    pub(crate) exp: u64,
}

pub(crate) struct TokenExchangeService {
    metadata: Arc<dyn MetadataStore>,
    xet_signer: Arc<XetSigner>,
}

impl TokenExchangeService {
    pub(crate) fn new(metadata: Arc<dyn MetadataStore>, xet_signer: Arc<XetSigner>) -> Self {
        Self {
            metadata,
            xet_signer,
        }
    }

    pub(crate) async fn exchange(
        &self,
        request: TokenExchangeRequest<'_>,
    ) -> Result<TokenExchangeResult, TokenExchangeServiceError> {
        let repo = self
            .load_repo(request.namespace, request.repo_name, request.repo_type)
            .await?;

        if !can_exchange_repo(&repo, request.username, request.required_scope) {
            return Err(TokenExchangeServiceError::NotFound(
                "Repository not found".to_string(),
            ));
        }

        let revision = self
            .resolve_exchange_revision(repo.id, request.revision)
            .await;
        let repo_id = format!("{}/{}", request.namespace, request.repo_name);
        let (access_token, exp) = self
            .xet_signer
            .sign(
                request.user_id,
                request.required_scope.as_str(),
                &repo_id,
                &request.repo_type.to_string(),
                &revision,
            )
            .map_err(|err| {
                TokenExchangeServiceError::Internal(format!("Failed to sign token: {}", err))
            })?;

        Ok(TokenExchangeResult { access_token, exp })
    }

    async fn load_repo(
        &self,
        namespace: &str,
        repo_name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, TokenExchangeServiceError> {
        self.metadata
            .get_repo(namespace, repo_name, repo_type)
            .await
            .map_err(|err| match err {
                MetadataError::RepoNotFound(_) => {
                    TokenExchangeServiceError::NotFound(err.to_string())
                }
                _ => TokenExchangeServiceError::Internal(err.to_string()),
            })
    }

    async fn resolve_exchange_revision(&self, repo_id: i64, revision: &str) -> String {
        if revision == "main" || revision.is_empty() {
            match self.metadata.get_head(repo_id).await {
                Ok(Some(head)) => head,
                Ok(None) | Err(_) => "main".to_string(),
            }
        } else {
            revision.to_string()
        }
    }
}

fn can_exchange_repo(repo: &Repo, username: &str, scope: ExchangeScope) -> bool {
    match scope {
        ExchangeScope::Read => can_access_repo(repo, username),
        ExchangeScope::Write => can_write_repo(repo, username),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    use crate::auth::xet_signer::XetSigner;
    use crate::metadata::{MetadataStore, RepoType, Revision, SqliteMetadataStore};

    use super::{
        ExchangeScope, TokenExchangeRequest, TokenExchangeService, TokenExchangeServiceError,
    };

    async fn metadata() -> Arc<dyn MetadataStore> {
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap())
    }

    fn signer() -> Arc<XetSigner> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300))
    }

    async fn add_head(metadata: &dyn MetadataStore, repo_id: i64, commit_id: &str) {
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

    #[tokio::test]
    async fn read_exchange_signs_requested_scope_for_public_repo() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let signer = signer();
        let service = TokenExchangeService::new(metadata, signer.clone());

        let response = service
            .exchange(TokenExchangeRequest {
                user_id: "user-id",
                username: "reader",
                namespace: "owner",
                repo_name: "repo",
                revision: "main",
                required_scope: ExchangeScope::Read,
                repo_type: RepoType::Model,
            })
            .await
            .unwrap();

        let claims = signer.verify_xet_token(&response.access_token).unwrap();
        assert_eq!(claims.sub, "user-id");
        assert_eq!(claims.scope, "read");
        assert_eq!(claims.repo_id, "owner/repo");
        assert_eq!(claims.repo_type, "model");
        assert_eq!(claims.revision, "main");
        assert_eq!(claims.exp, response.exp);
    }

    #[tokio::test]
    async fn main_revision_uses_head_when_available() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        add_head(metadata.as_ref(), repo.id, "abc123ef").await;
        let signer = signer();
        let service = TokenExchangeService::new(metadata, signer.clone());

        let response = service
            .exchange(TokenExchangeRequest {
                user_id: "owner-id",
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                revision: "main",
                required_scope: ExchangeScope::Read,
                repo_type: RepoType::Model,
            })
            .await
            .unwrap();

        let claims = signer.verify_xet_token(&response.access_token).unwrap();
        assert_eq!(claims.revision, "abc123ef");
    }

    #[tokio::test]
    async fn write_exchange_by_non_owner_is_not_found() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let service = TokenExchangeService::new(metadata, signer());

        let err = service
            .exchange(TokenExchangeRequest {
                user_id: "attacker-id",
                username: "attacker",
                namespace: "owner",
                repo_name: "repo",
                revision: "main",
                required_scope: ExchangeScope::Write,
                repo_type: RepoType::Model,
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            TokenExchangeServiceError::NotFound("Repository not found".to_string())
        );
    }

    #[tokio::test]
    async fn missing_repo_returns_not_found() {
        let metadata = metadata().await;
        let service = TokenExchangeService::new(metadata, signer());

        let err = service
            .exchange(TokenExchangeRequest {
                user_id: "user-id",
                username: "reader",
                namespace: "missing",
                repo_name: "repo",
                revision: "main",
                required_scope: ExchangeScope::Read,
                repo_type: RepoType::Model,
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            TokenExchangeServiceError::NotFound(
                "Repository not found: missing/repo/model".to_string()
            )
        );
    }
}
