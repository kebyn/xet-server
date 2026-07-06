use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClientTrait;
use crate::commit::content::decode_base64_content;
use crate::commit::id::{generate_commit_id, now_timestamp};
use crate::commit::types::{
    CommitHeader, CommitOperation, CommitResponse, DeletedEntryOperation, FileOperation,
    LfsFileOperation, MAX_INLINE_SIZE,
};
use crate::commit::validation::validate_file_path;
use crate::metadata::{FileEntry, MetadataError, MetadataStore, RepoType, Revision};

pub(crate) const MAX_COMMIT_BODY_SIZE: usize = 20 * 1024 * 1024;
pub(crate) const MAX_COMMIT_OPERATIONS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitServiceError {
    PayloadTooLarge {
        actual: usize,
        max: usize,
    },
    Validation(String),
    Forbidden(String),
    NotFound(String),
    Conflict {
        message: String,
        current_head: Option<String>,
        note: Option<&'static str>,
    },
    UnprocessableEntity(String),
    CasUpload {
        status: u16,
        message: String,
    },
    BadGateway(String),
    Internal(String),
}

pub(crate) struct CommitRequest<'a> {
    pub(crate) username: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) repo_name: &'a str,
    pub(crate) revision: &'a str,
    pub(crate) repo_type: RepoType,
    pub(crate) body: &'a str,
}

pub(crate) struct CommitService {
    metadata: Arc<dyn MetadataStore>,
    cas_client: Arc<dyn CasClientTrait>,
    signer: Arc<XetSigner>,
}

impl CommitService {
    pub(crate) fn new(
        metadata: Arc<dyn MetadataStore>,
        cas_client: Arc<dyn CasClientTrait>,
        signer: Arc<XetSigner>,
    ) -> Self {
        Self {
            metadata,
            cas_client,
            signer,
        }
    }

    pub(crate) async fn commit(
        &self,
        request: CommitRequest<'_>,
    ) -> Result<CommitResponse, CommitServiceError> {
        if request.body.len() > MAX_COMMIT_BODY_SIZE {
            return Err(CommitServiceError::PayloadTooLarge {
                actual: request.body.len(),
                max: MAX_COMMIT_BODY_SIZE,
            });
        }

        self.ensure_namespace_write_access(request.username, request.namespace)
            .await?;

        let operation_count = request
            .body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if operation_count > MAX_COMMIT_OPERATIONS {
            return Err(CommitServiceError::Validation(format!(
                "Too many operations in commit ({}, max {})",
                operation_count, MAX_COMMIT_OPERATIONS
            )));
        }

        let repo = self
            .metadata
            .get_repo(request.namespace, request.repo_name, request.repo_type)
            .await
            .map_err(map_metadata_load_error)?;

        let ParsedCommit {
            header,
            files,
            lfs_files,
            deleted_entries,
        } = parse_commit_body(request.body)?;

        let current_head = self.metadata.get_head(repo.id).await.ok().flatten();
        let parent_revision = header.parent_revision.clone();
        ensure_parent_matches_head(parent_revision.as_deref(), current_head.as_deref())?;

        let (internal_token, _) = self.signer.sign_internal().map_err(|err| {
            CommitServiceError::Internal(format!("Failed to sign internal token: {}", err))
        })?;

        let timestamp = now_timestamp();
        let commit_id =
            generate_commit_id(repo.id, current_head.as_deref(), &header.summary, timestamp);

        let cas_write_token = if files.is_empty() {
            String::new()
        } else {
            self.signer
                .sign(
                    request.username,
                    "write",
                    &format!("{}/{}", request.namespace, request.repo_name),
                    &request.repo_type.to_string(),
                    request.revision,
                )
                .map(|(token, _)| token)
                .map_err(|err| {
                    CommitServiceError::Internal(format!("Failed to sign CAS write token: {}", err))
                })?
        };

        let mut file_entries: Vec<FileEntry> = Vec::new();
        for file_op in files {
            let entry = self
                .process_inline_file(file_op, repo.id, &commit_id, &cas_write_token)
                .await?;
            file_entries.push(entry);
        }

        for lfs_op in lfs_files {
            let entry = self
                .process_lfs_file(lfs_op, repo.id, &commit_id, &internal_token)
                .await?;
            file_entries.push(entry);
        }

        let final_entries = self
            .build_final_tree(
                repo.id,
                current_head.as_deref(),
                &commit_id,
                deleted_entries,
                file_entries,
            )
            .await?;

        let revision = Revision {
            commit_id: commit_id.clone(),
            repo_id: repo.id,
            parent: current_head,
            message: header.summary,
            author: request.username.to_string(),
            created_at: timestamp,
        };

        self.metadata
            .commit_atomic(&revision, &final_entries, parent_revision.as_deref())
            .await
            .map_err(|err| match err {
                MetadataError::Conflict(actual_head) => CommitServiceError::Conflict {
                    message: "Parent revision does not match current HEAD".to_string(),
                    current_head: Some(actual_head),
                    note: None,
                },
                _ => CommitServiceError::Internal(err.to_string()),
            })?;

        Ok(CommitResponse {
            commit_oid: commit_id.clone(),
            commit_url: format!(
                "/{}/{}/commit/{}",
                request.namespace, request.repo_name, commit_id
            ),
            pr_url: None,
            pr_num: None,
        })
    }

    async fn ensure_namespace_write_access(
        &self,
        username: &str,
        namespace: &str,
    ) -> Result<(), CommitServiceError> {
        if namespace == username {
            return Ok(());
        }

        let has_access = self
            .metadata
            .is_namespace_member(username, namespace)
            .await
            .unwrap_or(false);
        if has_access {
            return Ok(());
        }

        Err(CommitServiceError::Forbidden(format!(
            "User '{}' cannot commit to namespace '{}'",
            username, namespace
        )))
    }

    async fn process_inline_file(
        &self,
        file_op: FileOperation,
        repo_id: i64,
        commit_id: &str,
        cas_write_token: &str,
    ) -> Result<FileEntry, CommitServiceError> {
        validate_file_path(&file_op.path)
            .map_err(|msg| CommitServiceError::Validation(format!("Invalid file path: {}", msg)))?;

        let decoded_content =
            decode_base64_content(&file_op.content).map_err(CommitServiceError::Validation)?;

        if decoded_content.len() > MAX_INLINE_SIZE {
            return Err(CommitServiceError::Validation(format!(
                "Inline file too large: {} bytes (max {})",
                decoded_content.len(),
                MAX_INLINE_SIZE
            )));
        }

        let oid = hex::encode(Sha256::digest(&decoded_content));
        let size = decoded_content.len() as u64;

        self.cas_client
            .proxy_lfs_upload(&oid, Bytes::from(decoded_content), cas_write_token)
            .await
            .map_err(|err| {
                tracing::error!(
                    "Failed to store inline file in CAS: status={}, error={}",
                    err.status,
                    err.message
                );
                CommitServiceError::CasUpload {
                    status: err.status,
                    message: err.message,
                }
            })?;

        Ok(FileEntry {
            path: file_op.path,
            repo_id,
            commit_id: commit_id.to_string(),
            size,
            cas_hash: oid,
            is_lfs: false,
        })
    }

    async fn process_lfs_file(
        &self,
        lfs_op: LfsFileOperation,
        repo_id: i64,
        commit_id: &str,
        internal_token: &str,
    ) -> Result<FileEntry, CommitServiceError> {
        validate_file_path(&lfs_op.path)
            .map_err(|msg| CommitServiceError::Validation(format!("Invalid file path: {}", msg)))?;

        if lfs_op.oid.len() != 64 || !lfs_op.oid.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CommitServiceError::Validation(format!(
                "Invalid LFS OID format for {}: expected 64-character hex string",
                lfs_op.path
            )));
        }

        match self.cas_client.head_blob(&lfs_op.oid, internal_token).await {
            Ok(_) => {}
            Err(crate::error::HubError::NotFound(_)) => {
                return Err(CommitServiceError::UnprocessableEntity(format!(
                    "LFS file not found in CAS: {}",
                    lfs_op.oid
                )));
            }
            Err(err) => {
                return Err(CommitServiceError::BadGateway(format!(
                    "CAS verification failed: {}",
                    err
                )));
            }
        }

        Ok(FileEntry {
            path: lfs_op.path,
            repo_id,
            commit_id: commit_id.to_string(),
            size: lfs_op.size,
            cas_hash: lfs_op.oid,
            is_lfs: true,
        })
    }

    async fn build_final_tree(
        &self,
        repo_id: i64,
        current_head: Option<&str>,
        commit_id: &str,
        deleted_entries: Vec<DeletedEntryOperation>,
        file_entries: Vec<FileEntry>,
    ) -> Result<Vec<FileEntry>, CommitServiceError> {
        let parent_entries = if let Some(parent_commit) = current_head {
            self.metadata
                .get_file_tree(repo_id, parent_commit)
                .await
                .ok()
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut final_entries: HashMap<String, FileEntry> = HashMap::new();
        for entry in parent_entries {
            final_entries.insert(entry.path.clone(), entry);
        }

        for deleted in deleted_entries {
            validate_file_path(&deleted.path).map_err(|msg| {
                CommitServiceError::Validation(format!("Invalid deleted entry path: {}", msg))
            })?;
            final_entries.remove(&deleted.path);
        }

        for entry in file_entries {
            final_entries.insert(
                entry.path.clone(),
                FileEntry {
                    path: entry.path,
                    repo_id,
                    commit_id: commit_id.to_string(),
                    size: entry.size,
                    cas_hash: entry.cas_hash,
                    is_lfs: entry.is_lfs,
                },
            );
        }

        Ok(final_entries.values().cloned().collect())
    }
}

struct ParsedCommit {
    header: CommitHeader,
    files: Vec<FileOperation>,
    lfs_files: Vec<LfsFileOperation>,
    deleted_entries: Vec<DeletedEntryOperation>,
}

fn parse_commit_body(body: &str) -> Result<ParsedCommit, CommitServiceError> {
    let mut header: Option<CommitHeader> = None;
    let mut files = Vec::new();
    let mut lfs_files = Vec::new();
    let mut deleted_entries = Vec::new();

    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let op: CommitOperation = serde_json::from_str(line).map_err(|err| {
            CommitServiceError::Validation(format!("Invalid NDJSON line: {}", err))
        })?;
        match op {
            CommitOperation::Header(parsed_header) => header = Some(parsed_header),
            CommitOperation::File(file) => files.push(file),
            CommitOperation::LfsFile(lfs_file) => lfs_files.push(lfs_file),
            CommitOperation::DeletedEntry(deleted_entry) => deleted_entries.push(deleted_entry),
        }
    }

    let header = header
        .ok_or_else(|| CommitServiceError::Validation("Missing header in commit".to_string()))?;

    Ok(ParsedCommit {
        header,
        files,
        lfs_files,
        deleted_entries,
    })
}

fn ensure_parent_matches_head(
    parent_revision: Option<&str>,
    current_head: Option<&str>,
) -> Result<(), CommitServiceError> {
    match (parent_revision, current_head) {
        (Some(parent), Some(head)) if parent != head => Err(CommitServiceError::Conflict {
            message: "Parent revision does not match current HEAD".to_string(),
            current_head: Some(head.to_string()),
            note: Some(
                "This is a pre-check for early error detection. The authoritative check happens atomically during commit.",
            ),
        }),
        (Some(_parent), None) => Err(CommitServiceError::Conflict {
            message: "Parent revision specified but repository has no HEAD".to_string(),
            current_head: None,
            note: Some(
                "This is a pre-check. The authoritative check happens atomically during commit.",
            ),
        }),
        (None, Some(head)) => Err(CommitServiceError::Conflict {
            message: format!(
                "No parent specified but repository already has HEAD: {}",
                head
            ),
            current_head: Some(head.to_string()),
            note: Some(
                "This is a pre-check. The authoritative check happens atomically during commit.",
            ),
        }),
        _ => Ok(()),
    }
}

fn map_metadata_load_error(err: MetadataError) -> CommitServiceError {
    match err {
        MetadataError::RepoNotFound(_) => CommitServiceError::NotFound(err.to_string()),
        _ => CommitServiceError::Internal(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    use crate::auth::xet_signer::XetSigner;
    use crate::cas_client::{BlobState, CasClientTrait, CasUploadError};
    use crate::error::HubError;
    use crate::metadata::{MetadataStore, RepoType, SqliteMetadataStore};

    use super::{CommitRequest, CommitService};

    struct MockCasClient;

    #[async_trait]
    impl CasClientTrait for MockCasClient {
        async fn head_blob(&self, oid: &str, _internal_token: &str) -> Result<BlobState, HubError> {
            Err(HubError::NotFound(format!("Blob not found: {}", oid)))
        }

        async fn proxy_lfs_upload(
            &self,
            _oid: &str,
            _data: bytes::Bytes,
            token: &str,
        ) -> Result<(), CasUploadError> {
            assert!(token.starts_with("xet_"));
            Ok(())
        }
    }

    fn signer() -> Arc<XetSigner> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300))
    }

    #[tokio::test]
    async fn inline_commit_returns_commit_result_and_updates_head() {
        let metadata: Arc<dyn MetadataStore> =
            Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let service = CommitService::new(metadata.clone(), Arc::new(MockCasClient), signer());

        let body = "{\"key\":\"header\",\"value\":{\"summary\":\"Add config\",\"parentRevision\":null}}\n\
                    {\"key\":\"file\",\"value\":{\"path\":\"config.json\",\"content\":\"e30=\"}}";
        let result = service
            .commit(CommitRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                revision: "main",
                repo_type: RepoType::Model,
                body,
            })
            .await
            .unwrap();

        assert_eq!(
            result.commit_url,
            format!("/owner/repo/commit/{}", result.commit_oid)
        );
        let repo = metadata
            .get_repo("owner", "repo", RepoType::Model)
            .await
            .unwrap();
        assert_eq!(
            metadata.get_head(repo.id).await.unwrap(),
            Some(result.commit_oid)
        );
    }
}
