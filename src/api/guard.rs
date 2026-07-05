//! Centralized request authentication guard for CAS HTTP handlers.
//!
//! `auth.rs` holds the pure token/crypto primitives. This module wraps them in
//! the single request-level decision every protected handler needs — extract the
//! token, verify it, and check authorization — so that decision lives in exactly
//! one place and individual handlers cannot accidentally skip a step or call the
//! wrong scope helper.
//!
//! Rendering stays flexible: `AuthReject` carries the HTTP status and message, and
//! the `respond`/`respond_message` helpers reproduce the two common error-body
//! shapes (`{"error": ..}` and the Git-LFS `{"message": ..}`). Handlers with a
//! bespoke body match on the variant and render directly.

use std::time::Instant;

use actix_web::{HttpRequest, HttpResponse};

use crate::api::auth::{
    AuthVerifier, XetClaims, authorize_endpoint, extract_token_from_request, is_internal_token,
};
use crate::metrics::GLOBAL_METRICS;

/// What a handler requires of the caller.
///
/// The optional message variants let a handler keep its existing 403 wording
/// while still routing the decision through `require_auth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfsOperation {
    Upload,
    Download,
}

impl LfsOperation {
    pub fn required_regular_scope(self) -> &'static str {
        match self {
            Self::Upload => "write",
            Self::Download => "read",
        }
    }

    pub fn proxy_scope(self) -> &'static str {
        match self {
            Self::Upload => "lfs-upload",
            Self::Download => "lfs-download",
        }
    }

    pub fn claim_value(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

pub enum AuthNeed {
    /// Caller must hold `scope` (or be an internal service token). 403 body uses
    /// the default "Insufficient scope" message.
    Scope(&'static str),
    /// Like `Scope`, but with a custom 403 message.
    ScopeMsg(&'static str, &'static str),
    /// Caller must present a valid internal service token (defense-in-depth:
    /// sub + scope + token_type all checked). Carries the 403 message to use.
    Internal(&'static str),
    LfsObject {
        operation: LfsOperation,
        oid: String,
        message: &'static str,
    },
}

pub fn authorize_lfs_object(claims: &XetClaims, operation: LfsOperation, oid: &str) -> bool {
    if claims.token_type == "proxy" {
        let has_proxy_scope = claims
            .scope
            .split_whitespace()
            .any(|s| s == operation.proxy_scope());
        return has_proxy_scope
            && claims.oid.as_deref() == Some(oid)
            && claims.operation.as_deref() == Some(operation.claim_value());
    }

    authorize_endpoint(claims, operation.required_regular_scope())
}

impl AuthNeed {
    fn forbidden_message(&self) -> &'static str {
        match self {
            AuthNeed::Scope(_) => "Insufficient scope",
            AuthNeed::ScopeMsg(_, msg) => msg,
            AuthNeed::Internal(msg) => msg,
            AuthNeed::LfsObject { message, .. } => message,
        }
    }
}

/// Why authentication was rejected. The `Forbidden` variant carries the message
/// supplied by the originating `AuthNeed`.
pub enum AuthReject {
    /// No usable Authorization header (401).
    MissingToken,
    /// Token present but failed signature/kid/expiry verification (401).
    InvalidToken,
    /// Token valid but not authorized for this endpoint (403).
    Forbidden(&'static str),
}

impl AuthReject {
    /// `(status_code, message)` for the standard `{"error": ..}` body.
    fn error_parts(&self) -> (u16, &'static str) {
        match self {
            AuthReject::MissingToken => (401, "Missing or invalid authorization token"),
            AuthReject::InvalidToken => (401, "Invalid token"),
            AuthReject::Forbidden(msg) => (403, msg),
        }
    }

    /// `(status_code, message)` for the Git-LFS `{"message": ..}` body, which
    /// uses a slightly different "missing" phrasing.
    fn message_parts(&self) -> (u16, &'static str) {
        match self {
            AuthReject::MissingToken => (401, "Missing or invalid authorization"),
            AuthReject::InvalidToken => (401, "Invalid token"),
            AuthReject::Forbidden(msg) => (403, msg),
        }
    }

    fn build(code: u16, body: serde_json::Value) -> HttpResponse {
        if code == 403 {
            HttpResponse::Forbidden().json(body)
        } else {
            HttpResponse::Unauthorized().json(body)
        }
    }

    /// Record request/latency metrics and return the standard `{"error": ..}` body.
    pub fn respond(self, start: Instant) -> HttpResponse {
        let (code, msg) = self.error_parts();
        GLOBAL_METRICS.record_request(code);
        GLOBAL_METRICS.record_latency(start);
        Self::build(code, serde_json::json!({ "error": msg }))
    }

    /// Record request/latency metrics and return the Git-LFS `{"message": ..}` body.
    pub fn respond_message(self, start: Instant) -> HttpResponse {
        let (code, msg) = self.message_parts();
        GLOBAL_METRICS.record_request(code);
        GLOBAL_METRICS.record_latency(start);
        Self::build(code, serde_json::json!({ "message": msg }))
    }
}

/// Extract, verify, and authorize the caller's token in one step.
///
/// On success returns the verified claims. On failure returns an [`AuthReject`]
/// describing which stage failed and the message to surface — the handler decides
/// how to render it (and whether to record metrics) via `AuthReject`'s helpers.
pub fn require_auth(
    req: &HttpRequest,
    auth: &AuthVerifier,
    need: AuthNeed,
) -> Result<XetClaims, AuthReject> {
    let token = extract_token_from_request(req).ok_or(AuthReject::MissingToken)?;
    let claims = auth
        .verify_token(&token)
        .map_err(|_| AuthReject::InvalidToken)?;

    let forbidden_msg = need.forbidden_message();
    let authorized = match need {
        AuthNeed::Scope(scope) | AuthNeed::ScopeMsg(scope, _) => authorize_endpoint(&claims, scope),
        AuthNeed::Internal(_) => is_internal_token(&claims),
        AuthNeed::LfsObject {
            operation, ref oid, ..
        } => authorize_lfs_object(&claims, operation, oid),
    };

    if authorized {
        Ok(claims)
    } else {
        Err(AuthReject::Forbidden(forbidden_msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xet_auth_types::XetClaims;

    fn claims(
        scope: &str,
        token_type: &str,
        oid: Option<&str>,
        operation: Option<&str>,
    ) -> XetClaims {
        XetClaims {
            sub: "u".to_string(),
            scope: scope.to_string(),
            repo_id: "ns/repo".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: 4_102_444_800,
            iat: 1_700_000_000,
            kid: "kid".to_string(),
            token_type: token_type.to_string(),
            oid: oid.map(str::to_string),
            operation: operation.map(str::to_string),
        }
    }

    fn internal_claims() -> XetClaims {
        XetClaims {
            sub: "hub-service".to_string(),
            scope: "internal".to_string(),
            repo_id: "*".to_string(),
            repo_type: "*".to_string(),
            revision: "*".to_string(),
            exp: 4_102_444_800,
            iat: 1_700_000_000,
            kid: "kid".to_string(),
            token_type: "internal".to_string(),
            oid: None,
            operation: None,
        }
    }

    #[test]
    fn test_lfs_proxy_requires_oid_and_operation() {
        let c = claims("lfs-download", "proxy", None, Some("download"));
        assert!(!authorize_lfs_object(&c, LfsOperation::Download, "a"));

        let c = claims("lfs-download", "proxy", Some("a"), None);
        assert!(!authorize_lfs_object(&c, LfsOperation::Download, "a"));
    }

    #[test]
    fn test_lfs_proxy_rejects_wrong_oid_or_operation() {
        let c = claims("lfs-download", "proxy", Some("a"), Some("download"));
        assert!(!authorize_lfs_object(&c, LfsOperation::Download, "b"));
        assert!(!authorize_lfs_object(&c, LfsOperation::Upload, "a"));
    }

    #[test]
    fn test_lfs_proxy_accepts_exact_binding() {
        let c = claims("lfs-download", "proxy", Some("a"), Some("download"));
        assert!(authorize_lfs_object(&c, LfsOperation::Download, "a"));

        let c = claims("lfs-upload", "proxy", Some("a"), Some("upload"));
        assert!(authorize_lfs_object(&c, LfsOperation::Upload, "a"));
    }

    #[test]
    fn test_lfs_proxy_regular_tokens_still_work() {
        let c = claims("read", "user", None, None);
        assert!(authorize_lfs_object(&c, LfsOperation::Download, "a"));
        assert!(!authorize_lfs_object(&c, LfsOperation::Upload, "a"));

        let c = claims("write", "user", None, None);
        assert!(authorize_lfs_object(&c, LfsOperation::Upload, "a"));
        assert!(!authorize_lfs_object(&c, LfsOperation::Download, "a"));
    }

    #[test]
    fn test_lfs_object_rejects_internal_token() {
        let c = internal_claims();
        assert!(!authorize_lfs_object(&c, LfsOperation::Download, "a"));
        assert!(!authorize_lfs_object(&c, LfsOperation::Upload, "a"));
    }
}
