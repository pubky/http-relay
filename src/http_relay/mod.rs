//! HTTP relay server and configuration.

pub(crate) mod link;
pub(crate) mod link2;
mod server;
mod waiting_list;

pub use server::{HttpRelay, HttpRelayBuilder};
pub(crate) use server::AppState;
#[cfg(test)]
pub(crate) use server::Config;
