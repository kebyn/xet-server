use crate::auth::extract::{AuthRead, AuthUser, AuthWrite};
use crate::metadata::{MetadataStore, Repo, RepoType};
use crate::services::repo::{RepoService, RepoServiceError, RepoServiceResult};
use actix_web::{HttpResponse, web};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

fn repo_service(metadata: &web::Data<Arc<dyn MetadataStore>>) -> RepoService {
    RepoService::new(metadata.get_ref().clone())
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn repo_service_error_response(err: RepoServiceError, not_found_type: &str) -> HttpResponse {
    match err {
        RepoServiceError::Validation(msg) => {
            HttpResponse::BadRequest().json(error_json(msg, "ValidationError"))
        }
        RepoServiceError::NotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, not_found_type))
        }
        RepoServiceError::Conflict(msg) => {
            HttpResponse::Conflict().json(error_json(msg, "ConflictError"))
        }
        RepoServiceError::Forbidden(msg) => {
            HttpResponse::Forbidden().json(error_json(msg, "AuthorizationError"))
        }
        RepoServiceError::RevisionNotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, "RevisionNotFoundError"))
        }
        RepoServiceError::Internal(msg) => {
            HttpResponse::InternalServerError().json(error_json(msg, "InternalError"))
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
    let service = repo_service(&metadata);
    let repo = match service
        .create_typed_repo(&auth.info.username, &body.name, repo_type, body.private)
        .await
    {
        Ok(repo) => repo,
        Err(err) => return repo_service_error_response(err, "NotFoundError"),
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
    let service = repo_service(&metadata);
    let repo = match service
        .create_unified_repo(
            &auth.info.username,
            body.organization.as_deref(),
            &body.name,
            body.repo_type.as_deref(),
            body.private,
        )
        .await
    {
        Ok(repo) => repo,
        Err(err) => return repo_service_error_response(err, "NotFoundError"),
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
    let service = repo_service(&metadata);
    let repo = match service
        .get_repo(&auth.info.username, &namespace, &repo_name, repo_type)
        .await
    {
        Ok(repo) => repo,
        Err(err) => return repo_service_error_response(err, "NotFoundError"),
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
    let service = repo_service(&metadata);
    match service
        .delete_repo(&auth.info.username, &namespace, &repo_name, repo_type)
        .await
    {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({
            "message": "Repository deleted successfully"
        })),
        Err(err) => repo_service_error_response(err, "NotFoundError"),
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
    let service = repo_service(&metadata);
    match service
        .get_revision(
            &auth.info.username,
            &namespace,
            &repo_name,
            &revision,
            repo_type,
        )
        .await
    {
        Ok(RepoServiceResult::Revision { repo, revision }) => {
            HttpResponse::Ok().json(serde_json::json!({
                "id": format!("{}/{}", repo.namespace, repo.name),
                "sha": revision.commit_id,
                "title": revision.message,
                "author": revision.author,
                "createdAt": chrono_datetime(revision.created_at),
                "siblings": [],
                "tags": [],
                "private": repo.private,
                "downloads": 0,
                "likes": 0,
                "shaRemote": null
            }))
        }
        Ok(RepoServiceResult::EmptyMainRevision { repo, head_sha }) => {
            let is_empty = head_sha.is_none();
            HttpResponse::Ok().json(serde_json::json!({
                "id": format!("{}/{}", repo.namespace, repo.name),
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
        }
        Err(err) => repo_service_error_response(err, "RepositoryNotFoundError"),
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
