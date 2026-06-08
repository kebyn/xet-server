use thiserror::Error;

#[derive(Error, Debug)]
pub enum XetError {
    #[error("Invalid hash format: {0}")]
    InvalidHashFormat(String),

    #[error("Invalid chunk size: {size} (expected {min}-{max})")]
    InvalidChunkSize { size: usize, min: usize, max: usize },

    #[error("Hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, XetError>;