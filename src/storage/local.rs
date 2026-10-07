//! Local filesystem storage backend

use super::{ObjectKeyStream, StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::path::{Component, Path, PathBuf};
use tokio::fs;

use crate::util::TempPathGuard;

/// 跨文件系统安全拷贝:先 copy 到临时文件,再原子 rename 到最终路径。
/// 避免中断时在最终 key 留下截断文件。
async fn copy_then_rename(source: &Path, dest: &Path) -> StorageResult<()> {
    let temp_dest = TempPathGuard::new(unique_temp_path(dest));
    fs::copy(source, temp_dest.path()).await.map_err(|e| {
        StorageError::internal_with_source(
            format!(
                "Failed to copy {} → {}",
                source.display(),
                temp_dest.path().display()
            ),
            e,
        )
    })?;
    fs::rename(temp_dest.path(), dest).await.map_err(|e| {
        StorageError::internal_with_source(
            format!(
                "Failed to rename {} → {}",
                temp_dest.path().display(),
                dest.display()
            ),
            e,
        )
    })?;
    Ok(())
}

fn unique_temp_path(dest: &Path) -> PathBuf {
    dest.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()))
}

fn ensure_resolved_within_base(base_path: &Path, resolved: PathBuf) -> StorageResult<PathBuf> {
    if resolved.starts_with(base_path) {
        Ok(resolved)
    } else {
        Err(StorageError::InvalidArgument(
            "Object key resolves outside the configured local storage root".to_string(),
        ))
    }
}

async fn reject_symlink_components(base_path: &Path, path: &Path) -> StorageResult<()> {
    match fs::symlink_metadata(base_path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StorageError::InvalidArgument(
                "Configured local storage root is no longer a directory".to_string(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::InvalidArgument(
                "Configured local storage root no longer exists".to_string(),
            ));
        }
        Err(error) => {
            return Err(StorageError::internal_with_source(
                format!(
                    "Failed to inspect local storage root {}",
                    base_path.display()
                ),
                error,
            ));
        }
    }

    let relative = path.strip_prefix(base_path).map_err(|_| {
        StorageError::InvalidArgument(
            "Object key resolves outside the configured local storage root".to_string(),
        )
    })?;
    let mut current = base_path.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StorageError::InvalidArgument(
                    "Object key contains a symbolic-link path component".to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(StorageError::internal_with_source(
                    format!("Failed to inspect local storage path {}", current.display()),
                    error,
                ));
            }
        }
    }
    Ok(())
}

async fn canonicalize_confined(base_path: &Path, path: &Path) -> StorageResult<PathBuf> {
    reject_symlink_components(base_path, path).await?;
    let resolved = fs::canonicalize(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StorageError::NotFound(path.to_string_lossy().into_owned())
        } else {
            StorageError::internal_with_source(
                format!("Failed to resolve local storage path {}", path.display()),
                error,
            )
        }
    })?;
    ensure_resolved_within_base(base_path, resolved)
}

pub struct LocalStorage {
    base_path: PathBuf,
}

impl LocalStorage {
    pub fn new(base_path: &str) -> StorageResult<Self> {
        let path = PathBuf::from(base_path);
        if path.as_os_str().is_empty() {
            return Err(StorageError::InvalidArgument(
                "Local storage path cannot be empty".to_string(),
            ));
        }
        std::fs::create_dir_all(&path).map_err(|error| {
            StorageError::internal_with_source(
                format!("Failed to create local storage root {}", path.display()),
                error,
            )
        })?;
        let base_path = std::fs::canonicalize(&path).map_err(|error| {
            StorageError::internal_with_source(
                format!("Failed to resolve local storage root {}", path.display()),
                error,
            )
        })?;
        Ok(Self { base_path })
    }

    /// Validate key and construct object path, preventing path traversal attacks.
    fn object_path(&self, key: &str) -> StorageResult<PathBuf> {
        let key_path = Path::new(key);
        if key.starts_with('/')
            || key.starts_with('\\')
            || key_path.is_absolute()
            || key_path
                .components()
                .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
        {
            return Err(StorageError::InvalidArgument(format!(
                "Invalid key: absolute path not allowed: {}",
                key
            )));
        }

        // Check for null bytes
        if key.contains('\0') {
            return Err(StorageError::InvalidArgument(
                "Invalid key: contains null bytes".to_string(),
            ));
        }

        // Check for empty key
        if key.is_empty() {
            return Err(StorageError::InvalidArgument(
                "Invalid key: empty key".to_string(),
            ));
        }

        // Reject path traversal: check each path component for ".."
        // Split on both '/' and '\\' to handle Windows-style separators.
        // This is more precise than key.contains("..") which would also reject
        // legitimate filenames like "file..name" or "..hidden".
        for component in key.split(['/', '\\']) {
            if component == ".." {
                return Err(StorageError::InvalidArgument(format!(
                    "Invalid key: path traversal detected: {}",
                    key
                )));
            }
        }

        Ok(self.base_path.join(key))
    }

    async fn prepare_parent(&self, path: &Path) -> StorageResult<()> {
        let Some(parent) = path.parent() else {
            return Err(StorageError::InvalidArgument(
                "Object key has no parent directory".to_string(),
            ));
        };
        reject_symlink_components(&self.base_path, parent).await?;
        fs::create_dir_all(parent)
            .await
            .map_err(|error| StorageError::internal_with_source("Failed to create dirs", error))?;
        canonicalize_confined(&self.base_path, parent).await?;
        Ok(())
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn health_check(&self) -> StorageResult<()> {
        reject_symlink_components(&self.base_path, &self.base_path).await
    }

    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        // 原子写:先写入临时文件,再 rename 到最终路径,避免崩溃时留下截断文件。
        let path = self.object_path(key)?;
        reject_symlink_components(&self.base_path, &path).await?;
        self.prepare_parent(&path).await?;
        let temp_path = TempPathGuard::new(unique_temp_path(&path));

        // Write to temp file
        fs::write(temp_path.path(), &data)
            .await
            .map_err(|e| StorageError::internal_with_source("Failed to write temp file", e))?;

        // Atomic rename
        fs::rename(temp_path.path(), &path)
            .await
            .map_err(|e| StorageError::internal_with_source("Failed to rename temp to final", e))?;

        Ok(())
    }

    /// Store an object by moving a file from disk.
    /// Tries atomic rename first (zero-copy on same filesystem).
    /// Falls back to copy+delete on cross-filesystem.
    async fn put_from_path(&self, key: &str, source: &Path) -> StorageResult<()> {
        let dest = self.object_path(key)?;
        reject_symlink_components(&self.base_path, &dest).await?;
        self.prepare_parent(&dest).await?;

        // Try atomic rename first (same filesystem → zero-copy)
        match fs::rename(source, &dest).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // Cross-filesystem: copy to temp + rename (atomic), then delete source.
                copy_then_rename(source, &dest).await?;
                let _ = fs::remove_file(source).await;
                Ok(())
            }
        }
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        let path = self.object_path(key)?;
        let path = canonicalize_confined(&self.base_path, &path)
            .await
            .map_err(|error| {
                if matches!(error, StorageError::NotFound(_)) {
                    StorageError::NotFound(key.to_string())
                } else {
                    error
                }
            })?;

        // Directly attempt read; map NotFound errors (avoids TOCTOU race with exists())
        match fs::read(&path).await {
            Ok(data) => Ok(Bytes::from(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::internal_with_source("Failed to read", e)),
        }
    }

    async fn get_path(&self, key: &str) -> StorageResult<Option<PathBuf>> {
        // Existing objects return their canonical confined path. Missing objects
        // keep the lexical path so callers preserve their existing File::open
        // not-found handling without following a symlink component.
        let path = self.object_path(key)?;
        reject_symlink_components(&self.base_path, &path).await?;
        match canonicalize_confined(&self.base_path, &path).await {
            Ok(path) => Ok(Some(path)),
            Err(StorageError::NotFound(_)) => Ok(Some(path)),
            Err(error) => Err(error),
        }
    }

    /// Download a local object to `dest` without routing through `get()`.
    ///
    /// This keeps shard validation bounded-memory for the default local backend:
    /// the object is copied by the filesystem into a temp file and then renamed
    /// into place, rather than being read fully into RAM.
    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
        let source = self.object_path(key)?;
        let source = canonicalize_confined(&self.base_path, &source)
            .await
            .map_err(|error| {
                if matches!(error, StorageError::NotFound(_)) {
                    StorageError::NotFound(key.to_string())
                } else {
                    error
                }
            })?;

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| StorageError::internal_with_source("Failed to create dirs", e))?;
        }

        match fs::metadata(&source).await {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                return Err(StorageError::internal(format!(
                    "Object path is not a file: {}",
                    source.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(key.to_string()));
            }
            Err(e) => {
                return Err(StorageError::internal_with_source(
                    format!("Failed to stat object {}", source.display()),
                    e,
                ));
            }
        }

        copy_then_rename(&source, dest).await
    }

    async fn exists(&self, key: &str) -> StorageResult<bool> {
        let path = self.object_path(key)?;
        reject_symlink_components(&self.base_path, &path).await?;
        match canonicalize_confined(&self.base_path, &path).await {
            Ok(_) => Ok(true),
            Err(StorageError::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        let path = self.object_path(key)?;
        let path = match canonicalize_confined(&self.base_path, &path).await {
            Ok(path) => path,
            Err(StorageError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };

        // Directly attempt delete; ignore NotFound (avoids TOCTOU race with exists())
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::internal_with_source("Failed to delete", e)),
        }
    }

    async fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.list_objects_stream(prefix).try_collect().await
    }

    fn list_objects_stream<'a>(&'a self, prefix: &'a str) -> ObjectKeyStream<'a> {
        struct LocalListState {
            base_path: PathBuf,
            root: Option<PathBuf>,
            directories: Vec<tokio::fs::ReadDir>,
        }

        let root = if prefix.is_empty() {
            self.base_path.clone()
        } else {
            match self.object_path(prefix) {
                Ok(path) => path,
                Err(error) => return stream::once(async move { Err(error) }).boxed(),
            }
        };
        let state = LocalListState {
            base_path: self.base_path.clone(),
            root: Some(root),
            directories: Vec::new(),
        };

        stream::try_unfold(state, |mut state| async move {
            if let Some(root) = state.root.take() {
                let root = match canonicalize_confined(&state.base_path, &root).await {
                    Ok(root) => root,
                    Err(StorageError::NotFound(_)) => return Ok(None),
                    Err(error) => return Err(error),
                };
                match fs::read_dir(&root).await {
                    Ok(directory) => state.directories.push(directory),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => {
                        return Err(StorageError::internal_with_source(
                            format!("Failed to read dir {}", root.display()),
                            error,
                        ));
                    }
                }
            }

            loop {
                let Some(directory) = state.directories.last_mut() else {
                    return Ok(None);
                };
                let entry = directory.next_entry().await.map_err(|error| {
                    StorageError::internal_with_source("Failed to read dir entry", error)
                })?;
                let Some(entry) = entry else {
                    state.directories.pop();
                    continue;
                };

                let path = entry.path();
                let file_type = entry.file_type().await.map_err(|error| {
                    StorageError::internal_with_source("Failed to get file type", error)
                })?;
                if file_type.is_dir() {
                    let path = match canonicalize_confined(&state.base_path, &path).await {
                        Ok(path) => path,
                        Err(StorageError::NotFound(_)) => continue,
                        Err(error) => return Err(error),
                    };
                    match fs::read_dir(&path).await {
                        Ok(directory) => state.directories.push(directory),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(StorageError::internal_with_source(
                                format!("Failed to read dir {}", path.display()),
                                error,
                            ));
                        }
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }

                let key = path
                    .strip_prefix(&state.base_path)
                    .map_err(|error| {
                        StorageError::internal_with_source("Failed to compute relative path", error)
                    })?
                    .to_string_lossy()
                    .to_string();
                return Ok(Some((key, state)));
            }
        })
        .boxed()
    }

    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        let path = self.object_path(key)?;
        let path = canonicalize_confined(&self.base_path, &path)
            .await
            .map_err(|error| {
                if matches!(error, StorageError::NotFound(_)) {
                    StorageError::NotFound(key.to_string())
                } else {
                    error
                }
            })?;

        match fs::metadata(&path).await {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::internal_with_source(
                "Failed to get metadata",
                e,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;

    #[tokio::test]
    async fn test_copy_then_rename_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("sub/dest.bin");
        tokio::fs::create_dir_all(dest.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&src, b"payload").await.unwrap();

        copy_then_rename(&src, &dest).await.unwrap();

        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"payload");
        // 不留下 .tmp 中间文件
        assert_no_tmp_files(dest.parent().unwrap());
    }

    #[tokio::test]
    async fn test_put_is_atomic_no_temp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        store
            .put("xorbs/abc", Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert_eq!(
            store.get("xorbs/abc").await.unwrap(),
            Bytes::from_static(b"data")
        );
        // 原子写不应残留 .tmp
        assert_no_tmp_files(&dir.path().join("xorbs"));
    }

    #[tokio::test]
    async fn test_concurrent_put_same_key_uses_independent_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let key = "objects/shared.bin";
        let data = Bytes::from_static(b"same content");

        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let data = data.clone();
            handles.push(tokio::spawn(async move { store.put(key, data).await }));
        }

        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        assert_eq!(store.get(key).await.unwrap(), data);
        assert_no_tmp_files(&dir.path().join("objects"));
    }

    #[tokio::test]
    async fn test_download_to_path_creates_parent_and_copies_without_tmp_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().join("store").to_str().unwrap()).unwrap();
        let data = Bytes::from_static(b"download without default get allocation");
        store.put("xorbs/object", data.clone()).await.unwrap();

        let dest = dir.path().join("downloads/nested/object.bin");
        store.download_to_path("xorbs/object", &dest).await.unwrap();

        assert_eq!(tokio::fs::read(&dest).await.unwrap(), data);
        assert_no_tmp_files(dest.parent().unwrap());
    }

    #[tokio::test]
    async fn list_objects_rejects_path_traversal_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let store = LocalStorage::new(store_path.to_str().unwrap()).unwrap();
        store
            .put("shards/inside", Bytes::from_static(b"inside"))
            .await
            .unwrap();

        let error = store
            .list_objects("../")
            .await
            .expect_err("list prefix must not escape the storage root");
        assert!(matches!(error, StorageError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn list_objects_stream_yields_nested_keys_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        store
            .put("shards/one", Bytes::from_static(b"one"))
            .await
            .unwrap();
        store
            .put("shards/nested/two", Bytes::from_static(b"two"))
            .await
            .unwrap();
        store
            .put("xorbs/ignored", Bytes::from_static(b"ignored"))
            .await
            .unwrap();

        let mut keys: Vec<String> = store
            .list_objects_stream("shards/")
            .try_collect()
            .await
            .unwrap();
        keys.sort();
        assert_eq!(keys, vec!["shards/nested/two", "shards/one"]);

        let error = store
            .list_objects_stream("../")
            .try_collect::<Vec<_>>()
            .await
            .expect_err("streaming list prefix must not escape the storage root");
        assert!(matches!(error, StorageError::InvalidArgument(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_operations_reject_symlink_escape_from_storage_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let outside_path = dir.path().join("outside");
        tokio::fs::create_dir_all(store_path.join("objects"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(&outside_path).await.unwrap();
        tokio::fs::write(outside_path.join("secret"), b"outside")
            .await
            .unwrap();
        symlink(&outside_path, store_path.join("objects/link")).unwrap();
        let store = LocalStorage::new(store_path.to_str().unwrap()).unwrap();
        let escaped_key = "objects/link/secret";

        assert!(matches!(
            store.get(escaped_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            store.get_path(escaped_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            store.get_size(escaped_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            store.exists(escaped_key).await,
            Err(StorageError::InvalidArgument(_))
        ));

        let download = dir.path().join("download");
        assert!(matches!(
            store.download_to_path(escaped_key, &download).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(!download.exists());

        assert!(matches!(
            store
                .put("objects/link/new", Bytes::from_static(b"new"))
                .await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(!outside_path.join("new").exists());

        let upload = dir.path().join("upload");
        tokio::fs::write(&upload, b"upload").await.unwrap();
        assert!(matches!(
            store.put_from_path("objects/link/moved", &upload).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(upload.exists());
        assert!(!outside_path.join("moved").exists());

        assert!(matches!(
            store.delete(escaped_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert_eq!(
            tokio::fs::read(outside_path.join("secret")).await.unwrap(),
            b"outside"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_objects_stream_rejects_symlink_prefix_outside_storage_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let outside_path = dir.path().join("outside");
        tokio::fs::create_dir_all(&store_path).await.unwrap();
        tokio::fs::create_dir_all(&outside_path).await.unwrap();
        tokio::fs::write(outside_path.join("external-shard"), b"outside")
            .await
            .unwrap();
        symlink(&outside_path, store_path.join("shards")).unwrap();
        let store = LocalStorage::new(store_path.to_str().unwrap()).unwrap();

        let error = store
            .list_objects_stream("shards/")
            .try_collect::<Vec<_>>()
            .await
            .expect_err("listing must not follow a prefix symlink outside storage");
        assert!(matches!(error, StorageError::InvalidArgument(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn put_rejects_storage_root_replaced_with_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let outside_path = dir.path().join("outside");
        let store = LocalStorage::new(store_path.to_str().unwrap()).unwrap();
        tokio::fs::create_dir_all(&outside_path).await.unwrap();
        tokio::fs::remove_dir(&store_path).await.unwrap();
        symlink(&outside_path, &store_path).unwrap();

        assert!(store.health_check().await.is_err());
        assert!(matches!(
            store
                .put("nested/object", Bytes::from_static(b"outside"))
                .await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(!outside_path.join("nested").exists());
    }

    fn assert_no_tmp_files(dir: &Path) {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
