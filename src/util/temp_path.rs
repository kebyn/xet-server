use std::path::{Path, PathBuf};

/// Owns a temporary path and removes it when the guard is dropped.
#[derive(Debug)]
pub struct TempPathGuard {
    path: Option<PathBuf>,
}

impl TempPathGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn try_path(&self) -> Result<&Path, String> {
        self.path
            .as_deref()
            .ok_or_else(|| "temp path already cleaned".to_string())
    }

    pub fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("live TempPathGuard must own its path")
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _ = std::fs::remove_file(path);
            });
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn removes_owned_path_on_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("owned.tmp");
        std::fs::write(&path, b"temporary").unwrap();

        let guard = TempPathGuard::new(path.clone());
        assert_eq!(guard.try_path().unwrap(), path);
        drop(guard);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!path.exists());
    }
}
