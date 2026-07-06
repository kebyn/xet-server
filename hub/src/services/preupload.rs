use std::sync::Arc;

use crate::metadata::{MetadataError, MetadataStore, RepoType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreuploadServiceError {
    Forbidden(String),
    NotFound(String),
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadMode {
    Regular,
    Lfs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreuploadFileInput {
    pub(crate) path: String,
    pub(crate) size: u64,
}

pub(crate) struct PreuploadRequest<'a> {
    pub(crate) username: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) repo_name: &'a str,
    pub(crate) repo_type: RepoType,
    pub(crate) inline_threshold: u64,
    pub(crate) files: Vec<PreuploadFileInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreuploadFileDecision {
    pub(crate) path: String,
    pub(crate) upload_mode: UploadMode,
    pub(crate) should_ignore: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreuploadResponse {
    pub(crate) files: Vec<PreuploadFileDecision>,
}

pub(crate) struct PreuploadService {
    metadata: Arc<dyn MetadataStore>,
}

impl PreuploadService {
    pub(crate) fn new(metadata: Arc<dyn MetadataStore>) -> Self {
        Self { metadata }
    }

    pub(crate) async fn prepare_upload(
        &self,
        request: PreuploadRequest<'_>,
    ) -> Result<PreuploadResponse, PreuploadServiceError> {
        if request.namespace != request.username {
            let has_access = self
                .metadata
                .is_namespace_member(request.username, request.namespace)
                .await
                .unwrap_or(false);
            if !has_access {
                return Err(PreuploadServiceError::Forbidden(format!(
                    "User '{}' cannot access namespace '{}'",
                    request.username, request.namespace
                )));
            }
        }

        self.metadata
            .get_repo(request.namespace, request.repo_name, request.repo_type)
            .await
            .map_err(|err| match err {
                MetadataError::RepoNotFound(_) => PreuploadServiceError::NotFound(err.to_string()),
                _ => PreuploadServiceError::Internal(err.to_string()),
            })?;

        Ok(PreuploadResponse {
            files: request
                .files
                .into_iter()
                .map(|file| PreuploadFileDecision {
                    upload_mode: classify_upload_mode(file.size, request.inline_threshold),
                    path: file.path,
                    should_ignore: false,
                })
                .collect(),
        })
    }
}

fn classify_upload_mode(size: u64, inline_threshold: u64) -> UploadMode {
    if size <= inline_threshold {
        UploadMode::Regular
    } else {
        UploadMode::Lfs
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::metadata::{MetadataStore, RepoType, SqliteMetadataStore};

    use super::{
        PreuploadFileInput, PreuploadRequest, PreuploadService, PreuploadServiceError, UploadMode,
    };

    async fn metadata() -> Arc<dyn MetadataStore> {
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn classifies_regular_and_lfs_files() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();

        let service = PreuploadService::new(metadata);
        let response = service
            .prepare_upload(PreuploadRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                inline_threshold: 1024,
                files: vec![
                    PreuploadFileInput {
                        path: "small.json".to_string(),
                        size: 1024,
                    },
                    PreuploadFileInput {
                        path: "large.bin".to_string(),
                        size: 1025,
                    },
                ],
            })
            .await
            .unwrap();

        assert_eq!(response.files.len(), 2);
        assert_eq!(response.files[0].path, "small.json");
        assert_eq!(response.files[0].upload_mode, UploadMode::Regular);
        assert!(!response.files[0].should_ignore);
        assert_eq!(response.files[1].path, "large.bin");
        assert_eq!(response.files[1].upload_mode, UploadMode::Lfs);
        assert!(!response.files[1].should_ignore);
    }

    #[tokio::test]
    async fn rejects_non_member_namespace_before_repo_lookup() {
        let metadata = metadata().await;
        let service = PreuploadService::new(metadata);

        let err = service
            .prepare_upload(PreuploadRequest {
                username: "attacker",
                namespace: "owner",
                repo_name: "private-repo",
                repo_type: RepoType::Model,
                inline_threshold: 1024,
                files: vec![PreuploadFileInput {
                    path: "file.bin".to_string(),
                    size: 1,
                }],
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            PreuploadServiceError::Forbidden(
                "User 'attacker' cannot access namespace 'owner'".to_string()
            )
        );
    }

    #[tokio::test]
    async fn missing_repo_returns_not_found_for_authorized_namespace() {
        let metadata = metadata().await;
        let service = PreuploadService::new(metadata);

        let err = service
            .prepare_upload(PreuploadRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "missing",
                repo_type: RepoType::Model,
                inline_threshold: 1024,
                files: vec![PreuploadFileInput {
                    path: "file.bin".to_string(),
                    size: 1,
                }],
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            PreuploadServiceError::NotFound(
                "Repository not found: owner/missing/model".to_string()
            )
        );
    }
}
