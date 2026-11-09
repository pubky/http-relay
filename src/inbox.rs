//! Inbox endpoints: store-and-forward messaging with ACK support.
//!
//! # Security
//!
//! Inbox IDs act as shared secrets. Anyone who knows an ID can read/write/ACK
//! that inbox. IDs must be cryptographically random (e.g., 128-bit UUIDs).
//! Predictable IDs allow attackers to intercept or acknowledge messages.
//!
//! Messages persist in plaintext until they're ACKed or expire. When
//! persistence is enabled, deleted bodies can remain on disk after that (see
//! "Deleted message bodies" in `storage/sqlite.rs`). Do not relay sensitive
//! one-time credentials unless encryption is applied at the application layer.

use axum::{extract::State, routing::get, Extension, Router};

use crate::client_ip::ClientIp;
use crate::endpoint::{wait_for, Id, RelayError};
use crate::server::AppState;
use crate::storage::Message;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/inbox/{id}",
            get(get_message).post(post_message).delete(ack_message),
        )
        .route("/inbox/{id}/ack", get(ack_status))
        .route("/inbox/{id}/await", get(await_ack))
}

/// POST /inbox/{id} - Store a message, return 200 immediately.
async fn post_message(
    Id(id): Id,
    State(state): State<AppState>,
    Extension(client_ip): Extension<ClientIp>,
    message: Message,
) -> Result<(), RelayError> {
    let ttl = state.config.inbox_cache_ttl;
    state
        .waiting_list
        .lock()
        .await
        .store(&id, message, ttl, client_ip.network())?;
    Ok(())
}

/// GET /inbox/{id} - Return the message, or wait up to `inbox_timeout` for one.
async fn get_message(Id(id): Id, State(state): State<AppState>) -> Result<Message, RelayError> {
    let delivery = state.waiting_list.lock().await.get_or_subscribe(&id)?;
    wait_for(delivery, state.config.inbox_timeout).await
}

/// DELETE /inbox/{id} - ACK the message.
async fn ack_message(Id(id): Id, State(state): State<AppState>) -> Result<(), RelayError> {
    if state.waiting_list.lock().await.ack(&id)? {
        Ok(())
    } else {
        Err(RelayError::NotFound)
    }
}

/// GET /inbox/{id}/ack - Return whether the message is ACKed: "true" or "false".
async fn ack_status(Id(id): Id, State(state): State<AppState>) -> Result<&'static str, RelayError> {
    match state.waiting_list.lock().await.is_acked(&id)? {
        Some(true) => Ok("true"),
        Some(false) => Ok("false"),
        None => Err(RelayError::NotFound),
    }
}

/// GET /inbox/{id}/await - Wait up to `inbox_timeout` for the ACK.
async fn await_ack(Id(id): Id, State(state): State<AppState>) -> Result<(), RelayError> {
    let delivery = state
        .waiting_list
        .lock()
        .await
        .subscribe_ack(&id)?
        .ok_or(RelayError::NotFound)?;
    wait_for(delivery, state.config.inbox_timeout).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;

    use crate::endpoint::MAX_ID_LENGTH;
    use crate::server::{test_server, Config};

    #[tokio::test]
    async fn test_post_and_get() {
        let server = test_server(Config::default());

        let response = server
            .post("/inbox/test-123")
            .bytes(Bytes::from_static(b"hello inbox"))
            .await;
        assert_eq!(response.status_code(), 200);

        let response = server.get("/inbox/test-123").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "hello inbox");
    }

    #[tokio::test]
    async fn test_get_timeout_when_no_message() {
        let server = test_server(Config {
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        });

        let response = server.get("/inbox/nonexistent").await;
        assert_eq!(response.status_code(), 408);
    }

    #[tokio::test]
    async fn test_get_long_poll_receives_message() {
        let server = test_server(Config {
            inbox_timeout: Duration::from_secs(5),
            ..Config::default()
        });

        let getter = async {
            let response = server.get("/inbox/long-poll-test").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "delayed message");
        };

        let poster = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = Bytes::from_static(b"delayed message");
            let response = server.post("/inbox/long-poll-test").bytes(body).await;
            assert_eq!(response.status_code(), 200);
        };

        tokio::join!(getter, poster);
    }

    #[tokio::test]
    async fn test_delete_acks_message() {
        let server = test_server(Config {
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        });
        server
            .post("/inbox/ack-test")
            .bytes(Bytes::from_static(b"to be acked"))
            .await;

        let response = server.delete("/inbox/ack-test").await;
        assert_eq!(response.status_code(), 200);

        let response = server.get("/inbox/ack-test/ack").await;
        assert_eq!(response.text(), "true");

        // Message is cleared after ack - GET now waits and times out (408)
        let response = server.get("/inbox/ack-test").await;
        assert_eq!(response.status_code(), 408);
    }

    #[tokio::test]
    async fn test_delete_not_found() {
        let server = test_server(Config::default());

        let response = server.delete("/inbox/nonexistent").await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_ack_status_false() {
        let server = test_server(Config::default());
        server
            .post("/inbox/pending-ack")
            .bytes(Bytes::from_static(b"pending"))
            .await;

        let response = server.get("/inbox/pending-ack/ack").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "false");
    }

    #[tokio::test]
    async fn test_ack_status_nonexistent() {
        let server = test_server(Config::default());

        let response = server.get("/inbox/nonexistent/ack").await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_await_returns_when_acked() {
        let server = test_server(Config::default());
        server
            .post("/inbox/await-test")
            .bytes(Bytes::from_static(b"await test"))
            .await;

        let awaiter = async {
            let response = server.get("/inbox/await-test/await").await;
            assert_eq!(response.status_code(), 200);
        };

        let acker = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let response = server.delete("/inbox/await-test").await;
            assert_eq!(response.status_code(), 200);
        };

        tokio::join!(awaiter, acker);
    }

    #[tokio::test]
    async fn test_await_already_acked() {
        let server = test_server(Config::default());
        server
            .post("/inbox/pre-acked")
            .bytes(Bytes::from_static(b"pre-acked"))
            .await;
        server.delete("/inbox/pre-acked").await;

        let response = server.get("/inbox/pre-acked/await").await;
        assert_eq!(response.status_code(), 200);
    }

    #[tokio::test]
    async fn test_await_timeout() {
        let server = test_server(Config {
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        });
        server
            .post("/inbox/timeout-test")
            .bytes(Bytes::from_static(b"timeout test"))
            .await;

        let response = server.get("/inbox/timeout-test/await").await;
        assert_eq!(response.status_code(), 408);
    }

    #[tokio::test]
    async fn test_await_not_found() {
        let server = test_server(Config::default());

        let response = server.get("/inbox/nonexistent/await").await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_await_not_triggered_by_get() {
        let server = test_server(Config {
            inbox_timeout: Duration::from_millis(300),
            ..Config::default()
        });
        server
            .post("/inbox/get-doesnt-ack")
            .bytes(Bytes::from_static(b"test message"))
            .await;

        let awaiter = async { server.get("/inbox/get-doesnt-ack/await").await };

        let getter_and_acker = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let response = server.get("/inbox/get-doesnt-ack").await;
            assert_eq!(response.text(), "test message");

            tokio::time::sleep(Duration::from_millis(50)).await;
            server.delete("/inbox/get-doesnt-ack").await;
        };

        let (await_response, _) = tokio::join!(awaiter, getter_and_acker);

        // 200 after the ACK, not right after the GET and not a timeout
        assert_eq!(await_response.status_code(), 200);
    }

    #[tokio::test]
    async fn test_html_message_is_served_inert_with_its_content_type() {
        let server = test_server(Config::default());
        server
            .post("/inbox/html-test")
            .content_type("text/html")
            .bytes(Bytes::from_static(b"<script>alert(1)</script>"))
            .await;

        let response = server.get("/inbox/html-test").await;

        assert_eq!(response.status_code(), 200);
        assert_eq!(response.header("content-type"), "text/html");
        assert_eq!(response.header("content-security-policy"), "sandbox");
        assert_eq!(response.header("x-content-type-options"), "nosniff");
    }

    #[tokio::test]
    async fn test_post_overwrites_previous() {
        let server = test_server(Config::default());
        server
            .post("/inbox/overwrite")
            .bytes(Bytes::from_static(b"first"))
            .await;
        server
            .post("/inbox/overwrite")
            .bytes(Bytes::from_static(b"second"))
            .await;

        let response = server.get("/inbox/overwrite").await;
        assert_eq!(response.text(), "second");
    }

    #[tokio::test]
    async fn test_inbox_id_too_long() {
        let server = test_server(Config::default());
        let long_id = "x".repeat(MAX_ID_LENGTH + 1);

        let responses = [
            server
                .post(&format!("/inbox/{long_id}"))
                .bytes(Bytes::from_static(b"test"))
                .await,
            server.get(&format!("/inbox/{long_id}")).await,
            server.delete(&format!("/inbox/{long_id}")).await,
            server.get(&format!("/inbox/{long_id}/ack")).await,
            server.get(&format!("/inbox/{long_id}/await")).await,
        ];

        for response in responses {
            assert_eq!(response.status_code(), 400);
            assert_eq!(response.text(), "ID too long");
        }

        let ok_id = "x".repeat(MAX_ID_LENGTH);
        let response = server.get(&format!("/inbox/{ok_id}/ack")).await;
        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_get_beyond_waiting_id_limit_is_rejected() {
        let server = test_server(Config {
            max_entries: 1,
            inbox_timeout: Duration::from_secs(5),
            ..Config::default()
        });

        let waiting = async { server.get("/inbox/occupied").await };
        let rejected = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let response = server.get("/inbox/new").await;
            server
                .post("/inbox/occupied")
                .bytes(Bytes::from_static(b"release"))
                .await;
            response
        };

        let (waiting, rejected) = tokio::join!(waiting, rejected);

        assert_eq!(waiting.status_code(), 200);
        assert_eq!(rejected.status_code(), 503);
    }

    #[tokio::test]
    async fn test_message_expires() {
        let server = test_server(Config {
            inbox_cache_ttl: Duration::from_millis(300),
            inbox_timeout: Duration::from_millis(50),
            ..Config::default()
        });
        server
            .post("/inbox/expire-test")
            .bytes(Bytes::from_static(b"ephemeral"))
            .await;

        let response = server.get("/inbox/expire-test").await;
        assert_eq!(response.status_code(), 200);

        tokio::time::sleep(Duration::from_millis(350)).await;

        let response = server.get("/inbox/expire-test").await;
        assert_eq!(response.status_code(), 408);
    }

    #[tokio::test]
    async fn test_post_returns_503_when_store_is_full() {
        let server = test_server(Config {
            max_entries: 1,
            ..Config::default()
        });
        let body = Bytes::from_static(b"undelivered");
        server.post("/inbox/first").bytes(body).await;

        let body = Bytes::from_static(b"rejected");
        let response = server.post("/inbox/second").bytes(body).await;

        assert_eq!(response.status_code(), 503);
        assert_eq!(response.text(), "Server at capacity");
        assert_eq!(server.get("/inbox/first").await.text(), "undelivered");
    }

    #[tokio::test]
    async fn test_post_succeeds_when_full_store_has_acked_entry() {
        let server = test_server(Config {
            max_entries: 1,
            ..Config::default()
        });
        let body = Bytes::from_static(b"delivered");
        server.post("/inbox/first").bytes(body).await;
        server.delete("/inbox/first").await;

        let body = Bytes::from_static(b"accepted");
        let response = server.post("/inbox/second").bytes(body).await;

        assert_eq!(response.status_code(), 200);
        assert_eq!(server.get("/inbox/second").await.text(), "accepted");
        // The ACKed entry made room, so its ACK status is gone.
        assert_eq!(server.get("/inbox/first/ack").await.status_code(), 404);
    }

    // ========== Limit per client ==========

    /// Serves a relay behind a trusted proxy at 127.0.0.1, where each client
    /// may have one unACKed message stored.
    fn server_with_limit_of_one() -> axum_test::TestServer {
        test_server(Config {
            max_entries_per_client: 1,
            trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
            ..Config::default()
        })
    }

    /// Posts a message to `path` as the client at `client_ip`.
    async fn post_as(
        server: &axum_test::TestServer,
        client_ip: &str,
        path: &str,
    ) -> axum_test::TestResponse {
        server
            .post(path)
            .add_header("X-Forwarded-For", client_ip)
            .bytes(Bytes::from_static(b"message"))
            .await
    }

    #[tokio::test]
    async fn test_post_over_client_limit_returns_429_and_other_clients_still_post() {
        let server = server_with_limit_of_one();
        post_as(&server, "198.51.100.1", "/inbox/first").await;

        let rejected = post_as(&server, "198.51.100.1", "/inbox/second").await;
        let other_client = post_as(&server, "198.51.100.2", "/inbox/third").await;

        assert_eq!(rejected.status_code(), 429);
        assert_eq!(
            rejected.text(),
            "Too many unacknowledged messages from this client"
        );
        assert_eq!(other_client.status_code(), 200);
    }

    #[tokio::test]
    async fn test_ack_frees_room_under_the_client_limit() {
        let server = server_with_limit_of_one();
        post_as(&server, "198.51.100.1", "/inbox/first").await;

        server
            .delete("/inbox/first")
            .add_header("X-Forwarded-For", "198.51.100.2")
            .await
            .assert_status_ok();
        let response = post_as(&server, "198.51.100.1", "/inbox/second").await;

        assert_eq!(response.status_code(), 200);
    }

    #[tokio::test]
    async fn test_ipv6_addresses_in_one_56_network_share_the_client_limit() {
        let server = server_with_limit_of_one();
        post_as(&server, "2001:db8:0:1::8", "/inbox/first").await;

        let same_56 = post_as(&server, "2001:db8:0:ff::9", "/inbox/second").await;
        let next_56 = post_as(&server, "2001:db8:0:100::8", "/inbox/third").await;

        assert_eq!(same_56.status_code(), 429);
        assert_eq!(next_56.status_code(), 200);
    }
}
