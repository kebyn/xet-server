use std::sync::Arc;

use crate::auth::xet_signer::XetSigner;
use crate::lfs_proxy::oid::validate_oid;
use crate::lfs_proxy::tokens::validate_proxy_token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LfsObjectOperation {
    Upload,
    Download,
}

impl LfsObjectOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LfsObjectOperation::Upload => "upload",
            LfsObjectOperation::Download => "download",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LfsObjectGuardError {
    InvalidOid,
    InvalidToken,
}

pub(crate) struct LfsObjectGuard {
    signer: Arc<XetSigner>,
}

impl LfsObjectGuard {
    pub(crate) fn new(signer: Arc<XetSigner>) -> Self {
        Self { signer }
    }

    pub(crate) fn authorize(
        &self,
        token: &str,
        oid: &str,
        operation: LfsObjectOperation,
    ) -> Result<(), LfsObjectGuardError> {
        if !validate_oid(oid) {
            return Err(LfsObjectGuardError::InvalidOid);
        }

        if !validate_proxy_token(token, oid, operation.as_str(), &self.signer) {
            return Err(LfsObjectGuardError::InvalidToken);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    use crate::auth::xet_signer::XetSigner;

    use super::{LfsObjectGuard, LfsObjectGuardError, LfsObjectOperation};

    fn signer() -> Arc<XetSigner> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300))
    }

    #[test]
    fn upload_proxy_token_bound_to_oid_and_operation_is_authorized() {
        let signer = signer();
        let oid = "a".repeat(64);
        let (token, _) = signer.sign_proxy("user", &oid, "upload", "", "").unwrap();
        let guard = LfsObjectGuard::new(signer);

        let result = guard.authorize(&token, &oid, LfsObjectOperation::Upload);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn invalid_oid_is_rejected_before_token_validation() {
        let guard = LfsObjectGuard::new(signer());

        let result = guard.authorize("not-a-token", "not-hex", LfsObjectOperation::Download);

        assert_eq!(result, Err(LfsObjectGuardError::InvalidOid));
    }
}
