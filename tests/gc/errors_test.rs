use xet_server::gc::errors::{GcError, StorageError};

#[test]
fn test_gc_error_display() {
    let err = GcError::BloomFilterCorrupted { expected: 12345, actual: 67890 };
    assert!(err.to_string().contains("Bloom Filter corrupted"));
    assert!(err.to_string().contains("12345"));
}

#[test]
fn test_storage_error_condition_failed() {
    let err = StorageError::ConditionFailed;
    assert!(matches!(err, StorageError::ConditionFailed));
}

#[test]
fn test_gc_error_from_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let gc_err: GcError = io_err.into();
    assert!(matches!(gc_err, GcError::Io(_)));
}
