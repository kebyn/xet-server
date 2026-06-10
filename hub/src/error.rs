use actix_web::{http::StatusCode, HttpResponse};
use serde::Serialize;

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
        HttpResponse::build(self.status_code()).json(ErrorBody {
            error: self.to_string(),
            error_type: self.error_type().to_string(),
        })
    }
}

impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        HubError::Internal(format!("Database error: {}", e))
    }
}

impl From<reqwest::Error> for HubError {
    fn from(e: reqwest::Error) -> Self {
        HubError::CasError(format!("CAS request failed: {}", e))
    }
}