/// Decode base64 content, accepting either a `base64:` prefix or raw base64.
pub(crate) fn decode_base64_content(content: &str) -> Result<Vec<u8>, String> {
    let content_to_decode = content.strip_prefix("base64:").unwrap_or(content);
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    STANDARD
        .decode(content_to_decode)
        .map_err(|e| format!("Base64 decode error: {}", e))
}
