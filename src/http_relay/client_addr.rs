//! Identifies the client behind each accepted connection.
//!
//! Limits per client need the real client address, even behind a reverse proxy.
//! HTTP forwarding headers such as `X-Forwarded-For` are easy for a client to forge,
//! so the relay reads the address from a PROXY protocol preface instead. The proxy
//! sends that preface before any HTTP bytes, and the relay only accepts it from
//! trusted proxy networks. `DEPLOY.md` shows the matching Nginx setup.

use std::{future::Future, io, pin::Pin};

use anyhow::Result;
use axum_server::accept::Accept;
use ipnet::IpNet;
use tcp_client_addr::{ClientAddr, IdentityMode, ProxyProtocol};
use tokio::net::TcpStream;
use tower_http::add_extension::AddExtension;

/// Identifies the client of each connection and gives handlers its [`ClientAddr`]
/// as a request extension.
///
/// axum-server runs the acceptor inside each connection's own task, so a proxy that
/// is slow to send its preface (at most 1 second) only delays that one connection,
/// not the accept loop.
#[derive(Clone)]
pub(crate) struct ClientAddrAcceptor {
    mode: IdentityMode,
}

impl ClientAddrAcceptor {
    /// Without trusted proxies, the client is the TCP peer. With them, every
    /// connection must come from a trusted proxy and start with a PROXY preface;
    /// there is no fallback to the peer address.
    pub fn new(trusted_proxies: &[IpNet]) -> Result<Self> {
        let mode = if trusted_proxies.is_empty() {
            IdentityMode::Direct
        } else {
            IdentityMode::ProxyProtocol(ProxyProtocol::new(trusted_proxies.iter().copied())?)
        };
        Ok(Self { mode })
    }
}

impl<S: Send + 'static> Accept<TcpStream, S> for ClientAddrAcceptor {
    type Stream = TcpStream;
    type Service = AddExtension<S, ClientAddr>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let mode = self.mode.clone();
        Box::pin(async move {
            let (stream, client_addr) = mode.identify(stream).await.map_err(|error| {
                // Debug, not warn: port scanners and direct health checks would
                // otherwise flood the log.
                tracing::debug!(%error, "Rejected connection");
                io::Error::other(error)
            })?;
            Ok((stream, AddExtension::new(service, client_addr)))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener};

    use axum::{routing::get, Extension, Router};
    use axum_server::Handle;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const GET_ROOT: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    /// Serves a router that answers with the client IP that the acceptor identified.
    fn serve_client_ip(trusted_proxies: &[IpNet]) -> (SocketAddr, Handle<SocketAddr>) {
        let router = Router::new().route(
            "/",
            get(|Extension(client): Extension<ClientAddr>| async move {
                client.client_ip().to_string()
            }),
        );
        let acceptor = ClientAddrAcceptor::new(trusted_proxies).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = Handle::new();
        let server = axum_server::from_tcp(listener)
            .unwrap()
            .acceptor(acceptor)
            .handle(handle.clone());
        tokio::spawn(server.serve(router.into_make_service()));
        (address, handle)
    }

    /// Sends `bytes` and returns everything the server sends back before closing.
    async fn exchange(address: SocketAddr, bytes: &[u8]) -> String {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(bytes).await.unwrap();
        let mut response = String::new();
        // A rejected connection may be reset instead of closed cleanly.
        let _ = stream.read_to_string(&mut response).await;
        response
    }

    #[tokio::test]
    async fn direct_mode_uses_peer_address_and_ignores_forwarding_headers() {
        let (address, _handle) = serve_client_ip(&[]);
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 198.51.100.8\r\nConnection: close\r\n\r\n";

        let response = exchange(address, request).await;

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("127.0.0.1"), "{response}");
    }

    #[tokio::test]
    async fn proxy_mode_uses_address_from_preface() {
        let (address, _handle) = serve_client_ip(&["127.0.0.1/32".parse().unwrap()]);
        let mut request = b"PROXY TCP4 198.51.100.8 192.0.2.1 4242 443\r\n".to_vec();
        request.extend_from_slice(GET_ROOT);

        let response = exchange(address, &request).await;

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("198.51.100.8"), "{response}");
    }

    #[tokio::test]
    async fn proxy_mode_rejects_connection_without_preface() {
        let (address, _handle) = serve_client_ip(&["127.0.0.1/32".parse().unwrap()]);

        let response = exchange(address, GET_ROOT).await;

        assert_eq!(response, "", "no response without a PROXY preface");
    }

    #[tokio::test]
    async fn proxy_mode_rejects_untrusted_peer() {
        let (address, _handle) = serve_client_ip(&["192.0.2.0/24".parse().unwrap()]);
        let mut request = b"PROXY TCP4 198.51.100.8 192.0.2.1 4242 443\r\n".to_vec();
        request.extend_from_slice(GET_ROOT);

        let response = exchange(address, &request).await;

        assert_eq!(response, "", "no response to an untrusted peer");
    }
}
