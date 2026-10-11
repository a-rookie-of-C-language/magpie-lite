//! magpie-gateway: the local server agents talk to.

pub mod config;
pub mod config_store;
pub mod cooldown;
mod providers;
pub mod server;
pub mod upstream;
pub mod usage;

pub use config::{Config, GroupConfig, KeyRotator, Protocol, ProviderConfig, config_path};
pub use config_store::ConfigStore;
pub use server::{AppState, build_router, serve, serve_with_config_path};
