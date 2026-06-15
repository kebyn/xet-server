use actix_web::{web, HttpResponse};
use crate::auth::extract::{AuthUser, AuthWrite};
use crate::metadata::{MetadataStore, RepoType};
use serde::{Deserialize, Serialize};

/// Preupload request
#[derive(Debug, Deserialize, Serialize)]
pub struct PreuploadRequest {
    pub files: Vec<PreuploadFile>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PreuploadFile {
    pub path: String,
    pub size: u64,
}

/// Preupload response
#[derive(Debug, Serialize, Deserialize)]
pub struct PreuploadResponse {
    pub files: Vec<PreuploadFileResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PreuploadFileResponse {
    pub path: String,
    #[serde(rename = "uploadMode")]
    pub upload_mode: String,
    #[serde(rename = "shouldIgnore")]
    pub should_ignore: bool,
}

/// Determine upload mode based on file size
/// Returns "regular" for small files (<=inline_threshold), "lfs" for larger files
fn classify_upload_mode(size: u64, inline_threshold: u64) -> String {
    if size <= inline_threshold {
        "regular".to_string()
    } else {
        "lfs".to_string()
    }
}

/// Internal helper for preupload handling
async fn handle_preupload(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    repo_type: RepoType,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo_name, _revision) = path.into_inner();

    // C-AUTH: preupload 是 commit 的写前置步骤,需校验对目标 namespace 的写权限
    // (与 handle_commit 一致)。在 repo 查询前返回 403,不泄露私有 repo 存在性。
    if namespace != auth.info.username {
        let has_access = metadata.is_namespace_member(&auth.info.username, &namespace).await.unwrap_or(false);
        if !has_access {
            return HttpResponse::Forbidden().json(serde_json::json!({
                "error": format!("User '{}' cannot access namespace '{}'", auth.info.username, namespace),
                "error_type": "ForbiddenError"
            }));
        }
    }

    // Check repo exists
    match metadata.get_repo(&namespace, &repo_name, repo_type).await {
        Ok(_) => {},
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

    // Classify each file's upload mode
    let file_responses: Vec<PreuploadFileResponse> = body.files.iter()
        .map(|f| PreuploadFileResponse {
            path: f.path.clone(),
            upload_mode: classify_upload_mode(f.size, config.storage.inline_threshold_bytes),
            should_ignore: false,
        })
        .collect();

    HttpResponse::Ok().json(PreuploadResponse { files: file_responses })
}

// Model preupload handler
pub async fn preupload_model(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    handle_preupload(auth, path, body, RepoType::Model, metadata, config).await
}

// Dataset preupload handler
pub async fn preupload_dataset(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    handle_preupload(auth, path, body, RepoType::Dataset, metadata, config).await
}

// Space preupload handler
pub async fn preupload_space(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    handle_preupload(auth, path, body, RepoType::Space, metadata, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, App};
    use crate::auth::token_store::TokenStore;
    use crate::metadata::SqliteMetadataStore;

    async fn setup_test_env() -> (std::sync::Arc<TokenStore>, std::sync::Arc<dyn MetadataStore>) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> = std::sync::Arc::new(
            SqliteMetadataStore::in_memory().await.unwrap()
        );
        (token_store, metadata)
    }

    #[actix_web::test]
    async fn test_preupload_mode_regular() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(crate::config::HubConfig::default()))
                .route("/api/models/{ns}/{repo}/preupload/{revision}", web::post().to(preupload_model))
        ).await;

        // Small file (< 1MB)
        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/preupload/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&PreuploadRequest {
                files: vec![PreuploadFile {
                    path: "config.json".to_string(),
                    size: 1024,
                }]
            })
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: PreuploadResponse = actix_test::read_body_json(resp).await;
        assert_eq!(body.files.len(), 1);
        assert_eq!(body.files[0].upload_mode, "regular");
    }

    #[actix_web::test]
    async fn test_preupload_mode_lfs() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(crate::config::HubConfig::default()))
                .route("/api/models/{ns}/{repo}/preupload/{revision}", web::post().to(preupload_model))
        ).await;

        // Medium file (1MB < size <= 10MB)
        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/preupload/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&PreuploadRequest {
                files: vec![PreuploadFile {
                    path: "model.bin".to_string(),
                    size: 5 * 1024 * 1024, // 5MB
                }]
            })
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: PreuploadResponse = actix_test::read_body_json(resp).await;
        assert_eq!(body.files.len(), 1);
        assert_eq!(body.files[0].upload_mode, "lfs");
    }

    #[actix_web::test]
    async fn test_preupload_mode_xet() {
        let (token_store, metadata) = setup_test_env().await;
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(crate::config::HubConfig::default()))
                .route("/api/models/{ns}/{repo}/preupload/{revision}", web::post().to(preupload_model))
        ).await;

        // Large file (> 10MB)
        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/preupload/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&PreuploadRequest {
                files: vec![PreuploadFile {
                    path: "model.bin".to_string(),
                    size: 100 * 1024 * 1024, // 100MB
                }]
            })
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: PreuploadResponse = actix_test::read_body_json(resp).await;
        assert_eq!(body.files.len(), 1);
        assert_eq!(body.files[0].upload_mode, "lfs");
    }

    #[test]
    fn test_classify_upload_mode() {
        let inline_threshold = 1024 * 1024; // 1MB

        // Regular: <= 1MB
        assert_eq!(classify_upload_mode(0, inline_threshold), "regular");
        assert_eq!(classify_upload_mode(1024, inline_threshold), "regular");
        assert_eq!(classify_upload_mode(1024 * 1024, inline_threshold), "regular");

        // LFS: > 1MB
        assert_eq!(classify_upload_mode(1024 * 1024 + 1, inline_threshold), "lfs");
        assert_eq!(classify_upload_mode(5 * 1024 * 1024, inline_threshold), "lfs");
        assert_eq!(classify_upload_mode(10 * 1024 * 1024, inline_threshold), "lfs");
        assert_eq!(classify_upload_mode(10 * 1024 * 1024 + 1, inline_threshold), "lfs");
        assert_eq!(classify_upload_mode(100 * 1024 * 1024, inline_threshold), "lfs");
        assert_eq!(classify_upload_mode(1024 * 1024 * 1024, inline_threshold), "lfs");
    }
}
