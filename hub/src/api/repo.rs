use actix_web::{web, HttpResponse};
use crate::auth::extract::{AuthUser, AuthAny, AuthWrite};
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
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoRequest>,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    // Namespace is derived from the user's username
    let namespace = auth.info.username.clone();
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
    auth: AuthUser<AuthWrite>,
    body: web::Json<CreateRepoUnifiedRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let namespace = body.organization.clone().unwrap_or_else(|| auth.info.username.clone());

    // Security: Only allow creating repos in own namespace (no org membership yet)
    if namespace != auth.info.username {
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
    _auth: AuthUser<AuthAny>,
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
                }))
            };
        }
    };

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
                }))
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
    auth: AuthUser<AuthAny>,
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
    auth: AuthUser<AuthAny>,
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
    auth: AuthUser<AuthAny>,
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
    auth: AuthUser<AuthAny>,
    path: web::Path<(String, String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(auth, path, RepoType::Model, metadata).await
}

pub async fn get_revision_dataset(
    auth: AuthUser<AuthAny>,
    path: web::Path<(String, String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(auth, path, RepoType::Dataset, metadata).await
}

pub async fn get_revision_space(
    auth: AuthUser<AuthAny>,
    path: web::Path<(String, String, String)>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    get_revision_handler(auth, path, RepoType::Space, metadata).await
}

async fn get_revision_handler(
    _auth: AuthUser<AuthAny>,
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
    use crate::auth::token_store::TokenStore;
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