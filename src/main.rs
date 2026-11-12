//! HTTP Relay server executable.

#![forbid(unsafe_code)]

#[cfg(feature = "persist")]
use std::path::PathBuf;
use std::{net::IpAddr, num::NonZeroUsize, time::Duration};

use anyhow::Result;
use clap::Parser;
use http_relay::HttpRelayBuilder;
use ipnet::IpNet;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "http-relay")]
#[command(about = "HTTP relay server for asynchronous producer/consumer communication")]
#[command(version)]
struct Args {
    /// Address to bind to
    #[arg(short, long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// Port to listen on (0 = random available port)
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Link endpoint timeout in seconds (how long producer/consumer wait for each other)
    #[arg(long, default_value_t = HttpRelayBuilder::DEFAULT_LINK_TIMEOUT.as_secs())]
    link_timeout: u64,

    /// Inbox long-poll timeout in seconds (shorter to avoid proxy timeouts)
    #[arg(long, default_value_t = HttpRelayBuilder::DEFAULT_INBOX_TIMEOUT.as_secs())]
    inbox_timeout: u64,

    /// Inbox message TTL in seconds (how long messages persist before expiring)
    #[arg(long, default_value_t = HttpRelayBuilder::DEFAULT_INBOX_CACHE_TTL.as_secs())]
    inbox_cache_ttl: u64,

    /// Maximum request body size in bytes
    #[arg(long, default_value_t = HttpRelayBuilder::DEFAULT_MAX_BODY_SIZE)]
    max_body_size: usize,

    /// Maximum stored entries (new IDs get 503 when every entry holds an
    /// unACKed message). Also limits the number of IDs with waiting requests
    /// (503 when full)
    #[arg(long, default_value_t = HttpRelayBuilder::DEFAULT_MAX_ENTRIES)]
    max_entries: usize,

    /// Maximum unACKed messages one client (IPv4 address or IPv6 /56) can have
    /// stored. Further messages get 429
    #[arg(
        long,
        default_value_t = NonZeroUsize::new(HttpRelayBuilder::DEFAULT_MAX_ENTRIES_PER_CLIENT)
            .expect("the default is not zero")
    )]
    max_entries_per_client: NonZeroUsize,

    /// Maximum requests one client (IPv4 address or IPv6 /56) can have
    /// waiting: inbox GET and /await, and link GET and POST. Further ones get
    /// 429
    #[arg(
        long,
        default_value_t = NonZeroUsize::new(HttpRelayBuilder::DEFAULT_MAX_WAITING_PER_CLIENT)
            .expect("the default is not zero")
    )]
    max_waiting_per_client: NonZeroUsize,

    /// Path to SQLite database for persistent storage.
    /// If not specified, uses in-memory storage (data lost on restart).
    #[cfg(feature = "persist")]
    #[arg(long)]
    persist_db: Option<PathBuf>,

    /// Verbosity level: -v (info), -vv (debug), -vvv (trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Enable permissive CORS headers (Access-Control-Allow-Origin: *)
    #[arg(long)]
    cors_allow_all: bool,

    /// Identify clients behind this proxy (IP or CIDR, repeatable) by the
    /// X-Forwarded-For header. Its requests without a valid header get 400.
    #[arg(long, value_name = "CIDR", value_parser = parse_network)]
    trusted_proxy: Vec<IpNet>,

    /// Silence all output
    #[arg(short, long)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    init_tracing(args.verbose, args.quiet);

    let builder = HttpRelayBuilder::default()
        .bind_address(args.bind)
        .http_port(args.port)
        .link_timeout(Duration::from_secs(args.link_timeout))
        .inbox_timeout(Duration::from_secs(args.inbox_timeout))
        .inbox_cache_ttl(Duration::from_secs(args.inbox_cache_ttl))
        .max_body_size(args.max_body_size)
        .max_entries(args.max_entries)
        .max_entries_per_client(args.max_entries_per_client.get())
        .max_waiting_per_client(args.max_waiting_per_client.get())
        .cors_allow_all(args.cors_allow_all)
        .trusted_proxies(args.trusted_proxy);
    #[cfg(feature = "persist")]
    let builder = builder.persist_db(args.persist_db);
    let relay = builder.run().await?;

    tracing::info!(
        address = %relay.http_address(),
        "HTTP relay server started"
    );
    #[cfg(feature = "link-compat")]
    tracing::info!(
        url = %relay.local_url(),
        "Endpoints: /inbox/{{id}} (recommended), /link/{{id}}"
    );
    #[cfg(not(feature = "link-compat"))]
    tracing::info!(
        url = %relay.local_url(),
        "Endpoint: /inbox/{{id}}"
    );

    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");
    relay.shutdown().await?;

    Ok(())
}

/// Parses a CIDR network; a bare IP address means just that address.
fn parse_network(value: &str) -> Result<IpNet, String> {
    value
        .parse::<IpNet>()
        .or_else(|_| value.parse::<IpAddr>().map(IpNet::from))
        .map_err(|_| format!("`{value}` is not an IP address or CIDR network"))
}

fn log_level(verbose: u8, quiet: bool) -> LevelFilter {
    if quiet {
        LevelFilter::OFF
    } else {
        match verbose {
            0 => LevelFilter::WARN,
            1 => LevelFilter::INFO,
            2 => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        }
    }
}

fn init_tracing(verbose: u8, quiet: bool) {
    let filter = EnvFilter::builder()
        .with_default_directive(log_level(verbose, quiet).into())
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_network_accepts_cidr_and_bare_ip() {
        assert_eq!(
            parse_network("10.0.0.0/8"),
            Ok("10.0.0.0/8".parse().unwrap())
        );
        assert_eq!(
            parse_network("127.0.0.1"),
            Ok("127.0.0.1/32".parse().unwrap())
        );
        assert_eq!(parse_network("::1"), Ok("::1/128".parse().unwrap()));
        assert!(parse_network("localhost").is_err());
    }

    #[test]
    fn test_args_are_consistent() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}
