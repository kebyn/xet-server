//! Tests for storage backends

use bytes::Bytes;
use tempfile::tempdir;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

#[tokio::test]
async fn test_local_storage_put_get() {
    let dir = tempdir().unwrap();
    let storage = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();

    let key = "test/object.bin";
    let data = Bytes::from("hello world");

    storage.put(key, data.clone()).await.unwrap();
    let retrieved = storage.get(key).await.unwrap();

    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn test_local_storage_exists() {
    let dir = tempdir().unwrap();
    let storage = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();

    let key = "test/exists.bin";
    let data = Bytes::from("test data");

    assert!(!storage.exists(key).await.unwrap());
    storage.put(key, data).await.unwrap();
    assert!(storage.exists(key).await.unwrap());
}

#[tokio::test]
async fn test_local_storage_delete() {
    let dir = tempdir().unwrap();
    let storage = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();

    let key = "test/delete.bin";
    let data = Bytes::from("to delete");

    storage.put(key, data).await.unwrap();
    assert!(storage.exists(key).await.unwrap());

    storage.delete(key).await.unwrap();
    assert!(!storage.exists(key).await.unwrap());
}

#[tokio::test]
async fn test_local_storage_nested_paths() {
    let dir = tempdir().unwrap();
    let storage = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();

    let key = "deeply/nested/path/object.bin";
    let data = Bytes::from("nested data");

    storage.put(key, data.clone()).await.unwrap();
    let retrieved = storage.get(key).await.unwrap();

    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn test_local_storage_not_found() {
    let dir = tempdir().unwrap();
    let storage = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();

    let result = storage.get("nonexistent/file.bin").await;
    assert!(matches!(result, Err(xet_server::storage::StorageError::NotFound(_))));
}
