//! Generic link handler that can operate with or without caching.
//!
//! # Security
//!
//! Channel IDs act as shared secrets. Anyone who knows an ID can read/write to that
//! channel. IDs must be cryptographically random (e.g., 128-bit UUIDs). Predictable
//! IDs allow attackers to intercept messages.
//!
//! When caching is enabled (`/link2`), delivered messages stay in plaintext memory
//! for the TTL duration. Multiple consumers can retrieve the same cached value.
//! Do not relay sensitive one-time credentials via cached endpoints.

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use super::response::{await_consumer_message, await_producer_completion, build_response};
use super::waiting_list::{LimitError, Message};
use super::AppState;

/// Maximum allowed length for channel IDs (in bytes).
/// Prevents DoS via extremely long IDs used as HashMap keys.
const MAX_CHANNEL_ID_LENGTH: usize = 256;

/// Configuration for link handler behavior.
#[derive(Clone, Copy)]
pub struct LinkConfig {
    /// Whether to use caching for this endpoint.
    pub caching_enabled: bool,
}

impl LinkConfig {
    /// Standard link endpoint without caching.
    pub const STANDARD: Self = Self {
        caching_enabled: false,
    };

    /// Link2 endpoint with caching enabled.
    pub const WITH_CACHE: Self = Self {
        caching_enabled: true,
    };
}

/// Returns the timeout duration based on link config.
fn get_timeout(state: &AppState, config: LinkConfig) -> Duration {
    if config.caching_enabled {
        state.config.link2_timeout
    } else {
        state.config.request_timeout
    }
}

/// A consumer requests data using GET method.
pub async fn get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    config: LinkConfig,
) -> Response {
    if id.len() > MAX_CHANNEL_ID_LENGTH {
        return build_response(StatusCode::BAD_REQUEST, "Channel ID too long".into(), None);
    }

    let mut pending_list = state.pending_list.lock().await;

    // Check cache if caching is enabled
    if config.caching_enabled {
        if let Some(cached) = pending_list.get_cached(&id) {
            return build_response(StatusCode::OK, cached.body, cached.content_type);
        }
    }

    if let Some(producer) = pending_list.remove_producer(&id) {
        // Cache the response if caching is enabled
        if config.caching_enabled {
            pending_list.insert_cached(
                &id,
                producer.body.clone(),
                producer.content_type.clone(),
                state.config.cache_ttl,
            );
        }
        let _ = producer.completion.send(());
        return build_response(StatusCode::OK, producer.body, producer.content_type);
    };

    // No producer ready. Insert consumer into pending list and wait.
    let receiver = match pending_list.insert_consumer(&id) {
        Ok(r) => r,
        Err(LimitError::PendingLimitReached) => {
            return build_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server at capacity".into(),
                None,
            );
        }
    };
    drop(pending_list);

    let timeout = get_timeout(&state, config);
    let pending_list = state.pending_list.clone();
    await_consumer_message(receiver, timeout, || async move {
        pending_list.lock().await.remove_consumer(&id);
    })
    .await
}

/// A producer sends data using POST method.
pub async fn post_handler(
    Path(channel): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
    config: LinkConfig,
) -> impl IntoResponse {
    if channel.len() > MAX_CHANNEL_ID_LENGTH {
        return (StatusCode::BAD_REQUEST, Bytes::from("Channel ID too long"));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut pending_list = state.pending_list.lock().await;

    // Invalidate cache if caching is enabled
    if config.caching_enabled {
        pending_list.remove_cached(&channel);
    }

    if let Some(consumer) = pending_list.remove_consumer(&channel) {
        // Cache the response if caching is enabled
        if config.caching_enabled {
            pending_list.insert_cached(
                &channel,
                body.clone(),
                content_type.clone(),
                state.config.cache_ttl,
            );
        }
        let msg = Message {
            body,
            content_type,
        };
        let _ = consumer.message_sender.send(msg);
        return (StatusCode::OK, Bytes::new());
    };

    // No consumer ready. Insert producer into pending list and wait.
    let receiver = match pending_list.insert_producer(&channel, body, content_type) {
        Ok(r) => r,
        Err(LimitError::PendingLimitReached) => {
            return (StatusCode::SERVICE_UNAVAILABLE, Bytes::from("Server at capacity"));
        }
    };
    drop(pending_list);

    let timeout = get_timeout(&state, config);
    let pending_list = state.pending_list.clone();
    await_producer_completion(receiver, timeout, || async move {
        pending_list.lock().await.remove_producer(&channel);
    })
    .await
}

// Thin wrapper handlers for the /link/ endpoint (no caching)
pub mod link {
    use super::*;

    pub async fn get_handler(path: Path<String>, state: State<AppState>) -> Response {
        super::get_handler(path, state, LinkConfig::STANDARD).await
    }

    pub async fn post_handler(
        path: Path<String>,
        state: State<AppState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        super::post_handler(path, state, headers, body, LinkConfig::STANDARD).await
    }
}

// Thin wrapper handlers for the /link2/ endpoint (with caching)
pub mod link2 {
    use super::*;

    pub async fn get_handler(path: Path<String>, state: State<AppState>) -> Response {
        super::get_handler(path, state, LinkConfig::WITH_CACHE).await
    }

    pub async fn post_handler(
        path: Path<String>,
        state: State<AppState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        super::post_handler(path, state, headers, body, LinkConfig::WITH_CACHE).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::http_relay::{Config, HttpRelay};

    // Tests for standard link endpoint (no caching)
    mod link_tests {
        use super::*;

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
                let body = axum::body::Bytes::from_static(b"Hello, world!");
                let response = server.post("/link/123").bytes(body).await;
                assert_eq!(response.status_code(), 200);
                assert_eq!(response.text(), "");
            };

            tokio::join!(consumer, producer);
            assert!(state.pending_list.lock().await.is_empty());
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
                let body = axum::body::Bytes::from_static(b"Hello, world!");
                let response = server.post("/link/123").bytes(body).await;
                assert_eq!(response.status_code(), 200);
                assert_eq!(response.text(), "");
            };

            tokio::join!(consumer, producer);
            assert!(state.pending_list.lock().await.is_empty());
        }

        #[tokio::test]
        async fn test_request_timeout() {
            let config = Config {
                request_timeout: Duration::from_millis(50),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // Consumer request timed out
            let response = server.get("/link/123").await;
            assert_eq!(response.status_code(), 408);
            assert_eq!(response.text(), "Request timed out");
            assert!(state.pending_list.lock().await.is_empty());

            // Producer request timed out
            let body = axum::body::Bytes::from_static(b"Hello, world!");
            let response = server.post("/link/123").bytes(body).await;
            assert_eq!(response.status_code(), 408);
            assert_eq!(response.text(), "Request timed out");
            assert!(state.pending_list.lock().await.is_empty());
        }

        #[tokio::test]
        async fn test_no_caching() {
            let config = Config {
                cache_ttl: Duration::from_secs(5),
                request_timeout: Duration::from_millis(100),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // Producer sends, consumer receives - but no caching on /link/
            let producer = async {
                let body = axum::body::Bytes::from_static(b"no cache data");
                server.post("/link/no-cache-test").bytes(body).await;
            };
            let consumer = async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let response = server.get("/link/no-cache-test").await;
                assert_eq!(response.text(), "no cache data");
            };
            tokio::join!(producer, consumer);

            // Value should NOT be cached for /link/
            assert_eq!(state.pending_list.lock().await.cache_len(), 0);

            // Second consumer should timeout since no caching
            let response = server.get("/link/no-cache-test").await;
            assert_eq!(response.status_code(), 408);
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
                let body = axum::body::Bytes::from_static(br#"{"key":"value"}"#);
                server
                    .post("/link/ct-test")
                    .content_type("application/json")
                    .bytes(body)
                    .await;
            };

            tokio::join!(consumer, producer);
        }
    }

    // Tests for link2 endpoint (with caching)
    mod link2_tests {
        use super::*;

        #[tokio::test]
        async fn test_cached_value_multiple_consumers() {
            let config = Config {
                cache_ttl: Duration::from_secs(5),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // Producer sends data
            let producer = async {
                let body = axum::body::Bytes::from_static(b"cached data");
                let response = server.post("/link2/cache-test").bytes(body).await;
                assert_eq!(response.status_code(), 200);
            };

            // First consumer receives it
            let first_consumer = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let response = server.get("/link2/cache-test").await;
                assert_eq!(response.status_code(), 200);
                assert_eq!(response.text(), "cached data");
            };

            tokio::join!(producer, first_consumer);

            // Value should now be cached
            assert_eq!(state.pending_list.lock().await.cache_len(), 1);

            // Second consumer can get the same cached value
            let response = server.get("/link2/cache-test").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "cached data");

            // Third consumer also gets it
            let response = server.get("/link2/cache-test").await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.text(), "cached data");
        }

        #[tokio::test]
        async fn test_cache_expires() {
            let config = Config {
                cache_ttl: Duration::from_millis(50),
                link2_timeout: Duration::from_millis(100),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // Producer sends, consumer receives (value gets cached)
            let producer = async {
                let body = axum::body::Bytes::from_static(b"ephemeral");
                server.post("/link2/expire-test").bytes(body).await;
            };
            let consumer = async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let response = server.get("/link2/expire-test").await;
                assert_eq!(response.text(), "ephemeral");
            };
            tokio::join!(producer, consumer);

            // Value is cached
            assert!(state
                .pending_list
                .lock()
                .await
                .get_cached("expire-test")
                .is_some());

            // Wait for cache to expire
            tokio::time::sleep(Duration::from_millis(1500)).await;

            // Value should be expired (get_cached returns None)
            assert!(state
                .pending_list
                .lock()
                .await
                .get_cached("expire-test")
                .is_none());
        }

        #[tokio::test]
        async fn test_post_overwrites_cache() {
            let config = Config {
                cache_ttl: Duration::from_secs(5),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // First producer-consumer pair
            let producer1 = async {
                let body = axum::body::Bytes::from_static(b"first value");
                server.post("/link2/overwrite-test").bytes(body).await;
            };
            let consumer1 = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                server.get("/link2/overwrite-test").await
            };
            tokio::join!(producer1, consumer1);

            // Verify first value is cached
            let cached = state
                .pending_list
                .lock()
                .await
                .get_cached("overwrite-test");
            assert_eq!(cached.unwrap().body.as_ref(), b"first value");

            // Second producer posts new value - should overwrite and wait
            let producer2 = async {
                let body = axum::body::Bytes::from_static(b"second value");
                server.post("/link2/overwrite-test").bytes(body).await;
            };
            let consumer2 = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                server.get("/link2/overwrite-test").await
            };
            let (_, response) = tokio::join!(producer2, consumer2);
            assert_eq!(response.text(), "second value");

            // Verify new value is cached
            let cached = state
                .pending_list
                .lock()
                .await
                .get_cached("overwrite-test");
            assert_eq!(cached.unwrap().body.as_ref(), b"second value");
        }

        #[tokio::test]
        async fn test_producer_timeout_does_not_cache() {
            let config = Config {
                link2_timeout: Duration::from_millis(50),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // Producer posts without any consumer waiting - should timeout
            let body = axum::body::Bytes::from_static(b"should not cache");
            let response = server.post("/link2/timeout-test").bytes(body).await;
            assert_eq!(response.status_code(), 408); // REQUEST_TIMEOUT

            // Value should NOT be cached
            assert!(state
                .pending_list
                .lock()
                .await
                .get_cached("timeout-test")
                .is_none());
        }

        #[tokio::test]
        async fn test_consumer_timeout() {
            let config = Config {
                link2_timeout: Duration::from_millis(50),
                ..Config::default()
            };
            let (server, _state) = HttpRelay::create_test_server(config);

            // Consumer waits without any producer - should timeout
            let response = server.get("/link2/consumer-timeout-test").await;
            assert_eq!(response.status_code(), 408); // REQUEST_TIMEOUT
        }

        #[tokio::test]
        async fn test_consumer_first_then_producer_caches() {
            let config = Config {
                cache_ttl: Duration::from_secs(5),
                ..Config::default()
            };
            let (server, state) = HttpRelay::create_test_server(config);

            // Consumer waits first, then producer sends
            let consumer = async {
                let response = server.get("/link2/consumer-first").await;
                assert_eq!(response.status_code(), 200);
                assert_eq!(response.text(), "delayed data");
            };

            let producer = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let body = axum::body::Bytes::from_static(b"delayed data");
                let response = server.post("/link2/consumer-first").bytes(body).await;
                assert_eq!(response.status_code(), 200);
            };

            tokio::join!(consumer, producer);

            // Value should be cached for subsequent consumers
            assert_eq!(state.pending_list.lock().await.cache_len(), 1);
            let response = server.get("/link2/consumer-first").await;
            assert_eq!(response.text(), "delayed data");
        }
    }

    // Tests for resource limits
    mod limit_tests {
        use super::*;
        use crate::http_relay::link_handler::MAX_CHANNEL_ID_LENGTH;
        use crate::http_relay::waiting_list::{LimitError, WaitingList};
        use axum::body::Bytes;

        #[test]
        fn test_pending_limit_consumer() {
            let mut list = WaitingList::new(2, 10);

            // First two consumers succeed
            assert!(list.insert_consumer("c1").is_ok());
            assert!(list.insert_consumer("c2").is_ok());
            assert_eq!(list.pending_count(), 2);

            // Third consumer fails
            assert_eq!(
                list.insert_consumer("c3").unwrap_err(),
                LimitError::PendingLimitReached
            );
        }

        #[test]
        fn test_pending_limit_producer() {
            let mut list = WaitingList::new(2, 10);

            // First two producers succeed
            assert!(list.insert_producer("p1", Bytes::new(), None).is_ok());
            assert!(list.insert_producer("p2", Bytes::new(), None).is_ok());
            assert_eq!(list.pending_count(), 2);

            // Third producer fails
            assert_eq!(
                list.insert_producer("p3", Bytes::new(), None).unwrap_err(),
                LimitError::PendingLimitReached
            );
        }

        #[test]
        fn test_pending_limit_mixed() {
            let mut list = WaitingList::new(2, 10);

            // One consumer and one producer
            assert!(list.insert_consumer("c1").is_ok());
            assert!(list.insert_producer("p1", Bytes::new(), None).is_ok());
            assert_eq!(list.pending_count(), 2);

            // Both consumer and producer fail at limit
            assert_eq!(
                list.insert_consumer("c2").unwrap_err(),
                LimitError::PendingLimitReached
            );
            assert_eq!(
                list.insert_producer("p2", Bytes::new(), None).unwrap_err(),
                LimitError::PendingLimitReached
            );
        }

        #[test]
        fn test_cache_lru_eviction() {
            let mut list = WaitingList::new(10, 2);
            let ttl = std::time::Duration::from_secs(60);

            // Insert two entries
            list.insert_cached("k1", Bytes::from("first"), None, ttl);
            list.insert_cached("k2", Bytes::from("second"), None, ttl);
            assert_eq!(list.cache_len(), 2);

            // Access k1 to make it recently used (k2 is now LRU)
            assert!(list.get_cached("k1").is_some());

            // Third insert should evict k2 (least recently used)
            list.insert_cached("k3", Bytes::from("third"), None, ttl);
            assert_eq!(list.cache_len(), 2);

            // k2 should be gone (was LRU), k1 and k3 should remain
            assert!(list.get_cached("k1").is_some());
            assert!(list.get_cached("k2").is_none());
            assert!(list.get_cached("k3").is_some());
        }

        #[test]
        fn test_removing_frees_capacity() {
            let mut list = WaitingList::new(1, 10);

            // Insert consumer at limit
            assert!(list.insert_consumer("c1").is_ok());
            assert!(list.insert_consumer("c2").is_err());

            // Remove it
            list.remove_consumer("c1");

            // Now we can insert again
            assert!(list.insert_consumer("c2").is_ok());
        }

        #[tokio::test]
        async fn test_channel_id_too_long() {
            let (server, _state) = HttpRelay::create_test_server(Config::default());

            // Create an ID that exceeds the limit
            let long_id = "x".repeat(MAX_CHANNEL_ID_LENGTH + 1);

            // GET should reject long IDs
            let response = server.get(&format!("/link/{}", long_id)).await;
            assert_eq!(response.status_code(), 400);
            assert_eq!(response.text(), "Channel ID too long");

            // POST should reject long IDs
            let body = axum::body::Bytes::from_static(b"test");
            let response = server.post(&format!("/link/{}", long_id)).bytes(body).await;
            assert_eq!(response.status_code(), 400);
            assert_eq!(response.text(), "Channel ID too long");

            // IDs at the limit should work
            let ok_id = "x".repeat(MAX_CHANNEL_ID_LENGTH);
            let response = server.get(&format!("/link2/{}", ok_id)).await;
            assert_ne!(response.status_code(), 400); // Should timeout, not reject
        }
    }
}
