# Deploy in production

This guide is for operators who run a public relay. For the CLI options, see
the [options table in the README](README.md#as-cli).

## Requirements

A production relay must run behind a reverse proxy that parses HTTP, such as
Nginx, HAProxy in HTTP mode, or Caddy. Exposing the relay directly to the
internet isn't supported. The relay relies on the proxy for the following:

- **TLS:** Inbox IDs are shared secrets and appear in every request path.
  Without TLS, anyone on the network path can read them and take over those
  inboxes.
- **Timeouts for slow clients:** The relay has no timeout for reading a
  request. A client that sends its headers or body very slowly holds a
  connection open for as long as it likes, and many such clients can exhaust
  the relay's file descriptors. The proxy must time out incomplete requests and
  buffer each request completely before forwarding it.
- **Connection limits per client IP:** Each long-poll request holds a
  connection open for up to `--inbox-timeout`, or `--link-timeout` for `/link`.
- **Read timeout:** Set the proxy's timeout for upstream responses longer than
  the longest long-poll you serve, so it doesn't cut requests off early. Use
  more than 25 seconds for the inbox endpoints and more than 10 minutes if you
  serve `/link`.
- **Client address:** The proxy sets the `X-Forwarded-For` header, so the
  relay can identify clients. See
  [How the relay identifies clients](#how-the-relay-identifies-clients).

A proxy that only forwards TCP connections, such as an Nginx `stream` server or
HAProxy in TCP mode, doesn't meet these requirements: it can't time out slow
requests.

Keep the relay's own port unreachable from the internet, for example by binding
it to `127.0.0.1` or to a private container network.

## How the relay identifies clients

To apply limits per client, the relay needs each client's real IP address.
Behind a proxy, the TCP connection comes from the proxy, not from the client.
The proxy passes on the client's address in the `X-Forwarded-For` header.

| Mode | Flag | Client address |
|------|------|----------------|
| Direct (default) | none | The TCP peer. The relay ignores `X-Forwarded-For`. |
| Behind a proxy | `--trusted-proxy <CIDR>` | For requests from a trusted proxy, the rightmost `X-Forwarded-For` entry that isn't a trusted proxy. |

A client can send its own `X-Forwarded-For` header, and a proxy that appends to
the header keeps what the client sent. Entries on the left can be forged, so
the relay reads the header from the right:

- It only reads the header from trusted proxies. For requests from any other
  address, the client is the TCP peer, and the header is ignored.
- It skips entries that are themselves trusted proxies. If several proxies
  forward a request, trust all of them. If every entry is a trusted proxy, the
  leftmost entry is the client. This happens when a host inside a trusted
  network sends the request.
- It ignores everything to the left of the client's entry.

The relay answers `400 Bad Request` when a trusted proxy sends no
`X-Forwarded-For` header, or when an entry it reads isn't a plain IP address.
Entries with a port, such as `198.51.100.8:4242`, count as invalid. The relay
never falls back to the proxy's own address, because all clients would then
share one address. Rejected requests appear in the relay's log as
`Rejected request from trusted proxy`.

Repeat the flag to trust several networks. A bare IP, such as `127.0.0.1`,
trusts only that address.

To check which address the relay identifies, start it with `-vv`. Each request
is then logged with a `client_ip` field.

## Limits per client

A stored message stays until its consumer ACKs it or it expires. If unACKed
messages fill the store, the relay rejects new messages with `503`. To keep a
single client from filling the store, the relay limits how many unACKed messages
each client can have stored, 100 by default. Further `POST` requests from that
client get `429 Too Many Requests`. Replacing one of the client's unACKed
messages is still allowed.

- **What counts as one client:** an IPv4 address, or an IPv6 /56 network. ISPs
  usually give each customer at least a /56, so a limit per IPv6 address would
  be easy to get around.
- **Shared addresses:** many users can share one IPv4 address, for example
  behind carrier-grade NAT. Each user normally has only a few unACKed messages
  at a time, so the default leaves room for many users per address. Raise
  `--max-entries-per-client` if your users' clients get `429`. To turn the
  limit off, set it higher than `--max-entries`.
- **What the limit doesn't cover:** clients with many addresses can still fill
  the store together. The limit raises the cost: with the defaults, filling the
  store takes 100 IPv4 addresses or IPv6 /56 networks. IPv6 networks are cheap,
  though: a /48, which some ISPs and free tunnel brokers hand out, holds 256
  /56 networks. One holder of a /48 can fill the store with the defaults.
- **Long-poll requests:** the limit only counts stored messages. A `GET` that
  waits for a new ID takes one of the `--max-entries` slots for IDs with
  waiting requests, and the relay doesn't limit these per client. When all
  slots are taken, long-poll requests and `/link` requests for new IDs get
  `503`. The relay doesn't limit request rates per client either. Use the
  proxy's connection and rate limits for both, and see the note on IPv6 in
  [Run behind Nginx](#run-behind-nginx).

With `--persist-db`, the database stores a keyed hash of each message's client,
not its IP address. The key is random and isn't saved, so after a restart, the
messages stored before it count against no client's limit until they expire.

## Run behind Nginx

Start the relay so it trusts only Nginx on the same host:

```bash
http-relay --bind 127.0.0.1 --port 8080 --trusted-proxy 127.0.0.1
```

Then add a server block for the relay to your Nginx configuration. If Nginx
already serves other sites, they stay unchanged. Replace `relay.example.com`
and the certificate paths:

```nginx
# In the http block.
limit_conn_zone $binary_remote_addr zone=relay_connections:10m;

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name relay.example.com;

    ssl_certificate     /etc/letsencrypt/live/relay.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/relay.example.com/privkey.pem;

    # Close connections from clients that send requests too slowly.
    client_header_timeout 10s;
    client_body_timeout   10s;

    # Many users can share one IP address, for example on conference Wi-Fi.
    limit_conn relay_connections 1000;

    location / {
        proxy_pass http://127.0.0.1:8080;
        # Replace any X-Forwarded-For header the client sent.
        proxy_set_header X-Forwarded-For $remote_addr;
        # Longer than the longest long-poll (`--link-timeout`, 10 minutes).
        proxy_read_timeout 11m;
    }
}
```

Notes:

- `client_header_timeout` limits the time for the complete request headers.
  `client_body_timeout` only limits the time between two reads, so a client
  can still send a body slowly. Nginx buffers the whole body before it
  forwards the request, so only Nginx holds that connection, not the relay.
  Keep `proxy_request_buffering` at its default, `on`.
- Clients can use HTTP/2 with Nginx. Nginx talks HTTP/1.x to the relay. The
  `http2` directive needs Nginx 1.25.1 or later; on older versions, use
  `listen 443 ssl http2;` instead.
- To also rate-limit requests per client, add a `limit_req_zone` and a
  `limit_req` directive.
- `$binary_remote_addr` is the full client address. Over IPv4 that is one
  host, but an IPv6 host usually has a whole /64 and can use a new address for
  each connection, so `limit_conn` doesn't limit it. Nginx can't group IPv6
  addresses by network itself. If abuse over IPv6 is a concern, limit
  connections per network in front of Nginx, for example in a firewall or a
  CDN.
- If the relay runs in a container, trust the address Nginx connects from, such
  as the Docker bridge gateway, instead of `127.0.0.1`. If you publish the
  relay's port, publish it on the loopback address only, for example
  `-p 127.0.0.1:8080:8080`. In some setups, such as rootless Docker and Docker
  Desktop, connections to a published port arrive from the bridge gateway even
  when they come from other hosts. On a port published on all interfaces, those
  clients would count as a trusted proxy and could claim any address.

### Run behind a CDN

If a CDN, such as Cloudflare, forwards requests to Nginx, Nginx sees the CDN's
addresses instead of the clients'. Configure Nginx to take the client address
from the CDN's `X-Forwarded-For` header, so that `limit_conn` and the header
sent to the relay use the real client address. Add the following to the
relay's server block, with one `set_real_ip_from` line for each of the address
ranges the CDN publishes:

```nginx
set_real_ip_from 203.0.113.0/24;
real_ip_header X-Forwarded-For;
real_ip_recursive on;
```

The relay's configuration stays the same. These directives need the
`ngx_http_realip_module`. The official Nginx packages include it; check with
`nginx -V 2>&1 | grep http_realip`.

## Security requirements

- **Trust only your proxy.** A trusted proxy can claim any client address.
  Firewall the relay's port, or bind it to `127.0.0.1` or a private network, so
  nothing else can reach it. Trusting `127.0.0.1` trusts every process on the
  host. List only the proxies' own addresses, not whole networks where clients
  also connect from: clients in a trusted network can claim any address.
- **The proxy must set `X-Forwarded-For` itself.** It must either replace the
  header (`$remote_addr` in Nginx) or append the address it received the
  connection from (`$proxy_add_x_forwarded_for`). A proxy that passes the
  client's header through unchanged lets clients choose their address.

## Health checks

The relay answers `400 Bad Request` to requests from a trusted proxy address
without an `X-Forwarded-For` header. With `--trusted-proxy 127.0.0.1`, that
includes checks run on the relay's host. Check through Nginx:

```bash
curl https://relay.example.com/
# Http Relay
```

To check the relay directly from its host, send the header yourself:

```bash
curl -H "X-Forwarded-For: 127.0.0.1" http://127.0.0.1:8080/
# Http Relay
```
