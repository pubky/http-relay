//! Shared response utilities for HTTP relay endpoints.
//!
//! # Response Patterns
//!
//! Handlers should use these patterns consistently:
//!
//! - **`build_response()`**: Use when forwarding stored messages that may have
//!   a Content-Type header (e.g., GET /inbox/{id}, GET /link/{id}).
//!
//! - **`(StatusCode, Bytes)`**: Use for simple responses without Content-Type
//!   (e.g., POST success, DELETE success/failure, errors, ACK status).
//!
//! This keeps simple cases simple while providing Content-Type forwarding
//! only where needed.

use axum::{
    body::Bytes,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

/// Maximum allowed length for IDs (in bytes).
/// 256 bytes accommodates typical ID patterns (UUIDs are 36 chars, base64-encoded
/// 256-bit keys are 44 chars) while preventing DoS via oversized HashMap keys.
pub const MAX_ID_LENGTH: usize = 256;

/// Validate that an ID doesn't exceed the maximum length.
/// Returns `Err` with a BAD_REQUEST response if the ID is too long.
pub fn validate_id_length(id: &str, id_name: &str) -> Result<(), (StatusCode, Bytes)> {
    if id.len() > MAX_ID_LENGTH {
        Err((
            StatusCode::BAD_REQUEST,
            Bytes::from(format!("{} too long", id_name)),
        ))
    } else {
        Ok(())
    }
}

/// Build a response with optional Content-Type header.
///
/// Use this for message bodies that may have a stored Content-Type.
/// For simple responses (errors, ACKs), prefer `(StatusCode, Bytes)` tuples.
pub fn build_response(status: StatusCode, body: Bytes, content_type: Option<String>) -> Response {
    let mut response = (status, body).into_response();
    if let Some(ct) = content_type {
        if let Ok(value) = ct.parse() {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}
