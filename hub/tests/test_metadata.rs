use hub_api::metadata::{FileEntry, MetadataStore, RepoType, Revision, SqliteMetadataStore};

fn create_test_store() -> SqliteMetadataStore {
    SqliteMetadataStore::in_memory().expect("Failed to create in-memory store")
}

#[tokio::test]
async fn test_create_and_get_repo() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    assert_eq!(repo.namespace, "testuser");
    assert_eq!(repo.name, "testrepo");
    assert_eq!(repo.repo_type, RepoType::Model);
    assert!(!repo.private);

    // Get the repo back
    let fetched = store
        .get_repo("testuser", "testrepo", RepoType::Model)
        .await
        .expect("Failed to get repo");

    assert_eq!(fetched.id, repo.id);
    assert_eq!(fetched.namespace, "testuser");
    assert_eq!(fetched.name, "testrepo");
}

#[tokio::test]
async fn test_create_duplicate_repo() {
    let store = create_test_store();

    store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Try to create the same repo again
    let result = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(
        err,
        hub_api::metadata::MetadataError::RepoAlreadyExists(_)
    ));
}

#[tokio::test]
async fn test_delete_repo() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Delete the repo
    store.delete_repo(repo.id).await.expect("Failed to delete repo");

    // Try to get the deleted repo
    let result = store.get_repo("testuser", "testrepo", RepoType::Model).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        hub_api::metadata::MetadataError::RepoNotFound(_)
    ));
}

#[tokio::test]
async fn test_add_and_get_revision() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    let revision = Revision {
        commit_id: "abc123".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "Initial commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1234567890,
    };

    store
        .add_revision(revision.clone())
        .await
        .expect("Failed to add revision");

    let fetched = store
        .get_revision(repo.id, "abc123")
        .await
        .expect("Failed to get revision");

    assert_eq!(fetched.commit_id, "abc123");
    assert_eq!(fetched.repo_id, repo.id);
    assert_eq!(fetched.message, "Initial commit");
    assert_eq!(fetched.author, "testuser");
}

#[tokio::test]
async fn test_head_management() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Initially no head
    let head = store.get_head(repo.id).await.expect("Failed to get head");
    assert!(head.is_none());

    // Add a revision and set as head
    let revision = Revision {
        commit_id: "abc123".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "Initial commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1234567890,
    };
    store.add_revision(revision).await.expect("Failed to add revision");

    store
        .set_head(repo.id, "abc123")
        .await
        .expect("Failed to set head");

    let head = store.get_head(repo.id).await.expect("Failed to get head");
    assert_eq!(head, Some("abc123".to_string()));

    // Update head to a new commit
    let revision2 = Revision {
        commit_id: "def456".to_string(),
        repo_id: repo.id,
        parent: Some("abc123".to_string()),
        message: "Second commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1234567891,
    };
    store.add_revision(revision2).await.expect("Failed to add revision");

    store
        .set_head(repo.id, "def456")
        .await
        .expect("Failed to update head");

    let head = store.get_head(repo.id).await.expect("Failed to get head");
    assert_eq!(head, Some("def456".to_string()));
}

#[tokio::test]
async fn test_file_tree_operations() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Add a revision
    let revision = Revision {
        commit_id: "abc123".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "Initial commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1234567890,
    };
    store.add_revision(revision).await.expect("Failed to add revision");

    // Add file entries
    let entries = vec![
        FileEntry {
            path: "model.bin".to_string(),
            repo_id: repo.id,
            commit_id: "abc123".to_string(),
            size: 1024,
            cas_hash: "hash1".to_string(),
            is_lfs: true,
        },
        FileEntry {
            path: "config.json".to_string(),
            repo_id: repo.id,
            commit_id: "abc123".to_string(),
            size: 256,
            cas_hash: "hash2".to_string(),
            is_lfs: false,
        },
        FileEntry {
            path: "README.md".to_string(),
            repo_id: repo.id,
            commit_id: "abc123".to_string(),
            size: 128,
            cas_hash: "hash3".to_string(),
            is_lfs: false,
        },
    ];

    store
        .add_file_entries(entries)
        .await
        .expect("Failed to add file entries");

    // Get file tree
    let tree = store
        .get_file_tree(repo.id, "abc123")
        .await
        .expect("Failed to get file tree");

    assert_eq!(tree.len(), 3);

    // Verify files are sorted by path
    assert_eq!(tree[0].path, "README.md");
    assert_eq!(tree[1].path, "config.json");
    assert_eq!(tree[2].path, "model.bin");
}

#[tokio::test]
async fn test_file_tree_prefix_filter() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Add a revision
    let revision = Revision {
        commit_id: "abc123".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "Initial commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1234567890,
    };
    store.add_revision(revision).await.expect("Failed to add revision");

    // Add file entries with nested paths
    let entries = vec![
        FileEntry {
            path: "models/model.bin".to_string(),
            repo_id: repo.id,
            commit_id: "abc123".to_string(),
            size: 1024,
            cas_hash: "hash1".to_string(),
            is_lfs: true,
        },
        FileEntry {
            path: "models/config.json".to_string(),
            repo_id: repo.id,
            commit_id: "abc123".to_string(),
            size: 256,
            cas_hash: "hash2".to_string(),
            is_lfs: false,
        },
        FileEntry {
            path: "data/train.csv".to_string(),
            repo_id: repo.id,
            commit_id: "abc123".to_string(),
            size: 512,
            cas_hash: "hash3".to_string(),
            is_lfs: false,
        },
    ];

    store
        .add_file_entries(entries)
        .await
        .expect("Failed to add file entries");

    // Filter by prefix
    let models = store
        .get_file_tree_prefix(repo.id, "abc123", "models/")
        .await
        .expect("Failed to get file tree prefix");

    assert_eq!(models.len(), 2);
    assert!(models.iter().all(|f| f.path.starts_with("models/")));

    let data = store
        .get_file_tree_prefix(repo.id, "abc123", "data/")
        .await
        .expect("Failed to get file tree prefix");

    assert_eq!(data.len(), 1);
    assert_eq!(data[0].path, "data/train.csv");
}

#[tokio::test]
async fn test_resolve_file() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Add a revision
    let revision = Revision {
        commit_id: "abc123".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "Initial commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1234567890,
    };
    store.add_revision(revision).await.expect("Failed to add revision");

    // Add file entry
    let entries = vec![FileEntry {
        path: "model.bin".to_string(),
        repo_id: repo.id,
        commit_id: "abc123".to_string(),
        size: 1024,
        cas_hash: "hash123".to_string(),
        is_lfs: true,
    }];

    store
        .add_file_entries(entries)
        .await
        .expect("Failed to add file entries");

    // Resolve file
    let file = store
        .resolve_file(repo.id, "abc123", "model.bin")
        .await
        .expect("Failed to resolve file");

    assert_eq!(file.path, "model.bin");
    assert_eq!(file.size, 1024);
    assert_eq!(file.cas_hash, "hash123");
    assert!(file.is_lfs);

    // Try to resolve non-existent file
    let result = store.resolve_file(repo.id, "abc123", "nonexistent.bin").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        hub_api::metadata::MetadataError::FileNotFound(_)
    ));
}

#[tokio::test]
async fn test_commit_log() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Create a chain of commits: c1 <- c2 <- c3
    let c1 = Revision {
        commit_id: "commit1".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "First commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1000,
    };
    store.add_revision(c1).await.unwrap();

    let c2 = Revision {
        commit_id: "commit2".to_string(),
        repo_id: repo.id,
        parent: Some("commit1".to_string()),
        message: "Second commit".to_string(),
        author: "testuser".to_string(),
        created_at: 2000,
    };
    store.add_revision(c2).await.unwrap();

    let c3 = Revision {
        commit_id: "commit3".to_string(),
        repo_id: repo.id,
        parent: Some("commit2".to_string()),
        message: "Third commit".to_string(),
        author: "testuser".to_string(),
        created_at: 3000,
    };
    store.add_revision(c3).await.unwrap();

    // Set HEAD to c3
    store.set_head(repo.id, "commit3").await.unwrap();

    // Get commit log
    let log = store.get_commit_log(repo.id, None).await.expect("Failed to get commit log");
    assert_eq!(log.len(), 3);
    assert_eq!(log[0].commit_id, "commit3");
    assert_eq!(log[1].commit_id, "commit2");
    assert_eq!(log[2].commit_id, "commit1");

    // Test limit
    let log_limited = store.get_commit_log(repo.id, Some(2)).await.expect("Failed to get commit log");
    assert_eq!(log_limited.len(), 2);
    assert_eq!(log_limited[0].commit_id, "commit3");
    assert_eq!(log_limited[1].commit_id, "commit2");
}

#[tokio::test]
async fn test_commit_atomic_success() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // First commit with no parent (expected_parent = None, HEAD is None)
    let rev = Revision {
        commit_id: "commit1".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "First commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1000,
    };
    let entries = vec![
        FileEntry {
            path: "file1.txt".to_string(),
            repo_id: repo.id,
            commit_id: "commit1".to_string(),
            size: 100,
            cas_hash: "hash1".to_string(),
            is_lfs: false,
        },
    ];

    // Should succeed since HEAD is None and expected_parent is None
    store.commit_atomic(&rev, &entries, None).await.expect("commit_atomic failed");

    // Verify HEAD was set
    let head = store.get_head(repo.id).await.expect("Failed to get head");
    assert_eq!(head, Some("commit1".to_string()));

    // Verify file entry was added
    let tree = store.get_file_tree(repo.id, "commit1").await.expect("Failed to get tree");
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].path, "file1.txt");
}

#[tokio::test]
async fn test_commit_atomic_rejects_mismatched_parent() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Create initial commit and set HEAD
    let rev1 = Revision {
        commit_id: "commit1".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "First commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1000,
    };
    store.add_revision(rev1).await.unwrap();
    store.set_head(repo.id, "commit1").await.unwrap();

    // Try to commit with wrong expected_parent
    let rev2 = Revision {
        commit_id: "commit2".to_string(),
        repo_id: repo.id,
        parent: Some("commit1".to_string()),
        message: "Second commit".to_string(),
        author: "testuser".to_string(),
        created_at: 2000,
    };
    let entries = vec![];

    // Should fail since HEAD is "commit1" but expected_parent is "wrong_parent"
    let result = store.commit_atomic(&rev2, &entries, Some("wrong_parent")).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, hub_api::metadata::MetadataError::Conflict(_)));

    // Verify the conflict contains the actual HEAD
    if let hub_api::metadata::MetadataError::Conflict(actual_head) = err {
        assert_eq!(actual_head, "commit1");
    }

    // Verify HEAD wasn't changed
    let head = store.get_head(repo.id).await.expect("Failed to get head");
    assert_eq!(head, Some("commit1".to_string()));
}

#[tokio::test]
async fn test_commit_atomic_concurrent_protection() {
    let store = create_test_store();

    let repo = store
        .create_repo("testuser", "testrepo", RepoType::Model, false)
        .await
        .expect("Failed to create repo");

    // Create initial commit atomically
    let rev1 = Revision {
        commit_id: "commit1".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "First commit".to_string(),
        author: "testuser".to_string(),
        created_at: 1000,
    };
    store.commit_atomic(&rev1, &[], None).await.expect("First commit failed");

    // First concurrent attempt with correct parent
    let rev2a = Revision {
        commit_id: "commit2a".to_string(),
        repo_id: repo.id,
        parent: Some("commit1".to_string()),
        message: "Concurrent A".to_string(),
        author: "testuser".to_string(),
        created_at: 2000,
    };

    // This should succeed
    store.commit_atomic(&rev2a, &[], Some("commit1")).await.expect("commit_atomic should succeed");

    // Second concurrent attempt with same parent (now stale)
    let rev2b = Revision {
        commit_id: "commit2b".to_string(),
        repo_id: repo.id,
        parent: Some("commit1".to_string()),
        message: "Concurrent B".to_string(),
        author: "testuser".to_string(),
        created_at: 2001,
    };

    // This should fail since HEAD is now "commit2a", not "commit1"
    let result = store.commit_atomic(&rev2b, &[], Some("commit1")).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), hub_api::metadata::MetadataError::Conflict(_)));

    // Verify HEAD is commit2a
    let head = store.get_head(repo.id).await.expect("Failed to get head");
    assert_eq!(head, Some("commit2a".to_string()));
}