/// Validate file paths supplied in commit operations.
///
/// Rejects empty paths, absolute paths, path traversal components, null bytes,
/// empty path components, and Windows reserved names.
pub(super) fn validate_file_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("File path cannot be empty".to_string());
    }

    if path.contains('\0') {
        return Err("File path cannot contain null bytes".to_string());
    }

    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("File path cannot be absolute: {}", path));
    }

    for component in path.split(['/', '\\']) {
        if component == ".." {
            return Err(format!("File path contains path traversal: {}", path));
        }
    }

    if path.contains("//") || path.contains("\\\\") {
        return Err(format!("File path contains empty components: {}", path));
    }

    let first_component = path.split(['/', '\\']).next().unwrap_or("");
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if reserved
        .iter()
        .any(|r| r.eq_ignore_ascii_case(first_component))
    {
        return Err(format!("File path uses reserved name: {}", first_component));
    }

    Ok(())
}
