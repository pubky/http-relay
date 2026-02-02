//! Standard link endpoint without caching - closer to httprelay.io spec.

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

    if let Some(producer) = pending_list.remove_producer(&id) {
        let _ = producer.completion.send(());
        return build_response(StatusCode::OK, producer.body, producer.content_type);
    };

    // No producer ready. Insert consumer into pending list and wait.
    let receiver = pending_list.insert_consumer(&id);
    drop(pending_list);

    // Wait for the producer, but with a timeout
    match tokio::time::timeout(state.config.request_timeout, receiver).await {
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

    if let Some(consumer) = pending_list.remove_consumer(&channel) {
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
    match tokio::time::timeout(state.config.request_timeout, receiver).await {
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
    async fn test_delayed_producer() {
        let (app, state) = HttpRelay::create_app(Config::default()).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(Config::default()).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, state) = HttpRelay::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
        let (app, _state) = HttpRelay::create_app(Config::default()).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();

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
