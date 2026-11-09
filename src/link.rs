//! Link endpoints: synchronous producer/consumer handoff with auto-ACK.
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
    extract::State,
    http::{HeaderName, HeaderValue},
    middleware::map_response,
    response::Response,
    routing::get,
    Router,
};

use crate::endpoint::{wait_for, Id, RelayError};
use crate::server::AppState;
use crate::storage::Message;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/link/{id}", get(get_message).post(post_message))
        .layer(map_response(mark_deprecated))
}

async fn mark_deprecated(mut response: Response) -> Response {
    response.headers_mut().insert(
        HeaderName::from_static("deprecation"),
        HeaderValue::from_static("true"),
    );
    response
}

/// GET /link/{id} - Consumer waits for producer, auto-ACKs on receive.
async fn get_message(Id(id): Id, State(state): State<AppState>) -> Result<Message, RelayError> {
    let delivery = state.waiting_list.lock().await.take_or_subscribe(&id)?;
    wait_for(delivery, state.config.link_timeout).await
}

/// POST /link/{id} - Producer sends and waits for the consumer's ACK.
async fn post_message(
    Id(id): Id,
    State(state): State<AppState>,
    message: Message,
) -> Result<(), RelayError> {
    let timeout = state.config.link_timeout;
    // The message expires just before its producer stops waiting, so no
    // consumer can fetch it after the producer got 408.
    let ttl = timeout;
    let delivery = state
        .waiting_list
        .lock()
        .await
        .store_and_subscribe_ack(&id, message, ttl)?;
    wait_for(delivery, timeout).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;

    use crate::server::{test_server, Config};

    #[tokio::test]
    async fn test_delayed_producer() {
        let server = test_server(Config::default());

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
    }

    #[tokio::test]
    async fn test_delayed_consumer() {
        let server = test_server(Config::default());

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
    }

    #[tokio::test]
    async fn test_link_timeout() {
        let server = test_server(Config {
            link_timeout: Duration::from_millis(50),
            ..Config::default()
        });

        let response = server.get("/link/timeout-test").await;
        assert_eq!(response.status_code(), 408);
        assert_eq!(response.text(), "Request timed out");

        let body = Bytes::from_static(b"Hello, world!");
        let response = server.post("/link/timeout-test2").bytes(body).await;
        assert_eq!(response.status_code(), 408);
        assert_eq!(response.text(), "Request timed out");
    }

    #[tokio::test]
    async fn test_replaced_producer_gets_not_found() {
        let server = test_server(Config::default());

        let replaced = async {
            let body = Bytes::from_static(b"first");
            server.post("/link/replace-test").bytes(body).await
        };
        let replacing = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = Bytes::from_static(b"second");
            let producer = server.post("/link/replace-test").bytes(body);
            let consumer = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                server.get("/link/replace-test").await
            };
            let (_, consumer) = tokio::join!(producer, consumer);
            consumer
        };

        let (replaced, consumer) = tokio::join!(replaced, replacing);

        assert_eq!(replaced.status_code(), 404);
        assert_eq!(replaced.text(), "Not found");
        assert_eq!(consumer.text(), "second");
    }

    #[tokio::test]
    async fn test_deprecation_header() {
        let server = test_server(Config {
            link_timeout: Duration::from_millis(100),
            ..Config::default()
        });

        let consumer = async {
            let response = server.get("/link/deprecation-test").await;
            assert_eq!(response.header("Deprecation"), "true");
        };

        let producer = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let body = Bytes::from_static(b"test");
            let response = server.post("/link/deprecation-test").bytes(body).await;
            assert_eq!(response.header("Deprecation"), "true");
        };

        tokio::join!(consumer, producer);
    }

    #[tokio::test]
    async fn test_post_returns_503_when_store_is_full() {
        let server = test_server(Config {
            max_entries: 1,
            link_timeout: Duration::from_millis(100),
            ..Config::default()
        });
        let body = Bytes::from_static(b"undelivered");
        server.post("/inbox/occupied").bytes(body).await;

        let body = Bytes::from_static(b"rejected");
        let response = server.post("/link/new-channel").bytes(body).await;

        assert_eq!(response.status_code(), 503);
        assert_eq!(response.text(), "Server at capacity");
        assert_eq!(server.get("/inbox/occupied").await.text(), "undelivered");
    }
}
