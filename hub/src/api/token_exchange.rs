use actix_web::{web, HttpResponse};
use crate::auth::extract::{AuthUser, AuthRead, AuthWrite};
use crate::auth::token_store::TokenInfo;
use crate::auth::xet_signer::XetSigner;
use crate::metadata::{MetadataStore, RepoType};
use serde::{Deserialize, Serialize};

/// Token exchange response
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenExchangeResponse {
    pub access_token: String,
    pub exp: u64,
    pub cas_url: String,
}

/// Internal helper to handle token exchange
#[allow(clippy::too_many_arguments)]
async fn do_exchange(
    info: &TokenInfo,
    path_namespace: &str,
    path_repo: &str,
    path_revision: &str,
    required_scope: &str,
    repo_type: RepoType,
    xet_signer: &std::sync::Arc<XetSigner>,
    metadata: &std::sync::Arc<dyn MetadataStore>,
    cas_url: &str,
) -> HttpResponse {
    // Check repo exists
    let repo = match metadata.get_repo(path_namespace, path_repo, repo_type).await {
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

    // Determine revision
    let revision = if path_revision == "main" || path_revision.is_empty() {
        match metadata.get_head(repo.id).await {
            Ok(Some(h)) => h,
            Ok(None) => "main".to_string(),
            Err(_) => "main".to_string(),
        }
    } else {
        path_revision.to_string()
    };

    // I2: Sign the xet token with the requested scope, not the user's full scope
    // This prevents a read token from getting write permissions via exchange
    let repo_id = format!("{}/{}", path_namespace, path_repo);
    let (xet_token, exp) = match xet_signer.sign(
        &info.user_id,
        required_scope,  // Use requested scope, not info.scope
        &repo_id,
        &repo_type.to_string(),
        &revision,
    ) {
        Ok(result) => result,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to sign token: {}", e),
                "error_type": "InternalError"
            }));
        }
    };

    HttpResponse::Ok().json(TokenExchangeResponse {
        access_token: xet_token,
        exp,
        cas_url: cas_url.to_string(),
    })
}

// Model endpoints
pub async fn exchange_model_read(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path.into_inner();
    do_exchange(
        &auth.info,
        &namespace,
        &repo,
        &revision,
        "read",
        RepoType::Model,
        &xet_signer,
        &metadata,
        &config.cas.base_url,
    ).await
}

pub async fn exchange_model_write(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path.into_inner();
    do_exchange(
        &auth.info,
        &namespace,
        &repo,
        &revision,
        "write",
        RepoType::Model,
        &xet_signer,
        &metadata,
        &config.cas.base_url,
    ).await
}

// Dataset endpoints
pub async fn exchange_dataset_read(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path.into_inner();
    do_exchange(
        &auth.info,
        &namespace,
        &repo,
        &revision,
        "read",
        RepoType::Dataset,
        &xet_signer,
        &metadata,
        &config.cas.base_url,
    ).await
}

pub async fn exchange_dataset_write(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path.into_inner();
    do_exchange(
        &auth.info,
        &namespace,
        &repo,
        &revision,
        "write",
        RepoType::Dataset,
        &xet_signer,
        &metadata,
        &config.cas.base_url,
    ).await
}

// Space endpoints
pub async fn exchange_space_read(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path.into_inner();
    do_exchange(
        &auth.info,
        &namespace,
        &repo,
        &revision,
        "read",
        RepoType::Space,
        &xet_signer,
        &metadata,
        &config.cas.base_url,
    ).await
}

pub async fn exchange_space_write(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path.into_inner();
    do_exchange(
        &auth.info,
        &namespace,
        &repo,
        &revision,
        "write",
        RepoType::Space,
        &xet_signer,
        &metadata,
        &config.cas.base_url,
    ).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};
    use crate::auth::token_store::TokenStore;
    use crate::metadata::SqliteMetadataStore;
    use crate::config::HubConfig;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    async fn setup_test_env() -> (
        std::sync::Arc<TokenStore>,
        std::sync::Arc<XetSigner>,
        std::sync::Arc<dyn MetadataStore>,
        HubConfig,
    ) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let xet_signer = std::sync::Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300));
        let metadata: std::sync::Arc<dyn MetadataStore> = std::sync::Arc::new(
            SqliteMetadataStore::in_memory().await.unwrap()
        );
        let config = HubConfig::default();
        (token_store, xet_signer, metadata, config)
    }

    #[actix_web::test]
    async fn test_exchange_model_read_success() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        // Create a token and repo
        let token = token_store.create_token("testuser", "test-token", "read").await.unwrap();
        metadata.create_repo("ns", "repo", RepoType::Model, false).await.unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/api/models/{namespace}/{repo}/read/{revision}", web::post().to(exchange_model_read))
        ).await;

        let req = test::TestRequest::post()
            .uri("/api/models/ns/repo/read/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: TokenExchangeResponse = test::read_body_json(resp).await;
        assert!(body.access_token.starts_with("xet_"));
        assert!(body.exp > 0);
    }

    #[actix_web::test]
    async fn test_exchange_model_write_with_read_token_fails() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        // Create a read-only token
        let token = token_store.create_token("testuser", "test-token", "read").await.unwrap();
        metadata.create_repo("ns", "repo", RepoType::Model, false).await.unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/api/models/{namespace}/{repo}/write/{revision}", web::post().to(exchange_model_write))
        ).await;

        let req = test::TestRequest::post()
            .uri("/api/models/ns/repo/write/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
    }

    #[actix_web::test]
    async fn test_exchange_model_write_with_write_token_succeeds() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        // Create a write token
        let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();
        metadata.create_repo("ns", "repo", RepoType::Model, false).await.unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/api/models/{namespace}/{repo}/write/{revision}", web::post().to(exchange_model_write))
        ).await;

        let req = test::TestRequest::post()
            .uri("/api/models/ns/repo/write/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_exchange_repo_not_found() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        let token = token_store.create_token("testuser", "test-token", "read").await.unwrap();
        // Don't create the repo

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/api/models/{namespace}/{repo}/read/{revision}", web::post().to(exchange_model_read))
        ).await;

        let req = test::TestRequest::post()
            .uri("/api/models/nonexistent/repo/read/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_exchange_invalid_token() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route("/api/models/{namespace}/{repo}/read/{revision}", web::post().to(exchange_model_read))
        ).await;

        let req = test::TestRequest::post()
            .uri("/api/models/ns/repo/read/main")
            .insert_header(("Authorization", "Bearer hf_invalid"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }
}
