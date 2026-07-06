use crate::auth::extract::{AuthUser, AuthWrite};
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClientTrait;
#[cfg(test)]
use crate::commit::content::decode_base64_content;
#[cfg(test)]
use crate::commit::id::generate_commit_id;
#[cfg(test)]
use crate::commit::types::MAX_INLINE_SIZE;
#[cfg(test)]
use crate::commit::validation::validate_file_path;
#[cfg(test)]
use crate::metadata::Revision;
use crate::metadata::{MetadataStore, RepoType};
use crate::services::commit::{CommitRequest, CommitService, CommitServiceError};
use actix_web::{HttpResponse, web};
use std::sync::Arc;

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
    metadata: web::Data<Arc<dyn MetadataStore>>,
    cas_client: web::Data<Arc<dyn CasClientTrait>>,
    signer: web::Data<Arc<XetSigner>>,
) -> HttpResponse {
    let (namespace, repo_name, revision) = path.into_inner();
    let service = CommitService::new(
        metadata.get_ref().clone(),
        cas_client.get_ref().clone(),
        signer.get_ref().clone(),
    );

    match service
        .commit(CommitRequest {
            username: &auth.info.username,
            namespace: &namespace,
            repo_name: &repo_name,
            revision: &revision,
            repo_type,
            body: &body,
        })
        .await
    {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(err) => commit_error_response(err),
    }
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn commit_error_response(err: CommitServiceError) -> HttpResponse {
    match err {
        CommitServiceError::PayloadTooLarge { actual, max } => HttpResponse::PayloadTooLarge()
            .json(error_json(
                format!(
                    "Commit body too large ({} bytes), max allowed: {} bytes",
                    actual, max
                ),
                "PayloadTooLarge",
            )),
        CommitServiceError::Validation(message) => {
            HttpResponse::BadRequest().json(error_json(message, "ValidationError"))
        }
        CommitServiceError::Forbidden(message) => {
            HttpResponse::Forbidden().json(error_json(message, "ForbiddenError"))
        }
        CommitServiceError::NotFound(message) => {
            HttpResponse::NotFound().json(error_json(message, "NotFoundError"))
        }
        CommitServiceError::Conflict {
            message,
            current_head,
            note,
        } => {
            let mut body = serde_json::json!({
                "error": message,
                "error_type": "ConflictError",
                "currentHead": current_head
            });
            if let Some(note) = note {
                body["note"] = serde_json::json!(note);
            }
            HttpResponse::Conflict().json(body)
        }
        CommitServiceError::UnprocessableEntity(message) => {
            HttpResponse::UnprocessableEntity().json(error_json(message, "UnprocessableEntity"))
        }
        CommitServiceError::CasUpload { status, message } => {
            let status_code = actix_web::http::StatusCode::from_u16(status)
                .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            HttpResponse::build(status_code).json(error_json(
                format!("Failed to store inline file in CAS: {}", message),
                "CasError",
            ))
        }
        CommitServiceError::BadGateway(message) => {
            HttpResponse::BadGateway().json(error_json(message, "CasError"))
        }
        CommitServiceError::Internal(message) => {
            HttpResponse::InternalServerError().json(error_json(message, "InternalError"))
        }
    }
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
    handle_commit(
        auth,
        path,
        body,
        RepoType::Model,
        metadata,
        cas_client,
        signer,
    )
    .await
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
    handle_commit(
        auth,
        path,
        body,
        RepoType::Dataset,
        metadata,
        cas_client,
        signer,
    )
    .await
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
    handle_commit(
        auth,
        path,
        body,
        RepoType::Space,
        metadata,
        cas_client,
        signer,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token_store::TokenStore;
    use crate::cas_client::{BlobState, CasUploadError};
    use crate::error::HubError;
    use crate::metadata::SqliteMetadataStore;
    use actix_web::{App, test as actix_test};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    /// Mock CAS client for testing without a real CAS server
    struct MockCasClient {
        /// OIDs that should be considered as existing in CAS
        existing_oids: std::collections::HashSet<String>,
        /// Whether uploads should succeed
        allow_uploads: bool,
        /// Optional prefix required for upload tokens.
        required_upload_token_prefix: Option<&'static str>,
    }

    impl MockCasClient {
        fn new() -> Self {
            Self {
                existing_oids: std::collections::HashSet::new(),
                allow_uploads: true,
                required_upload_token_prefix: None,
            }
        }

        fn requiring_upload_token_prefix(mut self, prefix: &'static str) -> Self {
            self.required_upload_token_prefix = Some(prefix);
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

        async fn proxy_lfs_upload(
            &self,
            _oid: &str,
            _data: bytes::Bytes,
            token: &str,
        ) -> Result<(), CasUploadError> {
            if let Some(prefix) = self.required_upload_token_prefix
                && !token.starts_with(prefix)
            {
                return Err(CasUploadError {
                    status: 401,
                    message: format!("expected upload token prefix {prefix}"),
                });
            }

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

    async fn setup_test_env_with_mock(
        mock_cas: MockCasClient,
    ) -> (
        std::sync::Arc<TokenStore>,
        std::sync::Arc<dyn MetadataStore>,
        std::sync::Arc<dyn CasClientTrait>,
        std::sync::Arc<XetSigner>,
    ) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> =
            std::sync::Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        let cas_client: std::sync::Arc<dyn CasClientTrait> = std::sync::Arc::new(mock_cas);
        let signing_key = SigningKey::generate(&mut OsRng);
        let signer = std::sync::Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300));
        (token_store, metadata, cas_client, signer)
    }

    // Test commit with inline file using mock CAS
    #[actix_web::test]
    async fn test_commit_with_inline_file() {
        let mock_cas = MockCasClient::new().requiring_upload_token_prefix("xet_");
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        // Create repo
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route(
                    "/api/models/{ns}/{repo}/commit/{revision}",
                    web::post().to(commit_model),
                ),
        )
        .await;

        // NDJSON body with inline file
        use base64::{Engine as _, engine::general_purpose::STANDARD};
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
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        // Create repo
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route(
                    "/api/models/{ns}/{repo}/commit/{revision}",
                    web::post().to(commit_model),
                ),
        )
        .await;

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
        assert_eq!(
            resp.status(),
            actix_web::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    // Test commit with invalid LFS OID format (defense-in-depth validation)
    #[actix_web::test]
    async fn test_commit_with_invalid_lfs_oid_format() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        // Create repo
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route(
                    "/api/models/{ns}/{repo}/commit/{revision}",
                    web::post().to(commit_model),
                ),
        )
        .await;

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
    async fn test_commit_atomic_rejects_mismatched_parent() {
        let mock_cas = MockCasClient::new();
        let (token_store, metadata, cas_client, signer) = setup_test_env_with_mock(mock_cas).await;
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        // Create repo and initial commit
        let repo = metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();
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
                .route(
                    "/api/models/{ns}/{repo}/commit/{revision}",
                    web::post().to(commit_model),
                ),
        )
        .await;

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
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

        // Create repo
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route(
                    "/api/models/{ns}/{repo}/commit/{revision}",
                    web::post().to(commit_model),
                ),
        )
        .await;

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
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        // Create repo
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        // Configure actix-web to accept larger payloads for testing
        let payload_config = web::PayloadConfig::default().limit(20 * 1024 * 1024);

        let app = actix_test::init_service(
            App::new()
                .app_data(payload_config)
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(cas_client.clone()))
                .app_data(web::Data::new(signer.clone()))
                .route(
                    "/api/models/{ns}/{repo}/commit/{revision}",
                    web::post().to(commit_model),
                ),
        )
        .await;

        // Create a large content (> 10MB)
        let large_content = vec![0u8; MAX_INLINE_SIZE + 1];
        use base64::{Engine as _, engine::general_purpose::STANDARD};
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
        assert!(
            resp_body["error"]
                .as_str()
                .unwrap()
                .contains("Inline file too large")
        );
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

    // I1 fix: Tests for path validation
    #[test]
    fn test_validate_file_path_valid() {
        assert!(validate_file_path("config.json").is_ok());
        assert!(validate_file_path("src/main.rs").is_ok());
        assert!(validate_file_path("a/b/c/d.txt").is_ok());
        assert!(validate_file_path("file..name").is_ok()); // Double dots in name (not component) OK
    }

    #[test]
    fn test_validate_file_path_empty() {
        assert!(validate_file_path("").is_err());
    }

    #[test]
    fn test_validate_file_path_absolute() {
        assert!(validate_file_path("/etc/passwd").is_err());
        assert!(validate_file_path("\\windows\\system32").is_err());
    }

    #[test]
    fn test_validate_file_path_traversal() {
        assert!(validate_file_path("../etc/passwd").is_err());
        assert!(validate_file_path("src/../../../etc/passwd").is_err());
        assert!(validate_file_path("..").is_err());
    }

    #[test]
    fn test_validate_file_path_null_bytes() {
        assert!(validate_file_path("file\0.txt").is_err());
    }

    #[test]
    fn test_validate_file_path_double_slash() {
        assert!(validate_file_path("a//b").is_err());
    }

    #[test]
    fn test_validate_file_path_reserved_names() {
        assert!(validate_file_path("CON").is_err());
        assert!(validate_file_path("NUL").is_err());
        assert!(validate_file_path("COM1/test").is_err());
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
