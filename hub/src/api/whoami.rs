use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;

/// GET /api/whoami - Get current user info from token
pub async fn whoami(
    req: HttpRequest,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
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
        Ok(Some(info)) => HttpResponse::Ok().json(serde_json::json!({
            "name": info.username,
            "email": "",
            "orgs": [],
            "auth": {
                "type": "access_token",
                "accessToken": {
                    "name": info.token_name,
                    "role": info.scope
                }
            }
        })),
        Ok(None) => HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid token",
            "error_type": "AuthenticationError"
        })),
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("{}", e),
            "error_type": "InternalError"
        })),
    }
}

/// Extract Bearer token from Authorization header
pub fn extract_bearer(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};

    #[actix_web::test]
    async fn test_whoami_valid_token() {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().unwrap());
        let token = token_store.create_token("testuser", "test-token", "read").unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route("/api/whoami", web::get().to(whoami))
        ).await;

        let req = test::TestRequest::get()
            .uri("/api/whoami")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["name"], "testuser");
        assert_eq!(body["auth"]["accessToken"]["name"], "test-token");
        assert_eq!(body["auth"]["accessToken"]["role"], "read");
    }

    #[actix_web::test]
    async fn test_whoami_invalid_token() {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().unwrap());

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route("/api/whoami", web::get().to(whoami))
        ).await;

        let req = test::TestRequest::get()
            .uri("/api/whoami")
            .insert_header(("Authorization", "Bearer hf_invalid"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn test_whoami_missing_auth() {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().unwrap());

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .route("/api/whoami", web::get().to(whoami))
        ).await;

        let req = test::TestRequest::get()
            .uri("/api/whoami")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn test_extract_bearer() {
        let req = actix_web::test::TestRequest::default()
            .insert_header(("Authorization", "Bearer hf_test123"))
            .to_http_request();

        let token = extract_bearer(&req);
        assert_eq!(token, Some("hf_test123".to_string()));
    }

    #[actix_web::test]
    async fn test_extract_bearer_missing_header() {
        let req = actix_web::test::TestRequest::default()
            .to_http_request();

        let token = extract_bearer(&req);
        assert!(token.is_none());
    }

    #[actix_web::test]
    async fn test_extract_bearer_wrong_prefix() {
        let req = actix_web::test::TestRequest::default()
            .insert_header(("Authorization", "Basic hf_test123"))
            .to_http_request();

        let token = extract_bearer(&req);
        assert!(token.is_none());
    }
}