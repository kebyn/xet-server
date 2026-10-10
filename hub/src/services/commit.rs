use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClientTrait;
use crate::commit::content::decode_base64_content;
use crate::commit::id::generate_commit_id;
use crate::commit::types::{
    CommitHeader, CommitOperation, CommitResponse, DeletedEntryOperation, FileOperation,
    LfsFileOperation, MAX_INLINE_SIZE,
};
use crate::commit::validation::validate_file_path;
use crate::metadata::{
    FileEntry, FileTreeChange, MetadataError, MetadataStore, RepoType, Revision,
};

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
    GatewayTimeout(String),
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

        if request.revision != "main" {
            return Err(CommitServiceError::Validation(
                "Commits currently support only the 'main' revision".to_string(),
            ));
        }

        let ParsedCommit { header, operations } = parse_commit_body(request.body)?;

        let current_head = self
            .metadata
            .get_head(repo.id)
            .await
            .map_err(map_metadata_load_error)?;
        let parent_revision = header.parent_revision.clone();
        ensure_parent_matches_head(parent_revision.as_deref(), current_head.as_deref())?;
        let internal_token = if operations
            .iter()
            .any(|operation| matches!(operation, TreeOperation::LfsFile(_)))
        {
            self.signer
                .sign_internal()
                .map(|(token, _)| token)
                .map_err(|err| {
                    CommitServiceError::Internal(format!("Failed to sign internal token: {}", err))
                })?
        } else {
            String::new()
        };

        let timestamp = crate::util::unix_now_secs() as i64;
        let commit_id =
            generate_commit_id(repo.id, current_head.as_deref(), &header.summary, timestamp);

        let cas_write_token = if operations
            .iter()
            .all(|operation| !matches!(operation, TreeOperation::File(_)))
        {
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

        let mut changes = Vec::with_capacity(operations.len());
        for operation in operations {
            let change = match operation {
                TreeOperation::File(file_op) => FileTreeChange::Upsert(
                    self.process_inline_file(file_op, repo.id, &commit_id, &cas_write_token)
                        .await?,
                ),
                TreeOperation::LfsFile(lfs_op) => FileTreeChange::Upsert(
                    self.process_lfs_file(lfs_op, repo.id, &commit_id, &internal_token)
                        .await?,
                ),
                TreeOperation::DeletedEntry(deleted) => {
                    validate_file_path(&deleted.path).map_err(|msg| {
                        CommitServiceError::Validation(format!(
                            "Invalid deleted entry path: {}",
                            msg
                        ))
                    })?;
                    FileTreeChange::Delete(deleted.path)
                }
            };
            changes.push(change);
        }

        let revision = Revision {
            commit_id: commit_id.clone(),
            repo_id: repo.id,
            parent: current_head,
            message: header.summary,
            author: request.username.to_string(),
            created_at: timestamp,
        };

        self.metadata
            .commit_changes_atomic(&revision, &changes, parent_revision.as_deref())
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
            .map_err(map_metadata_load_error)?;
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
        i64::try_from(lfs_op.size).map_err(|_| {
            CommitServiceError::Validation(format!(
                "LFS file size for {} exceeds the supported metadata range",
                lfs_op.path
            ))
        })?;

        match self.cas_client.head_blob(&lfs_op.oid, internal_token).await {
            Ok(blob_state) if blob_state.size == lfs_op.size => {}
            Ok(blob_state) => {
                return Err(CommitServiceError::UnprocessableEntity(format!(
                    "LFS file size mismatch for {}: commit declares {} bytes, CAS reports {} bytes",
                    lfs_op.path, lfs_op.size, blob_state.size
                )));
            }
            Err(crate::error::HubError::NotFound(_)) => {
                return Err(CommitServiceError::UnprocessableEntity(format!(
                    "LFS file not found in CAS: {}",
                    lfs_op.oid
                )));
            }
            Err(crate::error::HubError::CasTimeout(_)) => {
                return Err(CommitServiceError::GatewayTimeout(
                    "CAS verification timed out".to_string(),
                ));
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
}

struct ParsedCommit {
    header: CommitHeader,
    operations: Vec<TreeOperation>,
}

enum TreeOperation {
    File(FileOperation),
    LfsFile(LfsFileOperation),
    DeletedEntry(DeletedEntryOperation),
}

fn parse_commit_body(body: &str) -> Result<ParsedCommit, CommitServiceError> {
    let mut header: Option<CommitHeader> = None;
    let mut operations = Vec::new();
    let mut operation_index = 0usize;

    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let op: CommitOperation = serde_json::from_str(line).map_err(|err| {
            CommitServiceError::Validation(format!("Invalid NDJSON line: {}", err))
        })?;
        if operation_index == 0 {
            match op {
                CommitOperation::Header(parsed_header) => header = Some(parsed_header),
                _ => {
                    return Err(CommitServiceError::Validation(
                        "Commit header must be the first non-empty operation".to_string(),
                    ));
                }
            }
        } else {
            match op {
                CommitOperation::Header(_) => {
                    return Err(CommitServiceError::Validation(
                        "Commit must contain exactly one header".to_string(),
                    ));
                }
                CommitOperation::File(file) => operations.push(TreeOperation::File(file)),
                CommitOperation::LfsFile(lfs_file) => {
                    operations.push(TreeOperation::LfsFile(lfs_file));
                }
                CommitOperation::DeletedEntry(deleted_entry) => {
                    operations.push(TreeOperation::DeletedEntry(deleted_entry));
                }
            }
        }
        operation_index += 1;
    }

    let header = header
        .ok_or_else(|| CommitServiceError::Validation("Missing header in commit".to_string()))?;

    Ok(ParsedCommit { header, operations })
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    use crate::auth::xet_signer::XetSigner;
    use crate::cas_client::{BlobState, CasClientTrait, CasUploadError};
    use crate::error::HubError;
    use crate::metadata::{
        FileEntry, MetadataError, MetadataStore, Repo, RepoType, Revision, SqliteMetadataStore,
    };

    use super::{CommitRequest, CommitService, CommitServiceError, parse_commit_body};

    struct MockCasClient {
        upload_calls: Arc<AtomicUsize>,
    }

    impl MockCasClient {
        fn new() -> Self {
            Self {
                upload_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

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
            self.upload_calls.fetch_add(1, Ordering::SeqCst);
            assert!(token.starts_with("xet_"));
            Ok(())
        }
    }

    struct SizedBlobCasClient {
        size: u64,
    }

    #[async_trait]
    impl CasClientTrait for SizedBlobCasClient {
        async fn head_blob(&self, oid: &str, _internal_token: &str) -> Result<BlobState, HubError> {
            Ok(BlobState {
                state: "raw_only".to_string(),
                xet_file_id: None,
                size: self.size,
                sha256: oid.to_string(),
            })
        }

        async fn proxy_lfs_upload(
            &self,
            _oid: &str,
            _data: bytes::Bytes,
            _token: &str,
        ) -> Result<(), CasUploadError> {
            unreachable!("LFS size tests do not upload inline files")
        }
    }

    fn signer() -> Arc<XetSigner> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300))
    }

    #[derive(Clone, Copy)]
    enum FailurePoint {
        GetHead,
        GetFileTree,
        Membership,
    }

    struct FaultMetadataStore {
        failure: FailurePoint,
    }

    impl FaultMetadataStore {
        fn repo() -> Repo {
            Repo {
                id: 1,
                name: "repo".to_string(),
                namespace: "owner".to_string(),
                repo_type: RepoType::Model,
                sha: None,
                private: false,
                created_at: 1,
                updated_at: 1,
            }
        }

        fn injected_error(operation: &str) -> MetadataError {
            MetadataError::DatabaseError(sqlx::Error::Protocol(format!(
                "injected {} failure",
                operation
            )))
        }
    }

    #[async_trait]
    impl MetadataStore for FaultMetadataStore {
        async fn create_repo(
            &self,
            _namespace: &str,
            _name: &str,
            _repo_type: RepoType,
            _private: bool,
        ) -> Result<Repo, MetadataError> {
            unreachable!("create_repo is not used by these tests")
        }

        async fn get_repo(
            &self,
            _namespace: &str,
            _name: &str,
            _repo_type: RepoType,
        ) -> Result<Repo, MetadataError> {
            Ok(Self::repo())
        }

        async fn delete_repo(&self, _repo_id: i64) -> Result<(), MetadataError> {
            unreachable!("delete_repo is not used by these tests")
        }

        async fn add_revision(&self, _revision: Revision) -> Result<(), MetadataError> {
            unreachable!("add_revision is not used by these tests")
        }

        async fn get_revision(
            &self,
            _repo_id: i64,
            _commit_id: &str,
        ) -> Result<Revision, MetadataError> {
            unreachable!("get_revision is not used by these tests")
        }

        async fn get_head(&self, _repo_id: i64) -> Result<Option<String>, MetadataError> {
            match self.failure {
                FailurePoint::GetHead => Err(Self::injected_error("get_head")),
                _ => Ok(Some("parent".to_string())),
            }
        }

        async fn set_head(&self, _repo_id: i64, _commit_id: &str) -> Result<(), MetadataError> {
            unreachable!("set_head is not used by these tests")
        }

        async fn get_commit_log(
            &self,
            _repo_id: i64,
            _limit: Option<usize>,
        ) -> Result<Vec<Revision>, MetadataError> {
            unreachable!("get_commit_log is not used by these tests")
        }

        async fn add_file_entries(&self, _entries: Vec<FileEntry>) -> Result<(), MetadataError> {
            unreachable!("add_file_entries is not used by these tests")
        }

        async fn get_file_tree(
            &self,
            _repo_id: i64,
            _commit_id: &str,
        ) -> Result<Vec<FileEntry>, MetadataError> {
            match self.failure {
                FailurePoint::GetFileTree => Err(Self::injected_error("get_file_tree")),
                _ => Ok(Vec::new()),
            }
        }

        async fn get_file_tree_prefix(
            &self,
            _repo_id: i64,
            _commit_id: &str,
            _prefix: &str,
        ) -> Result<Vec<FileEntry>, MetadataError> {
            unreachable!("get_file_tree_prefix is not used by these tests")
        }

        async fn resolve_file(
            &self,
            _repo_id: i64,
            _commit_id: &str,
            _path: &str,
        ) -> Result<FileEntry, MetadataError> {
            unreachable!("resolve_file is not used by these tests")
        }

        async fn commit_atomic(
            &self,
            _rev: &Revision,
            _entries: &[FileEntry],
            _expected_parent: Option<&str>,
        ) -> Result<(), MetadataError> {
            unreachable!("commit_atomic must not run after an injected read failure")
        }

        async fn is_namespace_member(
            &self,
            _username: &str,
            _namespace: &str,
        ) -> Result<bool, MetadataError> {
            match self.failure {
                FailurePoint::Membership => Err(Self::injected_error("membership")),
                _ => Ok(true),
            }
        }
    }

    #[tokio::test]
    async fn inline_commit_returns_commit_result_and_updates_head() {
        let metadata: Arc<dyn MetadataStore> =
            Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let service =
            CommitService::new(metadata.clone(), Arc::new(MockCasClient::new()), signer());

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

    #[tokio::test]
    async fn commit_rejects_non_main_revision_before_head_or_cas_work() {
        let metadata = Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let cas = Arc::new(MockCasClient::new());
        let service = CommitService::new(metadata.clone(), cas.clone(), signer());
        let body = "{\"key\":\"header\",\"value\":{\"summary\":\"add\",\"parentRevision\":null}}\n\
                    {\"key\":\"file\",\"value\":{\"path\":\"a.txt\",\"content\":\"YQ==\"}}";

        for revision in ["feature", "0123456789abcdef", "Main"] {
            let error = service
                .commit(CommitRequest {
                    username: "owner",
                    namespace: "owner",
                    repo_name: "repo",
                    revision,
                    repo_type: RepoType::Model,
                    body,
                })
                .await
                .expect_err("only main is writable");
            assert_eq!(
                error,
                CommitServiceError::Validation(
                    "Commits currently support only the 'main' revision".to_string()
                )
            );
        }

        assert_eq!(metadata.get_head(repo.id).await.unwrap(), None);
        assert!(
            metadata
                .get_commit_log(repo.id, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(cas.upload_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn commit_header_must_be_unique_and_first() {
        let header =
            "{\"key\":\"header\",\"value\":{\"summary\":\"first\",\"parentRevision\":null}}";
        let file = "{\"key\":\"file\",\"value\":{\"path\":\"a.txt\",\"content\":\"YQ==\"}}";

        let err = parse_commit_body(&format!("{}\n{}", file, header))
            .err()
            .expect("header after a file must be rejected");
        assert!(matches!(err, CommitServiceError::Validation(_)));
        assert!(err_message(err).contains("first non-empty"));

        let err = parse_commit_body(&format!("{}\n{}", header, header))
            .err()
            .expect("duplicate header must be rejected");
        assert!(err_message(err).contains("exactly one header"));

        let parsed = parse_commit_body(&format!("\n  \n{}\n", header))
            .expect("blank lines before the sole header are allowed");
        assert_eq!(parsed.header.summary, "first");
    }

    fn err_message(err: CommitServiceError) -> String {
        match err {
            CommitServiceError::Validation(message)
            | CommitServiceError::UnprocessableEntity(message)
            | CommitServiceError::Internal(message) => message,
            other => panic!("unexpected commit error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn commit_preserves_parent_snapshot_and_request_operation_order() {
        let metadata = Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        let repo = metadata
            .create_repo("owner", "ordered", RepoType::Model, false)
            .await
            .unwrap();
        let service =
            CommitService::new(metadata.clone(), Arc::new(MockCasClient::new()), signer());

        let initial = service
            .commit(CommitRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "ordered",
                revision: "main",
                repo_type: RepoType::Model,
                body: "{\"key\":\"header\",\"value\":{\"summary\":\"initial\",\"parentRevision\":null}}\n\
                       {\"key\":\"file\",\"value\":{\"path\":\"keep.txt\",\"content\":\"a2VlcA==\"}}\n\
                       {\"key\":\"file\",\"value\":{\"path\":\"target.txt\",\"content\":\"b2xk\"}}",
            })
            .await
            .unwrap();

        let file_then_delete = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"file then delete\",\"parentRevision\":\"{}\"}}}}\n\
             {{\"key\":\"file\",\"value\":{{\"path\":\"target.txt\",\"content\":\"bmV3\"}}}}\n\
             {{\"key\":\"deletedEntry\",\"value\":{{\"path\":\"target.txt\"}}}}",
            initial.commit_oid
        );
        let second = service
            .commit(CommitRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "ordered",
                revision: "main",
                repo_type: RepoType::Model,
                body: &file_then_delete,
            })
            .await
            .unwrap();
        let second_tree = metadata
            .get_file_tree(repo.id, &second.commit_oid)
            .await
            .unwrap();
        assert_eq!(
            second_tree
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt"]
        );
        assert!(
            second_tree
                .iter()
                .all(|entry| entry.commit_id == second.commit_oid),
            "unchanged parent entries must be copied into the new snapshot"
        );

        let delete_then_file = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"delete then file\",\"parentRevision\":\"{}\"}}}}\n\
             {{\"key\":\"deletedEntry\",\"value\":{{\"path\":\"target.txt\"}}}}\n\
             {{\"key\":\"file\",\"value\":{{\"path\":\"target.txt\",\"content\":\"bmV3\"}}}}",
            second.commit_oid
        );
        let third = service
            .commit(CommitRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "ordered",
                revision: "main",
                repo_type: RepoType::Model,
                body: &delete_then_file,
            })
            .await
            .unwrap();
        let third_tree = metadata
            .get_file_tree(repo.id, &third.commit_oid)
            .await
            .unwrap();
        assert_eq!(
            third_tree
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt", "target.txt"]
        );
    }

    #[tokio::test]
    async fn commit_propagates_metadata_read_failures() {
        for (failure, username, namespace, parent, expected_message) in [
            (FailurePoint::GetHead, "owner", "owner", None, "get_head"),
            (
                FailurePoint::GetFileTree,
                "owner",
                "owner",
                Some("parent"),
                "get_file_tree",
            ),
            (
                FailurePoint::Membership,
                "member",
                "organization",
                None,
                "membership",
            ),
        ] {
            let metadata: Arc<dyn MetadataStore> = Arc::new(FaultMetadataStore { failure });
            let service = CommitService::new(metadata, Arc::new(MockCasClient::new()), signer());
            let parent_json = parent
                .map(|value| format!("\"{}\"", value))
                .unwrap_or_else(|| "null".to_string());
            let body = format!(
                "{{\"key\":\"header\",\"value\":{{\"summary\":\"failure\",\"parentRevision\":{}}}}}",
                parent_json
            );

            let err = service
                .commit(CommitRequest {
                    username,
                    namespace,
                    repo_name: "repo",
                    revision: "main",
                    repo_type: RepoType::Model,
                    body: &body,
                })
                .await
                .expect_err("metadata failure must abort the commit");

            assert!(matches!(err, CommitServiceError::Internal(_)));
            assert!(err_message(err).contains(expected_message));
        }
    }

    #[tokio::test]
    async fn commit_rejects_lfs_size_above_sqlite_integer_range() {
        let metadata: Arc<dyn MetadataStore> =
            Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        metadata
            .create_repo("owner", "oversized", RepoType::Model, false)
            .await
            .unwrap();
        let service = CommitService::new(metadata, Arc::new(MockCasClient::new()), signer());
        let body = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"oversized\",\"parentRevision\":null}}}}\n\
             {{\"key\":\"lfsFile\",\"value\":{{\"path\":\"huge.bin\",\"oid\":\"{}\",\"size\":{}}}}}",
            "a".repeat(64),
            u64::MAX
        );

        let err = service
            .commit(CommitRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "oversized",
                revision: "main",
                repo_type: RepoType::Model,
                body: &body,
            })
            .await
            .expect_err("oversized LFS metadata must be rejected before CAS lookup");

        assert!(matches!(err, CommitServiceError::Validation(_)));
        assert!(err_message(err).contains("supported metadata range"));
    }

    #[tokio::test]
    async fn commit_requires_declared_lfs_size_to_match_cas() {
        let metadata: Arc<dyn MetadataStore> =
            Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        metadata
            .create_repo("owner", "size-mismatch", RepoType::Model, false)
            .await
            .unwrap();
        let service = CommitService::new(
            metadata,
            Arc::new(SizedBlobCasClient { size: 42 }),
            signer(),
        );
        let body = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"mismatch\",\"parentRevision\":null}}}}\n\
             {{\"key\":\"lfsFile\",\"value\":{{\"path\":\"model.bin\",\"oid\":\"{}\",\"size\":41}}}}",
            "a".repeat(64)
        );

        let err = service
            .commit(CommitRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "size-mismatch",
                revision: "main",
                repo_type: RepoType::Model,
                body: &body,
            })
            .await
            .expect_err("declared and verified LFS sizes must match");

        assert!(matches!(err, CommitServiceError::UnprocessableEntity(_)));
        let message = err_message(err);
        assert!(message.contains("commit declares 41"));
        assert!(message.contains("CAS reports 42"));
    }
}
