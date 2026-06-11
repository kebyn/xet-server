use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
use crate::metadata::{MetadataStore, Repo, RepoType};
use serde::{Deserialize, Serialize};
use chrono::DateTime;

/// Request body for creating a repo
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateRepoRequest {
    pub name: String,
    #[serde(default)]
    pub private: bool,
}

/// Extract Bearer token from Authorization header
fn extract_bearer(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.to_string())
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

/// Convert Unix timestamp to ISO 8601 datetime string
fn chrono_datetime(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Internal helper to create a repo
async fn create_repo(
    req: HttpRequest,
    body: web::Json<CreateRepoRequest>,
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

    // Namespace is derived from the user's username
    let namespace = info.username.clone();
    let name = body.name.clone();
    let private = body.private;

    // Create the repo
    let repo = match metadata.create_repo(&namespace, &name, repo_type, private).await {
        Ok(r) => r,
        Err(e) => {
            return match e {
                crate::metadata::MetadataError::RepoAlreadyExists(_) => {
                    HttpResponse::Conflict().json(serde_json::json!({
                        "error": e.to_string(),
                        "error_type": "ConflictError"
                    }))
                }
                _ => HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": e.to_string(),
                    "error_type": "InternalError"
                }))
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
    req: HttpRequest,
    body: web::Json<CreateRepoUnifiedRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
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

    let namespace = body.organization.clone().unwrap_or_else(|| info.username.clone());

    // Security: Only allow creating repos in own namespace (no org membership yet)
    if namespace != info.username {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": format!("Cannot create repo in namespace '{}': not a member", namespace),
            "error_type": "AuthorizationError"
        }));
    }

    let name = body.name.clone();
    let private = body.private;

    let repo_type = match body.repo_type.as_deref() {
        Some("dataset") => RepoType::Dataset,
        Some("space") => RepoType::Space,
        _ => RepoType::Model,
    };

    // If repo already exists, return success (hf upload expects idempotent creation)
    let repo = match metadata.create_repo(&namespace, &name, repo_type, private).await {
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
    req: HttpRequest,
    path: web::Path<(String, String)>,
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
                }))
            };
        }
    };

    HttpResponse::Ok().json(repo_to_json(&repo))
}

/// Internal helper to delete a repo
async fn delete_repo_info(
    req: HttpRequest,
    path: web::Path<(String, String)>,
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
                }))
            };
        }
    };

    // Check ownership: namespace should match username
    if repo.namespace != info.username {
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
    req: HttpRequest,
    body: web::Json<CreateRepoRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    create_repo(req, body, RepoType::Model, token_store, metadata).await
}

pub async fn get_repo_model(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_repo_info(req, path, RepoType::Model, token_store, metadata).await
}

pub async fn delete_repo_model(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    delete_repo_info(req, path, RepoType::Model, token_store, metadata).await
}

// Dataset handlers
pub async fn create_dataset(
    req: HttpRequest,
    body: web::Json<CreateRepoRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    create_repo(req, body, RepoType::Dataset, token_store, metadata).await
}

pub async fn get_repo_dataset(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_repo_info(req, path, RepoType::Dataset, token_store, metadata).await
}

pub async fn delete_repo_dataset(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    delete_repo_info(req, path, RepoType::Dataset, token_store, metadata).await
}

// Space handlers
pub async fn create_space(
    req: HttpRequest,
    body: web::Json<CreateRepoRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    create_repo(req, body, RepoType::Space, token_store, metadata).await
}

pub async fn get_repo_space(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_repo_info(req, path, RepoType::Space, token_store, metadata).await
}

pub async fn delete_repo_space(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    delete_repo_info(req, path, RepoType::Space, token_store, metadata).await
}

/// GET /api/{models,datasets,spaces}/{ns}/{repo}/revision/{rev}
/// Returns revision info. For new repos with no commits, returns empty revision.
pub async fn get_revision_model(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(req, path, RepoType::Model, token_store, metadata).await
}

pub async fn get_revision_dataset(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(req, path, RepoType::Dataset, token_store, metadata).await
}

pub async fn get_revision_space(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(req, path, RepoType::Space, token_store, metadata).await
}

async fn get_revision_handler(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    repo_type: RepoType,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
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
        Err(_) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Token validation failed",
                "error_type": "InternalError"
            }));
        }
    };

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

    // Try to get the revision
    match metadata.get_revision(repo.id, &revision).await {
        Ok(rev) => {
            HttpResponse::Ok().json(serde_json::json!({
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
            }))
        }
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
    async fn test_create_repo() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "write").unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models", web::post().to(create_model))
        ).await;

        let req = actix_test::TestRequest::post()
            .uri("/api/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CreateRepoRequest { name: "my-model".to_string(), private: true })
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
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "write").unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models", web::post().to(create_model))
        ).await;

        // Create first repo
        let req = actix_test::TestRequest::post()
            .uri("/api/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CreateRepoRequest { name: "my-model".to_string(), private: false })
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Try to create duplicate
        let req = actix_test::TestRequest::post()
            .uri("/api/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CreateRepoRequest { name: "my-model".to_string(), private: false })
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
    }

    #[actix_web::test]
    async fn test_get_repo() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo directly
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::get().to(get_repo_model))
        ).await;

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
    async fn test_get_nonexistent_repo() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::get().to(get_repo_model))
        ).await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/nonexistent")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_delete_repo() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "write").unwrap();

        // Create repo directly
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::delete().to(delete_repo_model))
        ).await;

        let req = actix_test::TestRequest::delete()
            .uri("/api/models/testuser/my-model")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Verify repo is deleted
        let result = metadata.get_repo("testuser", "my-model", RepoType::Model).await;
        assert!(result.is_err());
    }

    #[actix_web::test]
    async fn test_delete_repo_not_owner() {
        let (token_store, metadata) = setup_test_env();
        let _token1 = token_store.create_token("user1", "token1", "write").unwrap();
        let token2 = token_store.create_token("user2", "token2", "write").unwrap();

        // Create repo with user1
        metadata.create_repo("user1", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route("/api/models/{ns}/{repo}", web::delete().to(delete_repo_model))
        ).await;

        // Try to delete with user2's token
        let req = actix_test::TestRequest::delete()
            .uri("/api/models/user1/my-model")
            .insert_header(("Authorization", format!("Bearer {}", token2)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
    }
}