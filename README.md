# http-relay

[![Crates.io](https://img.shields.io/crates/v/http-relay.svg)](https://crates.io/crates/http-relay)
[![CI](https://github.com/pubky/http-relay/actions/workflows/ci.yml/badge.svg)](https://github.com/pubky/http-relay/actions/workflows/ci.yml)
[![Documentation](https://docs.rs/http-relay/badge.svg)](https://docs.rs/http-relay)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A Rust implementation of the `/link` endpoint from the [HTTP Relay spec](https://httprelay.io/)
for asynchronous producer/consumer message passing.

## What is this?

An HTTP relay enables decoupled communication between distributed services.
Instead of direct synchronous calls, producers and consumers communicate through
relay endpoints, each waiting for their counterpart to arrive.

**Non-standard extension:** Adds a `/link2` endpoint optimized for mobile
clients with caching and shorter timeouts.

**Use cases:**
- Connecting services that can't communicate directly
- Mobile apps that need resilient message delivery with retry support
- Decoupled microservice communication

## Features

- **Async producer/consumer model** - Producers POST data, consumers GET it
- **Two endpoint variants:**
  - `/link/{id}` - **Deprecated.** Standard relay (10 min timeout)
  - `/link2/{id}` - **Recommended.** Mobile-friendly with caching (25s timeout, 5 min cache TTL)
- **Mobile resilience** - Cached responses allow retries after connection drops
- **Content-Type preservation** - Forwards producer's Content-Type to consumer
- **Configurable timeouts and caching**

## Installation

```bash
cargo install http-relay
```

Or add as a dependency:

```toml
[dependencies]
http-relay = "0.6"
```

## Usage

### As CLI

```bash
# Default: bind to 127.0.0.1:8080 (localhost only)
http-relay

# Bind to all interfaces (for production/Docker)
http-relay --bind 0.0.0.0

# Custom configuration
http-relay --bind 0.0.0.0 --port 15412 --link2-cache-ttl 300 --link2-timeout 25 -vv
```

**Options:**

| Flag | Description | Default |
|------|-------------|---------|
| `--bind <ADDR>` | Bind address | `127.0.0.1` |
| `--port <PORT>` | HTTP port (0 = random) | `8080` |
| `--link2-cache-ttl <SECS>` | Cache TTL for link2 | `300` |
| `--link2-timeout <SECS>` | Link2 endpoint timeout | `25` |
| `--max-body-size <BYTES>` | Max request body size | `10240` (10KB) |
| `--max-pending <N>` | Max pending requests | `10000` |
| `--max-cache <N>` | Max cached entries | `10000` |
| `-v` | Verbosity (repeat for more) | warn |
| `-q, --quiet` | Silence output | - |

### As Library

```rust
use http_relay::HttpRelayBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let relay = HttpRelayBuilder::default()
        .http_port(15412)
        .run()
        .await?;

    println!("Running at {}", relay.local_link_url());

    tokio::signal::ctrl_c().await?;
    relay.shutdown().await
}
```

## API

### POST `/link/{id}` or `/link2/{id}`

Producer sends a message. Waits for a consumer to retrieve it.

```bash
curl -X POST http://localhost:8080/link/my-channel \
  -H "Content-Type: application/json" \
  -d '{"hello": "world"}'
```

**Responses:**
- `200 OK` - Consumer received the message
- `408 Request Timeout` - No consumer arrived in time
- `503 Service Unavailable` - Server at capacity (max pending reached)

### GET `/link/{id}` or `/link2/{id}`

Consumer retrieves a message. Waits for a producer to send one.

```bash
curl http://localhost:8080/link/my-channel
```

**Responses:**
- `200 OK` - Returns producer's payload with original Content-Type
- `408 Request Timeout` - No producer arrived in time
- `503 Service Unavailable` - Server at capacity (max pending reached)

### Endpoint Differences

| Aspect | `/link/{id}` | `/link2/{id}` |
|--------|--------------|---------------|
| Status | **Deprecated** | **Recommended** |
| Timeout | 10 minutes | 25 seconds |
| Caching | No | Yes (5 min TTL) |

**Use `/link2` for all integrations.** It handles proxy timeouts gracefully and
supports retries via caching. The `/link` endpoint is deprecated and remains
only for backwards compatibility with existing clients.

### Why Link2 Exists

The original `/link` endpoint has a problem on mobile devices. When a mobile
app requests data from the relay and then gets backgrounded or killed by the
OS, the HTTP response never actually arrives on the device. From the relay's
perspective, the value was consumed successfully. But the mobile app never
received it—and when the user reopens the app, the value is gone.

`/link2` solves this with two changes:

1. **Caching**: After a successful delivery, the value is cached for 5 minutes.
   If the mobile app was killed, it can retry and still receive the value.

2. **Shorter timeout**: The 25-second timeout stays safely under typical proxy
   timeouts (nginx, Cloudflare often use 30s), preventing unexpected connection drops.

## Client Implementation Patterns

Because `/link2` has a short timeout (25s), clients should implement retry
loops. The producer/consumer may not connect on the first attempt, and that's
expected behavior.

### Consumer: Retry Until Value Received

The consumer loops until it successfully receives the producer's payload:

```javascript
async function consumeFromRelay(channelId) {
  while (true) {
    const response = await fetch(`http://relay.example.com/link2/${channelId}`);

    if (response.status === 200) {
      return await response.text(); // Success - got the value
    }

    if (response.status === 408) {
      continue; // Timeout - producer hasn't arrived yet, retry
    }

    throw new Error(`Unexpected status: ${response.status}`);
  }
}
```

### Producer: Retry Until Consumer Receives

The producer loops until a consumer successfully retrieves the message:

```javascript
async function produceToRelay(channelId, data) {
  while (true) {
    const response = await fetch(`http://relay.example.com/link2/${channelId}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    });

    if (response.status === 200) {
      return; // Success - consumer got the value
    }

    if (response.status === 408) {
      continue; // Timeout - no consumer yet, retry
    }

    throw new Error(`Unexpected status: ${response.status}`);
  }
}
```

### Why Retry Loops?

- **Short timeouts are intentional**: Proxies (nginx, cloudflare) often
  have 30s timeouts. The 25s link2 timeout stays safely under this limit.
- **Cache enables resilience**: Once delivered, the value is cached for 5 min.
  If a consumer's connection drops, they can retry and still receive it.
- **408 is not an error**: It just means the counterpart hasn't arrived yet.
  Keep trying until they do.

## Development

```bash
# Run tests
cargo test

# Run with debug logging
RUST_LOG=debug cargo run
```

