use actix_web::{web, HttpResponse};
use crate::auth::extract::{AuthUser, AuthWrite};
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClientTrait;
use crate::metadata::{FileEntry, MetadataStore, RepoType, Revision};
use sha2::{Sha256, Digest};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum size for inline file content (10MB)
const MAX_INLINE_SIZE: usize = 10 * 1024 * 1024;

/// NDJSON commit operations
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "key", content = "value")]
#[serde(rename_all = "camelCase")]
pub enum CommitOperation {
    Header(CommitHeader),
    File(FileOperation),
    LfsFile(LfsFileOperation),
    DeletedEntry(DeletedEntryOperation),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CommitHeader {
    pub summary: String,
    #[serde(default, rename = "parentRevision")]
    pub parent_revision: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FileOperation {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LfsFileOperation {
    pub path: String,
    pub oid: String,
    pub size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DeletedEntryOperation {
    pub path: String,
}

/// Commit response
#[derive(Debug, Serialize, Deserialize)]
pub struct CommitResponse {
    #[serde(rename = "commitOid")]
    pub commit_oid: String,
    #[serde(rename = "commitUrl")]
    pub commit_url: String,
    #[serde(rename = "prUrl")]
    pub pr_url: Option<String>,
    #[serde(rename = "prNum")]
    pub pr_num: Option<u64>,
}

/// Get current Unix timestamp
fn now_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Generate a commit ID from repo_id, parent, message, timestamp, and UUID nonce
fn generate_commit_id(repo_id: i64, parent: Option<&str>, message: &str, timestamp: i64) -> String {
    let nonce = uuid::Uuid::new_v4().to_string();
    let input = format!("{}:{}:{}:{}:{}", repo_id, parent.unwrap_or(""), message, timestamp, nonce);
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// Decode base64 content (handles "base64:" prefix or raw base64)
fn decode_base64_content(content: &str) -> Result<Vec<u8>, String> {
    let content_to_decode = content.strip_prefix("base64:").unwrap_or(content);
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.decode(content_to_decode).map_err(|e| format!("Base64 decode error: {}", e))
}

/// Internal helper for commit handling
///
/// **Known tradeoff:** Inline files are uploaded to CAS *before* the atomic metadata commit.
/// If the metadata commit fails (conflict), the inline file blob remains in CAS as an orphaned
/// object with no metadata reference. This is acceptable because:
/// 1. Blobs are content-addressed and deduplicated, so re-uploads of the same content are free
/// 2. Orphaned blobs don't affect correctness, only storage efficiency
/// 3. A background GC job could clean up orphaned blobs in the future if needed
async fn handle_commit(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: String,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    cas_client: web::Data<std::sync::Arc<dyn CasClientTrait>>,
    signer: web::Data<std::sync::Arc<XetSigner>>,
) -> HttpResponse {
    // Check body size limit (20MB) to prevent memory exhaustion
    const MAX_COMMIT_BODY_SIZE: usize = 20 * 1024 * 1024;
    if body.len() > MAX_COMMIT_BODY_SIZE {
        return HttpResponse::PayloadTooLarge().json(serde_json::json!({
            "error": format!("Commit body too large ({} bytes), max allowed: {} bytes", body.len(), MAX_COMMIT_BODY_SIZE),
            "error_type": "PayloadTooLarge"
        }));
    }

    let (namespace, repo_name, _revision) = path.into_inner();

    // Get the repo
    let repo = match metadata.get_repo(&namespace, &repo_name, repo_type).await {
        Ok(r) => r,
        Err(e) => {
            return match e {
                crate::metadata::MetadataError::RepoNotFound(_) => {
                    HttpResponse::NotFound().json(serde_json::json!({
                        "error": e.to_string(),
                        "error_type": "NotFoundError"
                    }))
                }
                _ => HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": e.to_string(),
                    "error_type": "InternalError"
                }))
            };
        }
    };

    // Parse NDJSON body line by line
    let mut header: Option<CommitHeader> = None;
    let mut files: Vec<FileOperation> = Vec::new();
    let mut lfs_files: Vec<LfsFileOperation> = Vec::new();
    let mut deleted_entries: Vec<DeletedEntryOperation> = Vec::new();

    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let op: CommitOperation = match serde_json::from_str(line) {
            Ok(o) => o,
            Err(e) => {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": format!("Invalid NDJSON line: {}", e),
                    "error_type": "ValidationError"
                }));
            }
        };
        match op {
            CommitOperation::Header(h) => header = Some(h),
            CommitOperation::File(f) => files.push(f),
            CommitOperation::LfsFile(lf) => lfs_files.push(lf),
            CommitOperation::DeletedEntry(d) => deleted_entries.push(d),
        }
    }

    let header = match header {
        Some(h) => h,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Missing header in commit",
                "error_type": "ValidationError"
            }));
        }
    };

    // Two-phase HEAD check (I3): this pre-check provides early rejection with specific
    // error messages before doing expensive CAS uploads. The authoritative check
    // happens inside commit_atomic() under BEGIN IMMEDIATE lock for correctness.
    // Both are necessary: this one for UX (better error messages), the atomic one
    // for race-safety (prevents conflicts from concurrent commits).
    // Get current HEAD for OCC check
    let current_head = metadata.get_head(repo.id).await.ok().flatten();
    let parent_revision = header.parent_revision.clone();

    // Validate parent revision matches current HEAD (pre-check for better error messages)
    match (&parent_revision, &current_head) {
        (Some(parent), Some(head)) => {
            if parent != head {
                return HttpResponse::Conflict().json(serde_json::json!({
                    "error": "Parent revision does not match current HEAD",
                    "error_type": "ConflictError",
                    "currentHead": head
                }));
            }
        }
        (Some(_parent), None) => {
            // Parent specified but no current HEAD - this is an error for non-empty repos
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "Parent revision specified but repository has no HEAD",
                "error_type": "ConflictError",
                "currentHead": null
            }));
        }
        (None, Some(head)) => {
            // I9: No parent specified but repo has HEAD - this is a conflict
            // Must explicitly specify the parent when repo already has commits
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": format!("No parent specified but repository already has HEAD: {}", head),
                "error_type": "ConflictError",
                "currentHead": head
            }));
        }
        (None, None) => {
            // No parent, no HEAD - first commit
        }
    }

    // Generate internal token for CAS operations
    let (internal_token, _) = signer.sign_internal();

    // Generate new commit ID
    let timestamp = now_timestamp();
    let commit_id = generate_commit_id(repo.id, current_head.as_deref(), &header.summary, timestamp);

    // Build file entries
    let mut file_entries: Vec<FileEntry> = Vec::new();

    // Process inline files - decode, check size, compute SHA256, and store in CAS
    for file_op in files {
        let decoded_content = match decode_base64_content(&file_op.content) {
            Ok(c) => c,
            Err(e) => {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": e,
                    "error_type": "ValidationError"
                }));
            }
        };

        // Check size limit (I4)
        if decoded_content.len() > MAX_INLINE_SIZE {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Inline file too large: {} bytes (max {})", decoded_content.len(), MAX_INLINE_SIZE),
                "error_type": "ValidationError"
            }));
        }

        // Compute SHA256 oid
        let oid = hex::encode(Sha256::digest(&decoded_content));
        let size = decoded_content.len() as u64;

        // Store inline file content in CAS (C1)
        if let Err(e) = cas_client.proxy_lfs_upload(&oid, bytes::Bytes::from(decoded_content), &internal_token).await {
            tracing::error!("Failed to store inline file in CAS: status={}, error={}", e.status, e.message);
            let status_code = actix_web::http::StatusCode::from_u16(e.status).unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            return HttpResponse::build(status_code).json(serde_json::json!({
                "error": format!("Failed to store inline file in CAS: {}", e.message),
                "error_type": "CasError"
            }));
        }

        file_entries.push(FileEntry {
            path: file_op.path,
            repo_id: repo.id,
            commit_id: commit_id.clone(),
            size,
            cas_hash: oid.clone(),
            is_lfs: false,
        });
    }

    // Process LFS files - verify they exist in CAS (C2)
    for lfs_op in lfs_files {
        // Validate OID format before CAS verification (defense-in-depth)
        if lfs_op.oid.len() != 64 || !lfs_op.oid.chars().all(|c| c.is_ascii_hexdigit()) {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid LFS OID format for {}: expected 64-character hex string", lfs_op.path),
                "error_type": "ValidationError"
            }));
        }

        // Verify LFS file exists in CAS before accepting
        match cas_client.head_blob(&lfs_op.oid, &internal_token).await {
            Ok(_) => { /* blob exists, proceed */ }
            Err(crate::error::HubError::NotFound(_)) => {
                return HttpResponse::UnprocessableEntity().json(serde_json::json!({
                    "error": format!("LFS file not found in CAS: {}", lfs_op.oid),
                    "error_type": "UnprocessableEntity"
                }));
            }
            Err(e) => {
                return HttpResponse::BadGateway().json(serde_json::json!({
                    "error": format!("CAS verification failed: {}", e),
                    "error_type": "CasError"
                }));
            }
        }

        file_entries.push(FileEntry {
            path: lfs_op.path,
            repo_id: repo.id,
            commit_id: commit_id.clone(),
            size: lfs_op.size,
            cas_hash: lfs_op.oid,
            is_lfs: true,
        });
    }

    // Copy parent's file tree (if parent exists) and apply changes
    let parent_entries: Vec<FileEntry> = if let Some(parent_commit) = &current_head {
        metadata.get_file_tree(repo.id, parent_commit).await.ok().unwrap_or_default()
    } else {
        Vec::new()
    };

    // Start from parent's tree, apply additions/deletions
    let mut final_entries: std::collections::HashMap<String, FileEntry> = std::collections::HashMap::new();
    for entry in parent_entries {
        final_entries.insert(entry.path.clone(), entry);
    }

    // Apply deletions
    for deleted in deleted_entries {
        final_entries.remove(&deleted.path);
    }

    // Apply additions/updates (copy entries but update commit_id)
    for entry in file_entries {
        final_entries.insert(entry.path.clone(), FileEntry {
            path: entry.path,
            repo_id: repo.id,
            commit_id: commit_id.clone(),
            size: entry.size,
            cas_hash: entry.cas_hash,
            is_lfs: entry.is_lfs,
        });
    }

    // Convert to vector for storage
    let final_entries_vec: Vec<FileEntry> = final_entries.values().cloned().collect();

    // Create revision
    let revision = Revision {
        commit_id: commit_id.clone(),
        repo_id: repo.id,
        parent: current_head.clone(),
        message: header.summary.clone(),
        author: auth.info.username.clone(),
        created_at: timestamp,
    };

    // Atomically commit: check OCC + add revision + add file entries + set HEAD (C3)
    match metadata.commit_atomic(&revision, &final_entries_vec, parent_revision.as_deref()).await {
        Ok(_) => {}
        Err(crate::metadata::MetadataError::Conflict(actual_head)) => {
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "Parent revision does not match current HEAD",
                "error_type": "ConflictError",
                "currentHead": actual_head
            }));
        }
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("{}", e),
                "error_type": "InternalError"
            }));
        }
    }

    HttpResponse::Ok().json(CommitResponse {
        commit_oid: commit_id.clone(),
        commit_url: format!("/{}/{}/commit/{}", namespace, repo_name, commit_id),
        pr_url: None,
        pr_num: None,
    })
}

// Model commit handler
pub async fn commit_model(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: String,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    cas_client: web::Data<std::sync::Arc<dyn CasClientTrait>>,
    signer: web::Data<std::sync::Arc<XetSigner>>,
) -> HttpResponse {
    handle_commit(auth, path, body, RepoType::Model, metadata, cas_client, signer).await
}

// Dataset commit handler
pub async fn commit_dataset(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: String,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    cas_client: web::Data<std::sync::Arc<dyn CasClientTrait>>,
    signer: web::Data<std::sync::Arc<XetSigner>>,
) -> HttpResponse {
    handle_commit(auth, path, body, RepoType::Dataset, metadata, cas_client, signer).await
}

// Space commit handler
pub async fn commit_space(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: String,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    cas_client: web::Data<std::sync::Arc<dyn CasClientTrait>>,
    signer: web::Data<std::sync::Arc<XetSigner>>,
) -> HttpResponse {
    handle_commit(auth, path, body, RepoType::Space, metadata, cas_client, signer).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, App};
    use crate::auth::token_store::TokenStore;
    use crate::metadata::SqliteMetadataStore;
    use crate::cas_client::{BlobState, CasUploadError};
    use crate::error::HubError;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    /// Mock CAS client for testing without a real CAS server
    struct MockCasClient {
        /// OIDs that should be considered as existing in CAS
        existing_oids: std::collections::HashSet<String>,
        /// Whether uploads should succeed
        allow_uploads: bool,
    }

    impl MockCasClient {
        fn new() -> Self {
            Self {
                existing_oids: std::collections::HashSet::new(),
                allow_uploads: true,
            }
        }

        fn with_existing_oid(mut self, oid: &str) -> Self {
            self.existing_oids.insert(oid.to_string());
            self
        }

        fn with_upload_failure(mut self) -> Self {
            self.allow_uploads = false;
            self
        }
    }

    #[async_trait::async_trait]
    impl CasClientTrait for MockCasClient {
        async fn head_blob(&self, oid: &str, _internal_token: &str) -> Result<BlobState, HubError> {
            if self.existing_oids.contains(oid) {
                Ok(BlobState {
                    state: "raw_only".to_string(),
                    xet_file_id: None,
                    size: 0,
                    sha256: oid.to_string(),
                })
            } else {
                Err(HubError::NotFound(format!("Blob not found: {}", oid)))
            }
        }

        async fn proxy_lfs_upload(&self, _oid: &str, _data: bytes::Bytes, _token: &str) -> Result<(), CasUploadError> {
            if self.allow_uploads {
                Ok(())
            } else {
                Err(CasUploadError {
                    status: 500,
                    message: "Mock CAS upload failure".to_string(),
                })
            }
        }
    }

    async fn setup_test_env_with_mock(mock_cas: MockCasClient) -> (std::sync::Arc<TokenStore>, std::sync::Arc<dyn MetadataStore>, std::sync::Arc<dyn CasClientTrait>, std::sync::Arc<XetSigner>) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> = std::sync::Arc::new(
            SqliteMetadataStore::in_memory().await.unwrap()
        );
        let cas_client: std::sync::Arc<dyn CasClientTrait> = std::sync::Arc::new(mock_cas);
        let signing_key = SigningKey::generate(&mut OsRng);
        let signer = std::sync::Arc::new(XetSigner::new(signing_key, "test-key", 3600));
        (token_store, metadata, cas_client, signer)
    }

    // Test commit with inline file using mock CAS
    #[actix_web::test]
    async fn test_commit_with_inline_file() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        // NDJSON body with inline file
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let content = STANDARD.encode("{\"test\": true}");
        let body = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"Add config\",\"parentRevision\":null}}}}\n\
             {{\"key\":\"file\",\"value\":{{\"path\":\"config.json\",\"content\":\"{}\"}}}}",
            content
        );

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        // With mock CAS that allows uploads, the commit should succeed
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
    }

    // Test commit with LFS file that doesn't exist in CAS using mock
    #[actix_web::test]
    async fn test_commit_with_lfs_file_not_in_cas() {
        let mock_cas = MockCasClient::new(); // No existing OIDs
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        // NDJSON body with LFS file that doesn't exist in CAS
        let oid = "a".repeat(64);
        let body = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"Add model\",\"parentRevision\":null}}}}\n\
             {{\"key\":\"lfsFile\",\"value\":{{\"path\":\"model.bin\",\"oid\":\"{}\",\"size\":1073741824}}}}",
            oid
        );

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        // Mock CAS will return NotFound for the LFS file, so we expect UnprocessableEntity (422)
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Test commit with invalid LFS OID format (defense-in-depth validation)
    #[actix_web::test]
    async fn test_commit_with_invalid_lfs_oid_format() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        // NDJSON body with LFS file that has invalid OID format (too short, not 64 chars)
        let body = "{\"key\":\"header\",\"value\":{\"summary\":\"Add model\",\"parentRevision\":null}}\n\
                   {\"key\":\"lfsFile\",\"value\":{\"path\":\"model.bin\",\"oid\":\"tooshort\",\"size\":1073741824}}";

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        // OID format validation should reject with BadRequest (400)
        assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn test_commitAtomic_rejects_mismatched_parent() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo and initial commit
        let repo = metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();
        let initial_commit = Revision {
            commit_id: "initial123".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "Initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        };
        metadata.add_revision(initial_commit).await.unwrap();
        metadata.set_head(repo.id, "initial123").await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        // Try to commit with wrong parent
        let body = "{\"key\":\"header\",\"value\":{\"summary\":\"Update\",\"parentRevision\":\"wrong_parent\"}}";

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["currentHead"], "initial123");
    }

    #[actix_web::test]
    async fn test_commit_read_only_token() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store.create_token("testuser", "test-token", "read").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        let body = "{\"key\":\"header\",\"value\":{\"summary\":\"Add config\"}}";

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
    }

    #[actix_web::test]
    async fn test_commit_inline_file_too_large() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        // Configure actix-web to accept larger payloads for testing
        let payload_config = web::PayloadConfig::default().limit(20 * 1024 * 1024);

        let app = actix_test::init_service(
            App::new()
                .app_data(payload_config)
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        // Create a large content (> 10MB)
        let large_content = vec![0u8; MAX_INLINE_SIZE + 1];
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let encoded = STANDARD.encode(&large_content);
        let body = format!(
            "{{\"key\":\"header\",\"value\":{{\"summary\":\"Add large file\",\"parentRevision\":null}}}}\n\
             {{\"key\":\"file\",\"value\":{{\"path\":\"large.bin\",\"content\":\"{}\"}}}}",
            encoded
        );

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);

        let resp_body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert!(resp_body["error"].as_str().unwrap().contains("Inline file too large"));
    }

    #[test]
    fn test_decode_base64_with_prefix() {
        let content = "base64:eyJ0ZXN0IjogdHJ1ZX0=";
        let decoded = decode_base64_content(content).unwrap();
        assert_eq!(decoded, b"{\"test\": true}".to_vec());
    }

    #[test]
    fn test_decode_base64_without_prefix() {
        let content = "eyJ0ZXN0IjogdHJ1ZX0=";
        let decoded = decode_base64_content(content).unwrap();
        assert_eq!(decoded, b"{\"test\": true}".to_vec());
    }

    #[test]
    fn test_generate_commit_id_unique_with_nonce() {
        // With UUID nonce, even same inputs should produce different IDs
        let id1 = generate_commit_id(1, Some("parent123"), "message", 1000);
        let id2 = generate_commit_id(1, Some("parent123"), "message", 1000);
        // IDs should be different due to UUID nonce
        assert_ne!(id1, id2, "Commit IDs should be unique due to UUID nonce");

        // Different inputs should also produce different IDs
        let id3 = generate_commit_id(1, Some("parent123"), "different message", 1000);
        assert_ne!(id1, id3);
    }
}
