use crate::metadata::Repo;

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
    fn test_can_write_repo() {
        assert!(can_write_repo(&repo("owner", false), "owner"));
        assert!(!can_write_repo(&repo("owner", false), "stranger"));
        assert!(can_write_repo(&repo("owner", true), "owner"));
        assert!(!can_write_repo(&repo("owner", true), "stranger"));
    }
}
