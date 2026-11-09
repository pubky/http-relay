//! What the relay's endpoints have in common: the ID in the path, messages in
//! requests and responses, waiting for a delivery, and errors.

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{FromRequest, FromRequestParts, Path, Request},
    http::{header, request::Parts, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

use crate::storage::Message;
use crate::waiting_list::{Delivery, WaitingListError};

/// Maximum allowed length for IDs (in bytes).
/// 256 bytes accommodates typical ID patterns (UUIDs are 36 chars, base64-encoded
/// 256-bit keys are 44 chars) while preventing DoS via oversized storage and
/// waiter keys.
pub(crate) const MAX_ID_LENGTH: usize = 256;

/// The `{id}` path segment, at most [`MAX_ID_LENGTH`] bytes long.
pub(crate) struct Id(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if id.len() > MAX_ID_LENGTH {
            return Err(RelayError::IdTooLong.into_response());
        }
        Ok(Self(id))
    }
}

/// Reads a message from the request body and its Content-Type.
impl<S: Send + Sync> FromRequest<S> for Message {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = Bytes::from_request(request, state)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Self { body, content_type })
    }
}

/// Forwards a stored message to its consumer as a 200 OK response.
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
impl IntoResponse for Message {
    fn into_response(self) -> Response {
        let mut response = (StatusCode::OK, self.body).into_response();
        let headers = response.headers_mut();
        if let Some(content_type) = self.content_type.and_then(|ct| ct.parse().ok()) {
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
}

/// Returns the delivered value, waiting at most `timeout` for it.
pub(crate) async fn wait_for<T>(delivery: Delivery<T>, timeout: Duration) -> Result<T, RelayError> {
    match delivery {
        Delivery::Ready(value) => Ok(value),
        Delivery::Pending(receiver) => match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(value)) => Ok(value),
            // The entry expired, or a new message replaced it.
            Ok(Err(_)) => Err(RelayError::NotFound),
            Err(_) => Err(RelayError::Timeout),
        },
    }
}

/// Why a request failed.
#[derive(Debug)]
pub(crate) enum RelayError {
    IdTooLong,
    NotFound,
    Timeout,
    TooManyWaiters,
    /// Every stored entry holds an unACKed message.
    StoreFull,
    Storage(anyhow::Error),
}

impl From<anyhow::Error> for RelayError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<WaitingListError> for RelayError {
    fn from(error: WaitingListError) -> Self {
        match error {
            WaitingListError::WaiterLimitReached => Self::TooManyWaiters,
            WaitingListError::StoreFull => Self::StoreFull,
            WaitingListError::Storage(error) => Self::Storage(error),
        }
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::IdTooLong => (StatusCode::BAD_REQUEST, "ID too long"),
            Self::NotFound => (StatusCode::NOT_FOUND, "Not found"),
            Self::Timeout => (StatusCode::REQUEST_TIMEOUT, "Request timed out"),
            Self::TooManyWaiters => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many concurrent requests",
            ),
            Self::StoreFull => (StatusCode::SERVICE_UNAVAILABLE, "Server at capacity"),
            Self::Storage(error) => {
                tracing::error!(?error, "Storage failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Storage error")
            }
        };
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn storage_error_is_500_without_details() {
        let error = RelayError::Storage(anyhow::anyhow!("disk /var/secret is full"));

        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, "Storage error");
    }

    #[tokio::test]
    async fn closed_delivery_is_not_found() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        drop(sender);

        let result = wait_for(Delivery::Pending(receiver), Duration::from_secs(1)).await;

        assert!(matches!(result, Err(RelayError::NotFound)));
    }
}
