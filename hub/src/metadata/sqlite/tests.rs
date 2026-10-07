//! Behavior tests for the sqlite store (transactions, guards, corruption).

use super::*;
use crate::metadata::{FileEntry, MetadataStore, RepoType};

#[tokio::test]
async fn test_get_file_tree_prefix_respects_path_boundary() {
    let store = SqliteMetadataStore::in_memory().await.unwrap();
    let repo = store
        .create_repo("ns", "repo", RepoType::Model, false)
        .await
        .unwrap();
    let commit_id = "c1";
    store
        .add_revision(Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "Initial".to_string(),
            author: "ns".to_string(),
            created_at: 1000,
        })
        .await
        .unwrap();
    store
        .add_file_entries(vec![
            FileEntry {
                path: "models/a.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 1,
                cas_hash: "h1".to_string(),
                is_lfs: true,
            },
            FileEntry {
                path: "models2/b.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 1,
                cas_hash: "h2".to_string(),
                is_lfs: true,
            },
        ])
        .await
        .unwrap();

    let entries = store
        .get_file_tree_prefix(repo.id, commit_id, "models")
        .await
        .unwrap();
    let paths: Vec<_> = entries.into_iter().map(|e| e.path).collect();
    assert_eq!(paths, vec!["models/a.bin".to_string()]);
}

#[tokio::test]
async fn test_negative_stored_file_size_is_reported_as_corruption() {
    let store = SqliteMetadataStore::in_memory().await.unwrap();
    let repo = store
        .create_repo("ns", "negative-size", RepoType::Model, false)
        .await
        .unwrap();
    let revision = Revision {
        commit_id: "negative-size-commit".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "corrupt fixture".to_string(),
        author: "ns".to_string(),
        created_at: 1,
    };
    store.add_revision(revision).await.unwrap();
    sqlx::query(
        "INSERT INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind("bad.bin")
    .bind(repo.id)
    .bind("negative-size-commit")
    .bind(-1_i64)
    .bind("bad-hash")
    .bind(1_i64)
    .execute(&store.pool)
    .await
    .unwrap();

    let err = store
        .get_file_tree(repo.id, "negative-size-commit")
        .await
        .expect_err("negative SQLite size must not become a huge u64");

    assert!(matches!(err, MetadataError::Corruption(_)));
    assert!(err.to_string().contains("negative size"));
}

#[tokio::test]
async fn test_commit_atomic_rejects_file_size_above_sqlite_range() {
    let store = SqliteMetadataStore::in_memory().await.unwrap();
    let repo = store
        .create_repo("ns", "oversized", RepoType::Model, false)
        .await
        .unwrap();
    let revision = Revision {
        commit_id: "oversized-commit".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "oversized".to_string(),
        author: "ns".to_string(),
        created_at: 1,
    };
    let entry = FileEntry {
        path: "huge.bin".to_string(),
        repo_id: repo.id,
        commit_id: revision.commit_id.clone(),
        size: u64::MAX,
        cas_hash: "hash".to_string(),
        is_lfs: true,
    };

    let err = store
        .commit_atomic(&revision, &[entry], None)
        .await
        .expect_err("u64 size above i64::MAX must be rejected");

    assert!(matches!(err, MetadataError::InvalidOperation(_)));
    assert_eq!(store.get_head(repo.id).await.unwrap(), None);
    assert!(matches!(
        store.get_revision(repo.id, &revision.commit_id).await,
        Err(MetadataError::RevisionNotFound(_))
    ));
}

#[tokio::test]
async fn commit_changes_copies_parent_and_applies_ordered_delta_atomically() {
    let store = SqliteMetadataStore::in_memory().await.unwrap();
    let repo = store
        .create_repo("ns", "delta", RepoType::Model, false)
        .await
        .unwrap();
    let parent = Revision {
        commit_id: "parent".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "parent".to_string(),
        author: "ns".to_string(),
        created_at: 1,
    };
    let parent_entries = vec![
        FileEntry {
            path: "keep.bin".to_string(),
            repo_id: repo.id,
            commit_id: parent.commit_id.clone(),
            size: 1,
            cas_hash: "keep-v1".to_string(),
            is_lfs: true,
        },
        FileEntry {
            path: "remove.bin".to_string(),
            repo_id: repo.id,
            commit_id: parent.commit_id.clone(),
            size: 2,
            cas_hash: "remove".to_string(),
            is_lfs: true,
        },
    ];
    store
        .commit_atomic(&parent, &parent_entries, None)
        .await
        .unwrap();

    let child = Revision {
        commit_id: "child".to_string(),
        repo_id: repo.id,
        parent: Some(parent.commit_id.clone()),
        message: "child".to_string(),
        author: "ns".to_string(),
        created_at: 2,
    };
    let changes = vec![
        FileTreeChange::Delete("remove.bin".to_string()),
        FileTreeChange::Upsert(FileEntry {
            path: "keep.bin".to_string(),
            repo_id: -1,
            commit_id: "ignored".to_string(),
            size: 3,
            cas_hash: "keep-v2".to_string(),
            is_lfs: false,
        }),
        FileTreeChange::Delete("keep.bin".to_string()),
        FileTreeChange::Upsert(FileEntry {
            path: "keep.bin".to_string(),
            repo_id: -1,
            commit_id: "ignored".to_string(),
            size: 4,
            cas_hash: "keep-final".to_string(),
            is_lfs: true,
        }),
    ];
    store
        .commit_changes_atomic(&child, &changes, Some("parent"))
        .await
        .unwrap();

    let parent_tree = store.get_file_tree(repo.id, "parent").await.unwrap();
    assert_eq!(parent_tree.len(), 2);
    let child_tree = store.get_file_tree(repo.id, "child").await.unwrap();
    assert_eq!(child_tree.len(), 1);
    assert_eq!(child_tree[0].path, "keep.bin");
    assert_eq!(child_tree[0].repo_id, repo.id);
    assert_eq!(child_tree[0].commit_id, "child");
    assert_eq!(child_tree[0].size, 4);
    assert_eq!(child_tree[0].cas_hash, "keep-final");
    assert_eq!(
        store.get_head(repo.id).await.unwrap().as_deref(),
        Some("child")
    );
}

#[tokio::test]
async fn commit_changes_failure_rolls_back_copied_snapshot_revision_and_head() {
    let store = SqliteMetadataStore::in_memory().await.unwrap();
    let repo = store
        .create_repo("ns", "delta-rollback", RepoType::Model, false)
        .await
        .unwrap();
    let parent = Revision {
        commit_id: "parent".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "parent".to_string(),
        author: "ns".to_string(),
        created_at: 1,
    };
    let parent_entry = FileEntry {
        path: "keep.bin".to_string(),
        repo_id: repo.id,
        commit_id: parent.commit_id.clone(),
        size: 1,
        cas_hash: "keep".to_string(),
        is_lfs: true,
    };
    store
        .commit_atomic(&parent, &[parent_entry], None)
        .await
        .unwrap();

    let child = Revision {
        commit_id: "child".to_string(),
        repo_id: repo.id,
        parent: Some(parent.commit_id.clone()),
        message: "child".to_string(),
        author: "ns".to_string(),
        created_at: 2,
    };
    let changes = [FileTreeChange::Upsert(FileEntry {
        path: "too-large.bin".to_string(),
        repo_id: repo.id,
        commit_id: child.commit_id.clone(),
        size: u64::MAX,
        cas_hash: "large".to_string(),
        is_lfs: true,
    })];
    let error = store
        .commit_changes_atomic(&child, &changes, Some("parent"))
        .await
        .expect_err("invalid delta must roll back the complete commit transaction");
    assert!(matches!(error, MetadataError::InvalidOperation(_)));

    assert_eq!(
        store.get_head(repo.id).await.unwrap().as_deref(),
        Some("parent")
    );
    assert!(matches!(
        store.get_revision(repo.id, "child").await,
        Err(MetadataError::RevisionNotFound(_))
    ));
    assert!(
        store
            .get_file_tree(repo.id, "child")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.get_file_tree(repo.id, "parent").await.unwrap().len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_commit_rolls_back_before_connection_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let database_path = dir.path().join("cancelled-commit.db");
    let store = std::sync::Arc::new(
        SqliteMetadataStore::new(database_path.to_str().unwrap(), 1)
            .await
            .unwrap(),
    );
    let repo = store
        .create_repo("ns", "cancelled", RepoType::Model, false)
        .await
        .unwrap();

    sqlx::query(
        "CREATE TRIGGER slow_file_insert BEFORE INSERT ON file_tree BEGIN \
         SELECT length(randomblob(64000000)); END",
    )
    .execute(&store.pool)
    .await
    .unwrap();

    let revision = Revision {
        commit_id: "cancelled-commit".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "cancelled".to_string(),
        author: "ns".to_string(),
        created_at: 1,
    };
    let entries = vec![FileEntry {
        path: "slow.bin".to_string(),
        repo_id: repo.id,
        commit_id: revision.commit_id.clone(),
        size: 1,
        cas_hash: "hash".to_string(),
        is_lfs: false,
    }];

    let task_store = store.clone();
    let task =
        tokio::spawn(async move { task_store.commit_atomic(&revision, &entries, None).await });

    while store.pool.num_idle() != 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert!(
        !task.is_finished(),
        "test trigger must keep the commit in flight"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    sqlx::query("DROP TRIGGER slow_file_insert")
        .execute(&store.pool)
        .await
        .unwrap();

    let replacement = Revision {
        commit_id: "replacement-commit".to_string(),
        repo_id: repo.id,
        parent: None,
        message: "replacement".to_string(),
        author: "ns".to_string(),
        created_at: 2,
    };
    store
        .commit_atomic(&replacement, &[], None)
        .await
        .expect("a cancelled commit must not poison the pooled connection");

    assert_eq!(
        store.get_head(repo.id).await.unwrap().as_deref(),
        Some("replacement-commit")
    );
    assert!(
        store
            .get_revision(repo.id, "cancelled-commit")
            .await
            .is_err()
    );
}
