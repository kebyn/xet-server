use actix_web::{dev::Payload, web, FromRequest, HttpRequest, HttpResponse};
use std::future::{ready, Future};
use std::pin::Pin;
use std::sync::Arc;

use crate::auth::token_store::{TokenInfo, TokenStore};

/// Marker trait for scope requirements.
/// Implemented by AuthAny, AuthRead, AuthWrite to enforce scope at compile time.
pub trait ScopeRequirement {
    fn check(info: &TokenInfo) -> bool;
    fn description() -> &'static str;
}

/// Accepts any valid authenticated token (no scope check).
pub struct AuthAny;
impl ScopeRequirement for AuthAny {
    fn check(_info: &TokenInfo) -> bool {
        true
    }
    fn description() -> &'static str {
        "authenticated"
    }
}

/// Requires "read" or "write" scope.
pub struct AuthRead;
impl ScopeRequirement for AuthRead {
    fn check(info: &TokenInfo) -> bool {
        info.scope == "read" || info.scope == "write"
    }
    fn description() -> &'static str {
        "read access"
    }
}

/// Requires "write" scope exactly.
pub struct AuthWrite;
impl ScopeRequirement for AuthWrite {
    fn check(info: &TokenInfo) -> bool {
        info.scope == "write"
    }
    fn description() -> &'static str {
        "write access"
    }
}

/// Authenticated user extracted from Bearer token.
/// Generic over scope requirement S.
pub struct AuthUser<S: ScopeRequirement = AuthAny> {
    pub info: TokenInfo,
    _scope: std::marker::PhantomData<S>,
}

impl<S: ScopeRequirement> AuthUser<S> {
    /// Convenience accessor for username
    pub fn username(&self) -> &str {
        &self.info.username
    }

    /// Convenience accessor for scope
    pub fn scope(&self) -> &str {
        &self.info.scope
    }
}

/// Extractor error types
#[derive(Debug)]
pub enum AuthError {
    MissingHeader,
    InvalidToken,
    InsufficientScope { required: &'static str },
    Internal(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::MissingHeader => write!(f, "Missing authorization header"),
            AuthError::InvalidToken => write!(f, "Invalid token"),
            AuthError::InsufficientScope { required } => {
                write!(f, "Insufficient scope: {} required", required)
            }
            AuthError::Internal(e) => write!(f, "Internal error: {}", e),
        }
    }
}

impl actix_web::ResponseError for AuthError {
    fn error_response(&self) -> HttpResponse {
        match self {
            AuthError::MissingHeader => HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            })),
            AuthError::InvalidToken => HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token",
                "error_type": "AuthenticationError"
            })),
            AuthError::InsufficientScope { required } => {
                HttpResponse::Forbidden().json(serde_json::json!({
                    "error": format!("Insufficient scope: {} required", required),
                    "error_type": "AuthorizationError"
                }))
            }
            AuthError::Internal(e) => HttpResponse::InternalServerError().json(serde_json::json!({
                "error": e,
                "error_type": "InternalError"
            })),
        }
    }
}

/// Shared Bearer token extraction logic
fn extract_bearer_from_request(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
}

impl<S: ScopeRequirement + 'static> FromRequest for AuthUser<S> {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let token_store = match req.app_data::<web::Data<Arc<TokenStore>>>() {
            Some(ts) => ts.clone(),
            None => {
                return Box::pin(ready(Err(
                    AuthError::Internal("TokenStore not configured".into()).into()
                )));
            }
        };

        let token_result = extract_bearer_from_request(req);

        Box::pin(async move {
            let token = token_result.ok_or(AuthError::MissingHeader)?;

            let info = token_store
                .validate_token(&token)
                .map_err(|e| AuthError::Internal(e.to_string()))?
                .ok_or(AuthError::InvalidToken)?;

            if !S::check(&info) {
                return Err(AuthError::InsufficientScope {
                    required: S::description(),
                }
                .into());
            }

            Ok(AuthUser {
                info,
                _scope: std::marker::PhantomData,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};

    #[actix_web::test]
    async fn test_auth_any_valid_token() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthAny>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/test")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_auth_any_missing_header() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthAny>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get().uri("/test").to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn test_auth_any_invalid_token() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthAny>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/test")
            .insert_header(("Authorization", "Bearer hf_invalid"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn test_auth_read_with_read_token() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());
        let token = token_store
            .create_token("testuser", "read-token", "read")
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthRead>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/test")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_auth_read_with_write_token() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());
        let token = token_store
            .create_token("testuser", "write-token", "write")
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthRead>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/test")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_auth_write_with_write_token() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());
        let token = token_store
            .create_token("testuser", "write-token", "write")
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthWrite>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/test")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_auth_write_with_read_token() {
        let token_store = Arc::new(TokenStore::in_memory().unwrap());
        let token = token_store
            .create_token("testuser", "read-token", "read")
            .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route(
                    "/test",
                    web::get().to(|_auth: AuthUser<AuthWrite>| async { "ok" }),
                ),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/test")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["error_type"], "AuthorizationError");
    }

    #[actix_web::test]
    async fn test_extract_bearer_from_request() {
        let req = test::TestRequest::default()
            .insert_header(("Authorization", "Bearer hf_test123"))
            .to_http_request();

        let token = extract_bearer_from_request(&req);
        assert_eq!(token, Some("hf_test123".to_string()));
    }

    #[actix_web::test]
    async fn test_extract_bearer_missing_header() {
        let req = test::TestRequest::default().to_http_request();

        let token = extract_bearer_from_request(&req);
        assert!(token.is_none());
    }

    #[actix_web::test]
    async fn test_extract_bearer_wrong_prefix() {
        let req = test::TestRequest::default()
            .insert_header(("Authorization", "Basic hf_test123"))
            .to_http_request();

        let token = extract_bearer_from_request(&req);
        assert!(token.is_none());
    }
}
