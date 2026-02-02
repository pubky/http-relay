//! Shared response utilities for HTTP relay endpoints.

use std::future::Future;
use std::time::Duration;

use axum::{
    body::Bytes,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use tokio::sync::oneshot;

use super::waiting_list::Message;

/// Build a response with optional Content-Type header.
pub fn build_response(status: StatusCode, body: Bytes, content_type: Option<String>) -> Response {
    let mut response = (status, body).into_response();
    if let Some(ct) = content_type {
        if let Ok(value) = ct.parse() {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}

/// Awaits a message from the producer with a timeout.
/// On success, returns OK with the message body and content type.
/// On channel close, returns NOT_FOUND.
/// On timeout, calls the async cleanup function and returns REQUEST_TIMEOUT.
pub async fn await_consumer_message<F, Fut>(
    receiver: oneshot::Receiver<Message>,
    timeout: Duration,
    on_timeout: F,
) -> Response
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(msg)) => build_response(StatusCode::OK, msg.body, msg.content_type),
        Ok(Err(_)) => build_response(StatusCode::NOT_FOUND, "Not Found".into(), None),
        Err(_) => {
            on_timeout().await;
            build_response(
                StatusCode::REQUEST_TIMEOUT,
                "Request timed out".into(),
                None,
            )
        }
    }
}

/// Awaits completion from the consumer with a timeout.
/// On success, returns OK with empty body.
/// On timeout, calls the async cleanup function and returns REQUEST_TIMEOUT.
pub async fn await_producer_completion<F, Fut>(
    receiver: oneshot::Receiver<()>,
    timeout: Duration,
    on_timeout: F,
) -> (StatusCode, Bytes)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    match tokio::time::timeout(timeout, receiver).await {
        Ok(_) => (StatusCode::OK, Bytes::new()),
        Err(_) => {
            on_timeout().await;
            (StatusCode::REQUEST_TIMEOUT, "Request timed out".into())
        }
    }
}
