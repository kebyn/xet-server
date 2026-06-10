use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
use crate::metadata::{MetadataStore, RepoType};
use crate::config::HubConfig;

/// Extract Bearer token from Authorization header
fn extract_bearer(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Resolve a revision name/branch to a commit ID
async fn resolve_revision(
    metadata: &dyn MetadataStore,
    repo_id: i64,
    revision: &str,
) -> Result<String, String> {
    // If revision looks like a commit hash (long hex string), use it directly
    if revision.len() >= 8 && revision.chars().all(|c| c.is_ascii_hexdigit()) {
        // Check if it's a known revision
        if metadata.get_revision(repo_id, revision).await.is_ok() {
            return Ok(revision.to_string());
        }
    }

    // If revision is "main" or a branch name, resolve to HEAD
    let head = metadata.get_head(repo_id).await.ok().flatten();
    match head {
        Some(h) => Ok(h),
        None => Err(format!("Revision not found: {}", revision)),
    }
}

/// File resolve response
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ResolveResponse {
    pub oid: String,
    pub size: u64,
    pub download_url: String,
}

/// Internal helper for file resolve/download
async fn handle_resolve(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    repo_type: RepoType,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
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

    match token_store.validate_token(&token) {
        Ok(Some(_)) => {},
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

    let (namespace, repo_name, revision, file_path) = path.into_inner();

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

    // Resolve revision
    let commit_id = match resolve_revision(metadata.as_ref().as_ref(), repo.id, &revision).await {
        Ok(c) => c,
        Err(e) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": e,
                "error_type": "NotFoundError"
            }));
        }
    };

    // Resolve file
    let file_entry = match metadata.resolve_file(repo.id, &commit_id, &file_path).await {
        Ok(f) => f,
        Err(e) => {
            return match e {
                crate::metadata::MetadataError::FileNotFound(_) => {
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

    // Build download URL
    let download_url = format!("{}/lfs/objects/{}", config.cas.base_url, file_entry.cas_hash);

    HttpResponse::Ok().json(ResolveResponse {
        oid: file_entry.cas_hash,
        size: file_entry.size,
        download_url,
    })
}

// Model resolve handler
pub async fn resolve_model(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    handle_resolve(req, path, RepoType::Model, token_store, metadata, config).await
}

// Dataset resolve handler
pub async fn resolve_dataset(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    handle_resolve(req, path, RepoType::Dataset, token_store, metadata, config).await
}

// Space resolve handler
pub async fn resolve_space(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    handle_resolve(req, path, RepoType::Space, token_store, metadata, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, App};
    use crate::metadata::{FileEntry, Revision, SqliteMetadataStore};

    fn setup_test_env_with_files() -> (
        std::sync::Arc<TokenStore>,
        std::sync::Arc<dyn MetadataStore>,
        HubConfig,
    ) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> = std::sync::Arc::new(
            SqliteMetadataStore::in_memory().unwrap()
        );
        let config = HubConfig::default();
        (token_store, metadata, config)
    }

    #[actix_web::test]
    async fn test_resolve_existing_file() {
        let (token_store, metadata, config) = setup_test_env_with_files();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo and add files
        let repo = metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();
        let commit_id = "abc123";
        let revision = Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "Initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        };
        metadata.add_revision(revision).await.unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();

        // Add file entry
        let entries = vec![
            FileEntry {
                path: "model.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 1024,
                cas_hash: "hash123".to_string(),
                is_lfs: true,
            },
        ];
        metadata.add_file_entries(entries).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/{ns}/{repo}/resolve/{revision}/{path}", web::get().to(resolve_model))
        ).await;

        let req = actix_test::TestRequest::get()
            .uri("/testuser/my-model/resolve/main/model.bin")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: ResolveResponse = actix_test::read_body_json(resp).await;
        assert_eq!(body.oid, "hash123");
        assert_eq!(body.size, 1024);
        assert!(body.download_url.contains("hash123"));
    }

    #[actix_web::test]
    async fn test_resolve_missing_file() {
        let (token_store, metadata, config) = setup_test_env_with_files();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo with no files
        let repo = metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();
        let commit_id = "abc123";
        let revision = Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "Initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        };
        metadata.add_revision(revision).await.unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/{ns}/{repo}/resolve/{revision}/{path}", web::get().to(resolve_model))
        ).await;

        let req = actix_test::TestRequest::get()
            .uri("/testuser/my-model/resolve/main/nonexistent.bin")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }
}