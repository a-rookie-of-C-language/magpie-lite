//! magpie-gateway: the local server agents talk to.

pub mod config;
pub mod server;
pub mod upstream;
pub mod usage;

pub use config::{Config, Protocol, ProviderConfig, config_path};
pub use server::{AppState, build_router, serve};
