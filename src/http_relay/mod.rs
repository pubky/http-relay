//! HTTP relay server and configuration.

pub(crate) mod inbox_handler;
pub(crate) mod link_handler;
pub mod persistence;
mod response;
mod server;
mod waiting_list;

pub(crate) use inbox_handler as inbox;
pub(crate) use link_handler as link;
pub(crate) use server::AppState;
#[cfg(test)]
pub(crate) use server::Config;
pub use server::{HttpRelay, HttpRelayBuilder};
