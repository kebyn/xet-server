//! Shared request-payload-to-temp-file streaming for upload handlers.
//!
//! Replaces three near-identical streaming loops (xorb, LFS object, shard
//! uploads). Handlers keep only their per-chunk hashing callback; size
//! enforcement, writes, error logging, and the final fsync live here.

use actix_web::{HttpResponse, web};
use tracing::error;

use super::TempFile;

/// Errors from streaming a request payload into a temp file.
#[derive(Debug)]
pub enum StreamPayloadError {
    /// The request payload stream failed mid-transfer.
    Payload(String),
    /// The payload exceeded the configured maximum size (in bytes).
    TooLarge(u64),
    /// Writing to the temp file failed.
    Write(String),
    /// fsync of the temp file failed.
    Sync(String),
    /// The process temporary-space budget or free-space reserve was exhausted.
    Quota,
}

impl StreamPayloadError {
    /// Map this error to the response the upload handlers have always sent.
    pub fn error_response(&self) -> HttpResponse {
        match self {
            Self::Payload(_) => HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid upload stream"
            })),
            Self::TooLarge(max_bytes) => HttpResponse::PayloadTooLarge().json(serde_json::json!({
                "error": format!(
                    "Upload exceeds maximum size of {} MB",
                    max_bytes / 1024 / 1024
                )
            })),
            Self::Quota => HttpResponse::ServiceUnavailable()
                .insert_header((actix_web::http::header::RETRY_AFTER, "5"))
                .json(serde_json::json!({"error": "Temporary storage unavailable"})),
            Self::Write(_) | Self::Sync(_) => {
                HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": crate::api::INTERNAL_ERROR_MESSAGE
                }))
            }
        }
    }
}

/// Stream a request payload into `temp_file`, enforcing `max_bytes`.
///
/// `on_chunk` runs on every received chunk before it is written, so callers
/// feed their incremental hashers (DualHasher / StreamingHasher). The file is
/// fsynced before returning. Returns the total number of bytes streamed.
/// Errors are logged here; the caller only maps them to a response.
pub async fn stream_payload_to_temp(
    payload: &mut web::Payload,
    temp_file: &mut TempFile,
    max_bytes: u64,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<u64, StreamPayloadError> {
    use futures_util::StreamExt;

    let mut total_bytes: u64 = 0;

    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                error!("Payload stream error: {}", e);
                return Err(StreamPayloadError::Payload(e.to_string()));
            }
        };

        total_bytes = match total_bytes.checked_add(chunk.len() as u64) {
            Some(total) => total,
            None => return Err(StreamPayloadError::Quota),
        };
        if total_bytes > max_bytes {
            return Err(StreamPayloadError::TooLarge(max_bytes));
        }

        on_chunk(&chunk);
        if let Err(e) = temp_file.write_all(&chunk).await {
            error!("Failed to write to temp file: {}", e);
            return Err(if matches!(e, crate::storage::StorageError::Quota) {
                StreamPayloadError::Quota
            } else {
                StreamPayloadError::Write(e.to_string())
            });
        }
    }

    if let Err(e) = temp_file.sync_all().await {
        error!("Failed to sync temp file: {}", e);
        return Err(StreamPayloadError::Sync(e.to_string()));
    }

    Ok(total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_statuses() {
        assert_eq!(
            StreamPayloadError::Payload("x".into())
                .error_response()
                .status(),
            400
        );
        assert_eq!(
            StreamPayloadError::TooLarge(2048 * 1024 * 1024)
                .error_response()
                .status(),
            413
        );
        assert_eq!(
            StreamPayloadError::Write("x".into())
                .error_response()
                .status(),
            500
        );
        assert_eq!(
            StreamPayloadError::Sync("x".into())
                .error_response()
                .status(),
            500
        );
    }
}
