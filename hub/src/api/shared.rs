use crate::metadata::Repo;

/// 访问控制:私有 repo 仅 owner(namespace == username)可访问;公开 repo 任何人可访问。
///
/// 集中此判定,避免每个 handler 各自实现导致遗漏(参见 C-AUTH 系列修复)。
/// 调用方在返回 false 时应回 404(而非 403),以免泄露私有 repo 的存在性。
pub(crate) use crate::services::shared::can_access_repo;

/// 写权限目前仅授予 repo owner。公开 repo 可读不代表可写。
pub fn can_write_repo(repo: &Repo, username: &str) -> bool {
    repo.namespace == username
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

    #[test]
    fn test_can_write_repo() {
        assert!(can_write_repo(&repo("owner", false), "owner"));
        assert!(!can_write_repo(&repo("owner", false), "stranger"));
        assert!(can_write_repo(&repo("owner", true), "owner"));
        assert!(!can_write_repo(&repo("owner", true), "stranger"));
    }
}
