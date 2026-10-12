//! The gateway itself: a local HTTP server that speaks OpenAI Chat
//! Completions, OpenAI Responses, Anthropic Messages, and Google Gemini to
//! agents, translates each request through the IR, and forwards it to
//! whichever provider serves the model.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::StreamExt;
use magpie_core::ir::{CompletionRequest, Usage};
use magpie_core::stream::{SseParser, StreamEvent};
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

use crate::config::{Config, GroupConfig, Protocol, ProviderConfig};
use crate::config_store::{ConfigStore, IdempotencyRequest};
use crate::cooldown::quota_cooldown;
use crate::stream_keepalive::{self, Keepalive, UpstreamActivity};
use crate::stream_loop::LoopGuard;
use crate::stream_timeout::{GroupFirstToken, ResponsesHold, StreamClock, StreamTimeout};
use crate::upstream;
use crate::usage::{RequestLog, RequestTracker};

/// The dependency-free local console. It contains no secrets and fetches
/// protected data only after the user supplies the gateway token.
const CONSOLE_HTML: &str = include_str!("console.html");

/// Shared handler state.
pub struct AppState {
    /// Optional shared agent lifecycle manager and actual listening port.
    pub agents: Option<crate::agents::AgentService>,
    /// The loaded configuration.
    pub config: ConfigStore,
    /// The pooled upstream client.
    pub http: reqwest::Client,
    /// In-memory request totals and recent history for `/stats`.
    pub requests: Arc<Mutex<RequestLog>>,
    /// Round-robin cursors for providers with multiple API keys.
    pub key_rotation: crate::config::KeyRotator,
}

/// Build the router around a state; what `serve` runs and tests drive.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/responses", post(responses_api))
        .route("/v1beta/models", get(gemini_models))
        .route("/v1beta/models/{*model}", post(gemini_generate))
        .route("/v1/models", get(list_models))
        .route("/agents", get(crate::agents::list))
        .route("/agents/{id}/preview", post(crate::agents::preview))
        .route("/agents/{id}/apply", post(crate::agents::apply))
        .route("/agents/{id}/recover", post(crate::agents::recover))
        .route("/stats", get(stats))
        .route("/config", get(config))
        .route("/config/providers", post(crate::providers::create))
        .route(
            "/config/providers/{id}",
            put(crate::providers::update).delete(crate::providers::delete),
        )
        .route(
            "/config/provider-tools/models",
            post(crate::provider_tools::models),
        )
        .route(
            "/config/provider-tools/test",
            post(crate::provider_tools::test),
        )
        .route("/config/groups", post(create_group))
        .route(
            "/config/groups/{id}",
            put(update_group).delete(delete_group),
        )
        // Every route above proves it knows the configured local token
        // before a request can reach a provider or read statistics. The
        // health probe and secret-free login page below are added after
        // the layer and stay public; the page itself fetches no protected
        // data until the user enters the token.
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .route("/health", get(health))
        .route("/", get(console))
        .route("/ui", get(console))
        .with_state(state)
}

/// Require the configured local token on every model route.
///
/// Agents send it in the header natural to their protocol: Bearer for
/// OpenAI-compatible clients, `x-api-key` or Bearer for Anthropic, and
/// `x-goog-api-key` for Gemini. The comparison is constant-time because a
/// local process can still attempt timing measurements.
async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let authorized = bearer_token(headers)
        .or_else(|| header_token(headers, "x-api-key"))
        .or_else(|| header_token(headers, "x-goog-api-key"))
        .is_some_and(|present| constant_time_eq(present, &state.config.current().token));
    if !authorized {
        let status = StatusCode::UNAUTHORIZED;
        return err_response(status, auth_error_body(request.uri(), status));
    }
    next.run(request).await
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return None;
    }
    Some(token)
}

fn header_token<'a>(headers: &'a axum::http::HeaderMap, name: &'static str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;

    let mut diff = (a.len() ^ b.len()) as u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff.ct_eq(&0).into()
}

fn auth_error_body(uri: &Uri, status: StatusCode) -> Value {
    let path = uri.path();
    if path.starts_with("/v1beta") {
        upstream::gemini_error(status, "missing or invalid magpie-lite token")
    } else if path == "/v1/messages" {
        upstream::anthropic_error(status, "missing or invalid magpie-lite token")
    } else if path == "/v1/responses" {
        upstream::responses_error(status, "missing or invalid magpie-lite token")
    } else {
        upstream::chat_error(status, "missing or invalid magpie-lite token")
    }
}

/// Run the gateway until the process is stopped.
pub async fn serve(config: Config, port: Option<u16>) -> anyhow::Result<()> {
    serve_with_config_path(config, port, None, None).await
}

/// Run with persistent console editing and an optional runtime-only history override.
pub async fn serve_with_config_path(
    config: Config,
    port: Option<u16>,
    config_path: Option<std::path::PathBuf>,
    history_path: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let requested_port = port.unwrap_or(config.port);
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], requested_port))).await?;
    let addr = listener.local_addr()?;
    let port = addr.port();
    let http = upstream::http_client()?;
    let requests = match history_path.as_ref().or(config.history_path.as_ref()) {
        Some(path) => {
            let log = RequestLog::open(path)?;
            Arc::new(Mutex::new(log))
        }
        None => Arc::new(Mutex::new(RequestLog::default())),
    };
    let agent_config = match &config_path {
        Some(path) => path.clone(),
        None => crate::config::config_path()?,
    };
    let agent_state_dir = magpie_agents::lifecycle::state_directory(&agent_config)?;
    let manager = magpie_agents::lifecycle::Manager::native(agent_state_dir.clone())?;
    let state = Arc::new(AppState {
        agents: Some(crate::agents::AgentService {
            manager: Arc::new(manager),
            port,
            state_dir: agent_state_dir,
        }),
        config: ConfigStore::new(config, config_path),
        http,
        requests,
        key_rotation: crate::config::KeyRotator::default(),
    });
    let app = build_router(state);

    tracing::info!("magpie-lite gateway listening on http://{addr}");
    println!("magpie-lite gateway listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// GET `/` and `/ui` — the static console shell. `/stats` and `/v1/models`
/// remain token-protected; this response has no gateway or provider secrets.
async fn console() -> Html<&'static str> {
    Html(CONSOLE_HTML)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Optional Unix-millisecond bounds for the recent-history part of `/stats`.
/// `from` is inclusive and `to` is exclusive. Lifetime process totals remain
/// unfiltered because they are not reconstructed from the bounded history log.
#[derive(Debug, Default, serde::Deserialize)]
struct StatsQuery {
    from: Option<u64>,
    to: Option<u64>,
}

/// GET /stats — totals and recent request metadata, token-protected.
async fn stats(State(state): State<Arc<AppState>>, Query(query): Query<StatsQuery>) -> Response {
    if let (Some(from), Some(to)) = (query.from, query.to)
        && from > to
    {
        return err_response(
            StatusCode::BAD_REQUEST,
            upstream::chat_error(StatusCode::BAD_REQUEST, "stats `from` must be <= `to`"),
        );
    }

    let mut snapshot = state
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot_filtered(query.from, query.to);
    snapshot.key_cooldowns = state
        .key_rotation
        .cooldown_snapshot(&state.config.current());
    Json(snapshot).into_response()
}

/// GET `/config` — secret-free routing metadata for the console.
async fn config(State(state): State<Arc<AppState>>) -> Response {
    config_response(StatusCode::OK, state.config.snapshot())
}

/// Parse the optional optimistic-concurrency precondition used by management writes.
/// Bare numeric values and standard quoted ETags are accepted; `*` disables the check.
pub(crate) fn expected_config_revision(headers: &HeaderMap) -> Result<Option<u64>, &'static str> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| "If-Match must contain a numeric configuration revision")?
        .trim();
    if raw == "*" {
        return Ok(None);
    }
    let revision = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(raw);
    revision
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "If-Match must contain a numeric configuration revision")
}

/// Parse an optional durable idempotency key for one management operation.
/// The request body is reduced to a token-keyed digest before it reaches the
/// durable ledger, so neither the token nor raw credentials are persisted.
pub(crate) fn idempotency_request(
    config: &ConfigStore,
    headers: &HeaderMap,
    scope: &str,
    payload: &[u8],
) -> Result<Option<IdempotencyRequest>, &'static str> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| "Idempotency-Key must contain visible ASCII characters")?
        .trim();
    if key.is_empty() {
        return Err("Idempotency-Key cannot be empty");
    }
    if key.len() > 200 {
        return Err("Idempotency-Key must be at most 200 bytes");
    }
    Ok(Some(IdempotencyRequest {
        scope: scope.to_owned(),
        key: key.to_owned(),
        digest: config.idempotency_digest(scope, payload),
    }))
}

pub(crate) fn config_response(
    status: StatusCode,
    snapshot: crate::config::ConfigSnapshot,
) -> Response {
    let etag = format!("\"{}\"", snapshot.revision);
    (status, [(header::ETAG, etag)], Json(snapshot)).into_response()
}

fn group_error(status: StatusCode, message: &str) -> Response {
    err_response(status, upstream::chat_error(status, message))
}

fn decode_group(bytes: &[u8]) -> Result<GroupConfig, &'static str> {
    serde_json::from_slice(bytes).map_err(|_| "expected JSON with id, optional name, and routes")
}

fn validate_group(config: &Config, group: &GroupConfig) -> Result<(), (StatusCode, String)> {
    group
        .validate()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    for route in &group.routes {
        let provider = route.split_once('/').expect("validated route").0;
        if config.provider(provider).is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown provider `{provider}`"),
            ));
        }
    }
    Ok(())
}

async fn create_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let expected = match expected_config_revision(&headers) {
        Ok(value) => value,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let request = match idempotency_request(&state.config, &headers, "group.create", &bytes) {
        Ok(value) => value,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let group = match decode_group(&bytes) {
        Ok(group) => group,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                validate_group(config, &group)?;
                if config.group.iter().any(|existing| existing.id == group.id) {
                    return Err((StatusCode::CONFLICT, "group already exists".to_owned()));
                }
                config.group.push(group);
                Ok(())
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                validate_group(config, &group)?;
                if config.group.iter().any(|existing| existing.id == group.id) {
                    return Err((StatusCode::CONFLICT, "group already exists".to_owned()));
                }
                config.group.push(group);
                Ok(())
            })
            .await
    };
    group_update_response(result, StatusCode::CREATED)
}

async fn update_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let expected = match expected_config_revision(&headers) {
        Ok(value) => value,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let scope = format!("group.update:{id}");
    let request = match idempotency_request(&state.config, &headers, &scope, &bytes) {
        Ok(value) => value,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let group = match decode_group(&bytes) {
        Ok(group) => group,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    if group.id != id {
        return group_error(StatusCode::BAD_REQUEST, "body id must match path id");
    }
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                validate_group(config, &group)?;
                let existing = config
                    .group
                    .iter_mut()
                    .find(|existing| existing.id == id)
                    .ok_or_else(|| (StatusCode::NOT_FOUND, "group not found".to_owned()))?;
                *existing = group;
                Ok(())
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                validate_group(config, &group)?;
                let existing = config
                    .group
                    .iter_mut()
                    .find(|existing| existing.id == id)
                    .ok_or_else(|| (StatusCode::NOT_FOUND, "group not found".to_owned()))?;
                *existing = group;
                Ok(())
            })
            .await
    };
    group_update_response(result, StatusCode::OK)
}

async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let expected = match expected_config_revision(&headers) {
        Ok(value) => value,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let scope = format!("group.delete:{id}");
    let request = match idempotency_request(&state.config, &headers, &scope, &[]) {
        Ok(value) => value,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                let index = config
                    .group
                    .iter()
                    .position(|group| group.id == id)
                    .ok_or_else(|| (StatusCode::NOT_FOUND, "group not found".to_owned()))?;
                let reference = format!("group/{id}");
                if config
                    .provider
                    .iter()
                    .any(|provider| provider.fallback.contains(&reference))
                {
                    return Err((
                        StatusCode::CONFLICT,
                        "group is referenced by a provider fallback".to_owned(),
                    ));
                }
                config.group.remove(index);
                Ok(())
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                let index = config
                    .group
                    .iter()
                    .position(|group| group.id == id)
                    .ok_or_else(|| (StatusCode::NOT_FOUND, "group not found".to_owned()))?;
                let reference = format!("group/{id}");
                if config
                    .provider
                    .iter()
                    .any(|provider| provider.fallback.contains(&reference))
                {
                    return Err((
                        StatusCode::CONFLICT,
                        "group is referenced by a provider fallback".to_owned(),
                    ));
                }
                config.group.remove(index);
                Ok(())
            })
            .await
    };
    group_update_response(result, StatusCode::OK)
}

fn group_update_response(
    result: Result<crate::config::ConfigSnapshot, (StatusCode, String)>,
    status: StatusCode,
) -> Response {
    match result {
        Ok(snapshot) => config_response(status, snapshot),
        Err((status, message)) => group_error(status, &message),
    }
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let data: Vec<Value> = state
        .config
        .current()
        .catalog()
        .into_iter()
        .map(|(id, owner)| json!({"id": id, "object": "model", "owned_by": owner}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

/// GET /v1beta/models — the same catalog in Google's shape.
async fn gemini_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let models: Vec<Value> = state
        .config
        .current()
        .catalog()
        .into_iter()
        .map(|(id, owner)| {
            json!({
                "name": format!("models/{id}"),
                "displayName": owner,
                "description": format!("{owner} via magpie-lite"),
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    Json(json!({"models": models}))
}

/// A resolved route: one provider/model and the ordered key pool to try
/// before moving on to the next route.
#[derive(Debug, Clone)]
struct UpstreamRoute {
    provider: Arc<ProviderConfig>,
    model: String,
    api_keys: Vec<(usize, String)>,
    group_id: Option<String>,
    group_route_index: Option<usize>,
}

#[derive(Debug, Clone)]
struct GroupRoutePlan {
    id: String,
    routes: Vec<(usize, String)>,
}

/// The concrete attempt that served (or last failed) a request.
#[derive(Debug, Clone)]
struct UpstreamCandidate {
    provider: Arc<ProviderConfig>,
    model: String,
    key_slot: usize,
}

impl UpstreamCandidate {
    fn new(route: &UpstreamRoute, key_slot: usize) -> Self {
        Self {
            provider: route.provider.clone(),
            model: route.model.clone(),
            key_slot,
        }
    }
}

/// The agent-facing protocol a request arrived in. Beside decoding, this
/// chooses the reply encoder and the error body.
#[derive(Debug, Clone, Copy)]
enum ClientWire {
    Chat { include_usage: bool },
    Anthropic,
    Responses,
    Gemini,
}

impl ClientWire {
    fn error_body(self, status: StatusCode, message: &str) -> Value {
        match self {
            Self::Chat { .. } => upstream::chat_error(status, message),
            Self::Anthropic => upstream::anthropic_error(status, message),
            Self::Responses => upstream::responses_error(status, message),
            Self::Gemini => upstream::gemini_error(status, message),
        }
    }

    /// The name used in request statistics.
    fn name(self) -> &'static str {
        match self {
            Self::Chat { .. } => "chat",
            Self::Anthropic => "anthropic",
            Self::Responses => "responses",
            Self::Gemini => "gemini",
        }
    }

    fn decode_request(self, body: &Value) -> magpie_core::Result<CompletionRequest> {
        match self {
            Self::Chat { .. } => chat::decode_request(body),
            Self::Anthropic => anthropic::decode_request(body),
            Self::Responses => responses::decode_request(body),
            Self::Gemini => gemini::decode_request(body),
        }
    }

    fn encode_response(self, ir: &magpie_core::ir::CompletionResponse) -> Value {
        match self {
            Self::Chat { .. } => chat::encode_response(ir),
            Self::Anthropic => anthropic::encode_response(ir),
            Self::Responses => responses::encode_response(ir),
            Self::Gemini => gemini::encode_response(ir),
        }
    }
}

/// The agent-facing stream encoders, behind one small interface so the
/// upstream translation loop can be shared by every endpoint.
enum ClientStreamEncoder {
    Chat(chat::ChatChunkEncoder),
    Anthropic(anthropic::AnthropicStreamEncoder),
    Responses(responses::ResponsesStreamEncoder),
    Gemini(gemini::GeminiStreamEncoder),
}

impl ClientStreamEncoder {
    fn new(wire: ClientWire, model: &str) -> Self {
        match wire {
            ClientWire::Chat { include_usage } => {
                Self::Chat(chat::ChatChunkEncoder::new(model, include_usage))
            }
            ClientWire::Anthropic => Self::Anthropic(anthropic::AnthropicStreamEncoder::new(model)),
            ClientWire::Responses => Self::Responses(responses::ResponsesStreamEncoder::new(model)),
            ClientWire::Gemini => Self::Gemini(gemini::GeminiStreamEncoder::new(model)),
        }
    }

    fn encode(&mut self, event: &StreamEvent) -> Option<String> {
        match self {
            Self::Chat(enc) => enc.encode(event),
            Self::Anthropic(enc) => enc.encode(event),
            Self::Responses(enc) => enc.encode(event),
            Self::Gemini(enc) => enc.encode(event),
        }
    }
}

/// Stateful decoders for upstream protocols whose chunks cannot be decoded
/// independently.
#[derive(Default)]
struct UpstreamDecoders {
    chat: chat::ChatStreamDecoder,
    anthropic: anthropic::AnthropicStreamDecoder,
    gemini: gemini::GeminiStreamDecoder,
}

impl UpstreamDecoders {
    fn new() -> Self {
        Self::default()
    }
}

/// Decode the body and resolve the initial model route.
fn prepare(
    config: &Config,
    key_rotation: &crate::config::KeyRotator,
    body: &Value,
    wire: ClientWire,
) -> Result<(CompletionRequest, UpstreamRoute, Option<GroupRoutePlan>), (StatusCode, Value)> {
    let creq = wire.decode_request(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            wire.error_body(StatusCode::BAD_REQUEST, &e.to_string()),
        )
    })?;

    let (provider, upstream_model, group_plan, group_route_index) =
        if let Some(group_id) = creq.model.strip_prefix("group/") {
            let Some(group) = config.group(group_id) else {
                return Err((
                    StatusCode::NOT_FOUND,
                    wire.error_body(
                        StatusCode::NOT_FOUND,
                        &format!("no provider serves model `{}`", creq.model),
                    ),
                ));
            };
            let routes = key_rotation.group_order(group_id, group.routing, &group.routes);
            let Some((route_index, route)) =
                routes.first().map(|(index, route)| (*index, route.clone()))
            else {
                return Err((
                    StatusCode::NOT_FOUND,
                    wire.error_body(
                        StatusCode::NOT_FOUND,
                        &format!("routing group `{group_id}` has no usable routes"),
                    ),
                ));
            };
            let Some((provider, upstream_model)) = config.resolve(&route) else {
                return Err((
                    StatusCode::NOT_FOUND,
                    wire.error_body(
                        StatusCode::NOT_FOUND,
                        &format!("no provider serves model `{}`", creq.model),
                    ),
                ));
            };
            (
                provider,
                upstream_model,
                Some(GroupRoutePlan {
                    id: group_id.to_owned(),
                    routes,
                }),
                Some(route_index),
            )
        } else {
            let Some((provider, upstream_model)) = config.resolve(&creq.model) else {
                let message = format!(
                    "no provider serves model `{}`; add one with `magpie-lite provider add`",
                    creq.model
                );
                return Err((
                    StatusCode::NOT_FOUND,
                    wire.error_body(StatusCode::NOT_FOUND, &message),
                ));
            };
            (provider, upstream_model, None, None)
        };

    Ok((
        creq,
        UpstreamRoute {
            provider: Arc::new(provider.clone()),
            model: upstream_model,
            api_keys: provider.api_key_pool().into_iter().enumerate().collect(),
            group_id: group_plan.as_ref().map(|plan| plan.id.clone()),
            group_route_index,
        },
        group_plan,
    ))
}

/// The initial provider followed by its fallback references, each resolved
/// with the ordinary model rules. Every route carries its own rotated key
/// pool. Chains are deliberately one level deep: `fallback` says where this
/// model goes next, not how to build a graph.
fn upstream_routes(
    config: &Config,
    key_rotation: &crate::config::KeyRotator,
    initial: UpstreamRoute,
    requested_model: &str,
    group_plan: Option<&GroupRoutePlan>,
) -> Vec<UpstreamRoute> {
    let initial_keys = initial
        .api_keys
        .iter()
        .map(|(_, key)| key.clone())
        .collect::<Vec<_>>();
    let initial = UpstreamRoute {
        api_keys: key_rotation.rotate(
            &initial.provider.id,
            initial.provider.key_generation,
            &initial_keys,
        ),
        ..initial
    };
    let mut out = vec![initial.clone()];
    // Ordinary provider fallbacks are provider-level: once a provider has
    // been attempted, its other fallback models must not create a loop. A
    // routing group is different: each configured provider/model pair is an
    // independent route, so one provider can intentionally contribute more
    // than one model to the same group.
    let mut seen_providers = HashSet::from([initial.provider.id.clone()]);
    let mut seen_group_routes =
        HashSet::from([(initial.provider.id.clone(), initial.model.clone())]);

    let mut fallback_references = group_plan
        .map(|plan| {
            plan.routes
                .iter()
                .skip(1)
                .map(|(index, route)| (Some(*index), route.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            config
                .fallback_references(requested_model, &initial.provider)
                .into_iter()
                .map(|route| (None, route))
                .collect()
        });
    for (group_route_index, reference) in fallback_references.drain(..) {
        let Some((provider, model)) = config.resolve(&reference) else {
            tracing::warn!("fallback `{reference}` does not resolve; skipping");
            continue;
        };
        if let Some(route_index) = group_route_index {
            if !seen_group_routes.insert((provider.id.clone(), model.clone())) {
                continue;
            }
            let api_keys = key_rotation.rotate(
                &provider.id,
                provider.key_generation,
                &provider.api_key_pool(),
            );
            out.push(UpstreamRoute {
                provider: Arc::new(provider.clone()),
                model,
                api_keys,
                group_id: group_plan.map(|plan| plan.id.clone()),
                group_route_index: Some(route_index),
            });
        } else {
            if !seen_providers.insert(provider.id.clone()) {
                continue;
            }
            let api_keys = key_rotation.rotate(
                &provider.id,
                provider.key_generation,
                &provider.api_key_pool(),
            );
            out.push(UpstreamRoute {
                provider: Arc::new(provider.clone()),
                model,
                api_keys,
                group_id: None,
                group_route_index: None,
            });
        }
    }
    out
}

/// Statuses that make the next key in the same provider worth one attempt.
fn key_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 402 | 403 | 429)
}

/// Quota and rate-limit responses should influence later key selection.
fn quota_limited_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 402 | 429)
}

/// Whether an upstream status is worth spending another route on.
fn route_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 402 | 403 | 404 | 408 | 429 | 500..=599
    )
}

type UpstreamEvents = Pin<Box<dyn futures::Stream<Item = UpstreamEvent> + Send>>;

/// Internal handoff signal, never a synthetic protocol event.
enum UpstreamRead {
    Event(UpstreamEvent),
    ReleaseOpening,
}

impl From<UpstreamEvent> for UpstreamRead {
    fn from(event: UpstreamEvent) -> Self {
        Self::Event(event)
    }
}

impl From<StreamEvent> for UpstreamRead {
    fn from(event: StreamEvent) -> Self {
        Self::Event(event.into())
    }
}

/// Owned by one request, including every key/provider/group attempt.
struct StreamLiveness {
    activity: UpstreamActivity,
    responses_hold: ResponsesHold,
}

enum UpstreamReply {
    Json(reqwest::Response),
    Stream(UpstreamEvents),
}

#[derive(Clone)]
struct StreamFailure {
    status: reqwest::StatusCode,
    body: Value,
}

struct UpstreamEvent {
    event: StreamEvent,
    failure: Option<StreamFailure>,
    usage_observed: bool,
    /// True only for the loop guard's reasoning terminal. This is an internal
    /// control signal; ordinary vendor errors must never trigger a reask.
    loop_reasoning: bool,
}
impl From<StreamEvent> for UpstreamEvent {
    fn from(event: StreamEvent) -> Self {
        let failure = matches!(event, StreamEvent::Error { .. }).then(|| StreamFailure {
            status: reqwest::StatusCode::BAD_GATEWAY,
            body: Value::Null,
        });
        Self {
            event,
            failure,
            usage_observed: false,
            loop_reasoning: false,
        }
    }
}

/// Send to the initial provider's key pool, then its fallback routes. All
/// routing decisions happen before a successful response body is streamed, so
/// an agent never receives a half-finished answer from an abandoned provider.
async fn send_with_fallback(
    state: &AppState,
    config: &Config,
    creq: &CompletionRequest,
    initial: UpstreamRoute,
    group_plan: Option<&GroupRoutePlan>,
    tracker: &mut RequestTracker,
    liveness: &StreamLiveness,
) -> Result<(UpstreamReply, UpstreamCandidate), (ProxyFailure, UpstreamCandidate)> {
    let routes = upstream_routes(
        config,
        &state.key_rotation,
        initial,
        &creq.model,
        group_plan,
    );
    let last_route = routes.len().saturating_sub(1);
    let no_route = routes
        .first()
        .map(|route| {
            let key_slot = route.api_keys.first().map(|(slot, _)| *slot).unwrap_or(0);
            UpstreamCandidate::new(route, key_slot)
        })
        .expect("initial route");

    for (route_index, route) in routes.into_iter().enumerate() {
        if let (Some(group_id), Some(group_route_index)) =
            (&route.group_id, route.group_route_index)
            && let Some(group) = config.group(group_id)
        {
            state.key_rotation.record_group_attempt(
                group_id,
                group.routing,
                &group.routes,
                group_route_index,
            );
        }
        for (attempt, (key_slot, api_key)) in route.api_keys.iter().enumerate() {
            let candidate = UpstreamCandidate::new(&route, *key_slot);
            tracker.begin_attempt(&route.provider.id, &route.model, *key_slot);
            liveness.activity.attempt();
            let body = upstream::build_request(&route.provider, creq, &route.model);
            tracing::debug!(
                provider = %route.provider.id,
                model = %route.model,
                key_slot,
                route_index,
                "routing upstream"
            );

            let first_token = GroupFirstToken::new(if creq.stream && route_index != last_route {
                route
                    .group_id
                    .as_deref()
                    .and_then(|id| config.group(id))
                    .map_or(0, |group| group.first_token_timeout_ms)
            } else {
                0
            });
            let clock = StreamClock::new(
                if creq.stream {
                    config.stream_first_content_timeout_ms
                } else {
                    0
                },
                config.stream_idle_timeout_ms,
            );
            let resp = match first_token
                .wait(clock.headers(upstream::send(
                    &state.http,
                    &route.provider,
                    api_key,
                    &body,
                    &route.model,
                    creq.stream,
                )))
                .await
                .and_then(|result| result)
            {
                Ok(Ok(resp)) => resp,
                Err(timeout) => {
                    tracker.record_attempt(&route.provider.id, *key_slot, timeout.attempt_status());
                    if route_index == last_route {
                        return Err((
                            ProxyFailure::new(StatusCode::GATEWAY_TIMEOUT, timeout.message()),
                            candidate,
                        ));
                    }
                    // No response headers: an endpoint timeout is not a bad key.
                    break;
                }
                Ok(Err(e)) => {
                    tracker.record_attempt(&route.provider.id, *key_slot, "network_error");
                    let message = format!("cannot reach {}: {e}", route.provider.id);
                    tracing::warn!("{message}");
                    if route_index == last_route {
                        return Err((
                            ProxyFailure::new(StatusCode::BAD_GATEWAY, message),
                            candidate,
                        ));
                    }
                    // A connection-level failure belongs to the endpoint, not
                    // to one credential, so try the next route directly.
                    break;
                }
            };

            let status = resp.status();
            liveness.activity.headers(status.is_success());
            if status.is_success() {
                if !creq.stream {
                    tracker.record_attempt(&route.provider.id, *key_slot, "ok");
                    return Ok((UpstreamReply::Json(resp), candidate));
                }
                let headers = resp.headers().clone();
                let (events, failure) = match first_token
                    .wait(prefetch_stream(
                        resp,
                        route.provider.protocol,
                        tracker,
                        clock,
                        liveness,
                        config.stream_loop_guard,
                    ))
                    .await
                {
                    Ok(result) => result,
                    Err(timeout) => {
                        tracker.record_attempt(
                            &route.provider.id,
                            *key_slot,
                            timeout.attempt_status(),
                        );
                        // Dropping prefetch cancels the held response. Slow starts
                        // belong to this route, not a credential: skip its other
                        // keys without cooldown. Only non-final routes have a cap.
                        break;
                    }
                };
                if let Some(failure) = failure {
                    tracker.record_attempt(&route.provider.id, *key_slot, "upstream_stream_error");
                    if quota_limited_status(failure.status) {
                        let cooldown = quota_cooldown(
                            &headers,
                            Some(&failure.body),
                            config.key_cooldown_for(&route.provider),
                        );
                        state.key_rotation.cool_down(
                            &route.provider.id,
                            route.provider.key_generation,
                            *key_slot,
                            cooldown,
                        );
                    }
                    if attempt + 1 < route.api_keys.len() && key_retryable_status(failure.status) {
                        drop(events);
                        continue;
                    }
                    if route_index != last_route && route_retryable_status(failure.status) {
                        drop(events);
                        break;
                    }
                } else {
                    tracker.record_attempt(&route.provider.id, *key_slot, "ok");
                }
                return Ok((UpstreamReply::Stream(events), candidate));
            }
            let headers = resp.headers().clone();
            let error = match clock.headers(upstream::error_detail(resp)).await {
                Ok(error) => error,
                Err(timeout) => {
                    tracker.record_attempt(&route.provider.id, *key_slot, "504");
                    if route_index == last_route {
                        return Err((
                            ProxyFailure::new(StatusCode::GATEWAY_TIMEOUT, timeout.message()),
                            candidate,
                        ));
                    }
                    break;
                }
            };
            tracker.record_attempt(&route.provider.id, *key_slot, &status.as_u16().to_string());
            if quota_limited_status(status) {
                let cooldown = quota_cooldown(
                    &headers,
                    Some(&error.body),
                    config.key_cooldown_for(&route.provider),
                );
                state.key_rotation.cool_down(
                    &route.provider.id,
                    route.provider.key_generation,
                    *key_slot,
                    cooldown,
                );
            }
            let detail = error.message;
            tracing::warn!(
                "upstream {} key {}: {detail}",
                route.provider.id,
                key_slot + 1
            );
            if attempt + 1 < route.api_keys.len() && key_retryable_status(status) {
                continue;
            }
            if route_index != last_route && route_retryable_status(status) {
                break;
            }
            let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            return Err((ProxyFailure::new(status, detail), candidate));
        }
    }

    Err((
        ProxyFailure::new(StatusCode::BAD_GATEWAY, "no upstream route"),
        no_route,
    ))
}

/// Re-ask a reasoning-only loop on the exact candidate that already
/// committed the response. This deliberately bypasses fallback and key
/// rotation: it is a continuation of the same upstream attempt, not a new
/// routing decision.
async fn reask_reasoning_stream(
    state: &AppState,
    config: &Config,
    creq: &CompletionRequest,
    candidate: &UpstreamCandidate,
    tracker: &mut RequestTracker,
    liveness: &StreamLiveness,
) -> Result<UpstreamEvents, String> {
    tracker.begin_reask();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let api_key = candidate
        .provider
        .api_key_pool()
        .into_iter()
        .nth(candidate.key_slot)
        .ok_or_else(|| "reask key is no longer available".to_owned())?;
    let body = upstream::build_request(&candidate.provider, creq, &candidate.model);
    let clock = StreamClock::new(
        config.stream_first_content_timeout_ms,
        config.stream_idle_timeout_ms,
    );
    let response = upstream::send(
        &state.http,
        &candidate.provider,
        &api_key,
        &body,
        &candidate.model,
        true,
    )
    .await
    .map_err(|error| format!("reasoning reask could not reach upstream: {error}"))?;

    if !response.status().is_success() {
        let detail = clock
            .headers(upstream::error_detail(response))
            .await
            .map_err(|timeout| timeout.message().to_owned())?;
        return Err(detail.message);
    }

    let (events, _failure) = prefetch_stream(
        response,
        candidate.provider.protocol,
        tracker,
        clock,
        liveness,
        config.stream_loop_guard,
    )
    .await;
    Ok(events)
}

struct ProxyFailure {
    status: StatusCode,
    message: String,
}

impl ProxyFailure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn response(self, wire: ClientWire) -> Response {
        err_response(self.status, wire.error_body(self.status, &self.message))
    }
}

fn err_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn ok_json(v: Value) -> Response {
    Json(v).into_response()
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static response headers")
}

fn decode_upstream_response(
    protocol: Protocol,
    body: &Value,
) -> magpie_core::Result<magpie_core::ir::CompletionResponse> {
    match protocol {
        Protocol::Chat => chat::decode_response(body),
        Protocol::Anthropic => anthropic::decode_response(body),
        Protocol::Responses => responses::decode_response(body),
        Protocol::Gemini => gemini::decode_response(body),
    }
}

fn decode_upstream_chunk(
    protocol: Protocol,
    data: &Value,
    decoders: &mut UpstreamDecoders,
) -> Vec<StreamEvent> {
    match protocol {
        Protocol::Chat => decoders.chat.decode(data),
        Protocol::Anthropic => decoders.anthropic.decode(data),
        Protocol::Responses => responses::decode_chunk(data),
        Protocol::Gemini => decoders.gemini.decode(data),
    }
}

/// A single handler implementation for every agent-facing protocol.
async fn proxy(state: Arc<AppState>, bytes: Bytes, wire: ClientWire) -> Response {
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            let message = format!("invalid JSON: {e}");
            return err_response(
                StatusCode::BAD_REQUEST,
                wire.error_body(StatusCode::BAD_REQUEST, &message),
            );
        }
    };
    let config = state.config.current();
    let (creq, initial, group_plan) = match prepare(&config, &state.key_rotation, &body, wire) {
        Ok(x) => x,
        Err((status, body)) => return err_response(status, body),
    };

    let (activity, keepalive) = if creq.stream && !matches!(wire, ClientWire::Gemini) {
        Keepalive::new(
            config.stream_keepalive_interval_ms,
            config.stream_keepalive_max_silence_ms,
        )
    } else {
        (UpstreamActivity::default(), None)
    };
    let liveness = StreamLiveness {
        activity,
        responses_hold: ResponsesHold::new(
            if creq.stream && matches!(wire, ClientWire::Responses) {
                config.stream_responses_hold_timeout_ms
            } else {
                0
            },
        ),
    };
    let model = creq.model.clone();
    // Keep ownership through both header waiting and body polling. Dropping
    // either stage cancels the same work and finalizes the same tracker.
    let execute = async move {
        let mut tracker = RequestTracker::new(
            state.requests.clone(),
            wire.name(),
            &creq,
            &initial.provider.id,
            &initial.model,
            0,
        );
        let (reply, candidate) = match send_with_fallback(
            &state,
            &config,
            &creq,
            initial,
            group_plan.as_ref(),
            &mut tracker,
            &liveness,
        )
        .await
        {
            Ok((resp, candidate)) => {
                tracker.set_route(&candidate.provider.id, &candidate.model, candidate.key_slot);
                (resp, candidate)
            }
            Err((response, candidate)) => {
                tracker.set_route(&candidate.provider.id, &candidate.model, candidate.key_slot);
                tracker.finish("upstream_error", &Usage::default());
                return Err(response);
            }
        };
        let protocol = candidate.provider.protocol;

        let mut events = match reply {
            UpstreamReply::Json(resp) => {
                let upstream_body: Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        let message = format!("bad upstream body: {e}");
                        tracker.finish("bad_upstream_body", &Usage::default());
                        return Err(ProxyFailure::new(StatusCode::BAD_GATEWAY, message));
                    }
                };
                let mut status = "ok";
                let ir = decode_upstream_response(protocol, &upstream_body).unwrap_or_else(|e| {
                    tracing::warn!("decoding upstream reply: {e}");
                    status = "upstream_decode_error";
                    magpie_core::ir::CompletionResponse::default()
                });
                tracker.finish(status, &ir.usage);
                return Ok(ok_json(wire.encode_response(&ir)));
            }
            UpstreamReply::Stream(events) => events,
        };

        let client_model = creq.model.clone();
        let reask_candidate = candidate.clone();
        let out = async_stream::stream! {
            let mut encoder = ClientStreamEncoder::new(wire, &client_model);
            let mut reasked = false;
            let mut emitted_text_or_tool = false;
            'attempt: loop {
                while let Some(item) = events.next().await {
                    let event = item.event;
                    if !item.usage_observed && let StreamEvent::Usage(usage) = &event {
                        tracker.observe_usage(usage);
                    }

                    // The second upstream sends its own Start frame, but the
                    // client sees one logical response and must not receive a
                    // second response.created/message_start/chunk opening.
                    let suppress_start = reasked && matches!(event, StreamEvent::Start { .. });

                    let text_or_tool = match &event {
                        StreamEvent::TextDelta { text } => !text.is_empty(),
                        StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. } => true,
                        StreamEvent::ToolCallDelta { args, .. } => !args.is_empty(),
                        _ => false,
                    };
                    emitted_text_or_tool |= text_or_tool;

                    if item.loop_reasoning && !reasked && !emitted_text_or_tool {
                        // The triggering reasoning delta has already been sent;
                        // replace only the synthetic terminal error with one
                        // same-candidate reask. Dropping the old stream first
                        // is what cancels its response body.
                        drop(events);
                        reasked = true;
                        match reask_reasoning_stream(
                            &state,
                            &config,
                            &creq,
                            &reask_candidate,
                            &mut tracker,
                            &liveness,
                        ).await {
                            Ok(next_events) => {
                                events = next_events;
                                continue 'attempt;
                            }
                            Err(message) => {
                                tracker.finish("upstream_stream_error", &Usage::default());
                                let error = StreamEvent::Error { message };
                                if let Some(frame) = encoder.encode(&error) {
                                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                                }
                                return;
                            }
                        }
                    }

                    let terminal_status = match &event {
                        StreamEvent::Done => Some("ok"),
                        StreamEvent::Error { .. } => Some("upstream_stream_error"),
                        _ => None,
                    };
                    if let Some(status) = terminal_status {
                        // Release the HTTP response before yielding the final frame,
                        // even if the downstream is backpressured or stops polling.
                        drop(events);
                        tracker.finish(status, &Usage::default());
                        if !suppress_start && let Some(frame) = encoder.encode(&event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                        return;
                    }
                    if !suppress_start && let Some(frame) = encoder.encode(&event) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                    }
                }
                // A well-formed upstream stream always emits Done or Error;
                // keep the existing cancellation/accounting behavior if it
                // closes unexpectedly.
                tracker.finish("upstream_stream_error", &Usage::default());
                return;
            }
        };
        Ok(sse_response(Body::from_stream(out)))
    };
    match keepalive {
        Some(keepalive) => keepalive_response(execute, keepalive, wire, model).await,
        None => execute.await.unwrap_or_else(|error| error.response(wire)),
    }
}

/// Delay HTTP commitment until a reply or the first comment. A comment exposes
/// no candidate metadata and must never prevent safe pre-content fallback.
async fn keepalive_response(
    execute: impl std::future::Future<Output = Result<Response, ProxyFailure>> + Send + 'static,
    mut keepalive: Keepalive,
    wire: ClientWire,
    model: String,
) -> Response {
    let mut execute = Box::pin(execute);
    let mut wrote = tokio::time::Instant::now();
    let first = tokio::select! {
        biased;
        reply = &mut execute => Some(reply),
        _ = keepalive.wait(wrote, false) => None,
    };
    if let Some(Err(error)) = first {
        return error.response(wire);
    }
    let out = async_stream::stream! {
        let reply = match first {
            Some(reply) => reply,
            None => loop {
                wrote = tokio::time::Instant::now();
                yield Ok::<Bytes, axum::Error>(Bytes::from_static(stream_keepalive::COMMENT));
                tokio::select! {
                    biased;
                    reply = &mut execute => break reply,
                    _ = keepalive.wait(wrote, false) => {},
                }
            },
        };
        drop(execute);
        let response = match reply {
            Ok(response) => response,
            Err(error) => {
                // Headers already say 200/SSE. Report exactly one stream error,
                // not a JSON HTTP error or a fabricated completion marker.
                let event = StreamEvent::Error { message: error.message };
                if let Some(frame) = ClientStreamEncoder::new(wire, &model).encode(&event) {
                    yield Ok(Bytes::from(frame));
                }
                return;
            }
        };
        let mut bytes = response.into_body().into_data_stream();
        loop {
            tokio::select! {
                biased;
                chunk = bytes.next() => match chunk {
                    Some(chunk) => {
                        if chunk.as_ref().is_ok_and(|chunk| !chunk.is_empty()) {
                            wrote = tokio::time::Instant::now();
                        }
                        yield chunk;
                    }
                    None => return,
                },
                _ = keepalive.wait(wrote, true) => {
                    wrote = tokio::time::Instant::now();
                    yield Ok(Bytes::from_static(stream_keepalive::COMMENT));
                },
            }
        }
    };
    sse_response(Body::from_stream(out))
}

/// Decode one SSE payload, including Chat's non-JSON completion sentinel.
fn decode_upstream_data(
    protocol: Protocol,
    data: &str,
    decoders: &mut UpstreamDecoders,
) -> Vec<UpstreamEvent> {
    if protocol == Protocol::Chat && data.trim() == "[DONE]" {
        return vec![StreamEvent::Done.into()];
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    decode_upstream_chunk(protocol, &value, decoders)
        .into_iter()
        .map(|event| {
            let failure = matches!(event, StreamEvent::Error { .. }).then(|| StreamFailure {
                status: crate::stream_status::error_status(&value),
                body: value.clone(),
            });
            UpstreamEvent {
                event,
                failure,
                usage_observed: false,
                loop_reasoning: false,
            }
        })
        .collect()
}

/// Hold only opening metadata, never a visible answer, while deciding whether
/// an HTTP-200 stream is usable. A finite event budget commits pathological
/// metadata-only streams instead of buffering indefinitely.
async fn prefetch_stream(
    response: reqwest::Response,
    protocol: Protocol,
    tracker: &mut RequestTracker,
    clock: StreamClock,
    liveness: &StreamLiveness,
    loop_guard_enabled: bool,
) -> (UpstreamEvents, Option<StreamFailure>) {
    const MAX_OPENING_EVENTS: usize = 64;
    const MAX_OPENING_METADATA_BYTES: usize = 64 * 1024;
    let mut metadata_bytes = 0;
    let mut reads = Box::pin(upstream_events(
        response,
        protocol,
        clock,
        liveness.activity.clone(),
        liveness.responses_hold,
        loop_guard_enabled,
    ));
    let mut prefix = Vec::new();
    while let Some(read) = reads.next().await {
        let UpstreamRead::Event(mut item) = read else {
            break;
        };
        if let StreamEvent::Usage(usage) = &item.event {
            tracker.observe_usage(usage);
            // Replaying buffered frames must not roll back newer observed usage.
            item.usage_observed = true;
        }
        let failure = item.failure.clone();
        let terminal = matches!(item.event, StreamEvent::Done | StreamEvent::Error { .. });
        let visible = match &item.event {
            StreamEvent::Start { .. } | StreamEvent::Usage(_) => false,
            StreamEvent::TextDelta { text } | StreamEvent::ThinkingDelta { text } => {
                !text.is_empty()
            }
            _ => true,
        };
        metadata_bytes += std::mem::size_of_val(&item);
        if let StreamEvent::Start { model } = &item.event {
            metadata_bytes += model.len();
        }
        prefix.push(item);
        if terminal {
            // Close held HTTP connections even if the downstream never polls.
            drop(reads);
            return (Box::pin(futures::stream::iter(prefix)), failure);
        }
        if visible
            || prefix.len() >= MAX_OPENING_EVENTS
            || metadata_bytes >= MAX_OPENING_METADATA_BYTES
        {
            break;
        }
    }
    let events = reads.filter_map(|read| {
        std::future::ready(match read {
            UpstreamRead::Event(event) => Some(event),
            UpstreamRead::ReleaseOpening => None,
        })
    });
    (Box::pin(futures::stream::iter(prefix).chain(events)), None)
}

fn stream_timeout_event(timeout: StreamTimeout) -> UpstreamEvent {
    UpstreamEvent {
        event: StreamEvent::Error {
            message: timeout.message().into(),
        },
        failure: Some(StreamFailure {
            status: StatusCode::GATEWAY_TIMEOUT,
            body: Value::Null,
        }),
        usage_observed: false,
        loop_reasoning: false,
    }
}

/// A dropped event stream owns and cancels its upstream response. Errors and
/// protocol completion markers are terminal, not dependent on a TCP EOF.
fn upstream_events(
    response: reqwest::Response,
    protocol: Protocol,
    mut clock: StreamClock,
    activity: UpstreamActivity,
    responses_hold: ResponsesHold,
    loop_guard_enabled: bool,
) -> impl futures::Stream<Item = UpstreamRead> {
    async_stream::stream! {
        let mut bytes = response.bytes_stream();
        let mut parser = SseParser::new();
        let mut decoders = UpstreamDecoders::new();
        let mut chat_stopped = false;
        let mut loop_guard = loop_guard_enabled.then(LoopGuard::default);
        let mut hold = Some(responses_hold);
        let mut opening_held = false;
        loop {
            let read = match hold.as_ref().filter(|_| opening_held) {
                Some(hold) => clock.held_chunk(bytes.next(), hold).await,
                None => clock.chunk(bytes.next()).await.map(Some),
            };
            let chunk = match read {
                Ok(Some(Some(chunk))) => chunk,
                Ok(Some(None)) => break,
                Ok(None) => {
                    hold = None;
                    yield UpstreamRead::ReleaseOpening;
                    continue;
                }
                Err(timeout) => {
                    yield stream_timeout_event(timeout).into();
                    return;
                }
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    tracing::warn!("upstream stream read failed: {error}");
                    yield StreamEvent::Error { message: "upstream stream read failed".into() }.into();
                    return;
                }
            };
            activity.chunk(&chunk);
            let mut remaining = chunk.as_ref();
            loop {
                let data = match parser.feed(&mut remaining) {
                    Ok(Some(data)) => data,
                    Ok(None) => break,
                    Err(error) => {
                        yield StreamEvent::Error { message: error.to_string() }.into();
                        return;
                    }
                };
                for event in decode_upstream_data(protocol, &data, &mut decoders) {
                    clock.observe(&event.event);
                    match &event.event {
                        StreamEvent::Start { .. } => opening_held = true,
                        StreamEvent::Usage(_) => {},
                        StreamEvent::TextDelta { text } | StreamEvent::ThinkingDelta { text } if text.is_empty() => {},
                        _ => hold = None,
                    }
                    chat_stopped |= protocol == Protocol::Chat && matches!(event.event, StreamEvent::Stop(_));
                    let trip = loop_guard.as_mut().and_then(|guard| guard.observe(&event.event));
                    yield event.into();
                    if let Some(trip) = trip {
                        yield UpstreamEvent {
                            event: StreamEvent::Error { message: trip.message().into() },
                            failure: Some(StreamFailure {
                                status: StatusCode::BAD_GATEWAY,
                                body: Value::Null,
                            }),
                            usage_observed: false,
                            loop_reasoning: trip.is_reasoning(),
                        }.into();
                        return;
                    }
                }
            }
        }
        // Tolerate a complete final data frame lacking its blank separator.
        match parser.flush() {
            Ok(Some(data)) => {
                for event in decode_upstream_data(protocol, &data, &mut decoders) {
                    chat_stopped |= protocol == Protocol::Chat && matches!(event.event, StreamEvent::Stop(_));
                    let trip = loop_guard.as_mut().and_then(|guard| guard.observe(&event.event));
                    yield event.into();
                    if let Some(trip) = trip {
                        yield UpstreamEvent {
                            event: StreamEvent::Error { message: trip.message().into() },
                            failure: Some(StreamFailure {
                                status: StatusCode::BAD_GATEWAY,
                                body: Value::Null,
                            }),
                            usage_observed: false,
                            loop_reasoning: trip.is_reasoning(),
                        }.into();
                        return;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                yield StreamEvent::Error { message: error.to_string() }.into();
                return;
            }
        }
        // Some Chat-compatible endpoints close cleanly after finish_reason,
        // omitting [DONE]. Do not end at Stop: a later usage frame is valid.
        if chat_stopped {
            yield StreamEvent::Done.into();
        } else {
            yield StreamEvent::Error {
                message: "upstream stream ended before a completion marker".into(),
            }.into();
        }
    }
}

/// POST /v1/chat/completions — the OpenAI Chat dialect.
async fn chat_completions(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let include_usage = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/stream_options/include_usage")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    proxy(state, body, ClientWire::Chat { include_usage }).await
}

/// POST /v1/messages — the Anthropic Messages dialect.
async fn anthropic_messages(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    proxy(state, body, ClientWire::Anthropic).await
}

/// POST /v1/responses — the OpenAI Responses dialect Codex speaks.
async fn responses_api(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    proxy(state, body, ClientWire::Responses).await
}

/// POST /v1beta/models/{model}:generateContent and its streaming sibling.
/// The wildcard also accepts gateway model ids containing a slash.
async fn gemini_generate(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let wire = ClientWire::Gemini;
    let Some((model, method)) = raw.rsplit_once(':') else {
        return err_response(
            StatusCode::NOT_FOUND,
            wire.error_body(StatusCode::NOT_FOUND, "expected :generateContent"),
        );
    };
    let stream = match method {
        "generateContent" => false,
        "streamGenerateContent" => true,
        _ => {
            return err_response(
                StatusCode::NOT_FOUND,
                wire.error_body(
                    StatusCode::NOT_FOUND,
                    &format!("unsupported Gemini method `{method}`"),
                ),
            );
        }
    };

    let mut value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let message = format!("invalid JSON: {e}");
            return err_response(
                StatusCode::BAD_REQUEST,
                wire.error_body(StatusCode::BAD_REQUEST, &message),
            );
        }
    };
    if !value.is_object() {
        return err_response(
            StatusCode::BAD_REQUEST,
            wire.error_body(
                StatusCode::BAD_REQUEST,
                "request body must be a JSON object",
            ),
        );
    }
    value["model"] = json!(model);
    value["stream"] = json!(stream);
    let bytes = Bytes::from(value.to_string());
    proxy(state, bytes, wire).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingPolicy;
    use std::time::Duration;

    struct TestServer {
        url: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn test_server(router: Router) -> TestServer {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        TestServer { url, task }
    }
    fn provider(id: &str, url: &str, model: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Chat,
            base_url: format!("{url}/v1"),
            api_key: "private-test-key".into(),
            api_keys: vec![],
            models: vec![model.into()],
            fallback: vec![],
            key_cooldown_ms: None,
            capabilities: Default::default(),
            key_generation: 0,
        }
    }

    /// This explicitly cancels the handler task, not a downstream HTTP socket.
    /// It proves accounting while waiting for fallback headers, not automatic
    /// cancellation of pending Axum handlers on a real client disconnect.
    #[tokio::test]
    async fn cancelled_pending_fallback_accounts_for_last_attempt_before_headers() {
        for stream in [false, true] {
            for grouped in [false, true] {
                let first = test_server(Router::new().route("/v1/chat/completions", post(move || async move {
                    if stream {
                        let script = concat!(
                            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":0}}\n\n",
                            "data: {\"error\":{\"code\":429,\"message\":\"rate limited\"}}\n\n"
                        );
                        sse_response(Body::from_stream(async_stream::stream! {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from_static(script.as_bytes()));
                            std::future::pending::<()>().await;
                        }))
                    } else {
                        err_response(StatusCode::TOO_MANY_REQUESTS, json!({"error":{"message":"rate limited"}}))
                    }
                }))).await;
                let notified = Arc::new(tokio::sync::Notify::new());
                let received = Arc::new(Mutex::new(None));
                let signal = notified.clone();
                let captured = received.clone();
                let second = test_server(Router::new().route(
                    "/v1/chat/completions",
                    post(move |Json(body): Json<Value>| {
                        let signal = signal.clone();
                        let captured = captured.clone();
                        async move {
                            *captured.lock().unwrap() = Some(body["model"].clone());
                            signal.notify_one();
                            std::future::pending::<Response>().await
                        }
                    }),
                ))
                .await;
                let mut primary = provider("primary", &first.url, "first-model");
                if !grouped {
                    primary.fallback = vec!["spare/second-model".into()];
                }
                let config = Config {
                    provider: vec![primary, provider("spare", &second.url, "second-model")],
                    group: if grouped {
                        vec![GroupConfig {
                            id: "daily".into(),
                            name: "Daily".into(),
                            routes: vec!["primary/first-model".into(), "spare/second-model".into()],
                            routing: RoutingPolicy::Order,
                            first_token_timeout_ms: 0,
                        }]
                    } else {
                        vec![]
                    },
                    ..Config::default()
                };
                let directory = tempfile::tempdir().unwrap();
                let history = directory.path().join("requests.jsonl");
                let log = Arc::new(Mutex::new(RequestLog::open(&history).unwrap()));
                let state = Arc::new(AppState {
                    agents: None,
                    config: ConfigStore::new(config, None),
                    http: upstream::http_client().unwrap(),
                    requests: log.clone(),
                    key_rotation: Default::default(),
                });
                let requested = if grouped {
                    "group/daily"
                } else {
                    "primary/first-model"
                };
                let body = Bytes::from(json!({"model":requested,"stream":stream,"messages":[{"role":"user","content":"private prompt"}]}).to_string());
                let task = tokio::spawn(proxy(
                    state,
                    body,
                    ClientWire::Chat {
                        include_usage: true,
                    },
                ));
                tokio::time::timeout(Duration::from_secs(4), notified.notified())
                    .await
                    .unwrap();
                assert_eq!(*received.lock().unwrap(), Some(json!("second-model")));
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                let records = log.lock().unwrap().snapshot().recent_requests;
                assert_eq!(records.len(), 1);
                let record = serde_json::to_value(&records[0]).unwrap();
                assert_eq!(record["status"], "client_disconnected");
                assert_eq!(record["requested_model"], requested);
                assert_eq!(record["provider"], "spare");
                assert_eq!(record["upstream_model"], "second-model");
                assert!(record["usage"]["input"].is_null());
                assert!(record["usage"]["output"].is_null());
                let persisted = std::fs::read_to_string(&history).unwrap();
                assert_eq!(persisted.lines().count(), 1);
                assert_eq!(serde_json::from_str::<Value>(&persisted).unwrap(), record);
                assert!(!persisted.contains("private"));
            }
        }
    }
}
