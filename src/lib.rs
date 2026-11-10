//! A Rust implementation of _some_ of [Http relay spec](https://httprelay.io/).
//!
//! Start a relay with [`HttpRelay::builder`].
//!
//! # Feature Flags
//!
//! - `persist` (default): SQLite persistence, configured with
//!   `HttpRelayBuilder::persist_db`. Without it, entries live in memory only.
//! - `link-compat` (default): Legacy `/link/{id}` endpoints (deprecated).
//! - `cli` (default): The `http-relay` binary.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod client_ip;
mod endpoint;
mod inbox;
#[cfg(feature = "link-compat")]
mod link;
mod server;
mod storage;
mod waiting_clients;
mod waiting_list;

pub use server::{HttpRelay, HttpRelayBuilder};
