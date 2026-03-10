//! HTTP Relay server executable.

use std::{net::IpAddr, path::PathBuf, time::Duration};

use anyhow::Result;
use clap::Parser;
use http_relay::HttpRelayBuilder;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Default maximum entries in the waiting list.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

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
    #[arg(long, default_value_t = 600)]
    link_timeout: u64,

    /// Inbox long-poll timeout in seconds (shorter to avoid proxy timeouts)
    #[arg(long, default_value_t = 25)]
    inbox_timeout: u64,

    /// Inbox message TTL in seconds (how long messages persist before expiring)
    #[arg(long, default_value_t = 300)]
    inbox_cache_ttl: u64,

    /// Maximum request body size in bytes
    #[arg(long, default_value_t = 2 * 1024)]
    max_body_size: usize,

    /// Maximum entries in the waiting list (oldest entries evicted when full)
    #[arg(long, default_value_t = DEFAULT_MAX_ENTRIES)]
    max_entries: usize,

    /// Path to SQLite database for persistent storage.
    /// If not specified, uses in-memory storage (data lost on restart).
    #[arg(long)]
    persist_db: Option<PathBuf>,

    /// Verbosity level: -v (info), -vv (debug), -vvv (trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Enable permissive CORS headers (Access-Control-Allow-Origin: *)
    #[arg(long)]
    cors_allow_all: bool,

    /// Silence all output
    #[arg(short, long)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    init_tracing(args.verbose, args.quiet);

    let relay = HttpRelayBuilder::default()
        .bind_address(args.bind)
        .http_port(args.port)
        .link_timeout(Duration::from_secs(args.link_timeout))
        .inbox_timeout(Duration::from_secs(args.inbox_timeout))
        .inbox_cache_ttl(Duration::from_secs(args.inbox_cache_ttl))
        .max_body_size(args.max_body_size)
        .max_entries(args.max_entries)
        .persist_db(args.persist_db)
        .cors_allow_all(args.cors_allow_all)
        .run()
        .await?;

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
    fn test_quiet_overrides_verbose() {
        assert_eq!(log_level(3, true), LevelFilter::OFF);
    }

    #[test]
    fn test_default_is_warn() {
        assert_eq!(log_level(0, false), LevelFilter::WARN);
    }

    #[test]
    fn test_verbose_info() {
        assert_eq!(log_level(1, false), LevelFilter::INFO);
    }

    #[test]
    fn test_verbose_debug() {
        assert_eq!(log_level(2, false), LevelFilter::DEBUG);
    }

    #[test]
    fn test_verbose_trace() {
        assert_eq!(log_level(3, false), LevelFilter::TRACE);
    }
}
