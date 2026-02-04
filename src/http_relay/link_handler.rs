//! Link handler - synchronous producer/consumer handoff with auto-ACK.
//!
//! # Deprecation Notice
//!
//! The `/link/` endpoint is deprecated. Use `/inbox/` instead for new integrations.
//! Link provides backwards-compatible synchronous handoff by auto-acknowledging on GET.
//!
//! # Security
//!
//! Channel IDs act as shared secrets. Anyone who knows an ID can read/write to that
//! channel. IDs must be cryptographically random (e.g., 128-bit UUIDs). Predictable
//! IDs allow attackers to intercept messages.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

use super::response::{build_response, validate_id_length};
#[cfg(test)]
use super::response::MAX_ID_LENGTH;
use super::waiting_list::{GetOrSubscribeResult, LimitError, Message};
use super::AppState;

/// GET /link/{id} - Consumer waits for producer, auto-ACKs on receive.
/// This maintains backwards compatibility with the synchronous handoff model.
pub async fn get_handler(Path(id): Path<String>, State(state): State<AppState>) -> Response {
    if let Err((status, body)) = validate_id_length(&id, "Channel ID") {
        return build_response(status, body, None);
    }

    // Get message or wait for it
    let result = {
        let mut pending_list = state.pending_list.lock().await;
        pending_list.get_or_subscribe(&id, state.config.link_timeout)
    };

    let msg = match result {
        Ok(GetOrSubscribeResult::Message(msg)) => msg,
        Ok(GetOrSubscribeResult::Waiting(receiver)) => {
            match tokio::time::timeout(state.config.link_timeout, receiver).await {
                Ok(Ok(msg)) => msg,
                Ok(Err(_)) => {
                    return build_response(StatusCode::NOT_FOUND, "Entry expired".into(), None);
                }
                Err(_) => {
                    return build_response(
                        StatusCode::REQUEST_TIMEOUT,
                        "Request timed out".into(),
                        None,
                    );
                }
            }
        }
        Err(LimitError::WaiterLimitReached) => {
            return build_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many concurrent requests".into(),
                None,
            );
        }
    };

    // Auto-ACK to signal producer that consumer received the message
    state.pending_list.lock().await.ack(&id);

    let mut response = build_response(StatusCode::OK, msg.body, msg.content_type);
    response
        .headers_mut()
        .insert("Deprecation", HeaderValue::from_static("true"));
    response
}

/// POST /link/{id} - Producer sends and waits for consumer ACK.
/// This maintains backwards compatibility with the synchronous handoff model.
pub async fn post_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((status, body)) = validate_id_length(&id, "Channel ID") {
        return (status, body).into_response();
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let message = Message {
        body: body.clone(),
        content_type,
    };

    // Store message and subscribe for ACK notification
    let receiver = {
        let mut pending_list = state.pending_list.lock().await;
        if let Err(e) = pending_list.store(id.clone(), message, state.config.link_timeout) {
            tracing::error!(?e, "Failed to persist message");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from("Failed to store message"),
            )
                .into_response();
        }

        // Subscribe to get notified when consumer ACKs
        match pending_list.subscribe_ack(&id) {
            Ok(rx) => rx,
            Err(None) => {
                // Entry was immediately evicted (shouldn't happen normally)
                return (StatusCode::SERVICE_UNAVAILABLE, Bytes::from("Server at capacity"))
                    .into_response();
            }
            Err(Some(LimitError::WaiterLimitReached)) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Bytes::from("Too many concurrent requests"),
                )
                    .into_response();
            }
        }
    };

    // Wait for consumer to ACK (i.e., GET the message)
    let result = match tokio::time::timeout(state.config.link_timeout, receiver).await {
        Ok(Ok(())) => (StatusCode::OK, Bytes::new()),
        Ok(Err(_)) => {
            // Entry expired or was evicted
            (StatusCode::NOT_FOUND, Bytes::from("Entry expired"))
        }
        Err(_) => (StatusCode::REQUEST_TIMEOUT, Bytes::from("Request timed out")),
    };

    let mut response = result.into_response();
    response
        .headers_mut()
        .insert("Deprecation", HeaderValue::from_static("true"));
    response
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;

    use crate::http_relay::{Config, HttpRelay};

    use super::MAX_ID_LENGTH;

    #[tokio::test]
    async fn test_delayed_producer() {
        let (server, state) = HttpRelay::create_test_server(Config::default());

        let consumer = async {
            let response = server.get("/link/123").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "Hello, world!");
        };

        let producer = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let body = Bytes::from_static(b"Hello, world!");
            let response = server.post("/link/123").bytes(body).await;
            assert_eq!(response.status_code(), 200);
        };

        tokio::join!(consumer, producer);
        // Entry may still exist (acked) until cleanup
        assert_eq!(state.pending_list.lock().await.is_acked("123"), Some(true));
    }

    #[tokio::test]
    async fn test_delayed_consumer() {
        let (server, state) = HttpRelay::create_test_server(Config::default());

        let consumer = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let response = server.get("/link/123").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "Hello, world!");
        };

        let producer = async {
            let body = Bytes::from_static(b"Hello, world!");
            let response = server.post("/link/123").bytes(body).await;
            assert_eq!(response.status_code(), 200);
        };

        tokio::join!(consumer, producer);
        assert_eq!(state.pending_list.lock().await.is_acked("123"), Some(true));
    }

    #[tokio::test]
    async fn test_link_timeout() {
        let config = Config {
            link_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        // Consumer request times out
        let response = server.get("/link/timeout-test").await;
        assert_eq!(response.status_code(), 408);
        assert_eq!(response.text(), "Request timed out");

        // Producer request times out
        let body = Bytes::from_static(b"Hello, world!");
        let response = server.post("/link/timeout-test2").bytes(body).await;
        assert_eq!(response.status_code(), 408);
        assert_eq!(response.text(), "Request timed out");
    }

    #[tokio::test]
    async fn test_content_type_forwarding() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        let consumer = async {
            let response = server.get("/link/ct-test").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), r#"{"key":"value"}"#);
            assert_eq!(
                response.header("content-type").to_str().unwrap(),
                "application/json"
            );
        };

        let producer = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = Bytes::from_static(br#"{"key":"value"}"#);
            server
                .post("/link/ct-test")
                .content_type("application/json")
                .bytes(body)
                .await;
        };

        tokio::join!(consumer, producer);
    }

    #[tokio::test]
    async fn test_channel_id_too_long() {
        let config = Config {
            link_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        let long_id = "x".repeat(MAX_ID_LENGTH + 1);

        // GET rejects long IDs
        let response = server.get(&format!("/link/{}", long_id)).await;
        assert_eq!(response.status_code(), 400);
        assert_eq!(response.text(), "Channel ID too long");

        // POST rejects long IDs
        let body = Bytes::from_static(b"test");
        let response = server.post(&format!("/link/{}", long_id)).bytes(body).await;
        assert_eq!(response.status_code(), 400);
        assert_eq!(response.text(), "Channel ID too long");

        // IDs at the limit should work (timeout instead of 400)
        let ok_id = "x".repeat(MAX_ID_LENGTH);
        let response = server.get(&format!("/link/{}", ok_id)).await;
        assert_ne!(response.status_code(), 400);
    }

    #[tokio::test]
    async fn test_deprecation_header() {
        let config = Config {
            link_timeout: Duration::from_millis(100),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        let consumer = async {
            let response = server.get("/link/deprecation-test").await;
            assert_eq!(response.header("Deprecation").to_str().unwrap(), "true");
        };

        let producer = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let body = Bytes::from_static(b"test");
            let response = server.post("/link/deprecation-test").bytes(body).await;
            assert_eq!(response.header("Deprecation").to_str().unwrap(), "true");
        };

        tokio::join!(consumer, producer);
    }
}
