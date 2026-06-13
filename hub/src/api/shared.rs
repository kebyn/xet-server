use crate::metadata::MetadataStore;

/// Resolve a revision name/branch to a commit ID
/// Shared helper used by multiple API handlers (resolve, tree, preupload)
pub async fn resolve_revision(
    metadata: &dyn MetadataStore,
    repo_id: i64,
    revision: &str,
) -> Result<String, String> {
    // If revision looks like a commit hash (long hex string), use it directly
    if revision.len() >= 8 && revision.chars().all(|c| c.is_ascii_hexdigit()) {
        // Check if it's a known revision
        if metadata.get_revision(repo_id, revision).await.is_ok() {
            return Ok(revision.to_string());
        }
        // I14: Return error for unknown commit hashes instead of falling through
        return Err(format!("Revision not found: {}", revision));
    }

    // I14: Only allow "main" as a branch name (no arbitrary branch resolution yet)
    if revision == "main" {
        let head = metadata.get_head(repo_id).await.ok().flatten();
        match head {
            Some(h) => Ok(h),
            None => Err("No HEAD found for repo".to_string()),
        }
    } else {
        Err(format!("Revision not found: {} (only 'main' branch or commit hashes are supported)", revision))
    }
}
