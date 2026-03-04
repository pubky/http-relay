//! A Rust implementation of _some_ of [Http relay spec](https://httprelay.io/).
//!
//! # Feature Flags
//!
//! - `server` (default): HTTP server with axum. Enables `HttpRelay` and `HttpRelayBuilder`.
//! - `persist` (default): SQLite persistence. Without this, uses in-memory HashMap storage.
//! - `link-compat`: Legacy `/link/{id}` endpoints (deprecated, requires `server`).
//!
//! # Library-only Usage
//!
//! Without the `server` feature, you can use the core types directly:
//! ```ignore
//! use http_relay::{EntryRepository, WaitingList, Message};
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![cfg_attr(any(), deny(clippy::unwrap_used))]

mod http_relay;

// Server exports (only with `server` feature)
#[cfg(feature = "server")]
pub use http_relay::{HttpRelay, HttpRelayBuilder};

// Core types (always available)
pub use http_relay::{
    EntryRepository, GetOrSubscribeResult, Message, StoredEntry, SubscribeError, WaitingList,
};
