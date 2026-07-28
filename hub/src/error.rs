use actix_web::{HttpResponse, http::StatusCode};
use serde::Serialize;
use std::fmt::Display;

pub(crate) const INTERNAL_ERROR_MESSAGE: &str = "Internal server error";
pub(crate) const CAS_ERROR_MESSAGE: &str = "Upstream CAS request failed";

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Already exists: {0}")]
    Conflict(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Unprocessable: {0}")]
    Unprocessable(String),
    #[error("CAS error: {0}")]
    CasError(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    error_type: String,
}

impl HubError {
    pub fn error_type(&self) -> &'static str {
        match self {
            HubError::NotFound(_) => "NotFoundError",
            HubError::Conflict(_) => "ConflictError",
            HubError::Unauthorized(_) => "AuthenticationError",
            HubError::Forbidden(_) => "AuthorizationError",
            HubError::BadRequest(_) => "ValidationError",
            HubError::Unprocessable(_) => "UnprocessableEntity",
            HubError::CasError(_) => "BadGateway",
            HubError::Internal(_) => "InternalError",
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            HubError::NotFound(_) => StatusCode::NOT_FOUND,
            HubError::Conflict(_) => StatusCode::CONFLICT,
            HubError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            HubError::Forbidden(_) => StatusCode::FORBIDDEN,
            HubError::BadRequest(_) => StatusCode::BAD_REQUEST,
            HubError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            HubError::CasError(_) => StatusCode::BAD_GATEWAY,
            HubError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl actix_web::ResponseError for HubError {
    fn error_response(&self) -> HttpResponse {
        match self {
            HubError::CasError(_) => {
                return bad_gateway_error_response(
                    "Hub request failed through CAS",
                    self,
                    "BadGateway",
                );
            }
            HubError::Internal(_) => {
                return internal_error_response("Hub request failed", self);
            }
            _ => {}
        }

        HttpResponse::build(self.status_code()).json(ErrorBody {
            error: self.to_string(),
            error_type: self.error_type().to_string(),
        })
    }
}

pub(crate) fn internal_error_response(context: &str, detail: impl Display) -> HttpResponse {
    tracing::error!("{}: {}", context, detail);
    HttpResponse::InternalServerError().json(ErrorBody {
        error: INTERNAL_ERROR_MESSAGE.to_string(),
        error_type: "InternalError".to_string(),
    })
}

pub(crate) fn bad_gateway_error_response(
    context: &str,
    detail: impl Display,
    error_type: &str,
) -> HttpResponse {
    tracing::error!("{}: {}", context, detail);
    HttpResponse::BadGateway().json(ErrorBody {
        error: CAS_ERROR_MESSAGE.to_string(),
        error_type: error_type.to_string(),
    })
}

impl From<sqlx::Error> for HubError {
    fn from(e: sqlx::Error) -> Self {
        HubError::Internal(format!("Database error: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use actix_web::ResponseError;

    use super::{CAS_ERROR_MESSAGE, HubError, INTERNAL_ERROR_MESSAGE};

    #[actix_web::test]
    async fn infrastructure_errors_are_sanitized_in_http_responses() {
        for (error, expected_message) in [
            (
                HubError::Internal("sqlite at /secret/hub.db failed".to_string()),
                INTERNAL_ERROR_MESSAGE,
            ),
            (
                HubError::CasError(
                    "request to http://private-cas:8081 exposed upstream body".to_string(),
                ),
                CAS_ERROR_MESSAGE,
            ),
        ] {
            let response = error.error_response();
            let body = actix_web::body::to_bytes(response.into_body())
                .await
                .expect("error response body should be readable");
            let body: serde_json::Value =
                serde_json::from_slice(&body).expect("error response should be JSON");
            assert_eq!(body["error"], expected_message);
            assert!(!body.to_string().contains("secret"));
            assert!(!body.to_string().contains("private-cas"));
        }
    }
}
