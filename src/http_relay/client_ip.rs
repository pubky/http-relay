//! Identifies the client behind each request.
//!
//! Limits per client need the real client address, even behind a reverse proxy.
//! Behind a proxy, the TCP peer is the proxy, so the relay reads the client
//! address from the `X-Forwarded-For` header instead. A client can send that
//! header too, so the relay only reads it from trusted proxy networks, and only
//! the entries those proxies added. `DEPLOY.md` shows the matching Nginx setup.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ipnet::IpNet;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The IP address of the client that sent a request.
///
/// [`identify_client`] adds it to every request as an extension, so handlers can
/// read it with `Extension<ClientIp>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClientIp(
    #[allow(dead_code, reason = "read by the planned per-client limits")] pub IpAddr,
);

/// The proxy networks whose `X-Forwarded-For` entries the relay trusts.
///
/// Empty by default: the client is then always the TCP peer.
#[derive(Debug, Clone, Default)]
pub(crate) struct TrustedProxies(Arc<[IpNet]>);

impl TrustedProxies {
    pub fn new(networks: &[IpNet]) -> Self {
        Self(networks.into())
    }

    /// Returns the IP address of the client that sent a request over a TCP
    /// connection from `peer_ip`.
    ///
    /// An untrusted peer is the client itself, and its `X-Forwarded-For` header is
    /// ignored. For a trusted proxy, the client is the rightmost entry in
    /// `X-Forwarded-For` that isn't itself a trusted proxy. If every entry is a
    /// trusted proxy, the leftmost entry is the client.
    ///
    /// IPv4-mapped IPv6 addresses (`::ffff:198.51.100.8`) are returned as IPv4, so
    /// the same client always gets the same address, however it was found.
    ///
    /// # Errors
    ///
    /// Fails if a trusted proxy sends no entries, or if an entry checked before the
    /// client is found isn't a plain IP address. Both mean that the proxy is
    /// misconfigured.
    pub fn client_ip(
        &self,
        peer_ip: IpAddr,
        headers: &HeaderMap,
    ) -> Result<IpAddr, ForwardedForError> {
        if !self.contains(peer_ip) {
            return Ok(peer_ip.to_canonical());
        }

        // Each proxy appends the address it received the connection from, so the
        // entries on the right come from trusted proxies. Anything to the left of
        // the client may be forged and is never parsed.
        let mut leftmost_proxy_ip = None;
        for value in headers.get_all(X_FORWARDED_FOR).iter().rev() {
            // Escape non-ASCII bytes, such as `\xe9`, so the log shows them exactly.
            let value = value.to_str().map_err(|_| {
                ForwardedForError::NotAnIpAddress(value.as_bytes().escape_ascii().to_string())
            })?;
            for entry in value
                .rsplit(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
            {
                let entry_ip: IpAddr = entry
                    .parse()
                    .map_err(|_| ForwardedForError::NotAnIpAddress(entry.to_owned()))?;
                if !self.contains(entry_ip) {
                    return Ok(entry_ip.to_canonical());
                }
                leftmost_proxy_ip = Some(entry_ip.to_canonical());
            }
        }
        leftmost_proxy_ip.ok_or(ForwardedForError::Missing)
    }

    fn contains(&self, ip: IpAddr) -> bool {
        // A dual-stack listener reports IPv4 peers as IPv4-mapped IPv6 addresses
        // (`::ffff:127.0.0.1`), which wouldn't match IPv4 networks.
        let ip = ip.to_canonical();
        self.0.iter().any(|network| network.contains(&ip))
    }
}

/// Why the relay couldn't read the client address from a trusted proxy's
/// `X-Forwarded-For` header.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ForwardedForError {
    /// The proxy sent no `X-Forwarded-For` entries.
    Missing,
    /// An entry isn't a plain IP address, for example because it has a port.
    NotAnIpAddress(String),
}

impl fmt::Display for ForwardedForError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(f, "missing X-Forwarded-For header"),
            Self::NotAnIpAddress(entry) => {
                write!(f, "X-Forwarded-For entry `{entry}` is not an IP address")
            }
        }
    }
}

/// Middleware that adds the [`ClientIp`] of each request as an extension.
///
/// It also records the address in the `client_ip` field of the current span, if
/// that span declares the field. The relay's `TraceLayer` span does.
///
/// Answers `400 Bad Request` if a trusted proxy doesn't send a valid
/// `X-Forwarded-For` header. The relay never falls back to the proxy's own
/// address, because that would make all of the proxy's clients look like one.
///
/// Needs the router to be served with `ConnectInfo<SocketAddr>`.
pub(crate) async fn identify_client(
    State(trusted_proxies): State<TrustedProxies>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    match trusted_proxies.client_ip(peer.ip(), request.headers()) {
        Ok(client_ip) => {
            tracing::Span::current().record("client_ip", tracing::field::display(client_ip));
            request.extensions_mut().insert(ClientIp(client_ip));
            next.run(request).await
        }
        Err(error) => {
            // A proxy that doesn't set the header triggers this warning on every
            // request. Operators must notice that misconfiguration, so it stays at
            // warn level instead of being sampled.
            tracing::warn!(%peer, %error, "Rejected request from trusted proxy");
            (StatusCode::BAD_REQUEST, "Invalid X-Forwarded-For header").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{http::HeaderValue, middleware, routing::get, Extension, Router};
    use axum_test::TestServer;

    use super::*;

    const NGINX_IP: &str = "127.0.0.1";
    const CLIENT_IP: &str = "198.51.100.8";

    /// Trusts the given networks; a bare IP address trusts just that address.
    fn trusted(networks: &[&str]) -> TrustedProxies {
        let networks: Vec<IpNet> = networks
            .iter()
            .map(|network| network.parse().unwrap_or_else(|_| IpNet::from(ip(network))))
            .collect();
        TrustedProxies::new(&networks)
    }

    /// Builds headers with one `X-Forwarded-For` line per value, in order.
    fn forwarded_for(lines: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(X_FORWARDED_FOR, line.parse().unwrap());
        }
        headers
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn without_trusted_proxies_the_peer_is_the_client() {
        let headers = forwarded_for(&[CLIENT_IP]);

        let client = TrustedProxies::default().client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip(NGINX_IP)));
    }

    #[test]
    fn untrusted_peer_is_the_client_and_its_header_is_ignored() {
        let proxies = trusted(&["192.0.2.0/24"]);
        let headers = forwarded_for(&[CLIENT_IP]);

        let client = proxies.client_ip(ip("203.0.113.9"), &headers);

        assert_eq!(client, Ok(ip("203.0.113.9")));
    }

    #[test]
    fn trusted_proxy_entry_is_the_client() {
        let headers = forwarded_for(&[CLIENT_IP]);

        let client = trusted(&[NGINX_IP]).client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn rightmost_entry_wins_over_entries_forged_by_the_client() {
        // Nginx with `$proxy_add_x_forwarded_for` appends the real client address
        // to whatever the client sent.
        let headers = forwarded_for(&["6.6.6.6, 198.51.100.8"]);

        let client = trusted(&[NGINX_IP]).client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn entries_left_of_the_client_are_not_parsed() {
        let headers = forwarded_for(&["unknown, 198.51.100.8"]);

        let client = trusted(&[NGINX_IP]).client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn trusted_proxies_in_a_chain_are_skipped() {
        // A CDN at 10.0.0.5 forwards to Nginx, which forwards to the relay.
        let proxies = trusted(&[NGINX_IP, "10.0.0.0/8"]);
        let headers = forwarded_for(&["6.6.6.6, 198.51.100.8, 10.0.0.5"]);

        let client = proxies.client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn several_header_lines_form_one_list_in_order() {
        let proxies = trusted(&[NGINX_IP, "10.0.0.0/8"]);
        let headers = forwarded_for(&["6.6.6.6", "198.51.100.8", "10.0.0.5"]);

        let client = proxies.client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn leftmost_entry_is_the_client_when_all_entries_are_trusted() {
        let proxies = trusted(&[NGINX_IP, "10.0.0.0/8"]);
        let headers = forwarded_for(&["10.0.0.7, 10.0.0.5"]);

        let client = proxies.client_ip(ip(NGINX_IP), &headers);

        assert_eq!(client, Ok(ip("10.0.0.7")));
    }

    #[test]
    fn ipv6_entries_and_peers_work() {
        let headers = forwarded_for(&["2001:db8::8"]);

        let client = trusted(&["::1"]).client_ip(ip("::1"), &headers);

        assert_eq!(client, Ok(ip("2001:db8::8")));
    }

    #[test]
    fn ipv4_mapped_peer_matches_ipv4_network() {
        let headers = forwarded_for(&[CLIENT_IP]);

        let client = trusted(&[NGINX_IP]).client_ip(ip("::ffff:127.0.0.1"), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn ipv4_mapped_untrusted_peer_is_returned_as_ipv4() {
        let headers = HeaderMap::new();

        let client = trusted(&[NGINX_IP]).client_ip(ip("::ffff:198.51.100.8"), &headers);

        assert_eq!(client, Ok(ip(CLIENT_IP)));
    }

    #[test]
    fn trusted_proxy_without_entries_is_an_error() {
        let proxies = trusted(&[NGINX_IP]);

        assert_eq!(
            proxies.client_ip(ip(NGINX_IP), &HeaderMap::new()),
            Err(ForwardedForError::Missing)
        );
        assert_eq!(
            proxies.client_ip(ip(NGINX_IP), &forwarded_for(&[""])),
            Err(ForwardedForError::Missing)
        );
    }

    #[test]
    fn entry_that_is_not_an_ip_address_is_an_error() {
        let headers = forwarded_for(&["198.51.100.8:4242"]);

        let client = trusted(&[NGINX_IP]).client_ip(ip(NGINX_IP), &headers);

        assert_eq!(
            client,
            Err(ForwardedForError::NotAnIpAddress(
                "198.51.100.8:4242".into()
            ))
        );
    }

    #[test]
    fn entry_with_non_ascii_bytes_is_an_error_with_the_bytes_escaped() {
        let mut headers = HeaderMap::new();
        headers.append(
            X_FORWARDED_FOR,
            HeaderValue::from_bytes(b"198.51.100.8\xe9").unwrap(),
        );

        let client = trusted(&[NGINX_IP]).client_ip(ip(NGINX_IP), &headers);

        assert_eq!(
            client,
            Err(ForwardedForError::NotAnIpAddress(
                r"198.51.100.8\xe9".into()
            ))
        );
    }

    /// Serves a router that answers with the client IP that the middleware found.
    /// The test server connects from 127.0.0.1.
    fn serve_client_ip(trusted_proxies: TrustedProxies) -> TestServer {
        let router = Router::new()
            .route(
                "/",
                get(|Extension(ClientIp(client_ip))| async move { client_ip.to_string() }),
            )
            .layer(middleware::from_fn_with_state(
                trusted_proxies,
                identify_client,
            ));
        TestServer::new(router.into_make_service_with_connect_info::<SocketAddr>()).unwrap()
    }

    #[tokio::test]
    async fn middleware_adds_client_ip_from_trusted_proxy() {
        let server = serve_client_ip(trusted(&[NGINX_IP]));

        let response = server
            .get("/")
            .add_header("X-Forwarded-For", CLIENT_IP)
            .await;

        response.assert_status_ok();
        response.assert_text(CLIENT_IP);
    }

    #[tokio::test]
    async fn middleware_rejects_trusted_proxy_without_header() {
        let server = serve_client_ip(trusted(&[NGINX_IP]));

        let response = server.get("/").await;

        response.assert_status_bad_request();
    }
}
