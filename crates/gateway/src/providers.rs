//! Token-protected provider edits; responses contain routing metadata only.
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::config::{Config, ConfigSnapshot, Protocol, ProviderCapabilities, ProviderConfig};
use crate::config_store::ConfigError;
use crate::server::AppState;
use crate::upstream;

/// Metadata is replaced; omitted keys preserve existing credentials.
/// An explicit empty pool clears credentials for an unauthenticated upstream.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderEdit {
    id: String,
    #[serde(default)]
    name: String,
    protocol: Protocol,
    base_url: String,
    /// Omitted on update to preserve existing metadata; `{}` clears it.
    capabilities: Option<ProviderCapabilities>,
    api_keys: Option<Vec<String>>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    fallback: Vec<String>,
    key_cooldown_ms: Option<u64>,
    #[serde(default)]
    make_default: bool,
}

impl ProviderEdit {
    pub(crate) fn materialize(self, config: &Config) -> ProviderConfig {
        let existing = config.provider(&self.id);
        let capabilities = self
            .capabilities
            .unwrap_or_else(|| existing.map(|p| p.capabilities.clone()).unwrap_or_default());
        let (api_key, api_keys) = match self.api_keys {
            Some(keys) => (String::new(), keys),
            None => existing
                .map(|p| (p.api_key.clone(), p.api_keys.clone()))
                .unwrap_or_default(),
        };
        ProviderConfig {
            id: self.id,
            name: self.name,
            protocol: self.protocol,
            base_url: self.base_url,
            capabilities,
            api_key,
            api_keys,
            models: self.models,
            fallback: self.fallback,
            key_cooldown_ms: self.key_cooldown_ms,
            key_generation: 0,
        }
    }

    fn apply(self, config: &mut Config) -> Result<(), ConfigError> {
        let make_default = self.make_default;
        let provider = self.materialize(config);
        config
            .set_provider(provider, make_default)
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))
    }
}

fn decode(bytes: &[u8]) -> Result<ProviderEdit, &'static str> {
    serde_json::from_slice(bytes).map_err(
        |_| "expected provider JSON with id, protocol, base_url, optional metadata and api_keys",
    )
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(upstream::chat_error(status, message))).into_response()
}

fn response(result: Result<ConfigSnapshot, ConfigError>, status: StatusCode) -> Response {
    match result {
        Ok(snapshot) => (status, Json(snapshot)).into_response(),
        Err((status, message)) => error(status, &message),
    }
}

pub async fn create(State(state): State<Arc<AppState>>, bytes: Bytes) -> Response {
    let edit = match decode(&bytes) {
        Ok(edit) => edit,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    response(
        state
            .config
            .update(move |config| {
                if config.provider(&edit.id).is_some() {
                    return Err((StatusCode::CONFLICT, "provider already exists".to_owned()));
                }
                edit.apply(config)
            })
            .await,
        StatusCode::CREATED,
    )
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Response {
    let edit = match decode(&bytes) {
        Ok(edit) => edit,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if edit.id != id {
        return error(StatusCode::BAD_REQUEST, "body id must match path id");
    }
    response(
        state
            .config
            .update(move |config| {
                if config.provider(&id).is_none() {
                    return Err((StatusCode::NOT_FOUND, "provider not found".to_owned()));
                }
                edit.apply(config)
            })
            .await,
        StatusCode::OK,
    )
}

pub async fn delete(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    response(
        state
            .config
            .update(move |config| {
                if config.provider(&id).is_none() {
                    return Err((StatusCode::NOT_FOUND, "provider not found".to_owned()));
                }
                config
                    .remove_provider(&id)
                    .map_err(|error| (StatusCode::CONFLICT, error.to_string()))
            })
            .await,
        StatusCode::OK,
    )
}
