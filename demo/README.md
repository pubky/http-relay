# HTTP Relay Demo

A simple web UI to test the http-relay `/inbox` endpoint.

## Quick Start

```bash
# 1. Start the relay (from repo root)
cargo run

# 2. Start the demo (from this folder)
npm install
npm run dev
```

Open http://localhost:3000

## Usage

1. **Set Channel ID** — Click "Random" or enter your own
2. **Producer** — Type a message and POST it to the channel
3. **Consumer** — Long-poll for the message, then send an ACK
4. **Producer (optional)** — Check or await the ACK
5. **Watch the log** — See the request/response flow

## Endpoints

| Method | Endpoint | Role | Purpose |
|--------|----------|------|---------|
| POST | `/inbox/{id}` | Producer | Store a message |
| GET | `/inbox/{id}` | Consumer | Long-poll for message (408 on timeout) |
| DELETE | `/inbox/{id}` | Consumer | Acknowledge receipt |
| GET | `/inbox/{id}/ack` | Producer | Check ACK status |
| GET | `/inbox/{id}/await` | Producer | Long-poll until ACKed (408 on timeout) |

## Sharing Channels

The channel ID syncs with the URL. Share links like:

```
http://localhost:3000?channel=my-channel
```

Your friend opens the link → same channel ID is pre-filled → they can immediately start as consumer or producer.

## Configuration

### Relay URL

Default: `http://localhost:8080`

Set via environment variable:
```bash
NEXT_PUBLIC_RELAY_URL=https://relay.example.com npm run dev
```

Or via URL query param:
```
http://localhost:3000?relay=https://relay.example.com
```

### Channel ID

Any string, shared between consumer and producer. Can be set via `?channel=` query param.
