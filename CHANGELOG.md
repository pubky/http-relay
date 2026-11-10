# Changelog

All notable changes are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `--trusted-proxy <CIDR>` (repeatable) and `HttpRelayBuilder::trusted_proxies`:
  behind a reverse proxy, the relay identifies clients by the rightmost
  `X-Forwarded-For` entry that isn't a trusted proxy. Requests from a trusted
  proxy without a valid header get `400 Invalid X-Forwarded-For header`; the
  relay never falls back to the proxy's own address. Without the flag, the
  client is the TCP peer, as before. See
  [How the relay identifies clients](DEPLOY.md#how-the-relay-identifies-clients).
- Two limits per client, 100 by default each and counted separately. A client
  is an IPv4 address or an IPv6 /56 network. Each limit must be at least 1.
  See [Limits per client](DEPLOY.md#limits-per-client).
  - `--max-entries-per-client` and `HttpRelayBuilder::max_entries_per_client`:
    the unACKed messages the client can have stored. Further `POST`s to
    `/inbox/{id}` and `/link/{id}` get `429 Too Many Requests` with a
    `Retry-After` header. Replacing one of the client's unACKed messages is
    always allowed.
  - `--max-waiting-per-client` and `HttpRelayBuilder::max_waiting_per_client`:
    the requests the client can have waiting: `GET /inbox/{id}`,
    `GET /inbox/{id}/await`, and both `/link/{id}` methods. Further ones get
    `429 Too Many Requests` with `Retry-After: 1`, and a rejected
    `POST /link/{id}` stores nothing.
    A `GET` for a message that is already stored doesn't wait, so the limit
    doesn't apply to it. Without this limit, a few clients could take every
    slot for waiting IDs, and everyone else's requests for new IDs would get
    `503`.
- `DEPLOY.md`: what a production relay requires from its reverse proxy, an
  Nginx setup, running behind a CDN, and the known gaps in the limits per
  client.
- The builder's defaults are public constants, such as
  `HttpRelayBuilder::DEFAULT_MAX_ENTRIES`.

### Changed
- **Breaking:** The crate exports only `HttpRelay` and `HttpRelayBuilder`.
  `EntryRepository`, `WaitingList`, `Message`, `StoredEntry`, `InsertOutcome`,
  `GetOrSubscribeResult`, `SubscribeError`, and `StoreAndSubscribeError` are
  now private.
- **Breaking:** The minimum supported Rust version is 1.89.
- **Breaking:** When the store is full, a `POST` to a new ID first deletes
  expired entries, then the ACKed entries stored longest ago. If that isn't
  enough, it gets `503 Server at capacity`. Before, the relay deleted the
  oldest entry, even one whose message hadn't been delivered.
- `--max-entries` also limits how many IDs can have waiting requests. A
  long-poll or `/link` request for a new ID beyond it gets
  `503 Too many concurrent requests`. Before, the number was unlimited.
- With `--persist-db`, opening a database file from an earlier version adds an
  `owner` column. It holds a keyed hash of the client network, not its
  address. The key isn't saved, so after a restart, stored messages count
  against no client's limit until they expire. Version 0.7.0 can still open
  the file, so you can roll back.
- With `--persist-db`, SQLite's page cache can grow to 64 MB, up from 2 MB.
- The `server` feature has no effect; the server is always built. The feature
  stays so that dependents that enable it still build.

### Fixed
- A `POST /link/{id}` that gets `503` no longer leaves its message stored for
  consumers.
- Requests that disconnected no longer count toward the 10 waiting requests
  allowed per ID. Before, a client that reconnected often, such as a phone on
  a flaky network, could get `503` because of its own earlier requests.
- The cleanup task no longer looks up each ID while holding the lock that
  every request needs.
- A rejected or failed `POST` no longer changes anything in storage.
- The Dockerfile builds again.

### Security
- Relayed messages are served with `Content-Security-Policy: sandbox` and
  `X-Content-Type-Options: nosniff`, so a `text/html` message can't run
  scripts on the relay's origin.
- Long-poll requests for unique IDs that disconnected right away could grow
  the relay's memory without limit.
- With `--persist-db`, SQLite overwrites deleted messages with zeros
  (`secure_delete`). Copies can still remain on disk, for example in the
  `-wal` file; don't relay sensitive data without encrypting it.
- The bundled SQLite is updated to 3.54.1 for RUSTSEC-2026-0459
  (CVE-2026-119375).
- Dependencies are updated for RUSTSEC-2026-0190, RUSTSEC-2026-0258,
  RUSTSEC-2026-0009, and RUSTSEC-2026-0386.

## [0.7.0] - 2026-03-10

### Changed
- **Breaking:** CORS headers are off by default. Start the relay with
  `--cors-allow-all` to send `Access-Control-Allow-Origin: *` as before.

## [0.6.0] - 2026-03-05

### Added
- `/inbox/{id}` endpoints: store-and-forward with explicit ACK. Consumers
  long-poll with `GET` and ACK with `DELETE`; producers check the ACK with
  `/ack` or wait for it with `/await`.

[Unreleased]: https://github.com/pubky/http-relay/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/pubky/http-relay/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/pubky/http-relay/releases/tag/v0.6.0
