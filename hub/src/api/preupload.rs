use crate::auth::extract::{AuthUser, AuthWrite};
use crate::metadata::{MetadataStore, RepoType};
use crate::services::preupload::{
    PreuploadFileInput, PreuploadRequest as ServicePreuploadRequest, PreuploadService,
    PreuploadServiceError, UploadMode,
};
use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

fn preupload_service(metadata: &web::Data<Arc<dyn MetadataStore>>) -> PreuploadService {
    PreuploadService::new(metadata.get_ref().clone())
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn preupload_service_error_response(err: PreuploadServiceError) -> HttpResponse {
    match err {
        PreuploadServiceError::Forbidden(msg) => {
            HttpResponse::Forbidden().json(error_json(msg, "ForbiddenError"))
        }
        PreuploadServiceError::NotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, "NotFoundError"))
        }
        PreuploadServiceError::Internal(msg) => {
            HttpResponse::InternalServerError().json(error_json(msg, "InternalError"))
        }
    }
}

fn upload_mode_to_api(mode: UploadMode) -> &'static str {
    match mode {
        UploadMode::Regular => "regular",
        UploadMode::Lfs => "lfs",
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
    let service = preupload_service(&metadata);
    let response = match service
        .prepare_upload(ServicePreuploadRequest {
            username: &auth.info.username,
            namespace: &namespace,
            repo_name: &repo_name,
            repo_type,
            inline_threshold: config.storage.inline_threshold_bytes,
            files: body
                .files
                .iter()
                .map(|file| PreuploadFileInput {
                    path: file.path.clone(),
                    size: file.size,
                })
                .collect(),
        })
        .await
    {
        Ok(response) => response,
        Err(err) => return preupload_service_error_response(err),
    };

    let file_responses: Vec<PreuploadFileResponse> = response
        .files
        .into_iter()
        .map(|file| PreuploadFileResponse {
            path: file.path,
            upload_mode: upload_mode_to_api(file.upload_mode).to_string(),
            should_ignore: file.should_ignore,
        })
        .collect();

    HttpResponse::Ok().json(PreuploadResponse {
        files: file_responses,
    })
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
    async fn test_preupload_mode_regular() {
        let (token_store, metadata) = setup_test_env().await;
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
                .app_data(web::Data::new(crate::config::HubConfig::default()))
                .route(
                    "/api/models/{ns}/{repo}/preupload/{revision}",
                    web::post().to(preupload_model),
                ),
        )
        .await;

        // Small file (< 1MB)
        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/preupload/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&PreuploadRequest {
                files: vec![PreuploadFile {
                    path: "config.json".to_string(),
                    size: 1024,
                }],
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
                .app_data(web::Data::new(crate::config::HubConfig::default()))
                .route(
                    "/api/models/{ns}/{repo}/preupload/{revision}",
                    web::post().to(preupload_model),
                ),
        )
        .await;

        // Medium file (1MB < size <= 10MB)
        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/preupload/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&PreuploadRequest {
                files: vec![PreuploadFile {
                    path: "model.bin".to_string(),
                    size: 5 * 1024 * 1024, // 5MB
                }],
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
                .app_data(web::Data::new(crate::config::HubConfig::default()))
                .route(
                    "/api/models/{ns}/{repo}/preupload/{revision}",
                    web::post().to(preupload_model),
                ),
        )
        .await;

        // Large file (> 10MB)
        let req = actix_test::TestRequest::post()
            .uri("/api/models/testuser/my-model/preupload/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&PreuploadRequest {
                files: vec![PreuploadFile {
                    path: "model.bin".to_string(),
                    size: 100 * 1024 * 1024, // 100MB
                }],
            })
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: PreuploadResponse = actix_test::read_body_json(resp).await;
        assert_eq!(body.files.len(), 1);
        assert_eq!(body.files[0].upload_mode, "lfs");
    }
}
