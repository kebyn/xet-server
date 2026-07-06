use std::{collections::HashSet, sync::Arc};

use crate::metadata::{FileEntry, MetadataError, MetadataStore, Repo, RepoType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TreeServiceError {
    NotFound(String),
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeListingEntryType {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeListingEntry {
    pub(crate) entry_type: TreeListingEntryType,
    pub(crate) oid: Option<String>,
    pub(crate) size: u64,
    pub(crate) path: String,
}

pub(crate) struct TreeListRequest<'a> {
    pub(crate) username: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) repo_name: &'a str,
    pub(crate) repo_type: RepoType,
    pub(crate) revision: &'a str,
    pub(crate) tree_path: &'a str,
    pub(crate) recursive: bool,
}

pub(crate) struct TreeService {
    metadata: Arc<dyn MetadataStore>,
}

impl TreeService {
    pub(crate) fn new(metadata: Arc<dyn MetadataStore>) -> Self {
        Self { metadata }
    }

    pub(crate) async fn list_tree(
        &self,
        request: TreeListRequest<'_>,
    ) -> Result<Vec<TreeListingEntry>, TreeServiceError> {
        let tree_path = normalize_tree_path(request.tree_path);
        let repo = self
            .load_repo(request.namespace, request.repo_name, request.repo_type)
            .await?;

        if !can_access_repo(&repo, request.username) {
            return Err(TreeServiceError::NotFound(
                "Repository not found".to_string(),
            ));
        }

        let commit_id = resolve_revision_id(self.metadata.as_ref(), repo.id, request.revision)
            .await
            .map_err(TreeServiceError::NotFound)?;

        let entries = self
            .metadata
            .get_file_tree_prefix(repo.id, &commit_id, &tree_path)
            .await
            .map_err(|err| TreeServiceError::Internal(err.to_string()))?;

        let mut tree_entries = if request.recursive {
            recursive_entries(&entries, &tree_path)
        } else {
            non_recursive_entries(&entries, &tree_path)
        };
        tree_entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(tree_entries)
    }

    async fn load_repo(
        &self,
        namespace: &str,
        repo_name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, TreeServiceError> {
        self.metadata
            .get_repo(namespace, repo_name, repo_type)
            .await
            .map_err(|err| match err {
                MetadataError::RepoNotFound(_) => TreeServiceError::NotFound(err.to_string()),
                _ => TreeServiceError::Internal(err.to_string()),
            })
    }
}

fn can_access_repo(repo: &Repo, username: &str) -> bool {
    !repo.private || repo.namespace == username
}

fn normalize_tree_path(path: &str) -> String {
    path.trim_matches('/').to_string()
}

fn strip_tree_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    if path == prefix {
        return Some("");
    }
    path.strip_prefix(&format!("{}/", prefix))
}

fn join_tree_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

fn infer_directories(entries: &[FileEntry], prefix: &str) -> Vec<String> {
    let mut dirs = HashSet::new();

    for entry in entries {
        let rel_path = match strip_tree_prefix(&entry.path, prefix) {
            Some(path) => path,
            None => continue,
        };
        if rel_path.is_empty() {
            continue;
        }

        if let Some(pos) = rel_path.find('/') {
            dirs.insert(rel_path[..pos].to_string());
        }
    }

    dirs.into_iter().collect()
}

fn recursive_entries(entries: &[FileEntry], tree_path: &str) -> Vec<TreeListingEntry> {
    entries
        .iter()
        .filter_map(|entry| {
            let rel_path = strip_tree_prefix(&entry.path, tree_path)?;
            if rel_path.is_empty() {
                return None;
            }

            Some(TreeListingEntry {
                entry_type: TreeListingEntryType::File,
                oid: Some(entry.cas_hash.clone()),
                size: entry.size,
                path: rel_path.to_string(),
            })
        })
        .collect()
}

fn non_recursive_entries(entries: &[FileEntry], tree_path: &str) -> Vec<TreeListingEntry> {
    let mut tree_entries = Vec::new();

    for dir in infer_directories(entries, tree_path) {
        tree_entries.push(TreeListingEntry {
            entry_type: TreeListingEntryType::Directory,
            oid: None,
            size: 0,
            path: join_tree_path(tree_path, &dir),
        });
    }

    for entry in entries {
        let Some(rel_path) = strip_tree_prefix(&entry.path, tree_path) else {
            continue;
        };
        if !rel_path.is_empty() && !rel_path.contains('/') {
            tree_entries.push(TreeListingEntry {
                entry_type: TreeListingEntryType::File,
                oid: Some(entry.cas_hash.clone()),
                size: entry.size,
                path: entry.path.clone(),
            });
        }
    }

    tree_entries
}

async fn resolve_revision_id(
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
    use std::sync::Arc;

    use crate::metadata::{FileEntry, MetadataStore, RepoType, Revision, SqliteMetadataStore};

    use super::{TreeListRequest, TreeListingEntryType, TreeService, TreeServiceError};

    async fn metadata() -> Arc<dyn MetadataStore> {
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap())
    }

    async fn add_revision(metadata: &dyn MetadataStore, repo_id: i64, commit_id: &str) {
        metadata
            .add_revision(Revision {
                commit_id: commit_id.to_string(),
                repo_id,
                parent: None,
                message: "initial".to_string(),
                author: "owner".to_string(),
                created_at: 1000,
            })
            .await
            .unwrap();
        metadata.set_head(repo_id, commit_id).await.unwrap();
    }

    fn file(repo_id: i64, commit_id: &str, path: &str, size: u64, hash: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            repo_id,
            commit_id: commit_id.to_string(),
            size,
            cas_hash: hash.to_string(),
            is_lfs: true,
        }
    }

    #[tokio::test]
    async fn non_recursive_listing_infers_directories_and_current_level_files() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123").await;
        metadata
            .add_file_entries(vec![
                file(repo.id, "abc123", "README.md", 10, "readme_hash"),
                file(repo.id, "abc123", "models/model.bin", 100, "model_hash"),
                file(repo.id, "abc123", "models/sub/a.bin", 1, "nested_hash"),
            ])
            .await
            .unwrap();

        let service = TreeService::new(metadata);
        let entries = service
            .list_tree(TreeListRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                tree_path: "",
                recursive: false,
            })
            .await
            .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, TreeListingEntryType::File);
        assert_eq!(entries[0].path, "README.md");
        assert_eq!(entries[0].oid.as_deref(), Some("readme_hash"));
        assert_eq!(entries[0].size, 10);
        assert_eq!(entries[1].entry_type, TreeListingEntryType::Directory);
        assert_eq!(entries[1].path, "models");
        assert_eq!(entries[1].oid, None);
        assert_eq!(entries[1].size, 0);
    }

    #[tokio::test]
    async fn recursive_listing_returns_relative_files_below_prefix() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123").await;
        metadata
            .add_file_entries(vec![
                file(repo.id, "abc123", "README.md", 10, "readme_hash"),
                file(repo.id, "abc123", "models/config.json", 2, "config_hash"),
                file(repo.id, "abc123", "models/sub/a.bin", 1, "nested_hash"),
            ])
            .await
            .unwrap();

        let service = TreeService::new(metadata);
        let entries = service
            .list_tree(TreeListRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                tree_path: "models",
                recursive: true,
            })
            .await
            .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, TreeListingEntryType::File);
        assert_eq!(entries[0].path, "config.json");
        assert_eq!(entries[0].oid.as_deref(), Some("config_hash"));
        assert_eq!(entries[1].entry_type, TreeListingEntryType::File);
        assert_eq!(entries[1].path, "sub/a.bin");
        assert_eq!(entries[1].oid.as_deref(), Some("nested_hash"));
    }

    #[tokio::test]
    async fn recursive_listing_respects_prefix_boundaries() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123").await;
        metadata
            .add_file_entries(vec![
                file(repo.id, "abc123", "models/a.bin", 1, "a_hash"),
                file(repo.id, "abc123", "models2/b.bin", 2, "b_hash"),
            ])
            .await
            .unwrap();

        let service = TreeService::new(metadata);
        let entries = service
            .list_tree(TreeListRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                tree_path: "models",
                recursive: true,
            })
            .await
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a.bin");
        assert_eq!(entries[0].oid.as_deref(), Some("a_hash"));
    }

    #[tokio::test]
    async fn private_repo_read_by_non_owner_is_not_found() {
        let metadata = metadata().await;
        let repo = metadata
            .create_repo("owner", "secret", RepoType::Model, true)
            .await
            .unwrap();
        add_revision(metadata.as_ref(), repo.id, "abc123").await;

        let service = TreeService::new(metadata);
        let err = service
            .list_tree(TreeListRequest {
                username: "attacker",
                namespace: "owner",
                repo_name: "secret",
                repo_type: RepoType::Model,
                revision: "main",
                tree_path: "",
                recursive: false,
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            TreeServiceError::NotFound("Repository not found".to_string())
        );
    }

    #[tokio::test]
    async fn missing_revision_returns_not_found() {
        let metadata = metadata().await;
        metadata
            .create_repo("owner", "repo", RepoType::Model, false)
            .await
            .unwrap();

        let service = TreeService::new(metadata);
        let err = service
            .list_tree(TreeListRequest {
                username: "owner",
                namespace: "owner",
                repo_name: "repo",
                repo_type: RepoType::Model,
                revision: "main",
                tree_path: "",
                recursive: false,
            })
            .await
            .unwrap_err();

        assert_eq!(
            err,
            TreeServiceError::NotFound("No HEAD found for repo".to_string())
        );
    }
}
