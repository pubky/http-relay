//! Shared response utilities for HTTP relay endpoints.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::{
    body::Bytes,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use http_body::{Body, Frame};
use tokio::sync::oneshot;

use super::waiting_list::MessageWithAck;

/// A response body that sends an ACK when fully delivered.
///
/// This enables two-phase acknowledgment: the counterpart waits for the ACK
/// to confirm the HTTP response was actually written to the client.
/// If the client disconnects before the body is fully sent, the ACK channel
/// closes without sending, allowing the waiter to detect the failure.
pub struct AckBody {
    data: Option<Bytes>,
    ack_sender: Option<oneshot::Sender<()>>,
}

impl AckBody {
    pub fn new(data: Bytes, ack_sender: oneshot::Sender<()>) -> Self {
        Self {
            data: Some(data),
            ack_sender: Some(ack_sender),
        }
    }
}

impl Body for AckBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(data) = self.data.take() {
            // Return the data frame
            Poll::Ready(Some(Ok(Frame::data(data))))
        } else {
            // Body complete - send ACK to signal successful delivery
            if let Some(ack) = self.ack_sender.take() {
                let _ = ack.send(());
            }
            Poll::Ready(None)
        }
    }
}

/// Build a response with AckBody for two-phase acknowledgment.
pub fn build_ack_response(
    status: StatusCode,
    body: Bytes,
    content_type: Option<String>,
    ack_sender: oneshot::Sender<()>,
) -> Response {
    let ack_body = AckBody::new(body, ack_sender);
    let mut response = Response::builder()
        .status(status)
        .body(axum::body::Body::new(ack_body))
        .unwrap();
    if let Some(ct) = content_type {
        if let Ok(value) = ct.parse() {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}

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

/// Awaits a message from the producer with a timeout (with two-phase ACK).
/// Returns an AckBody response that signals the producer when delivery completes.
/// On channel close, returns NOT_FOUND.
/// On timeout, calls the async cleanup function and returns REQUEST_TIMEOUT.
pub async fn await_consumer_message<F, Fut>(
    receiver: oneshot::Receiver<MessageWithAck>,
    timeout: Duration,
    on_timeout: F,
) -> Response
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(msg)) => {
            build_ack_response(StatusCode::OK, msg.body, msg.content_type, msg.ack_sender)
        }
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

/// Awaits completion from the consumer with two-phase ACK.
/// First receives the ack_receiver from consumer, then waits for the ACK.
/// On success (ACK received), returns OK with empty body.
/// On timeout or ACK failure, calls cleanup and returns REQUEST_TIMEOUT.
pub async fn await_producer_completion<F, Fut>(
    receiver: oneshot::Receiver<oneshot::Receiver<()>>,
    timeout: Duration,
    on_timeout: F,
) -> (StatusCode, Bytes)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    // First wait for consumer to send us the ack_receiver
    let ack_receiver = match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(ack_rx)) => ack_rx,
        Ok(Err(_)) => {
            // Channel closed - consumer disappeared before sending ack_receiver
            on_timeout().await;
            return (StatusCode::REQUEST_TIMEOUT, "Request timed out".into());
        }
        Err(_) => {
            on_timeout().await;
            return (StatusCode::REQUEST_TIMEOUT, "Request timed out".into());
        }
    };

    // Now wait for the actual ACK (consumer's response was delivered)
    match tokio::time::timeout(timeout, ack_receiver).await {
        Ok(Ok(())) => (StatusCode::OK, Bytes::new()),
        Ok(Err(_)) => {
            // ACK channel closed without ACK - consumer disconnected before delivery
            (StatusCode::REQUEST_TIMEOUT, "Consumer disconnected".into())
        }
        Err(_) => {
            on_timeout().await;
            (StatusCode::REQUEST_TIMEOUT, "Request timed out".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn test_ack_body_sends_ack_on_completion() {
        let (ack_tx, ack_rx) = oneshot::channel();
        let body = AckBody::new(Bytes::from("test data"), ack_tx);

        // Consume the entire body
        let collected = axum::body::Body::new(body).collect().await.unwrap();
        assert_eq!(collected.to_bytes(), Bytes::from("test data"));

        // ACK should have been sent
        assert!(ack_rx.await.is_ok());
    }

    #[tokio::test]
    async fn test_ack_body_no_ack_on_drop() {
        let (ack_tx, ack_rx) = oneshot::channel();
        let body = AckBody::new(Bytes::from("test data"), ack_tx);

        // Drop body without consuming it (simulates client disconnect)
        drop(body);

        // ACK channel should be closed without sending
        assert!(ack_rx.await.is_err());
    }

    #[tokio::test]
    async fn test_ack_body_no_ack_on_partial_read() {
        let (ack_tx, ack_rx) = oneshot::channel();
        let body = AckBody::new(Bytes::from("test data"), ack_tx);
        let mut body = axum::body::Body::new(body);

        // Read only the first frame, don't complete
        let _frame = body.frame().await;
        // Drop before completion
        drop(body);

        // ACK channel should be closed without sending
        assert!(ack_rx.await.is_err());
    }
}
