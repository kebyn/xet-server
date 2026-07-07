use crate::auth::xet_signer::XetSigner;
use crate::lfs_proxy::oid::validate_oid;

/// Maximum number of objects allowed in a single batch request.
/// Mirrors CAS-side limit for defense-in-depth.
pub(crate) const MAX_BATCH_SIZE: usize = 1000;

/// Rewrite URLs in batch response from CAS URLs to Hub URLs,
/// and replace internal CAS auth tokens with short-lived proxy tokens.
pub(crate) fn rewrite_batch_urls(
    response: &mut serde_json::Value,
    hub_base: &str,
    signer: &XetSigner,
    username: &str,
) {
    use url::Url;

    let hub_url = match Url::parse(hub_base) {
        Ok(u) => u,
        Err(_) => return,
    };

    if let Some(objects) = response.get_mut("objects")
        && let Some(arr) = objects.as_array_mut()
    {
        for obj in arr {
            let oid = obj
                .get("oid")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();

            if !validate_oid(&oid) {
                continue;
            }

            if let Some(actions) = obj.get_mut("actions") {
                if let Some(upload_action) = actions.get_mut("upload") {
                    match signer.sign_proxy(username, &oid, "upload", "", "") {
                        Ok((proxy_token, _)) => {
                            if !rewrite_action_url(upload_action, &hub_url, &proxy_token)
                                && let Some(actions_obj) = actions.as_object_mut()
                            {
                                actions_obj.remove("upload");
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to sign proxy token for upload {}: {}", oid, e);
                            if let Some(actions_obj) = actions.as_object_mut() {
                                actions_obj.remove("upload");
                            }
                        }
                    }
                }
                if let Some(download_action) = actions.get_mut("download") {
                    match signer.sign_proxy(username, &oid, "download", "", "") {
                        Ok((proxy_token, _)) => {
                            if !rewrite_action_url(download_action, &hub_url, &proxy_token)
                                && let Some(actions_obj) = actions.as_object_mut()
                            {
                                actions_obj.remove("download");
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to sign proxy token for download {}: {}",
                                oid,
                                e
                            );
                            if let Some(actions_obj) = actions.as_object_mut() {
                                actions_obj.remove("download");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Rewrite a single action's URL and auth header with proxy token.
/// Returns false when href cannot be parsed, so callers can drop the action
/// instead of leaking an internal CAS URL.
pub(crate) fn rewrite_action_url(
    action: &mut serde_json::Value,
    hub_url: &url::Url,
    proxy_token: &str,
) -> bool {
    let new_href = action
        .get("href")
        .and_then(|h| h.as_str())
        .and_then(|h| url::Url::parse(h).ok())
        .map(|mut url| {
            url.set_scheme(hub_url.scheme()).ok();
            url.set_host(hub_url.host_str()).ok();
            if let Some(port) = hub_url.port() {
                url.set_port(Some(port)).ok();
            } else {
                url.set_port(None).ok();
            }

            url.query_pairs_mut().append_pair("token", proxy_token);
            url.to_string()
        });

    let Some(href) = new_href else {
        return false;
    };
    if let Some(action_obj) = action.as_object_mut() {
        action_obj.insert("href".to_string(), serde_json::Value::String(href));
    }

    if action
        .get("header")
        .and_then(|h| h.get("Authorization"))
        .is_some()
        && let Some(header_obj) = action.get_mut("header").and_then(|h| h.as_object_mut())
    {
        header_obj.insert(
            "Authorization".to_string(),
            serde_json::Value::String(format!("Bearer {}", proxy_token)),
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use serde_json::json;

    use crate::auth::xet_signer::XetSigner;

    use super::{rewrite_action_url, rewrite_batch_urls};

    fn signer() -> XetSigner {
        let signing_key = SigningKey::generate(&mut OsRng);
        XetSigner::new(signing_key, "test-key", 3600, 300)
    }

    #[test]
    fn rewrite_action_url_drops_on_parse_failure() {
        let hub = url::Url::parse("https://hub.example.com").unwrap();
        let mut action = json!({"href": "not a valid url at all"});

        let ok = rewrite_action_url(&mut action, &hub, "proxy_tok");

        assert!(!ok, "unparseable href should let caller drop action");
    }

    #[test]
    fn rewrite_action_url_rewrites_valid_href() {
        let hub = url::Url::parse("https://hub.example.com:9000").unwrap();
        let mut action = json!({"href": "http://cas-internal:5000/lfs/objects/abc"});

        let ok = rewrite_action_url(&mut action, &hub, "proxy_tok");

        assert!(ok);
        let href = action.get("href").unwrap().as_str().unwrap();
        assert!(href.contains("hub.example.com"));
        assert!(href.contains("token=proxy_tok"));
        assert!(!href.contains("cas-internal"));
    }

    #[test]
    fn rewrite_batch_urls_rewrites_upload_and_download_actions() {
        let signer = signer();
        let valid_oid = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let mut response = json!({
            "objects": [
                {
                    "oid": valid_oid,
                    "size": 1024,
                    "actions": {
                        "upload": {
                            "href": format!("http://cas:9090/lfs/objects/{}", valid_oid)
                        },
                        "download": {
                            "href": format!("http://cas:9090/lfs/objects/{}", valid_oid)
                        }
                    }
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", &signer, "testuser");

        let objects = response.get("objects").unwrap().as_array().unwrap();
        let actions = objects[0].get("actions").unwrap();
        let upload_href = actions
            .get("upload")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();
        let download_href = actions
            .get("download")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();

        assert!(upload_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            valid_oid
        )));
        assert!(download_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            valid_oid
        )));
    }

    #[test]
    fn rewrite_batch_urls_leaves_objects_without_actions_unchanged() {
        let signer = signer();
        let mut response = json!({
            "objects": [
                {
                    "oid": "abc123",
                    "size": 1024
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", &signer, "testuser");

        assert_eq!(
            response,
            json!({
                "objects": [
                    {
                        "oid": "abc123",
                        "size": 1024
                    }
                ]
            })
        );
    }

    #[test]
    fn rewrite_batch_urls_handles_partial_action_sets() {
        let signer = signer();
        let oid1 = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let oid2 = "b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3";
        let mut response = json!({
            "objects": [
                {
                    "oid": oid1,
                    "size": 1024,
                    "actions": {
                        "upload": {
                            "href": format!("http://cas:9090/lfs/objects/{}", oid1)
                        }
                    }
                },
                {
                    "oid": oid2,
                    "size": 2048,
                    "actions": {
                        "download": {
                            "href": format!("http://cas:9090/lfs/objects/{}", oid2)
                        }
                    }
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", &signer, "testuser");

        let objects = response.get("objects").unwrap().as_array().unwrap();
        let upload_href = objects[0]
            .get("actions")
            .unwrap()
            .get("upload")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();
        let download_href = objects[1]
            .get("actions")
            .unwrap()
            .get("download")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();

        assert!(upload_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            oid1
        )));
        assert!(download_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            oid2
        )));
    }
}
