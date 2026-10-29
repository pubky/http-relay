# Deploy in production

This guide is for operators who run a public relay. For the CLI options, see
the [options table in the README](README.md#as-cli).

## Requirements

Run the relay behind a reverse proxy. The relay has no authentication and
doesn't terminate TLS, so it relies on the proxy for the following:

- **TLS:** Inbox IDs are shared secrets and appear in every request path.
  Without TLS, anyone on the network path can read them and take over those
  inboxes.
- **Connection limits per client IP:** Each long-poll request holds a
  connection open for up to `--inbox-timeout`, or `--link-timeout` for `/link`.
- **Idle timeout:** Set the proxy's timeout longer than the longest long-poll
  you serve, so it doesn't cut requests off early. Use more than 25 seconds for
  the inbox endpoints and more than 10 minutes if you serve `/link`.

Keep the relay's own port unreachable from the internet, for example by binding
it to `127.0.0.1` or to a private container network.

The relay speaks HTTP/1.1 only. It closes connections that don't send complete
request headers within 10 seconds, so slow clients can't hold connections open
forever. In the Nginx setup below, this is the only protection against slow
headers: a `stream` server forwards TCP without parsing HTTP.

## How the relay identifies clients

To apply limits per client, the relay needs each client's real IP address.
Behind a proxy, the TCP connection comes from the proxy, not from the client.

The relay doesn't read `X-Forwarded-For` or similar headers, because a client
can forge them. Instead, it reads the [PROXY protocol][proxy-protocol]: the
proxy sends the client's address in a short preface before any HTTP bytes.

| Mode | Flag | Client address |
|------|------|----------------|
| Direct (default) | none | The TCP peer. Use this when clients connect to the relay directly. |
| PROXY protocol | `--proxy-protocol-trusted <CIDR>` | The address in the PROXY preface. |

With `--proxy-protocol-trusted`, the relay rejects:

- Connections from addresses outside the trusted networks.
- Connections that don't start with a valid PROXY preface within 1 second.
  The relay never falls back to the TCP peer address.

Repeat the flag to trust several networks. A bare IP, such as `127.0.0.1`,
trusts only that address.

## Run behind Nginx

Nginx can only send a PROXY preface from its `stream` module, which forwards
TCP connections. The `http` module opens its own connections to the upstream
and can't send one. So the relay needs a `stream` server, even if Nginx already
serves other sites from `http` blocks.

The setup below works when Nginx terminates TLS and also serves other sites on
port 443:

1. A `stream` server on port 443 reads the TLS server name (SNI) without
   decrypting and routes each connection to an internal port.
2. Connections for the relay go to a second `stream` server that terminates
   TLS and forwards plain HTTP to the relay, with a PROXY preface.
3. All other connections go to your existing `http` sites, which move from
   port 443 to an internal port.

Both hops carry the client's address in a PROXY preface, so it survives the
extra internal hop.

Start the relay so it trusts only Nginx on the same host:

```bash
http-relay --bind 127.0.0.1 --port 8080 --proxy-protocol-trusted 127.0.0.1
```

Then configure Nginx. Replace `relay.example.com` and the certificate paths:

```nginx
stream {
    # The relay's TLS server must be reached over the client's address family.
    # Nginx builds the PROXY header for the relay from the client's address and
    # the TLS server's own address. If their families differ, the header is
    # invalid and the relay rejects the connection.
    map $remote_addr $relay_tls_upstream {
        ~:       [::1]:8444;      # IPv6 client
        default  127.0.0.1:8444;  # IPv4 client
    }

    # Route by TLS server name without decrypting.
    map $ssl_preread_server_name $tls_upstream {
        relay.example.com  $relay_tls_upstream;  # relay, below
        default            127.0.0.1:8443;       # existing http sites
    }

    server {
        listen 443;
        listen [::]:443;
        ssl_preread on;
        proxy_pass $tls_upstream;
        proxy_protocol on;
    }

    limit_conn_zone $binary_remote_addr zone=relay_connections:10m;

    # Terminate TLS for the relay and forward plain HTTP.
    server {
        listen 127.0.0.1:8444 ssl proxy_protocol;
        listen [::1]:8444 ssl proxy_protocol;
        # Use the client address from the first hop's preface. This also sets
        # the address that `proxy_protocol on` sends to the relay.
        set_real_ip_from 127.0.0.1;
        set_real_ip_from ::1;

        ssl_certificate     /etc/letsencrypt/live/relay.example.com/fullchain.pem;
        ssl_certificate_key /etc/letsencrypt/live/relay.example.com/privkey.pem;

        # Many users can share one IP address, for example on conference Wi-Fi.
        limit_conn relay_connections 1000;
        # Longer than the longest long-poll (`--link-timeout`, 10 minutes).
        proxy_timeout 11m;

        proxy_pass 127.0.0.1:8080;
        proxy_protocol on;
    }
}

http {
    # Each existing site moves from port 443 to the internal port.
    server {
        listen 127.0.0.1:8443 ssl proxy_protocol;
        # Keep the real client address in logs and `limit_req`.
        set_real_ip_from 127.0.0.1;
        real_ip_header proxy_protocol;

        server_name www.example.com;
        # ... unchanged site configuration ...
    }
}
```

Notes:

- To check the setup, connect over both IPv4 and IPv6, for example with
  `curl -4` and `curl -6`. Each request must reach the relay. Rejected
  connections appear in the relay's log at debug level (`-vv`) as
  `Rejected connection`.
- `set_real_ip_from` in the `stream` block needs the
  `ngx_stream_realip_module`. The official Nginx packages include it; check
  with `nginx -V 2>&1 | grep stream_realip`.
- Nginx's `limit_req` is part of the `http` module, so it can't rate-limit
  requests to the relay in this setup. Use `limit_conn` in the `stream` server
  instead.
- Don't add `ssl_alpn h2` to the relay's `stream` server. Clients would then
  send HTTP/2, which the relay rejects.
- The `stream` server only serves the relay's hostname. Port 80 and ACME
  challenges stay in your `http` blocks. After renewing the relay's
  certificate, reload Nginx so the `stream` server uses it.
- If the relay runs in a container, trust the address Nginx connects from, such
  as the Docker bridge gateway, instead of `127.0.0.1`.

## Security requirements for PROXY protocol

- **Trust only your proxy.** A trusted proxy can claim any client address.
  Firewall the relay's port, or bind it to `127.0.0.1` or a private network, so
  nothing else can reach it. Trusting `127.0.0.1` trusts every process on the
  host.
- **The first proxy must see the real client.** In a chain of proxies, each
  hop must forward the address it received in a PROXY preface, as in the setup
  above. It must never copy a client-supplied header.
- **Use one upstream connection per client.** The relay applies the address in
  the preface to every request on that connection. Nginx `stream` servers
  already do this.

## Health checks

With `--proxy-protocol-trusted`, the relay rejects connections without a PROXY
preface, so a check that connects to the relay's port directly fails. Check
through Nginx instead:

```bash
curl https://relay.example.com/
# Http Relay
```

[proxy-protocol]: https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt
