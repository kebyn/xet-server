//! Tests for the StorageStateManager trait and SQLite implementation.

use tempfile::tempdir;
use xet_server::state::{SqliteStateManager, StateError, StorageState, StorageStateManager};

/// Test that registering a raw blob works and get_state returns RawOnly.
#[tokio::test]
async fn test_register_raw_blob() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "abc123def456";
    let size = 1024u64;

    // Register a raw blob
    manager.register_raw_blob(oid, size).await.unwrap();

    // Get state and verify
    let state = manager.get_state(oid).await.unwrap();
    assert!(state.is_some());

    let file_state = state.unwrap();
    assert_eq!(file_state.state, StorageState::RawOnly);
    assert_eq!(file_state.size, size);
    assert_eq!(file_state.sha256, oid); // sha256 is the oid
    assert!(file_state.xet_file_id.is_none());
    assert!(file_state.converted_at.is_none());
}

/// Test that marking a raw blob as converted works.
#[tokio::test]
async fn test_mark_converted() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "abc123def456";
    let size = 2048u64;
    let file_id = "xet_file_001";

    // Register a raw blob
    manager.register_raw_blob(oid, size).await.unwrap();

    // Mark as converted
    manager.mark_converted(oid, file_id).await.unwrap();

    // Get state and verify
    let state = manager.get_state(oid).await.unwrap();
    assert!(state.is_some());

    let file_state = state.unwrap();
    assert_eq!(file_state.state, StorageState::XetOnly);
    assert_eq!(file_state.size, size);
    assert_eq!(file_state.xet_file_id, Some(file_id.to_string()));
    assert!(file_state.converted_at.is_some());
}

/// Test that get_state for unknown OID returns None.
#[tokio::test]
async fn test_get_nonexistent() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "nonexistent_oid";

    // Get state for non-existent OID
    let state = manager.get_state(oid).await.unwrap();
    assert!(state.is_none());
}

/// Test batch query with mix of existing and non-existing OIDs.
#[tokio::test]
async fn test_get_states_batch() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid1 = "oid1";
    let oid2 = "oid2";
    let oid3 = "oid3";

    // Register oid1 as raw and oid2 as xet_only
    manager.register_raw_blob(oid1, 100).await.unwrap();
    manager
        .register_xet_only(oid2, "xet_file_2", 200)
        .await
        .unwrap();

    // Batch query
    let results = manager
        .get_states(&[oid1.to_string(), oid2.to_string(), oid3.to_string()])
        .await
        .unwrap();

    assert_eq!(results.len(), 3);

    // Check oid1 (raw_only)
    let (returned_oid, state) = &results[0];
    assert_eq!(returned_oid, oid1);
    assert!(state.is_some());
    let file_state = state.as_ref().unwrap();
    assert_eq!(file_state.state, StorageState::RawOnly);
    assert_eq!(file_state.size, 100);

    // Check oid2 (xet_only)
    let (returned_oid, state) = &results[1];
    assert_eq!(returned_oid, oid2);
    assert!(state.is_some());
    let file_state = state.as_ref().unwrap();
    assert_eq!(file_state.state, StorageState::XetOnly);
    assert_eq!(file_state.xet_file_id, Some("xet_file_2".to_string()));
    assert_eq!(file_state.size, 200);

    // Check oid3 (non-existent)
    let (returned_oid, state) = &results[2];
    assert_eq!(returned_oid, oid3);
    assert!(state.is_none());
}

/// Test direct xet registration.
#[tokio::test]
async fn test_register_xet_only() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "xet_oid_123";
    let file_id = "xet_file_abc";
    let size = 4096u64;

    // Register as xet_only
    manager
        .register_xet_only(oid, file_id, size)
        .await
        .unwrap();

    // Get state and verify
    let state = manager.get_state(oid).await.unwrap();
    assert!(state.is_some());

    let file_state = state.unwrap();
    assert_eq!(file_state.state, StorageState::XetOnly);
    assert_eq!(file_state.size, size);
    assert_eq!(file_state.xet_file_id, Some(file_id.to_string()));
    assert!(file_state.converted_at.is_some());
}

/// Test that registering same OID twice doesn't error (idempotent).
#[tokio::test]
async fn test_idempotent_register() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "idempotent_oid";
    let size = 512u64;

    // Register same blob twice
    manager.register_raw_blob(oid, size).await.unwrap();
    manager.register_raw_blob(oid, size).await.unwrap();

    // Should still have one entry with correct data
    let state = manager.get_state(oid).await.unwrap();
    assert!(state.is_some());

    let file_state = state.unwrap();
    assert_eq!(file_state.state, StorageState::RawOnly);
    assert_eq!(file_state.size, size);
}

/// Test that mark_converted errors when OID not found.
#[tokio::test]
async fn test_mark_converted_nonexistent() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "nonexistent";
    let file_id = "xet_file_xyz";

    // Try to mark non-existent blob as converted
    let result = manager.mark_converted(oid, file_id).await;
    assert!(result.is_err());

    match result {
        Err(StateError::Database(msg)) => {
            assert!(msg.contains("not found"));
        }
        _ => panic!("Expected Database error"),
    }
}

/// Test with file-based SQLite database.
#[tokio::test]
async fn test_file_based_database() {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("state.db");
    let db_path_str = db_path.to_str().unwrap();

    let manager = SqliteStateManager::new(db_path_str).unwrap();

    let oid = "file_based_oid";
    let size = 1024u64;

    manager.register_raw_blob(oid, size).await.unwrap();

    let state = manager.get_state(oid).await.unwrap();
    assert!(state.is_some());

    let file_state = state.unwrap();
    assert_eq!(file_state.state, StorageState::RawOnly);
    assert_eq!(file_state.size, size);
}

/// Test that register_xet_only replaces existing raw blob.
#[tokio::test]
async fn test_xet_only_replaces_raw() {
    let manager = SqliteStateManager::new_in_memory().unwrap();

    let oid = "replacement_oid";
    let raw_size = 100u64;
    let xet_size = 200u64;
    let file_id = "xet_file_new";

    // First register as raw
    manager.register_raw_blob(oid, raw_size).await.unwrap();
    let state = manager.get_state(oid).await.unwrap().unwrap();
    assert_eq!(state.state, StorageState::RawOnly);
    assert!(state.xet_file_id.is_none());

    // Now register as xet_only (should replace)
    manager
        .register_xet_only(oid, file_id, xet_size)
        .await
        .unwrap();

    let state = manager.get_state(oid).await.unwrap().unwrap();
    assert_eq!(state.state, StorageState::XetOnly);
    assert_eq!(state.xet_file_id, Some(file_id.to_string()));
    assert_eq!(state.size, xet_size);
}