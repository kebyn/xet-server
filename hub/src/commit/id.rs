use sha2::{Digest, Sha256};

/// Generate a commit ID from repo_id, parent, message, timestamp, and UUID nonce.
pub(crate) fn generate_commit_id(
    repo_id: i64,
    parent: Option<&str>,
    message: &str,
    timestamp: i64,
) -> String {
    let nonce = uuid::Uuid::new_v4().to_string();
    let input = format!(
        "{}:{}:{}:{}:{}",
        repo_id,
        parent.unwrap_or(""),
        message,
        timestamp,
        nonce
    );
    hex::encode(Sha256::digest(input.as_bytes()))
}
