//! Inbox handler for store-and-forward messaging with ACK support.
//!
//! # Security
//!
//! Inbox IDs act as shared secrets. Anyone who knows an ID can read/write/ACK
//! that inbox. IDs must be cryptographically random (e.g., 128-bit UUIDs).
//! Predictable IDs allow attackers to intercept or acknowledge messages.
//!
//! Messages persist in plaintext memory for the TTL duration. Do not relay
//! sensitive one-time credentials unless encryption is applied at the
//! application layer.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};

#[cfg(test)]
use super::response::MAX_ID_LENGTH;
use super::response::{build_response, validate_id_length};
use super::waiting_list::{GetOrSubscribeResult, LimitError, Message};
use super::AppState;

/// POST /inbox/{id} - Store a message, return 200 immediately.
pub async fn post_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    validate_id_length(&id, "Inbox ID")?;

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let message = Message { body, content_type };

    let ttl = state.config.inbox_cache_ttl;
    if let Err(e) = state.pending_list.lock().await.store(id, message, ttl) {
        tracing::error!(?e, "Failed to persist message");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Bytes::from("Failed to store message"),
        ));
    }

    Ok::<_, (StatusCode, Bytes)>((StatusCode::OK, Bytes::new()))
}

/// GET /inbox/{id} - Return message or wait (long-poll).
/// Returns 200 immediately if message exists, otherwise waits up to inbox_timeout.
/// Returns 408 Request Timeout if no message arrives within the timeout period.
pub async fn get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err((status, body)) = validate_id_length(&id, "Inbox ID") {
        return build_response(status, body, None);
    }

    // Atomically check for message or subscribe (fixes TOCTOU race)
    let result = {
        let mut pending_list = state.pending_list.lock().await;
        pending_list.get_or_subscribe(&id, state.config.inbox_cache_ttl)
    };

    match result {
        Ok(GetOrSubscribeResult::Message(msg)) => {
            build_response(StatusCode::OK, msg.body, msg.content_type)
        }
        Ok(GetOrSubscribeResult::Waiting(receiver)) => {
            match tokio::time::timeout(state.config.inbox_timeout, receiver).await {
                Ok(Ok(msg)) => build_response(StatusCode::OK, msg.body, msg.content_type),
                Ok(Err(_)) => build_response(StatusCode::NOT_FOUND, "Entry expired".into(), None),
                Err(_) => build_response(
                    StatusCode::REQUEST_TIMEOUT,
                    "Request timed out".into(),
                    None,
                ),
            }
        }
        Err(LimitError::WaiterLimitReached) => build_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many concurrent requests".into(),
            None,
        ),
    }
}

/// DELETE /inbox/{id} - ACK message, return 200 or 404.
pub async fn delete_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<(StatusCode, Bytes), (StatusCode, Bytes)> {
    validate_id_length(&id, "Inbox ID")?;

    let acked = state.pending_list.lock().await.ack(&id);

    if acked {
        Ok((StatusCode::OK, Bytes::new()))
    } else {
        Ok((StatusCode::NOT_FOUND, Bytes::from("Not found")))
    }
}

/// GET /inbox/{id}/ack - Return "true", "false", or 404 if no message exists.
pub async fn ack_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    validate_id_length(&id, "Inbox ID")?;

    match state.pending_list.lock().await.is_acked(&id) {
        Some(true) => Ok((StatusCode::OK, Bytes::from("true"))),
        Some(false) => Ok((StatusCode::OK, Bytes::from("false"))),
        None => Err((StatusCode::NOT_FOUND, Bytes::from("Not found"))),
    }
}

/// GET /inbox/{id}/await - Wait for ACK with timeout.
pub async fn await_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<(StatusCode, Bytes), (StatusCode, Bytes)> {
    validate_id_length(&id, "Inbox ID")?;

    // Check if already acked or subscribe for notification
    let receiver = {
        let mut pending_list = state.pending_list.lock().await;

        if pending_list.is_acked(&id) == Some(true) {
            return Ok((StatusCode::OK, Bytes::new()));
        }

        match pending_list.subscribe_ack(&id) {
            Ok(rx) => rx,
            Err(None) => {
                // Entry doesn't exist or expired
                return Ok((StatusCode::NOT_FOUND, Bytes::from("Not found")));
            }
            Err(Some(LimitError::WaiterLimitReached)) => {
                return Ok((
                    StatusCode::SERVICE_UNAVAILABLE,
                    Bytes::from("Too many concurrent requests"),
                ));
            }
        }
    };

    let timeout = state.config.inbox_timeout;
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(())) => Ok((StatusCode::OK, Bytes::new())),
        Ok(Err(_)) => {
            // Sender dropped without sending - entry expired or was removed
            Ok((StatusCode::NOT_FOUND, Bytes::from("Not found")))
        }
        Err(_) => Ok((
            StatusCode::REQUEST_TIMEOUT,
            Bytes::from("Request timed out"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;

    use crate::http_relay::{Config, HttpRelay};

    use super::MAX_ID_LENGTH;

    #[tokio::test]
    async fn test_post_and_get() {
        let (server, state) = HttpRelay::create_test_server(Config::default());

        // POST stores message
        let body = Bytes::from_static(b"hello inbox");
        let response = server.post("/inbox/test-123").bytes(body).await;
        assert_eq!(response.status_code(), 200);

        // GET retrieves it
        let response = server.get("/inbox/test-123").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "hello inbox");

        // Entry exists
        assert_eq!(state.pending_list.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn test_get_timeout_when_no_message() {
        let config = Config {
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        // GET on nonexistent inbox waits then times out
        let response = server.get("/inbox/nonexistent").await;
        assert_eq!(response.status_code(), 408);
    }

    #[tokio::test]
    async fn test_get_long_poll_receives_message() {
        let config = Config {
            inbox_timeout: Duration::from_secs(5),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        // GET starts waiting, POST delivers message
        let getter = async {
            let response = server.get("/inbox/long-poll-test").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "delayed message");
        };

        let poster = async {
            // POST after short delay
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = Bytes::from_static(b"delayed message");
            let response = server.post("/inbox/long-poll-test").bytes(body).await;
            assert_eq!(response.status_code(), 200);
        };

        tokio::join!(getter, poster);
    }

    #[tokio::test]
    async fn test_delete_acks_message() {
        let config = Config {
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (server, state) = HttpRelay::create_test_server(config);

        // POST a message
        let body = Bytes::from_static(b"to be acked");
        server.post("/inbox/ack-test").bytes(body).await;

        // DELETE acks it
        let response = server.delete("/inbox/ack-test").await;
        assert_eq!(response.status_code(), 200);

        // Check ack status
        let response = server.get("/inbox/ack-test/ack").await;
        assert_eq!(response.text(), "true");

        // Message is cleared after ack - GET now waits and times out (408)
        let response = server.get("/inbox/ack-test").await;
        assert_eq!(response.status_code(), 408);

        // Entry still exists (for ack tracking)
        assert_eq!(state.pending_list.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_not_found() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        let response = server.delete("/inbox/nonexistent").await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_ack_status_false() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // POST a message (not acked yet)
        let body = Bytes::from_static(b"pending");
        server.post("/inbox/pending-ack").bytes(body).await;

        // Check ack status is false
        let response = server.get("/inbox/pending-ack/ack").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "false");
    }

    #[tokio::test]
    async fn test_ack_status_nonexistent() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // Check ack status for nonexistent entry returns 404
        let response = server.get("/inbox/nonexistent/ack").await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_await_returns_when_acked() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // POST a message
        let body = Bytes::from_static(b"await test");
        server.post("/inbox/await-test").bytes(body).await;

        // Await and ACK concurrently
        let awaiter = async {
            let response = server.get("/inbox/await-test/await").await;
            assert_eq!(response.status_code(), 200);
        };

        let acker = async {
            // ACK after short delay
            tokio::time::sleep(Duration::from_millis(50)).await;
            let response = server.delete("/inbox/await-test").await;
            assert_eq!(response.status_code(), 200);
        };

        tokio::join!(awaiter, acker);
    }

    #[tokio::test]
    async fn test_await_already_acked() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // POST and immediately ACK
        let body = Bytes::from_static(b"pre-acked");
        server.post("/inbox/pre-acked").bytes(body).await;
        server.delete("/inbox/pre-acked").await;

        // Await should return immediately with 200
        let response = server.get("/inbox/pre-acked/await").await;
        assert_eq!(response.status_code(), 200);
    }

    #[tokio::test]
    async fn test_await_timeout() {
        let config = Config {
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        // POST a message but don't ACK
        let body = Bytes::from_static(b"timeout test");
        server.post("/inbox/timeout-test").bytes(body).await;

        // Await should timeout
        let response = server.get("/inbox/timeout-test/await").await;
        assert_eq!(response.status_code(), 408);
    }

    #[tokio::test]
    async fn test_await_not_found() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // Await on nonexistent entry
        let response = server.get("/inbox/nonexistent/await").await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_await_not_triggered_by_get() {
        let config = Config {
            inbox_timeout: Duration::from_millis(300),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        // 1. POST message
        let body = Bytes::from_static(b"test message");
        let response = server.post("/inbox/get-doesnt-ack").bytes(body).await;
        assert_eq!(response.status_code(), 200);

        // 2. Await ACK, GET message, then ACK - all concurrently
        let awaiter = async {
            let response = server.get("/inbox/get-doesnt-ack/await").await;
            response.status_code()
        };

        let getter_and_acker = async {
            // Wait a bit, then GET message
            tokio::time::sleep(Duration::from_millis(50)).await;
            let response = server.get("/inbox/get-doesnt-ack").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "test message");

            // GET completed - await should still be waiting
            // Now wait more, then ACK
            tokio::time::sleep(Duration::from_millis(50)).await;
            let response = server.delete("/inbox/get-doesnt-ack").await;
            assert_eq!(response.status_code(), 200);
        };

        let (await_status, _) = tokio::join!(awaiter, getter_and_acker);

        // Await should return 200 (ACKed), not 408 (timeout)
        assert_eq!(
            await_status, 200,
            "Await should return 200 after ACK, not be triggered by GET"
        );
    }

    #[tokio::test]
    async fn test_content_type_preserved() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // POST with content-type
        let body = Bytes::from_static(br#"{"key":"value"}"#);
        server
            .post("/inbox/ct-test")
            .content_type("application/json")
            .bytes(body)
            .await;

        // GET preserves content-type
        let response = server.get("/inbox/ct-test").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(
            response.header("content-type").to_str().unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn test_post_overwrites_previous() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        // First POST
        let body = Bytes::from_static(b"first");
        server.post("/inbox/overwrite").bytes(body).await;

        // Second POST overwrites
        let body = Bytes::from_static(b"second");
        server.post("/inbox/overwrite").bytes(body).await;

        // GET returns second value
        let response = server.get("/inbox/overwrite").await;
        assert_eq!(response.text(), "second");
    }

    #[tokio::test]
    async fn test_inbox_id_too_long() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());

        let long_id = "x".repeat(MAX_ID_LENGTH + 1);

        // POST rejects long ID
        let body = Bytes::from_static(b"test");
        let response = server
            .post(&format!("/inbox/{}", long_id))
            .bytes(body)
            .await;
        assert_eq!(response.status_code(), 400);
        assert_eq!(response.text(), "Inbox ID too long");

        // GET rejects long ID
        let response = server.get(&format!("/inbox/{}", long_id)).await;
        assert_eq!(response.status_code(), 400);

        // DELETE rejects long ID
        let response = server.delete(&format!("/inbox/{}", long_id)).await;
        assert_eq!(response.status_code(), 400);

        // /ack rejects long ID
        let response = server.get(&format!("/inbox/{}/ack", long_id)).await;
        assert_eq!(response.status_code(), 400);

        // /await rejects long ID
        let response = server.get(&format!("/inbox/{}/await", long_id)).await;
        assert_eq!(response.status_code(), 400);
    }

    #[tokio::test]
    async fn test_message_expires() {
        let config = Config {
            inbox_cache_ttl: Duration::from_millis(50),
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (server, _state) = HttpRelay::create_test_server(config);

        // POST a message
        let body = Bytes::from_static(b"ephemeral");
        server.post("/inbox/expire-test").bytes(body).await;

        // GET before expiry
        let response = server.get("/inbox/expire-test").await;
        assert_eq!(response.status_code(), 200);

        // Wait for expiry
        tokio::time::sleep(Duration::from_millis(60)).await;

        // GET after expiry - now times out waiting (408)
        let response = server.get("/inbox/expire-test").await;
        assert_eq!(response.status_code(), 408);
    }
}
