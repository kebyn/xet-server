/// Validate OID format (64 hex characters).
pub(crate) fn validate_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.chars().all(|c| c.is_ascii_hexdigit())
}
