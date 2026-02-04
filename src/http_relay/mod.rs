//! HTTP relay server and configuration.

pub mod types;

// Storage: use SQLite when `persist` feature is enabled, otherwise in-memory HashMap
#[cfg(feature = "persist")]
pub mod persistence;
#[cfg(not(feature = "persist"))]
#[path = "storage_memory.rs"]
pub mod persistence;

mod waiting_list;

// Server components (only with `server` feature)
#[cfg(feature = "server")]
pub(crate) mod inbox_handler;
#[cfg(feature = "server")]
mod response;
#[cfg(feature = "server")]
mod server;

// Legacy link handler (only with `link-compat` feature, requires `server`)
#[cfg(all(feature = "server", feature = "link-compat"))]
pub(crate) mod link_handler;

#[cfg(feature = "server")]
pub(crate) use inbox_handler as inbox;
#[cfg(all(feature = "server", feature = "link-compat"))]
pub(crate) use link_handler as link;
#[cfg(feature = "server")]
pub(crate) use server::AppState;
#[cfg(all(test, feature = "server"))]
pub(crate) use server::Config;
#[cfg(feature = "server")]
pub use server::{HttpRelay, HttpRelayBuilder};

// Re-export core types for library users (available without server feature)
pub use persistence::EntryRepository;
pub use types::StoredEntry;
pub use waiting_list::{GetOrSubscribeResult, LimitError, Message, WaitingList};

/// Returns the current Unix timestamp in milliseconds.
pub(crate) fn unix_timestamp_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time before Unix epoch")
        .as_millis() as i64
}
