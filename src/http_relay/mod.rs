//! HTTP relay server and configuration.

mod link_handler;
mod response;
mod server;
mod waiting_list;

pub(crate) use link_handler::{link, link2};
pub use server::{HttpRelay, HttpRelayBuilder};
pub(crate) use server::AppState;
#[cfg(test)]
pub(crate) use server::Config;
