use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
use crate::metadata::{FileEntry, MetadataStore, RepoType, Revision};
use sha2::{Sha256, Digest};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub commit_oid: String,
}

/// Extract Bearer token from Authorization header
fn extract_bearer(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Get current Unix timestamp
fn now_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Generate a commit ID from repo_id, parent, message, and timestamp
fn generate_commit_id(repo_id: i64, parent: Option<&str>, message: &str, timestamp: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.to_string().as_bytes());
    hasher.update(parent.unwrap_or("").as_bytes());
    hasher.update(message.as_bytes());
    hasher.update(timestamp.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

/// Decode base64 content (handles "base64:" prefix or raw base64)
fn decode_base64_content(content: &str) -> Result<Vec<u8>, String> {
    let content_to_decode = if content.starts_with("base64:") {
        &content[7..]
    } else {
        content
    };
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.decode(content_to_decode).map_err(|e| format!("Base64 decode error: {}", e))
}

/// Internal helper for commit handling
async fn handle_commit(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: String,
    repo_type: RepoType,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    // Extract and validate Bearer token
    let token = match extract_bearer(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let info = match token_store.validate_token(&token) {
        Ok(Some(i)) => i,
        Ok(None) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token",
                "error_type": "AuthenticationError"
            }));
        }
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("{}", e),
                "error_type": "InternalError"
            }));
        }
    };

    // Check write scope
    if info.scope != "write" {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Token does not have write scope",
            "error_type": "AuthorizationError"
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

    // Get current HEAD
    let current_head = metadata.get_head(repo.id).await.ok().flatten();

    // Check parentRevision matches current HEAD (OCC)
    let parent_revision = header.parent_revision.clone();
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
        (None, Some(_head)) => {
            // No parent specified but repo has HEAD - use current HEAD as parent
            // This is acceptable for the first commit to the repo
        }
        (None, None) => {
            // No parent, no HEAD - first commit
        }
    }

    // Generate new commit ID
    let timestamp = now_timestamp();
    let commit_id = generate_commit_id(repo.id, current_head.as_deref(), &header.summary, timestamp);

    // Build file entries
    let mut file_entries: Vec<FileEntry> = Vec::new();

    // Process inline files
    for file_op in files {
        let content = match decode_base64_content(&file_op.content) {
            Ok(c) => c,
            Err(e) => {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": e,
                    "error_type": "ValidationError"
                }));
            }
        };
        // Compute SHA256 oid
        let oid = hex::encode(Sha256::digest(&content));
        let size = content.len() as u64;

        file_entries.push(FileEntry {
            path: file_op.path,
            repo_id: repo.id,
            commit_id: commit_id.clone(),
            size,
            cas_hash: oid.clone(),
            is_lfs: false,
        });
    }

    // Process LFS files
    for lfs_op in lfs_files {
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

    // Add revision
    let revision = Revision {
        commit_id: commit_id.clone(),
        repo_id: repo.id,
        parent: current_head,
        message: header.summary.clone(),
        author: info.username.clone(),
        created_at: timestamp,
    };

    if let Err(e) = metadata.add_revision(revision).await {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("{}", e),
            "error_type": "InternalError"
        }));
    }

    // Add file entries
    if !final_entries_vec.is_empty() {
        if let Err(e) = metadata.add_file_entries(final_entries_vec).await {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("{}", e),
                "error_type": "InternalError"
            }));
        }
    }

    // Set HEAD to new revision
    if let Err(e) = metadata.set_head(repo.id, &commit_id).await {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("{}", e),
            "error_type": "InternalError"
        }));
    }

    HttpResponse::Ok().json(CommitResponse { commit_oid: commit_id })
}

// Model commit handler
pub async fn commit_model(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: String,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_commit(req, path, body, RepoType::Model, token_store, metadata).await
}

// Dataset commit handler
pub async fn commit_dataset(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: String,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_commit(req, path, body, RepoType::Dataset, token_store, metadata).await
}

// Space commit handler
pub async fn commit_space(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: String,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_commit(req, path, body, RepoType::Space, token_store, metadata).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, App};
    use crate::metadata::SqliteMetadataStore;

    fn setup_test_env() -> (std::sync::Arc<TokenStore>, std::sync::Arc<dyn MetadataStore>) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> = std::sync::Arc::new(
            SqliteMetadataStore::in_memory().unwrap()
        );
        (token_store, metadata)
    }

    #[actix_web::test]
    async fn test_commit_with_inline_file() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "write").unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
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
        assert!(resp.status().is_success());

        let body: CommitResponse = actix_test::read_body_json(resp).await;
        assert!(!body.commit_oid.is_empty());

        // Verify HEAD was set
        let head = metadata.get_head(metadata.get_repo("testuser", "my-model", RepoType::Model).await.unwrap().id).await.unwrap();
        assert!(head.is_some());
    }

    #[actix_web::test]
    async fn test_commit_with_lfs_file() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "write").unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}/commit/{revision}", web::post().to(commit_model))
        ).await;

        // NDJSON body with LFS file
        let body = "{\"key\":\"header\",\"value\":{\"summary\":\"Add model\",\"parentRevision\":null}}\n\
                   {\"key\":\"lfsFile\",\"value\":{\"path\":\"model.bin\",\"oid\":\"abc123\",\"size\":1073741824}}";

        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/commit/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/x-ndjson"))
            .set_payload(body)
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: CommitResponse = actix_test::read_body_json(resp).await;
        assert!(!body.commit_oid.is_empty());
    }

    #[actix_web::test]
    async fn test_commit_conflict_wrong_parent() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "write").unwrap();

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
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
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
    fn test_generate_commit_id() {
        let id1 = generate_commit_id(1, Some("parent123"), "message", 1000);
        let id2 = generate_commit_id(1, Some("parent123"), "message", 1000);
        assert_eq!(id1, id2);

        let id3 = generate_commit_id(1, Some("parent123"), "different message", 1000);
        assert_ne!(id1, id3);
    }
}