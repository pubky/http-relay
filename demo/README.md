# HTTP Relay Demo

A simple web UI to test the http-relay `/link2` endpoint.

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

1. **Set Channel ID** - Click "Random" or enter your own
2. **Start Consumer** - Waits for a message on the channel
3. **Send from Producer** - Delivers message to the waiting consumer
4. **Watch the log** - See the request/response flow

The consumer and producer retry automatically on 408 timeouts until they connect.

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

### Endpoint

Toggle between `/link2` (recommended, with caching) and `/link` (deprecated) in the UI.

### Channel ID

Any string, shared between consumer and producer. Can be set via `?channel=` query param.
