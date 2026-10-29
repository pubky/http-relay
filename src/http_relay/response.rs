//! Shared response utilities for HTTP relay endpoints.
//!
//! # Response Patterns
//!
//! Handlers should use these patterns consistently:
//!
//! - **`message_response()`**: Use when forwarding a stored message to a
//!   consumer (e.g., GET /inbox/{id}, GET /link/{id}). It forwards the
//!   producer's Content-Type and makes the body inert in browsers.
//!
//! - **`(StatusCode, Bytes)`**: Use for simple responses without Content-Type
//!   (e.g., POST success, DELETE success/failure, errors, ACK status).
//!
//! This keeps simple cases simple while giving every relayed message body the
//! same protective headers.

use axum::{
    body::Bytes,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

use super::waiting_list::Message;

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

/// Build a 200 OK response that forwards a stored message to its consumer.
///
/// The producer controls both the body and its Content-Type, so a producer can
/// store an HTML page with scripts and send a victim a link to it. To keep such
/// a page from running on the relay's origin, the response always carries:
///
/// - `Content-Security-Policy: sandbox`: browsers render the page without
///   scripts and treat it as a unique origin, not the relay's.
/// - `X-Content-Type-Options: nosniff`: browsers don't guess a more dangerous
///   type than the one sent.
///
/// Neither header affects consumers that read the response with `fetch()` or a
/// non-browser HTTP client.
///
/// A stored Content-Type that isn't a valid header value is dropped.
pub fn message_response(message: Message) -> Response {
    let mut response = (StatusCode::OK, message.body).into_response();
    let headers = response.headers_mut();
    if let Some(content_type) = message.content_type.and_then(|ct| ct.parse().ok()) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
