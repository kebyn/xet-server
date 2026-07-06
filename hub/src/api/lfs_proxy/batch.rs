use super::oid::validate_oid;
use crate::auth::xet_signer::XetSigner;

/// Maximum number of objects allowed in a single batch request.
/// Mirrors CAS-side limit for defense-in-depth.
pub(super) const MAX_BATCH_SIZE: usize = 1000;

/// Rewrite URLs in batch response from CAS URLs to Hub URLs,
/// and replace internal CAS auth tokens with short-lived proxy tokens.
pub(super) fn rewrite_batch_urls(
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
pub(super) fn rewrite_action_url(
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
