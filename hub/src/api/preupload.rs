use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
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
    pub upload_mode: String,
}

// Threshold constants
const INLINE_THRESHOLD: u64 = 1 * 1024 * 1024; // 1MB
const LFS_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB

/// Extract Bearer token from Authorization header
fn extract_bearer(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Determine upload mode based on file size
fn classify_upload_mode(size: u64) -> String {
    if size <= INLINE_THRESHOLD {
        "regular".to_string()
    } else if size <= LFS_THRESHOLD {
        "lfs".to_string()
    } else {
        "xet".to_string()
    }
}

/// Internal helper for preupload handling
async fn handle_preupload(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
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

    let (namespace, repo_name, _revision) = path.into_inner();

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
            upload_mode: classify_upload_mode(f.size),
        })
        .collect();

    HttpResponse::Ok().json(PreuploadResponse { files: file_responses })
}

// Model preupload handler
pub async fn preupload_model(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_preupload(req, path, body, RepoType::Model, token_store, metadata).await
}

// Dataset preupload handler
pub async fn preupload_dataset(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_preupload(req, path, body, RepoType::Dataset, token_store, metadata).await
}

// Space preupload handler
pub async fn preupload_space(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    body: web::Json<PreuploadRequest>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_preupload(req, path, body, RepoType::Space, token_store, metadata).await
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
    async fn test_preupload_mode_regular() {
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
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
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
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
        let (token_store, metadata) = setup_test_env();
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        // Create repo
        metadata.create_repo("testuser", "my-model", RepoType::Model, false).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
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
        assert_eq!(body.files[0].upload_mode, "xet");
    }

    #[test]
    fn test_classify_upload_mode() {
        // Regular: <= 1MB
        assert_eq!(classify_upload_mode(0), "regular");
        assert_eq!(classify_upload_mode(1024), "regular");
        assert_eq!(classify_upload_mode(1 * 1024 * 1024), "regular");

        // LFS: 1MB < size <= 10MB
        assert_eq!(classify_upload_mode(1 * 1024 * 1024 + 1), "lfs");
        assert_eq!(classify_upload_mode(5 * 1024 * 1024), "lfs");
        assert_eq!(classify_upload_mode(10 * 1024 * 1024), "lfs");

        // Xet: > 10MB
        assert_eq!(classify_upload_mode(10 * 1024 * 1024 + 1), "xet");
        assert_eq!(classify_upload_mode(100 * 1024 * 1024), "xet");
        assert_eq!(classify_upload_mode(1024 * 1024 * 1024), "xet");
    }
}