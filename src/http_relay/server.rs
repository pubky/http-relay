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
    sync::Arc,
    time::Duration,
};

use anyhow::Result;

use axum::{extract::DefaultBodyLimit, routing::get, Router};
use axum_server::Handle;
use tokio::sync::Mutex;

use tower_http::{cors::CorsLayer, trace::TraceLayer};
use url::Url;

use super::{link, link2};
use super::waiting_list::WaitingList;

/// The timeout for a request to be considered unused.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The default time-to-live for cached values after first consumer retrieves them.
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// The default timeout for link2 endpoints (shorter to avoid proxy timeouts like nginx).
const DEFAULT_LINK2_TIMEOUT: Duration = Duration::from_secs(25);

/// Default maximum request body size (10KB).
const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024;

/// Default maximum pending requests (producers + consumers).
const DEFAULT_MAX_PENDING: usize = 10_000;

/// Default maximum cached entries.
const DEFAULT_MAX_CACHE: usize = 10_000;

#[derive(Clone)]
pub(crate) struct AppState {
    pub config: Config,
    pub pending_list: Arc<Mutex<WaitingList>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let waiting_list = WaitingList::new(config.max_pending, config.max_cache);
        Self {
            config,
            pending_list: Arc::new(Mutex::new(waiting_list)),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub bind_address: IpAddr,
    pub http_port: u16,
    pub request_timeout: Duration,
    /// How long to keep values cached after the first consumer retrieves them.
    pub cache_ttl: Duration,
    /// Timeout for link2 endpoints (shorter to avoid proxy timeouts).
    pub link2_timeout: Duration,
    /// Maximum request body size in bytes.
    pub max_body_size: usize,
    /// Maximum pending requests (producers + consumers combined).
    pub max_pending: usize,
    /// Maximum cached entries.
    pub max_cache: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            http_port: 0,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            cache_ttl: DEFAULT_CACHE_TTL,
            link2_timeout: DEFAULT_LINK2_TIMEOUT,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_pending: DEFAULT_MAX_PENDING,
            max_cache: DEFAULT_MAX_CACHE,
        }
    }
}

/// Builder for [HttpRelay].
#[derive(Debug, Default)]
pub struct HttpRelayBuilder(Config);

impl HttpRelayBuilder {
    /// Configure the address to bind to (default: 0.0.0.0).
    pub fn bind_address(mut self, addr: IpAddr) -> Self {
        self.0.bind_address = addr;
        self
    }

    /// Configure the port used for HTTP server.
    pub fn http_port(mut self, port: u16) -> Self {
        self.0.http_port = port;
        self
    }

    /// Configure the TTL for cached values (default: 5 minutes).
    /// Values remain available for this duration after the first consumer
    /// retrieves them.
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.0.cache_ttl = ttl;
        self
    }

    /// Configure the timeout for link2 endpoints (default: 25 seconds).
    /// Shorter than the default request timeout to avoid proxy timeouts.
    pub fn link2_timeout(mut self, timeout: Duration) -> Self {
        self.0.link2_timeout = timeout;
        self
    }

    /// Configure the maximum request body size (default: 10KB).
    pub fn max_body_size(mut self, size: usize) -> Self {
        self.0.max_body_size = size;
        self
    }

    /// Configure the maximum pending requests (default: 10000).
    pub fn max_pending(mut self, max: usize) -> Self {
        self.0.max_pending = max;
        self
    }

    /// Configure the maximum cached entries (default: 10000).
    pub fn max_cache(mut self, max: usize) -> Self {
        self.0.max_cache = max;
        self
    }

    /// Start running an HTTP relay.
    pub async fn run(self) -> Result<HttpRelay> {
        HttpRelay::start(self.0).await
    }
}

/// An implementation of _some_ of [Http relay spec](https://httprelay.io/).
pub struct HttpRelay {
    pub(crate) http_handle: Handle,
    http_address: SocketAddr,
}

impl HttpRelay {
    /// Builds the router with all routes and middleware.
    fn build_router(state: AppState) -> Router {
        let max_body_size = state.config.max_body_size;
        Router::new()
            .route(
                "/link/{id}",
                get(link::get_handler).post(link::post_handler),
            )
            .route(
                "/link2/{id}",
                get(link2::get_handler).post(link2::post_handler),
            )
            .layer(DefaultBodyLimit::max(max_body_size))
            .layer(CorsLayer::very_permissive())
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    }

    /// Creates the HTTP router for the HTTP relay.
    #[cfg(test)]
    pub(crate) fn create_app(config: Config) -> Result<(Router, AppState)> {
        let app_state = AppState::new(config);
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
        let app_state = AppState::new(config.clone());
        let app = Self::build_router(app_state.clone());

        let http_handle = Handle::new();
        let shutdown_handle = http_handle.clone();

        let http_listener = TcpListener::bind(SocketAddr::new(config.bind_address, config.http_port))?;
        let http_address = http_listener.local_addr()?;

        tokio::spawn(async move {
            axum_server::from_tcp(http_listener)
                .handle(http_handle.clone())
                .serve(app.into_make_service())
                .await
                .map_err(|error| tracing::error!(?error, "HttpRelay http server error"))
        });

        // Spawn background task to clean up expired cache entries
        let cleanup_interval = Duration::from_secs(1);
        let pending_list = app_state.pending_list.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(cleanup_interval).await;
                let mut list = pending_list.lock().await;
                let removed = list.cleanup_expired_cache();
                if removed > 0 {
                    tracing::debug!(removed, "Cleaned up expired cache entries");
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
        Url::parse(&format!("http://localhost:{}", self.http_address.port()))
            .expect("local_url should be formatted fine")
    }

    /// Returns the localhost URL of Link endpoints
    pub fn local_link_url(&self) -> Url {
        let mut url = self.local_url();

        let mut segments = url
            .path_segments_mut()
            .expect("HttpRelay::local_link_url path_segments_mut");

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
