//! Generic link handler that can operate with or without caching.

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use super::response::{await_consumer_message, await_producer_completion, build_response};
use super::waiting_list::Message;
use super::AppState;

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
    let receiver = pending_list.insert_consumer(&id);
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
    let receiver = pending_list.insert_producer(&channel, body, content_type);
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
}
