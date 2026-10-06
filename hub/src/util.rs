//! Small cross-module utilities.

/// Current Unix time in whole seconds.
///
/// Shared replacement for the three per-module timestamp helpers that
/// previously duplicated this logic (token_store `now_secs`,
/// commit::id `now_timestamp`, metadata::sqlite `chrono_timestamp` — the
/// last of which never used chrono despite its name).
pub(crate) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
