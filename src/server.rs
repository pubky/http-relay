//! The relay's configuration, router and server lifecycle.
//!
//! # Deployment assumptions
//!
//! Production deployments require a reverse proxy that parses HTTP. The proxy
//! terminates TLS, times out slow clients, and limits connections per client IP.
//! The relay itself has no authentication, no request rate limits, and no
//! protection against clients that send requests slowly. Its only limits per
//! client are on stored unACKed messages and on waiting requests. See
//! `DEPLOY.md`.
//!
//! # CORS Configuration
//!
//! By default, no CORS headers are added. This is suitable for deployments behind a reverse
//! proxy (nginx, caddy) that manages CORS. [`HttpRelayBuilder::cors_allow_all`] enables
//! permissive CORS (`Access-Control-Allow-Origin: *`), which allows web browsers to
//! communicate from any origin.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};

#[cfg(feature = "persist")]
use std::path::PathBuf;

use anyhow::Result;
use axum::{
    extract::{DefaultBodyLimit, Request},
    middleware,
    routing::get,
    Router,
};
use axum_server::Handle;
use ipnet::IpNet;
use tokio::{sync::Mutex, task::JoinHandle};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use url::Url;

use crate::client_ip::{identify_client, TrustedProxies};
use crate::storage::{Capacity, EntryRepository};
use crate::waiting_list::{WaitingLimits, WaitingList};

/// How often expired entries are deleted. Balances memory reclamation against
/// the cost of scanning all entries.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub(crate) struct AppState {
    pub config: Arc<Config>,
    pub waiting_list: Arc<Mutex<WaitingList>>,
}

#[derive(Debug)]
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
    /// Maximum stored entries, and maximum IDs with waiting requests.
    pub max_entries: usize,
    /// Maximum live unACKed entries stored by one client.
    pub max_entries_per_client: usize,
    /// Maximum open waiting requests of one client.
    pub max_waiting_per_client: usize,
    /// Path to SQLite database for persistent storage.
    /// If None, uses in-memory storage (data lost on restart).
    #[cfg(feature = "persist")]
    pub persist_db: Option<PathBuf>,
    /// When true, adds permissive CORS headers (`Access-Control-Allow-Origin: *`).
    pub cors_allow_all: bool,
    /// Proxy networks whose `X-Forwarded-For` header identifies the client. If
    /// empty, the client is always the TCP peer.
    pub trusted_proxies: Vec<IpNet>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 0,
            link_timeout: HttpRelayBuilder::DEFAULT_LINK_TIMEOUT,
            inbox_timeout: HttpRelayBuilder::DEFAULT_INBOX_TIMEOUT,
            inbox_cache_ttl: HttpRelayBuilder::DEFAULT_INBOX_CACHE_TTL,
            max_body_size: HttpRelayBuilder::DEFAULT_MAX_BODY_SIZE,
            max_entries: HttpRelayBuilder::DEFAULT_MAX_ENTRIES,
            max_entries_per_client: HttpRelayBuilder::DEFAULT_MAX_ENTRIES_PER_CLIENT,
            max_waiting_per_client: HttpRelayBuilder::DEFAULT_MAX_WAITING_PER_CLIENT,
            #[cfg(feature = "persist")]
            persist_db: None,
            cors_allow_all: false,
            trusted_proxies: Vec::new(),
        }
    }
}

impl Config {
    fn open_storage(&self) -> Result<EntryRepository> {
        let capacity = Capacity {
            max_entries: self.max_entries,
            max_entries_per_client: self.max_entries_per_client,
        };
        #[cfg(feature = "persist")]
        {
            match &self.persist_db {
                Some(path) => {
                    tracing::info!(path = %path.display(), "Persistence enabled with SQLite")
                }
                None => tracing::debug!("Using in-memory storage (no persistence)"),
            }
            EntryRepository::open(self.persist_db.as_deref(), capacity)
        }
        #[cfg(not(feature = "persist"))]
        {
            tracing::debug!("Using in-memory storage (no persistence)");
            Ok(EntryRepository::new(capacity))
        }
    }
}

/// Builder for [HttpRelay].
#[derive(Debug, Default)]
pub struct HttpRelayBuilder(Config);

impl HttpRelayBuilder {
    /// Default for [`Self::link_timeout`].
    pub const DEFAULT_LINK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
    /// Default for [`Self::inbox_timeout`].
    pub const DEFAULT_INBOX_TIMEOUT: Duration = Duration::from_secs(25);
    /// Default for [`Self::inbox_cache_ttl`].
    pub const DEFAULT_INBOX_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
    /// Default for [`Self::max_body_size`].
    pub const DEFAULT_MAX_BODY_SIZE: usize = 2 * 1024;
    /// Default for [`Self::max_entries`].
    pub const DEFAULT_MAX_ENTRIES: usize = 10_000;
    /// Default for [`Self::max_entries_per_client`].
    pub const DEFAULT_MAX_ENTRIES_PER_CLIENT: usize = 100;
    /// Default for [`Self::max_waiting_per_client`].
    pub const DEFAULT_MAX_WAITING_PER_CLIENT: usize = 100;

    /// Configure the address to bind to (default: 127.0.0.1).
    pub fn bind_address(mut self, addr: IpAddr) -> Self {
        self.0.bind_address = addr;
        self
    }

    /// Configure the port used for HTTP server (default: 0, a random free port).
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

    /// Configure the maximum stored entries (default: 10000).
    /// When the store is full, expired and ACKed entries make room for new IDs;
    /// if every entry holds an unACKed message, `POST` to a new ID gets
    /// `503 Service Unavailable`.
    /// It also limits the number of IDs with waiting requests; requests for new
    /// IDs beyond it get `503 Service Unavailable`.
    pub fn max_entries(mut self, max: usize) -> Self {
        self.0.max_entries = max;
        self
    }

    /// Configure the maximum unACKed messages one client can have stored
    /// (default: 100). `POST` of a further message gets `429 Too Many
    /// Requests`, until one of the client's messages is ACKed or expires.
    /// Replacing one of the client's unACKed messages is always allowed.
    ///
    /// A client is an IPv4 address or an IPv6 /56 network; see
    /// [`Self::trusted_proxies`] for how the relay finds it. At
    /// [`Self::max_entries`] or higher, one client can fill the store. Must be
    /// at least 1; otherwise [`Self::run`] fails.
    pub fn max_entries_per_client(mut self, max: usize) -> Self {
        self.0.max_entries_per_client = max;
        self
    }

    /// Configure the maximum requests one client can have waiting (default:
    /// 100). A further request that would wait gets `429 Too Many Requests`,
    /// until one of the client's waiting requests ends. Counted separately from
    /// [`Self::max_entries_per_client`].
    ///
    /// Waiting requests are `GET /inbox/{id}`, `GET /inbox/{id}/await`, and
    /// both `/link/{id}` methods. A `GET` for a message that is already stored
    /// doesn't wait, so the limit doesn't apply to it.
    ///
    /// Clients are grouped as for [`Self::max_entries_per_client`]. At
    /// [`Self::max_entries`] or higher, one client can take every slot for
    /// waiting IDs. Must be at least 1; otherwise [`Self::run`] fails.
    pub fn max_waiting_per_client(mut self, max: usize) -> Self {
        self.0.max_waiting_per_client = max;
        self
    }

    /// Configure the path to SQLite database for persistent storage.
    /// If not specified, uses in-memory storage (data lost on restart).
    #[cfg(feature = "persist")]
    pub fn persist_db(mut self, path: Option<PathBuf>) -> Self {
        self.0.persist_db = path;
        self
    }

    /// Enable permissive CORS headers (`Access-Control-Allow-Origin: *`).
    pub fn cors_allow_all(mut self, cors_allow_all: bool) -> Self {
        self.0.cors_allow_all = cors_allow_all;
        self
    }

    /// Identify clients behind these proxy networks by the `X-Forwarded-For` header
    /// (default: none, the client is always the TCP peer).
    ///
    /// For requests from a trusted proxy, the client is the rightmost entry that
    /// isn't itself a trusted proxy. Such requests without a valid header get
    /// `400 Bad Request`. Requests from other addresses are identified by their TCP
    /// peer, and their header is ignored. A trusted proxy can claim any client
    /// address, so list only proxies you control.
    pub fn trusted_proxies(mut self, networks: impl IntoIterator<Item = IpNet>) -> Self {
        self.0.trusted_proxies = networks.into_iter().collect();
        self
    }

    /// Start running an HTTP relay.
    pub async fn run(self) -> Result<HttpRelay> {
        HttpRelay::start(self.0).await
    }
}

/// Creates the span that `TraceLayer` wraps each request in.
///
/// Has the same fields as tower-http's default span, plus `client_ip`. The span
/// starts before the client is known, so [`identify_client`] fills it in later.
fn request_span(request: &Request) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %request.method(),
        uri = %request.uri(),
        version = ?request.version(),
        client_ip = tracing::field::Empty,
    )
}

/// Builds the router with all routes and middleware.
///
/// The router must be served with `ConnectInfo<SocketAddr>`, because
/// [`identify_client`] needs the TCP peer address.
fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/", get(|| async { "Http Relay" }))
        .merge(crate::inbox::routes());
    #[cfg(feature = "link-compat")]
    let router = router.merge(crate::link::routes());

    let router = router
        .layer(DefaultBodyLimit::max(state.config.max_body_size))
        .layer(middleware::from_fn_with_state(
            TrustedProxies::new(&state.config.trusted_proxies),
            identify_client,
        ))
        .layer(TraceLayer::new_for_http().make_span_with(request_span));

    let router = if state.config.cors_allow_all {
        router.layer(CorsLayer::very_permissive())
    } else {
        router
    };

    router.with_state(state)
}

fn app_state(config: Config) -> Result<AppState> {
    let limits = WaitingLimits {
        max_ids: config.max_entries,
        max_requests_per_client: config.max_waiting_per_client,
    };
    let waiting_list = WaitingList::new(config.open_storage()?, limits);
    Ok(AppState {
        config: Arc::new(config),
        waiting_list: Arc::new(Mutex::new(waiting_list)),
    })
}

/// Deletes expired entries every [`CLEANUP_INTERVAL`].
fn spawn_cleanup(waiting_list: Arc<Mutex<WaitingList>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CLEANUP_INTERVAL).await;
            match waiting_list.lock().await.remove_expired() {
                Ok(0) => {}
                Ok(removed) => tracing::debug!(removed, "Cleaned up expired entries"),
                Err(error) => tracing::error!(?error, "Failed to clean up expired entries"),
            }
        }
    })
}

/// Serves a test router on a random local port, because the router needs the
/// connection's peer address.
#[cfg(test)]
pub(crate) fn test_server(config: Config) -> axum_test::TestServer {
    let app = build_router(app_state(config).unwrap());
    axum_test::TestServer::new(app.into_make_service_with_connect_info::<SocketAddr>()).unwrap()
}

/// An implementation of _some_ of [Http relay spec](https://httprelay.io/).
pub struct HttpRelay {
    http_handle: Handle<SocketAddr>,
    http_address: SocketAddr,
    server_task: JoinHandle<std::io::Result<()>>,
    cleanup_task: JoinHandle<()>,
}

impl HttpRelay {
    async fn start(config: Config) -> Result<Self> {
        // A limit of 0 would reject every POST, or every waiting request.
        anyhow::ensure!(
            config.max_entries_per_client > 0,
            "max_entries_per_client must be at least 1"
        );
        anyhow::ensure!(
            config.max_waiting_per_client > 0,
            "max_waiting_per_client must be at least 1"
        );
        if !config.trusted_proxies.is_empty() {
            tracing::info!(
                trusted = ?config.trusted_proxies,
                "Identifying clients behind trusted proxies by X-Forwarded-For"
            );
        }
        let http_listener =
            TcpListener::bind(SocketAddr::new(config.bind_address, config.http_port))?;
        http_listener.set_nonblocking(true)?;
        let http_address = http_listener.local_addr()?;

        let state = app_state(config)?;
        let cleanup_task = spawn_cleanup(state.waiting_list.clone());
        let app = build_router(state);

        let http_handle = Handle::new();
        let server = axum_server::from_tcp(http_listener)?.handle(http_handle.clone());
        let server_task = tokio::spawn(async move {
            let result = server
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await;
            if let Err(error) = &result {
                tracing::error!(?error, "HttpRelay http server error");
            }
            result
        });

        Ok(Self {
            http_handle,
            http_address,
            server_task,
            cleanup_task,
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
            .expect("hardcoded URL scheme and localhost are always valid")
    }

    /// Returns the localhost URL of Link endpoints (deprecated, use inbox instead).
    #[cfg(feature = "link-compat")]
    pub fn local_link_url(&self) -> Url {
        self.local_url()
            .join("link")
            .expect("a relative path joins any http URL")
    }

    /// Stops accepting connections, gives open requests up to a second to
    /// finish, and waits for the server to stop.
    ///
    /// Returns the error the server stopped with, if any.
    pub async fn shutdown(mut self) -> Result<()> {
        self.http_handle
            .graceful_shutdown(Some(Duration::from_secs(1)));
        (&mut self.server_task).await??;
        Ok(())
    }
}

impl Drop for HttpRelay {
    fn drop(&mut self) {
        self.http_handle.shutdown();
        self.cleanup_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_run_rejects_a_client_limit_of_zero() {
        let result = HttpRelayBuilder::default()
            .http_port(0)
            .max_entries_per_client(0)
            .run()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_rejects_a_waiting_limit_of_zero() {
        let result = HttpRelayBuilder::default()
            .http_port(0)
            .max_waiting_per_client(0)
            .run()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_root_returns_http_relay() {
        let server = test_server(Config::default());
        let response = server.get("/").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "Http Relay");
    }

    #[tokio::test]
    async fn test_no_cors_headers_by_default() {
        let server = test_server(Config::default());
        let response = server
            .get("/")
            .add_header("Origin", "https://example.com")
            .await;
        assert!(response
            .maybe_header("access-control-allow-origin")
            .is_none());
    }

    #[tokio::test]
    async fn test_cors_allow_all_adds_headers() {
        let server = test_server(Config {
            cors_allow_all: true,
            ..Config::default()
        });
        let response = server
            .get("/")
            .add_header("Origin", "https://example.com")
            .await;
        assert!(response
            .maybe_header("access-control-allow-origin")
            .is_some());
    }

    #[tokio::test]
    async fn test_body_over_limit_is_rejected() {
        let server = test_server(Config {
            max_body_size: 4,
            ..Config::default()
        });

        let response = server.post("/inbox/id").text("too long").await;

        assert_eq!(response.status_code(), 413);
    }

    /// Tests the full HttpRelay::start() lifecycle: bind, serve, respond, shutdown.
    /// This covers the real server startup path that test_server() bypasses.
    #[tokio::test]
    async fn test_start_and_shutdown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let relay = HttpRelay::builder().run().await.unwrap();
        let addr = relay.http_address();

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();

        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Http Relay"));

        relay.shutdown().await.unwrap();
        assert!(TcpStream::connect(addr).await.is_err());
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn test_local_link_url() {
        let relay = HttpRelay::builder().run().await.unwrap();

        let url = relay.local_link_url();

        let port = relay.http_address().port();
        assert_eq!(url.as_str(), format!("http://localhost:{port}/link"));
    }
}
