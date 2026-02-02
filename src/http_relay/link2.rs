//! Link endpoint with caching enabled for mobile retry support.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use super::waiting_list::Message;

use super::AppState;

/// Build a response with optional Content-Type header.
fn build_response(status: StatusCode, body: Bytes, content_type: Option<String>) -> Response {
    let mut response = (status, body).into_response();
    if let Some(ct) = content_type {
        if let Ok(value) = ct.parse() {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}

/// A consumer requests data using GET method.
pub async fn get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let mut pending_list = state.pending_list.lock().await;

    // First, check if there's a cached value
    if let Some(cached) = pending_list.get_cached(&id) {
        return build_response(StatusCode::OK, cached.body, cached.content_type);
    }

    if let Some(producer) = pending_list.remove_producer(&id) {
        // Producer is ready to send data - cache it for future consumers
        let body = producer.body.clone();
        let content_type = producer.content_type.clone();
        pending_list.insert_cached(&id, body, content_type.clone(), state.config.cache_ttl);
        let _ = producer.completion.send(());
        return build_response(StatusCode::OK, producer.body, content_type);
    };

    // No producer ready. Insert consumer into pending list and wait.
    let receiver = pending_list.insert_consumer(&id);
    drop(pending_list);

    // Wait for the producer, but with a timeout to avoid proxy timeouts
    match tokio::time::timeout(state.config.link2_timeout, receiver).await {
        Ok(Ok(msg)) => build_response(StatusCode::OK, msg.body, msg.content_type),
        Ok(Err(_)) => build_response(StatusCode::NOT_FOUND, "Not Found".into(), None),
        Err(_) => {
            // Timeout. Remove the consumer from the pending list again
            let mut pending_list = state.pending_list.lock().await;
            pending_list.remove_consumer(&id);
            build_response(StatusCode::REQUEST_TIMEOUT, "Request timed out".into(), None)
        }
    }
}

/// A producer sends data using POST method.
pub async fn post_handler(
    Path(channel): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut pending_list = state.pending_list.lock().await;

    // If there's a cached value, remove it (new POST overwrites)
    pending_list.cache.remove(&channel);

    if let Some(consumer) = pending_list.remove_consumer(&channel) {
        // Consumer is ready to receive data - also cache it
        pending_list.insert_cached(&channel, body.clone(), content_type.clone(), state.config.cache_ttl);
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
    match tokio::time::timeout(state.config.link2_timeout, receiver).await {
        Ok(_) => (StatusCode::OK, Bytes::new()),
        Err(_) => {
            // Timeout. Remove the producer from the pending list again
            let mut pending_list = state.pending_list.lock().await;
            pending_list.remove_producer(&channel);
            (StatusCode::REQUEST_TIMEOUT, "Request timed out".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::http_relay::{Config, HttpRelay};

    #[tokio::test]
    async fn test_cached_value_multiple_consumers() {
        let config = Config {
            cache_ttl: Duration::from_secs(5),
            ..Config::default()
        };
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, _state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
