use serde::Serialize;

use crate::auth::token_store::TokenInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WhoamiResponse {
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) orgs: Vec<String>,
    pub(crate) auth: WhoamiAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WhoamiAuth {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(rename = "accessToken")]
    pub(crate) access_token: WhoamiAccessToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WhoamiAccessToken {
    pub(crate) name: String,
    pub(crate) role: String,
}

pub(crate) struct WhoamiService;

impl WhoamiService {
    pub(crate) fn build_response(token_info: &TokenInfo) -> WhoamiResponse {
        WhoamiResponse {
            name: token_info.username.clone(),
            email: String::new(),
            orgs: Vec::new(),
            auth: WhoamiAuth {
                kind: "access_token".to_string(),
                access_token: WhoamiAccessToken {
                    name: token_info.token_name.clone(),
                    role: token_info.scope.clone(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::token_store::TokenInfo;

    use super::WhoamiService;

    fn token_info() -> TokenInfo {
        TokenInfo {
            user_id: "user_123".to_string(),
            username: "testuser".to_string(),
            token_name: "test-token".to_string(),
            scope: "read write".to_string(),
        }
    }

    #[test]
    fn builds_huggingface_whoami_response_from_token_info() {
        let response = WhoamiService::build_response(&token_info());

        assert_eq!(response.name, "testuser");
        assert_eq!(response.email, "");
        assert!(response.orgs.is_empty());
        assert_eq!(response.auth.kind, "access_token");
        assert_eq!(response.auth.access_token.name, "test-token");
        assert_eq!(response.auth.access_token.role, "read write");
    }
}
