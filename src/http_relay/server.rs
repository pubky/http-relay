//! HTTP relay server implementation.
//!
//! # CORS Configuration
//!
//! This server uses permissive CORS (`Access-Control-Allow-Origin: *`) to allow
//! web browsers to communicate from any origin. This is intentional for public
//! relay deployments. For restricted environments, modify `CorsLayer::very_permissive()`
//! in `build_router()`.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;

use axum::{extract::DefaultBodyLimit, routing::get, Router};
use axum_server::Handle;
use tokio::sync::Mutex;

use tower_http::{cors::CorsLayer, trace::TraceLayer};
use url::Url;

use super::persistence::EntryRepository;
use super::waiting_list::WaitingList;
use super::{inbox, link};

/// The default timeout for link endpoints (synchronous handoff).
const DEFAULT_LINK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The default timeout for inbox long-poll endpoints (shorter to avoid proxy timeouts).
const DEFAULT_INBOX_TIMEOUT: Duration = Duration::from_secs(25);

/// The default time-to-live for inbox messages.
const DEFAULT_INBOX_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Default maximum request body size (2KB).
const DEFAULT_MAX_BODY_SIZE: usize = 2 * 1024;

/// Default maximum entries in the unified LRU cache.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

#[derive(Clone)]
pub(crate) struct AppState {
    pub config: Config,
    pub pending_list: Arc<Mutex<WaitingList>>,
}

impl AppState {
    /// Creates a new AppState. Returns error if persistence initialization fails.
    pub fn new(config: Config) -> anyhow::Result<Self> {
        if let Some(path) = &config.persist_db {
            tracing::info!(path = %path.display(), "Persistence enabled with SQLite");
        } else {
            tracing::debug!("Using in-memory storage (no persistence)");
        }

        let repository = EntryRepository::new(
            config.persist_db.as_deref(),
            config.max_entries,
        )?;
        let waiting_list = WaitingList::new(repository);
        Ok(Self {
            config,
            pending_list: Arc::new(Mutex::new(waiting_list)),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub bind_address: IpAddr,
    pub http_port: u16,
    /// Timeout for link endpoints (synchronous handoff wait time).
    pub link_timeout: Duration,
    /// Timeout for inbox long-poll endpoints (shorter to avoid proxy timeouts).
    pub inbox_timeout: Duration,
    /// How long inbox messages persist before expiring.
    pub inbox_cache_ttl: Duration,
    /// Maximum request body size in bytes.
    pub max_body_size: usize,
    /// Maximum entries in the waiting list.
    pub max_entries: usize,
    /// Path to SQLite database for persistent storage.
    /// If None, uses in-memory storage (data lost on restart).
    pub persist_db: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            http_port: 0,
            link_timeout: DEFAULT_LINK_TIMEOUT,
            inbox_timeout: DEFAULT_INBOX_TIMEOUT,
            inbox_cache_ttl: DEFAULT_INBOX_CACHE_TTL,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_entries: DEFAULT_MAX_ENTRIES,
            persist_db: None,
        }
    }
}

/// Builder for [HttpRelay].
#[derive(Debug, Default)]
pub struct HttpRelayBuilder(Config);

impl HttpRelayBuilder {
    /// Configure the address to bind to (default: 127.0.0.1).
    pub fn bind_address(mut self, addr: IpAddr) -> Self {
        self.0.bind_address = addr;
        self
    }

    /// Configure the port used for HTTP server.
    pub fn http_port(mut self, port: u16) -> Self {
        self.0.http_port = port;
        self
    }

    /// Configure the timeout for link endpoints (default: 10 minutes).
    /// This is how long producers wait for consumers (and vice versa).
    pub fn link_timeout(mut self, timeout: Duration) -> Self {
        self.0.link_timeout = timeout;
        self
    }

    /// Configure the timeout for inbox long-poll endpoints (default: 25 seconds).
    /// Shorter than the default request timeout to avoid proxy timeouts.
    pub fn inbox_timeout(mut self, timeout: Duration) -> Self {
        self.0.inbox_timeout = timeout;
        self
    }

    /// Configure the TTL for inbox messages (default: 5 minutes).
    /// Messages persist for this duration after being sent.
    pub fn inbox_cache_ttl(mut self, ttl: Duration) -> Self {
        self.0.inbox_cache_ttl = ttl;
        self
    }

    /// Configure the maximum request body size (default: 2KB).
    pub fn max_body_size(mut self, size: usize) -> Self {
        self.0.max_body_size = size;
        self
    }

    /// Configure the maximum entries in the waiting list (default: 10000).
    /// When this limit is reached, oldest entries are evicted.
    pub fn max_entries(mut self, max: usize) -> Self {
        self.0.max_entries = max;
        self
    }

    /// Configure the path to SQLite database for persistent storage.
    /// If not specified, uses in-memory storage (data lost on restart).
    pub fn persist_db(mut self, path: Option<PathBuf>) -> Self {
        self.0.persist_db = path;
        self
    }

    /// Start running an HTTP relay.
    pub async fn run(self) -> Result<HttpRelay> {
        HttpRelay::start(self.0).await
    }
}

/// An implementation of _some_ of [Http relay spec](https://httprelay.io/).
pub struct HttpRelay {
    pub(crate) http_handle: Handle<SocketAddr>,
    http_address: SocketAddr,
}

impl HttpRelay {
    /// Builds the router with all routes and middleware.
    fn build_router(state: AppState) -> Router {
        let max_body_size = state.config.max_body_size;
        Router::new()
            .route("/", get(|| async { "Http Relay" }))
            .route(
                "/link/{id}",
                get(link::get_handler).post(link::post_handler),
            )
            // Inbox: specific routes first to avoid conflicts with /{id}
            .route("/inbox/{id}/ack", get(inbox::ack_handler))
            .route("/inbox/{id}/await", get(inbox::await_handler))
            .route(
                "/inbox/{id}",
                get(inbox::get_handler)
                    .post(inbox::post_handler)
                    .delete(inbox::delete_handler),
            )
            .layer(DefaultBodyLimit::max(max_body_size))
            .layer(CorsLayer::very_permissive())
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    }

    /// Creates the HTTP router for the HTTP relay.
    #[cfg(test)]
    pub(crate) fn create_app(config: Config) -> Result<(Router, AppState)> {
        let app_state = AppState::new(config)?;
        let app = Self::build_router(app_state.clone());
        Ok((app, app_state))
    }

    /// Creates a test server with the given config. Returns both the server and app state.
    #[cfg(test)]
    pub(crate) fn create_test_server(config: Config) -> (axum_test::TestServer, AppState) {
        let (app, state) = Self::create_app(config).unwrap();
        let server = axum_test::TestServer::new(app).unwrap();
        (server, state)
    }

    async fn start(config: Config) -> Result<Self> {
        let app_state = AppState::new(config.clone())?;
        let app = Self::build_router(app_state.clone());

        let http_handle = Handle::new();
        let shutdown_handle = http_handle.clone();

        let http_listener =
            TcpListener::bind(SocketAddr::new(config.bind_address, config.http_port))?;
        http_listener.set_nonblocking(true)?;
        let http_address = http_listener.local_addr()?;

        let server = axum_server::from_tcp(http_listener)?;
        tokio::spawn(async move {
            server
                .handle(http_handle.clone())
                .serve(app.into_make_service())
                .await
                .map_err(|error| tracing::error!(?error, "HttpRelay http server error"))
        });

        // Spawn background task to clean up expired entries.
        // 15 seconds balances memory reclamation (don't let stale entries pile up)
        // against CPU overhead (cleanup scans the entire LRU cache).
        let cleanup_interval = Duration::from_secs(15);
        let pending_list = app_state.pending_list.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(cleanup_interval).await;
                let removed = pending_list.lock().await.cleanup_expired();
                if removed > 0 {
                    tracing::debug!(removed, "Cleaned up expired entries");
                }
            }
        });

        Ok(Self {
            http_handle: shutdown_handle,
            http_address,
        })
    }

    /// Create [HttpRelayBuilder].
    pub fn builder() -> HttpRelayBuilder {
        HttpRelayBuilder::default()
    }

    /// Returns the HTTP address of this http relay.
    pub fn http_address(&self) -> SocketAddr {
        self.http_address
    }

    /// Returns the localhost Url of this server.
    pub fn local_url(&self) -> Url {
        // Infallible: "http://localhost:{port}" is always a valid URL format
        Url::parse(&format!("http://localhost:{}", self.http_address.port()))
            .expect("hardcoded URL scheme and localhost are always valid")
    }

    /// Returns the localhost URL of Link endpoints
    pub fn local_link_url(&self) -> Url {
        let mut url = self.local_url();

        // Infallible: http:// URLs always support path segments (only cannot-be-a-base URLs fail)
        let mut segments = url
            .path_segments_mut()
            .expect("http URLs always have path segments");

        segments.push("link");

        drop(segments);

        url
    }

    /// Gracefully shuts down the HTTP relay.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.http_handle
            .graceful_shutdown(Some(Duration::from_secs(1)));
        Ok(())
    }
}

impl Drop for HttpRelay {
    fn drop(&mut self) {
        self.http_handle.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_root_returns_http_relay() {
        let (server, _state) = HttpRelay::create_test_server(Config::default());
        let response = server.get("/").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "Http Relay");
    }
}
