//! HTTP Relay server executable.

use std::{net::IpAddr, time::Duration};

use anyhow::Result;
use clap::Parser;
use http_relay::HttpRelayBuilder;
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

    /// Cache TTL in seconds for retry support (link2 endpoint)
    #[arg(long, default_value_t = 300)]
    link2_cache_ttl: u64,

    /// Link2 endpoint timeout in seconds (shorter to avoid proxy timeouts)
    #[arg(long, default_value_t = 25)]
    link2_timeout: u64,

    /// Maximum request body size in bytes (default: 10KB)
    #[arg(long, default_value_t = 10 * 1024)]
    max_body_size: usize,

    /// Maximum pending requests (producers + consumers combined, default: 10000)
    #[arg(long, default_value_t = 10_000)]
    max_pending: usize,

    /// Maximum cached entries (default: 10000)
    #[arg(long, default_value_t = 10_000)]
    max_cache: usize,

    /// Verbosity level: -v (info), -vv (debug), -vvv (trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

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
        .cache_ttl(Duration::from_secs(args.link2_cache_ttl))
        .link2_timeout(Duration::from_secs(args.link2_timeout))
        .max_body_size(args.max_body_size)
        .max_pending(args.max_pending)
        .max_cache(args.max_cache)
        .run()
        .await?;

    tracing::info!(
        address = %relay.http_address(),
        "HTTP relay server started"
    );
    tracing::info!(
        link = %relay.local_link_url(),
        "Link endpoint available at /link/{{id}} and /link2/{{id}}"
    );

    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");
    relay.shutdown().await?;

    Ok(())
}

fn init_tracing(verbose: u8, quiet: bool) {
    let level = if quiet {
        LevelFilter::OFF
    } else {
        match verbose {
            0 => LevelFilter::WARN,
            1 => LevelFilter::INFO,
            2 => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        }
    };

    let filter = EnvFilter::builder()
        .with_default_directive(level.into())
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
