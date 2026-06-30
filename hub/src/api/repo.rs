use super::shared::can_access_repo;
use crate::auth::extract::{AuthRead, AuthUser, AuthWrite};
use crate::metadata::{MetadataStore, Repo, RepoType};
use actix_web::{HttpResponse, web};
use chrono::DateTime;
use serde::{Deserialize, Serialize};

/// Request body for creating a repo
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateRepoRequest {
    pub name: String,
    #[serde(default)]
    pub private: bool,
}

// I5 fix: Validate repository name to prevent injection, path traversal, and abuse
fn validate_repo_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Repository name cannot be empty".to_string());
    }
    if name.len() > 96 {
        return Err(format!(
            "Repository name too long ({} chars, max 96)",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(format!(
            "Repository name '{}' contains invalid characters. Only alphanumeric, '.', '_', '-' are allowed",
            name
        ));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(format!(
            "Repository name '{}' cannot start with '.' or '-'",
            name
        ));
    }
    if name.ends_with('.') {
        return Err(format!("Repository name '{}' cannot end with '.'", name));
    }
    if name.contains("..") {
        return Err(format!("Repository name '{}' cannot contain '..'", name));
    }
    Ok(())
}

/// Convert Repo to HF-compatible JSON response
fn repo_to_json(repo: &Repo) -> serde_json::Value {
    serde_json::json!({
        "id": format!("{}/{}", repo.namespace, repo.name),
        "name": repo.name,
        "private": repo.private,
        "createdAt": chrono_datetime(repo.created_at),
        "updatedAt": chrono_datetime(repo.updated_at),
        "tags": [],
        "downloads": 0,
        "likes": 0,
        "url": format!("/{}/{}", repo.namespace, repo.name)
    })
}

/// Convert Unix timestamp to ISO 8601 datetime string.
/// M6 fix: Log warning on invalid timestamp instead of silently returning epoch.
fn chrono_datetime(timestamp: i64) -> String {
    match DateTime::from_timestamp(timestamp, 0) {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => {
            tracing::warn!(
                "Invalid Unix timestamp: {} (out of range for DateTime)",
                timestamp
            );
            "1970-01-01T00:00:00Z".to_string()
        }
    }
}

/// Internal helper to create a repo
async fn create_repo(
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoRequest>,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    // Namespace is derived from the user's username
    let namespace = auth.info.username.clone();
    let name = body.name.clone();
    let private = body.private;

    // I5 fix: Validate repo name
    if let Err(msg) = validate_repo_name(&name) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": msg,
            "error_type": "ValidationError"
        }));
    }

    // Create the repo
    let repo = match metadata
        .create_repo(&namespace, &name, repo_type, private)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return match e {
                crate::metadata::MetadataError::RepoAlreadyExists(_) => HttpResponse::Conflict()
                    .json(serde_json::json!({
                        "error": e.to_string(),
                        "error_type": "ConflictError"
                    })),
                _ => HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": e.to_string(),
                    "error_type": "InternalError"
                })),
            };
        }
    };

    HttpResponse::Ok().json(repo_to_json(&repo))
}

/// Request body for the unified /api/repos/create endpoint (used by hf CLI)
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateRepoUnifiedRequest {
    pub name: String,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default, rename = "type")]
    pub repo_type: Option<String>, // "model", "dataset", "space"
    #[serde(default)]
    pub private: bool,
}

/// POST /api/repos/create — unified repo creation endpoint used by hf CLI
pub async fn create_repo_unified(
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoUnifiedRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let namespace = body
        .organization
        .clone()
        .unwrap_or_else(|| auth.info.username.clone());

    // Security: Only allow creating repos in own namespace (no org membership yet)
    if namespace != auth.info.username {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": format!("Cannot create repo in namespace '{}': not a member", namespace),
            "error_type": "AuthorizationError"
        }));
    }

    let name = body.name.clone();
    let private = body.private;

    // I5 fix: Validate repo name
    if let Err(msg) = validate_repo_name(&name) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": msg,
            "error_type": "ValidationError"
        }));
    }

    let repo_type = match body.repo_type.as_deref() {
        Some("dataset") => RepoType::Dataset,
        Some("space") => RepoType::Space,
        _ => RepoType::Model,
    };

    // If repo already exists, return success (hf upload expects idempotent creation)
    let repo = match metadata
        .create_repo(&namespace, &name, repo_type, private)
        .await
    {
        Ok(r) => r,
        Err(crate::metadata::MetadataError::RepoAlreadyExists(_)) => {
            // Return existing repo info
            match metadata.get_repo(&namespace, &name, repo_type).await {
                Ok(r) => r,
                Err(_) => {
                    return HttpResponse::Conflict().json(serde_json::json!({
                        "error": "Repo already exists",
                        "error_type": "ConflictError"
                    }));
                }
            }
        }
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": e.to_string(),
                "error_type": "InternalError"
            }));
        }
    };

    HttpResponse::Ok().json(repo_to_json(&repo))
}

/// Internal helper to get repo info
async fn get_repo_info(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String)>,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (namespace, repo_name) = path.into_inner();

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
                })),
            };
        }
    };

    // C-AUTH: 私有 repo 仅 owner 可读元数据。404 不泄露存在性。
    if !can_access_repo(&repo, &auth.info.username) {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": "Repository not found",
            "error_type": "NotFoundError"
        }));
    }

    HttpResponse::Ok().json(repo_to_json(&repo))
}

/// Internal helper to delete a repo
async fn delete_repo_info(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String)>,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (namespace, repo_name) = path.into_inner();

    // Verify the repo exists and user owns it (namespace matches username)
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
                })),
            };
        }
    };

    // Check ownership: namespace should match username
    if repo.namespace != auth.info.username {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "You do not have permission to delete this repository",
            "error_type": "AuthorizationError"
        }));
    }

    // Delete the repo
    match metadata.delete_repo(repo.id).await {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({
            "message": "Repository deleted successfully"
        })),
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({
            "error": e.to_string(),
            "error_type": "InternalError"
        })),
    }
}

// Model handlers
pub async fn create_model(
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    create_repo(auth, body, RepoType::Model, metadata).await
}

pub async fn get_repo_model(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_repo_info(auth, path, RepoType::Model, metadata).await
}

pub async fn delete_repo_model(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    delete_repo_info(auth, path, RepoType::Model, metadata).await
}

// Dataset handlers
pub async fn create_dataset(
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    create_repo(auth, body, RepoType::Dataset, metadata).await
}

pub async fn get_repo_dataset(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_repo_info(auth, path, RepoType::Dataset, metadata).await
}

pub async fn delete_repo_dataset(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    delete_repo_info(auth, path, RepoType::Dataset, metadata).await
}

// Space handlers
pub async fn create_space(
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    create_repo(auth, body, RepoType::Space, metadata).await
}

pub async fn get_repo_space(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_repo_info(auth, path, RepoType::Space, metadata).await
}

pub async fn delete_repo_space(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    delete_repo_info(auth, path, RepoType::Space, metadata).await
}

/// GET /api/{models,datasets,spaces}/{ns}/{repo}/revision/{rev}
/// Returns revision info. For new repos with no commits, returns empty revision.
pub async fn get_revision_model(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(auth, path, RepoType::Model, metadata).await
}

pub async fn get_revision_dataset(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(auth, path, RepoType::Dataset, metadata).await
}

pub async fn get_revision_space(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(auth, path, RepoType::Space, metadata).await
}

async fn get_revision_handler(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (namespace, repo_name, revision) = path.into_inner();

    // Check repo exists
    let repo = match metadata.get_repo(&namespace, &repo_name, repo_type).await {
        Ok(r) => r,
        Err(_) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Repository not found",
                "error_type": "RepositoryNotFoundError"
            }));
        }
    };

    // C-AUTH: 私有 repo 仅 owner 可读 commit 元数据。404 不泄露存在性。
    if !can_access_repo(&repo, &auth.info.username) {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": "Repository not found",
            "error_type": "RepositoryNotFoundError"
        }));
    }

    // Try to get the revision
    match metadata.get_revision(repo.id, &revision).await {
        Ok(rev) => HttpResponse::Ok().json(serde_json::json!({
            "id": format!("{}/{}", namespace, repo_name),
            "sha": rev.commit_id,
            "title": rev.message,
            "author": rev.author,
            "createdAt": chrono_datetime(rev.created_at),
            "siblings": [],
            "tags": [],
            "private": repo.private,
            "downloads": 0,
            "likes": 0,
            "shaRemote": null
        })),
        Err(_) => {
            if revision == "main" {
                // Get the actual HEAD commit hash; null if repo has no commits
                let head_sha = metadata.get_head(repo.id).await.ok().flatten();
                let is_empty = head_sha.is_none();
                HttpResponse::Ok().json(serde_json::json!({
                    "id": format!("{}/{}", namespace, repo_name),
                    "sha": head_sha,
                    "title": if is_empty { "Empty repository" } else { "Initial commit" },
                    "author": "system",
                    "createdAt": chrono_datetime(repo.created_at),
                    "siblings": [],
                    "tags": [],
                    "private": repo.private,
                    "downloads": 0,
                    "likes": 0,
                    "shaRemote": null,
                    "empty": is_empty
                }))
            } else {
                HttpResponse::NotFound().json(serde_json::json!({
                    "error": format!("Revision not found: {}", revision),
                    "error_type": "RevisionNotFoundError"
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token_store::TokenStore;
    use crate::metadata::SqliteMetadataStore;
    use actix_web::{App, test as actix_test};

    async fn setup_test_env() -> (
        std::sync::Arc<TokenStore>,
        std::sync::Arc<dyn MetadataStore>,
    ) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> =
            std::sync::Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        (token_store, metadata)
    }

    #[actix_web::test]
    async fn test_create_repo() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models", web::post().to(create_model)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CreateRepoRequest {
                name: "my-model".to_string(),
                private: true,
            })
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["id"], "testuser/my-model");
        assert_eq!(body["name"], "my-model");
        assert_eq!(body["private"], true);
    }

    #[actix_web::test]
    async fn test_create_duplicate_repo() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models", web::post().to(create_model)),
        )
        .await;

        // Create first repo
        let req = actix_test::TestRequest::post()
            .uri("/api/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CreateRepoRequest {
                name: "my-model".to_string(),
                private: false,
            })
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Try to create duplicate
        let req = actix_test::TestRequest::post()
            .uri("/api/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CreateRepoRequest {
                name: "my-model".to_string(),
                private: false,
            })
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
    }

    #[actix_web::test]
    async fn test_get_repo() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

        // Create repo directly
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::get().to(get_repo_model)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/my-model")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["id"], "testuser/my-model");
        assert_eq!(body["name"], "my-model");
    }

    #[actix_web::test]
    async fn test_get_repo_info_private_denies_non_owner() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("attacker", "t", "read")
            .await
            .unwrap();
        // 私有 repo,owner 是别人
        metadata
            .create_repo("owner", "secret", RepoType::Model, true)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::get().to(get_repo_model)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/owner/secret")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_get_revision_private_denies_non_owner() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("attacker", "t", "read")
            .await
            .unwrap();
        // 私有 repo;访问校验在 repo 加载后、revision 解析前触发,无需 HEAD。
        metadata
            .create_repo("owner", "secret", RepoType::Model, true)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}/revision/{revision}",
                    web::get().to(get_revision_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/owner/secret/revision/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_get_nonexistent_repo() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::get().to(get_repo_model)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/nonexistent")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_delete_repo() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();

        // Create repo directly
        metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}",
                    web::delete().to(delete_repo_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::delete()
            .uri("/api/models/testuser/my-model")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Verify repo is deleted
        let result = metadata
            .get_repo("testuser", "my-model", RepoType::Model)
            .await;
        assert!(result.is_err());
    }

    #[actix_web::test]
    async fn test_delete_repo_not_owner() {
        let (token_store, metadata) = setup_test_env().await;
        let _token1 = token_store
            .create_token("user1", "token1", "write")
            .await
            .unwrap();
        let token2 = token_store
            .create_token("user2", "token2", "write")
            .await
            .unwrap();

        // Create repo with user1
        metadata
            .create_repo("user1", "my-model", RepoType::Model, false)
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}",
                    web::delete().to(delete_repo_model),
                ),
        )
        .await;

        // Try to delete with user2's token
        let req = actix_test::TestRequest::delete()
            .uri("/api/models/user1/my-model")
            .insert_header(("Authorization", format!("Bearer {}", token2)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
    }
}
