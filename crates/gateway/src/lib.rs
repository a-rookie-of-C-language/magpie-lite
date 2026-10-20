//! magpie-gateway: the local server agents talk to.

pub mod affinity;
pub mod agents;
mod billing;
pub mod classify;
pub mod config;
pub mod config_store;
mod continuation;
pub mod cooldown;
mod deepseek_schema;
mod edge_block;
mod gemini_schema;
mod grok_integral;
mod grok_schema;
mod kimi_schema;
pub mod lanes;
mod maxoutput;
mod model_gone;
mod object_root;
mod oneof_schema;
mod optional_fields;
pub mod presets;
mod provider_tools;
mod providers;
mod reply_room;
mod retired;
mod roomy;
pub mod rules;
pub mod server;
mod stream_empty;
mod stream_keepalive;
mod stream_loop;
mod stream_refusal;
mod stream_status;
mod stream_timeout;
mod tool_schema;
mod unfit_fields;
pub mod upstream;
pub mod usage;
mod wrong_endpoint;

pub use affinity::AffinityStore;
pub use config::{
    Affinity, Config, GroupConfig, KeyRotator, Protocol, ProviderCapabilities, ProviderConfig,
    RoutingPolicy, config_path,
};
pub use config_store::ConfigStore;
pub use retired::{init as init_retired_store, is_retired as model_is_retired};
pub use rules::Rule;
pub use server::{AppState, build_router, serve, serve_with_config_path};
