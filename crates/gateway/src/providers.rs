//! Token-protected provider edits; responses contain routing metadata only.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::config::{
    Affinity, Config, ConfigSnapshot, Protocol, ProviderCapabilities, ProviderConfig,
    RoutingPolicy, default_provider_routing,
};
use crate::config_store::ConfigError;
use crate::server::{AppState, config_response, expected_config_revision, idempotency_request};
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
    /// Omitted on update to preserve the existing windows; `{}` clears them.
    contexts: Option<BTreeMap<String, u64>>,
    /// Omitted on update to preserve the existing Max outputs; `{}` clears them.
    outputs: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    fallback: Vec<String>,
    key_cooldown_ms: Option<u64>,
    /// Omitted on update to preserve the existing policy.
    routing: Option<RoutingPolicy>,
    /// Omitted on update to preserve the existing affinity.
    affinity: Option<Affinity>,
    /// Omitted on update to preserve the existing limits.
    max_concurrency: Option<u32>,
    max_rpm: Option<u32>,
    queue_limit: Option<u32>,
    queue_wait_s: Option<u32>,
    /// Omitted on update to preserve the existing weights; `[]` clears them.
    weights: Option<Vec<u32>>,
    /// Omitted on update to preserve the existing per-key overrides; `[]`
    /// clears them.
    key_limits: Option<Vec<u32>>,
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
        let routing = self.routing.unwrap_or_else(|| {
            existing
                .map(|p| p.routing)
                .unwrap_or_else(default_provider_routing)
        });
        let weights = self
            .weights
            .unwrap_or_else(|| existing.map(|p| p.weights.clone()).unwrap_or_default());
        let key_limits = self
            .key_limits
            .unwrap_or_else(|| existing.map(|p| p.key_limits.clone()).unwrap_or_default());
        let contexts = self
            .contexts
            .unwrap_or_else(|| existing.map(|p| p.contexts.clone()).unwrap_or_default());
        let outputs = self
            .outputs
            .unwrap_or_else(|| existing.map(|p| p.outputs.clone()).unwrap_or_default());
        let affinity = self
            .affinity
            .unwrap_or_else(|| existing.map(|p| p.affinity).unwrap_or_default());
        let max_concurrency = self
            .max_concurrency
            .or_else(|| existing.map(|p| p.max_concurrency).unwrap_or_default());
        let max_rpm = self
            .max_rpm
            .or_else(|| existing.map(|p| p.max_rpm).unwrap_or_default());
        let queue_limit = self
            .queue_limit
            .or_else(|| existing.map(|p| p.queue_limit).unwrap_or_default());
        let queue_wait_s = self
            .queue_wait_s
            .or_else(|| existing.map(|p| p.queue_wait_s).unwrap_or_default());
        ProviderConfig {
            id: self.id,
            name: self.name,
            protocol: self.protocol,
            base_url: self.base_url,
            capabilities,
            api_key,
            api_keys,
            models: self.models,
            contexts,
            outputs,
            fallback: self.fallback,
            key_cooldown_ms: self.key_cooldown_ms,
            routing,
            weights,
            key_limits,
            affinity,
            max_concurrency,
            max_rpm,
            queue_limit,
            queue_wait_s,
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
        Ok(snapshot) => config_response(status, snapshot),
        Err((status, message)) => error(status, &message),
    }
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let expected = match expected_config_revision(&headers) {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let request = match idempotency_request(&state.config, &headers, "provider.create", &bytes) {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let edit = match decode(&bytes) {
        Ok(edit) => edit,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                if config.provider(&edit.id).is_some() {
                    return Err((StatusCode::CONFLICT, "provider already exists".to_owned()));
                }
                edit.apply(config)
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                if config.provider(&edit.id).is_some() {
                    return Err((StatusCode::CONFLICT, "provider already exists".to_owned()));
                }
                edit.apply(config)
            })
            .await
    };
    response(result, StatusCode::CREATED)
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let expected = match expected_config_revision(&headers) {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let scope = format!("provider.update:{id}");
    let request = match idempotency_request(&state.config, &headers, &scope, &bytes) {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let edit = match decode(&bytes) {
        Ok(edit) => edit,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if edit.id != id {
        return error(StatusCode::BAD_REQUEST, "body id must match path id");
    }
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                if config.provider(&id).is_none() {
                    return Err((StatusCode::NOT_FOUND, "provider not found".to_owned()));
                }
                edit.apply(config)
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                if config.provider(&id).is_none() {
                    return Err((StatusCode::NOT_FOUND, "provider not found".to_owned()));
                }
                edit.apply(config)
            })
            .await
    };
    response(result, StatusCode::OK)
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let expected = match expected_config_revision(&headers) {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let scope = format!("provider.delete:{id}");
    let request = match idempotency_request(&state.config, &headers, &scope, &[]) {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                if config.provider(&id).is_none() {
                    return Err((StatusCode::NOT_FOUND, "provider not found".to_owned()));
                }
                config
                    .remove_provider(&id)
                    .map_err(|error| (StatusCode::CONFLICT, error.to_string()))
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                if config.provider(&id).is_none() {
                    return Err((StatusCode::NOT_FOUND, "provider not found".to_owned()));
                }
                config
                    .remove_provider(&id)
                    .map_err(|error| (StatusCode::CONFLICT, error.to_string()))
            })
            .await
    };
    response(result, StatusCode::OK)
}
