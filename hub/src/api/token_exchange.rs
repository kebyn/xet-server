use crate::auth::extract::{AuthRead, AuthUser, AuthWrite};
use crate::auth::token_store::TokenInfo;
use crate::auth::xet_signer::XetSigner;
use crate::metadata::{MetadataStore, RepoType};
use crate::services::token_exchange::{
    ExchangeScope, TokenExchangeRequest, TokenExchangeService, TokenExchangeServiceError,
};
use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Token exchange response
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenExchangeResponse {
    pub access_token: String,
    pub exp: u64,
    pub cas_url: String,
}

fn token_exchange_service(
    metadata: &web::Data<Arc<dyn MetadataStore>>,
    xet_signer: &web::Data<Arc<XetSigner>>,
) -> TokenExchangeService {
    TokenExchangeService::new(metadata.get_ref().clone(), xet_signer.get_ref().clone())
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn token_exchange_error_response(err: TokenExchangeServiceError) -> HttpResponse {
    match err {
        TokenExchangeServiceError::NotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, "NotFoundError"))
        }
        TokenExchangeServiceError::Internal(msg) => {
            HttpResponse::InternalServerError().json(error_json(msg, "InternalError"))
        }
    }
}

async fn do_exchange(
    info: &TokenInfo,
    path: (String, String, String),
    required_scope: ExchangeScope,
    repo_type: RepoType,
    xet_signer: web::Data<Arc<XetSigner>>,
    metadata: web::Data<Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    let (namespace, repo, revision) = path;
    let service = token_exchange_service(&metadata, &xet_signer);
    let result = match service
        .exchange(TokenExchangeRequest {
            user_id: &info.user_id,
            username: &info.username,
            namespace: &namespace,
            repo_name: &repo,
            revision: &revision,
            required_scope,
            repo_type,
        })
        .await
    {
        Ok(result) => result,
        Err(err) => return token_exchange_error_response(err),
    };

    HttpResponse::Ok().json(TokenExchangeResponse {
        access_token: result.access_token,
        exp: result.exp,
        cas_url: config.cas.base_url.clone(),
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
    do_exchange(
        &auth.info,
        path.into_inner(),
        ExchangeScope::Read,
        RepoType::Model,
        xet_signer,
        metadata,
        config,
    )
    .await
}

pub async fn exchange_model_write(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    do_exchange(
        &auth.info,
        path.into_inner(),
        ExchangeScope::Write,
        RepoType::Model,
        xet_signer,
        metadata,
        config,
    )
    .await
}

// Dataset endpoints
pub async fn exchange_dataset_read(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    do_exchange(
        &auth.info,
        path.into_inner(),
        ExchangeScope::Read,
        RepoType::Dataset,
        xet_signer,
        metadata,
        config,
    )
    .await
}

pub async fn exchange_dataset_write(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    do_exchange(
        &auth.info,
        path.into_inner(),
        ExchangeScope::Write,
        RepoType::Dataset,
        xet_signer,
        metadata,
        config,
    )
    .await
}

// Space endpoints
pub async fn exchange_space_read(
    auth: AuthUser<AuthRead>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    do_exchange(
        &auth.info,
        path.into_inner(),
        ExchangeScope::Read,
        RepoType::Space,
        xet_signer,
        metadata,
        config,
    )
    .await
}

pub async fn exchange_space_write(
    auth: AuthUser<AuthWrite>,
    path: web::Path<(String, String, String)>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<crate::config::HubConfig>,
) -> HttpResponse {
    do_exchange(
        &auth.info,
        path.into_inner(),
        ExchangeScope::Write,
        RepoType::Space,
        xet_signer,
        metadata,
        config,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token_store::TokenStore;
    use crate::config::HubConfig;
    use crate::metadata::SqliteMetadataStore;
    use actix_web::{App, test};
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
        let metadata: std::sync::Arc<dyn MetadataStore> =
            std::sync::Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        let config = HubConfig::default();
        (token_store, xet_signer, metadata, config)
    }

    #[actix_web::test]
    async fn test_exchange_model_read_success() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        // Create a token and repo
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        metadata
            .create_repo("ns", "repo", RepoType::Model, false)
            .await
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/api/models/{namespace}/{repo}/read/{revision}",
                    web::post().to(exchange_model_read),
                ),
        )
        .await;

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
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        metadata
            .create_repo("ns", "repo", RepoType::Model, false)
            .await
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/api/models/{namespace}/{repo}/write/{revision}",
                    web::post().to(exchange_model_write),
                ),
        )
        .await;

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
        let token = token_store
            .create_token("testuser", "test-token", "write")
            .await
            .unwrap();
        metadata
            .create_repo("testuser", "repo", RepoType::Model, false)
            .await
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/api/models/{namespace}/{repo}/write/{revision}",
                    web::post().to(exchange_model_write),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/models/testuser/repo/write/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_exchange_public_repo_write_denies_non_owner() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        let token = token_store
            .create_token("attacker", "write-token", "write")
            .await
            .unwrap();
        metadata
            .create_repo("owner", "public-repo", RepoType::Model, false)
            .await
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/api/models/{namespace}/{repo}/write/{revision}",
                    web::post().to(exchange_model_write),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/models/owner/public-repo/write/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_exchange_private_repo_denies_non_owner() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;
        let token = token_store
            .create_token("attacker", "t", "read")
            .await
            .unwrap();
        metadata
            .create_repo("owner", "repo", RepoType::Model, true)
            .await
            .unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/api/models/{namespace}/{repo}/read/{revision}",
                    web::post().to(exchange_model_read),
                ),
        )
        .await;
        let req = test::TestRequest::post()
            .uri("/api/models/owner/repo/read/main")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_exchange_repo_not_found() {
        let (token_store, xet_signer, metadata, config) = setup_test_env().await;

        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        // Don't create the repo

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(xet_signer.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/api/models/{namespace}/{repo}/read/{revision}",
                    web::post().to(exchange_model_read),
                ),
        )
        .await;

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
                .route(
                    "/api/models/{namespace}/{repo}/read/{revision}",
                    web::post().to(exchange_model_read),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/models/ns/repo/read/main")
            .insert_header(("Authorization", "Bearer hf_invalid"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }
}
