//! magpie-gateway: the local server agents talk to.

pub mod agents;
pub mod config;
pub mod config_store;
pub mod cooldown;
mod provider_tools;
mod providers;
pub mod server;
mod stream_status;
mod stream_timeout;
pub mod upstream;
pub mod usage;

pub use config::{
    Config, GroupConfig, KeyRotator, Protocol, ProviderCapabilities, ProviderConfig, RoutingPolicy,
    config_path,
};
pub use config_store::ConfigStore;
pub use server::{AppState, build_router, serve, serve_with_config_path};
