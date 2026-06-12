//! Disk space utilities

/// Check if there's enough disk space for an upload.
/// Returns Ok(()) if sufficient space is available, Err with description otherwise.
pub fn check_disk_space(path: &std::path::Path, required_bytes: u64) -> Result<(), String> {
    // Use statvfs on Unix-like systems to check actual available space
    #[cfg(unix)]
    {
        // Verify path exists first
        if !path.exists() {
            return Err(format!("Path does not exist: {}", path.display()));
        }

        let stat = nix::sys::statvfs::statvfs(path)
            .map_err(|e| format!("statvfs failed for {}: {}", path.display(), e))?;

        // Note: blocks_available() represents space available to unprivileged users.
        // Some filesystems (e.g., ext4) reserve 5% for root by default.
        // blocks_available() already accounts for this reservation, so this check
        // correctly reports space available to the current process.
        let available = stat.fragment_size() as u64 * stat.blocks_available() as u64;
        if available < required_bytes {
            return Err(format!(
                "Insufficient disk space: need {} MB, have {} MB available",
                required_bytes / 1024 / 1024,
                available / 1024 / 1024
            ));
        }

        Ok(())
    }

    #[cfg(not(unix))]
    {
        // Non-Unix platforms: log warning and skip check
        tracing::warn!(
            "Disk space check not implemented for non-Unix platforms. \
             Upload of {} MB may fail if insufficient space.",
            required_bytes / 1024 / 1024
        );
        let _ = required_bytes;
        Ok(())
    }
}
