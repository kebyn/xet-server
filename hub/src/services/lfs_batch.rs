use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::extract::scope_allows;
use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::error::HubError;
use crate::lfs_proxy::batch::{MAX_BATCH_SIZE, rewrite_batch_urls};

#[async_trait]
pub(crate) trait LfsBatchCasClient: Send + Sync {
    async fn proxy_batch(
        &self,
        body: &serde_json::Value,
        token: &str,
    ) -> Result<serde_json::Value, HubError>;
}

#[async_trait]
impl LfsBatchCasClient for CasClient {
    async fn proxy_batch(
        &self,
        body: &serde_json::Value,
        token: &str,
    ) -> Result<serde_json::Value, HubError> {
        CasClient::proxy_batch(self, body, token).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LfsBatchServiceError {
    InvalidToken,
    Validation(String),
    Authorization(String),
    BadGateway(String),
    Internal(String),
}

pub(crate) struct LfsBatchRequest<'a> {
    pub(crate) user_token: &'a str,
    pub(crate) body: &'a serde_json::Value,
    pub(crate) hub_base_url: &'a str,
}

pub(crate) struct LfsBatchService {
    token_store: Arc<TokenStore>,
    xet_signer: Arc<XetSigner>,
    cas_client: Arc<dyn LfsBatchCasClient>,
}

impl LfsBatchService {
    pub(crate) fn new(
        token_store: Arc<TokenStore>,
        xet_signer: Arc<XetSigner>,
        cas_client: Arc<dyn LfsBatchCasClient>,
    ) -> Self {
        Self {
            token_store,
            xet_signer,
            cas_client,
        }
    }

    pub(crate) async fn batch(
        &self,
        request: LfsBatchRequest<'_>,
    ) -> Result<serde_json::Value, LfsBatchServiceError> {
        let token_info = self
            .token_store
            .validate_token(request.user_token)
            .await
            .map_err(|err| LfsBatchServiceError::Internal(err.to_string()))?
            .ok_or(LfsBatchServiceError::InvalidToken)?;

        let operation = request
            .body
            .get("operation")
            .and_then(|operation| operation.as_str())
            .unwrap_or("download");
        let required_scope = match operation {
            "upload" => "write",
            "download" => "read",
            _ => {
                return Err(LfsBatchServiceError::Validation(format!(
                    "Invalid operation: {}",
                    operation
                )));
            }
        };

        if !scope_allows(&token_info.scope, required_scope) {
            return Err(LfsBatchServiceError::Authorization(format!(
                "Token scope '{}' insufficient for {} operation (requires '{}')",
                token_info.scope, operation, required_scope
            )));
        }

        let object_count = request
            .body
            .get("objects")
            .and_then(|objects| objects.as_array())
            .map(|objects| objects.len())
            .unwrap_or(0);
        if object_count > MAX_BATCH_SIZE {
            return Err(LfsBatchServiceError::Validation(format!(
                "Too many objects: {} exceeds limit of {}",
                object_count, MAX_BATCH_SIZE
            )));
        }

        tracing::debug!(
            object_count,
            user = %token_info.username,
            "Processing LFS batch request"
        );

        let (cas_batch_token, _) = self
            .xet_signer
            .sign(&token_info.username, required_scope, "", "", "")
            .map_err(|err| {
                LfsBatchServiceError::Internal(format!("Failed to sign CAS batch token: {}", err))
            })?;

        let mut response = self
            .cas_client
            .proxy_batch(request.body, &cas_batch_token)
            .await
            .map_err(|err| LfsBatchServiceError::BadGateway(err.to_string()))?;

        rewrite_batch_urls(
            &mut response,
            request.hub_base_url,
            &self.xet_signer,
            &token_info.username,
        );

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use serde_json::json;

    use crate::auth::token_store::TokenStore;
    use crate::auth::xet_signer::XetSigner;
    use crate::error::HubError;

    use super::{LfsBatchCasClient, LfsBatchRequest, LfsBatchService};

    struct MockBatchCasClient {
        forwarded_token: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl LfsBatchCasClient for MockBatchCasClient {
        async fn proxy_batch(
            &self,
            _body: &serde_json::Value,
            token: &str,
        ) -> Result<serde_json::Value, HubError> {
            *self.forwarded_token.lock().unwrap() = Some(token.to_string());
            let oid = "a".repeat(64);
            Ok(json!({
                "objects": [{
                    "oid": oid,
                    "size": 123,
                    "actions": {
                        "download": {
                            "href": format!("http://cas:8081/lfs/objects/{}", oid),
                            "header": {"Authorization": "Bearer internal"}
                        }
                    }
                }]
            }))
        }
    }

    fn signer() -> Arc<XetSigner> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300))
    }

    #[tokio::test]
    async fn download_batch_forwards_read_scoped_cas_token_and_rewrites_actions() {
        let token_store = Arc::new(TokenStore::in_memory().await.unwrap());
        let user_token = token_store
            .create_token("reader", "reader-token", "read")
            .await
            .unwrap();
        let signer = signer();
        let forwarded_token = Arc::new(Mutex::new(None));
        let service = LfsBatchService::new(
            token_store,
            signer.clone(),
            Arc::new(MockBatchCasClient {
                forwarded_token: forwarded_token.clone(),
            }),
        );

        let oid = "a".repeat(64);
        let response = service
            .batch(LfsBatchRequest {
                user_token: &user_token,
                body: &json!({
                    "operation": "download",
                    "objects": [{"oid": oid, "size": 123}]
                }),
                hub_base_url: "http://hub:8080",
            })
            .await
            .unwrap();

        let cas_token = forwarded_token.lock().unwrap().clone().unwrap();
        let claims = signer.verify_xet_token(&cas_token).unwrap();
        assert_eq!(claims.sub, "reader");
        assert_eq!(claims.scope, "read");

        let href = response["objects"][0]["actions"]["download"]["href"]
            .as_str()
            .unwrap();
        assert!(href.starts_with("http://hub:8080/lfs/objects/"));
        assert!(href.contains("?token=proxy_"));
        assert!(
            response["objects"][0]["actions"]["download"]["header"]["Authorization"]
                .as_str()
                .unwrap()
                .starts_with("Bearer proxy_")
        );
    }
}
