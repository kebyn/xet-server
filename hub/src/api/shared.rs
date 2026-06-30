use crate::metadata::{MetadataStore, Repo};

/// 访问控制:私有 repo 仅 owner(namespace == username)可访问;公开 repo 任何人可访问。
///
/// 集中此判定,避免每个 handler 各自实现导致遗漏(参见 C-AUTH 系列修复)。
/// 调用方在返回 false 时应回 404(而非 403),以免泄露私有 repo 的存在性。
pub fn can_access_repo(repo: &Repo, username: &str) -> bool {
    !repo.private || repo.namespace == username
}

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
        Err(format!(
            "Revision not found: {} (only 'main' branch or commit hashes are supported)",
            revision
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::RepoType;

    fn repo(namespace: &str, private: bool) -> Repo {
        Repo {
            id: 1,
            name: "r".to_string(),
            namespace: namespace.to_string(),
            repo_type: RepoType::Model,
            sha: None,
            private,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_can_access_repo() {
        // 公开 repo:任何人可访问
        assert!(can_access_repo(&repo("owner", false), "owner"));
        assert!(can_access_repo(&repo("owner", false), "stranger"));
        // 私有 repo:仅 owner
        assert!(can_access_repo(&repo("owner", true), "owner"));
        assert!(!can_access_repo(&repo("owner", true), "stranger"));
    }
}
