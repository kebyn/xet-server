use crate::metadata::{MetadataStore, Repo};

pub(crate) fn can_access_repo(repo: &Repo, username: &str) -> bool {
    !repo.private || repo.namespace == username
}

pub(crate) async fn resolve_revision_id(
    metadata: &dyn MetadataStore,
    repo_id: i64,
    revision: &str,
) -> Result<String, String> {
    if revision.len() >= 8 && revision.chars().all(|c| c.is_ascii_hexdigit()) {
        if metadata.get_revision(repo_id, revision).await.is_ok() {
            return Ok(revision.to_string());
        }
        return Err(format!("Revision not found: {}", revision));
    }

    if revision == "main" {
        let head = metadata.get_head(repo_id).await.ok().flatten();
        match head {
            Some(head) => Ok(head),
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
            name: "repo".to_string(),
            namespace: namespace.to_string(),
            repo_type: RepoType::Model,
            sha: None,
            private,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn public_repo_can_be_read_by_any_user() {
        assert!(can_access_repo(&repo("owner", false), "owner"));
        assert!(can_access_repo(&repo("owner", false), "reader"));
    }

    #[test]
    fn private_repo_can_only_be_read_by_owner() {
        assert!(can_access_repo(&repo("owner", true), "owner"));
        assert!(!can_access_repo(&repo("owner", true), "reader"));
    }
}
