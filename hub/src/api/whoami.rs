use actix_web::HttpResponse;
use crate::auth::extract::AuthUser;
use crate::auth::extract::AuthAny;

/// GET /api/whoami - Get current user info from token
pub async fn whoami(
    auth: AuthUser<AuthAny>,
) -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "name": auth.info.username,
        "email": "",
        "orgs": [],
        "auth": {
            "type": "access_token",
            "accessToken": {
                "name": auth.info.token_name,
                "role": auth.info.scope
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App, web};
    use crate::auth::token_store::TokenStore;

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
}