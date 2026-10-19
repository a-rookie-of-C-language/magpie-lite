//! The gateway itself: a local HTTP server that speaks OpenAI Chat
//! Completions, OpenAI Responses, Anthropic Messages, and Google Gemini to
//! agents, translates each request through the IR, and forwards it to
//! whichever provider serves the model.

use std::collections::{HashMap, HashSet};
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
use magpie_core::ir::{CompletionRequest, CompletionResponse, PartKind, SignatureProtocol, Usage};
use magpie_core::stream::SignatureTarget;
use magpie_core::stream::{SseParser, StreamEvent};
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::affinity::{AffinityStore, Decision, Seat, who_of};
use crate::config::{Affinity, Config, GroupConfig, Protocol, ProviderConfig};
use crate::config_store::{ConfigStore, IdempotencyRequest};
use crate::cooldown::quota_cooldown;
use crate::edge_block::{BLOCKED_HINT, edge_blocked, edge_blocked_text};
use crate::lanes;
use crate::model_gone::{
    context_overflowed, model_refused, model_retired, plain_forbidden, prompt_refused,
    protection_refused, shape_refused, zdr_refused,
};
use crate::object_root;
use crate::optional_fields;
use crate::reply_room;
use crate::retired as retired_store;
use crate::roomy;
use crate::stream_empty::{EMPTY_REPLY, GeminiReply};
use crate::stream_keepalive::{self, Keepalive, UpstreamActivity};
use crate::stream_loop::LoopGuard;
use crate::stream_refusal::{self, REFUSAL};
use crate::stream_timeout::{GroupFirstToken, ResponsesHold, StreamClock, StreamTimeout};
use crate::unfit_fields;
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
    /// Which key answered each conversation, so its prompt cache keeps
    /// hitting.
    pub affinity: AffinityStore,
    /// Per-key concurrency slots and request-per-minute windows.
    pub lanes: lanes::Lanes,
    pub rpms: lanes::Rpms,
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
        // An Ollama-shaped server, for a client that takes a custom
        // endpoint only as one (Visual Studio's and VS Code's Copilot
        // BYOK; the reference's `ollama.go`, 94df2392).
        .route("/api/version", get(ollama_version))
        .route("/api/tags", get(ollama_tags))
        .route("/api/show", post(ollama_show))
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
        // The gateway's own auth refusal keeps the Gemini reason it has
        // always said (a 401 reads PERMISSION_DENIED here), not the
        // reference's UNAUTHENTICATED mapping for upstream errors.
        upstream::gemini_error_reason(
            status,
            "missing or invalid magpie-lite token",
            "PERMISSION_DENIED",
        )
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
    retired_store::init(agent_state_dir.join("retired.json"));
    let manager = magpie_agents::lifecycle::Manager::native(agent_state_dir.clone())?;
    // Persist affinity beside the resolved config even when the embedded
    // caller supplied no path and the default config location is in use.
    let affinity = AffinityStore::beside(Some(agent_config.as_path()));
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
        affinity,
        lanes: lanes::Lanes::default(),
        rpms: lanes::Rpms::default(),
    });
    let app = build_router(Arc::clone(&state));

    tracing::info!("magpie-lite gateway listening on http://{addr}");
    println!("magpie-lite gateway listening on http://{addr}");
    // Stop on Ctrl-C, writing what affinity has waiting so the next start
    // (or a handover) reads every answer.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    state.affinity.flush();
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
    snapshot.classifier_status = crate::classify::failures();
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

/// Bring a removed found group back: saving any group with its id
/// unhides it (a group written with a found id is the user's own).
fn unhide_group(config: &mut crate::config::Config, id: &str) {
    config.hidden_auto_groups.retain(|hidden| hidden != id);
}

/// Save a group over the user's own with its id, or over a found one the
/// user just changed (theirs now, as the reference keeps found groups out
/// of the file).
fn place_group(
    config: &mut crate::config::Config,
    group: GroupConfig,
) -> Result<(), (StatusCode, String)> {
    unhide_group(config, &group.id);
    if let Some(existing) = config
        .group
        .iter_mut()
        .find(|existing| existing.id == group.id)
    {
        *existing = group;
        return Ok(());
    }
    if config.group(&group.id).is_some() {
        config.group.push(group);
        return Ok(());
    }
    Err((StatusCode::NOT_FOUND, "group not found".to_owned()))
}

/// Remove a group: one the user wrote is gone; one magpie found hides
/// until brought back, unless a provider fallback still names it.
fn remove_group(config: &mut crate::config::Config, id: &str) -> Result<(), (StatusCode, String)> {
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
    if let Some(index) = config.group.iter().position(|group| group.id == id) {
        config.group.remove(index);
    } else if config.group(id).is_some() {
        if !config.hidden_auto_groups.iter().any(|hidden| hidden == id) {
            config.hidden_auto_groups.push(id.to_owned());
        }
    } else {
        return Err((StatusCode::NOT_FOUND, "group not found".to_owned()));
    }
    Ok(())
}

fn validate_group(config: &Config, group: &mut GroupConfig) -> Result<(), (StatusCode, String)> {
    config.clean_member_efforts(group);
    group
        .validate()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    group
        .check_classifier(config)
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
    let mut group = match decode_group(&bytes) {
        Ok(group) => group,
        Err(message) => return group_error(StatusCode::BAD_REQUEST, message),
    };
    let result = if let Some(request) = request {
        state
            .config
            .update_idempotent(expected, request, move |config| {
                validate_group(config, &mut group)?;
                if config.group.iter().any(|existing| existing.id == group.id) {
                    return Err((StatusCode::CONFLICT, "group already exists".to_owned()));
                }
                unhide_group(config, &group.id);
                config.group.push(group);
                Ok(())
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                validate_group(config, &mut group)?;
                if config.group.iter().any(|existing| existing.id == group.id) {
                    return Err((StatusCode::CONFLICT, "group already exists".to_owned()));
                }
                unhide_group(config, &group.id);
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
    let mut group = match decode_group(&bytes) {
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
                validate_group(config, &mut group)?;
                place_group(config, group)
            })
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| {
                validate_group(config, &mut group)?;
                place_group(config, group)
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
            .update_idempotent(expected, request, move |config| remove_group(config, &id))
            .await
            .map(|(snapshot, _)| snapshot)
    } else {
        state
            .config
            .update_if_revision(expected, move |config| remove_group(config, &id))
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

/// The Ollama release lite answers `/api/version` as: one past every
/// client's minimum (VS Code's Copilot asks for 0.6.4).
const OLLAMA_VERSION: &str = "0.12.6";

/// The window a client assumes when a model's own isn't told: what VS
/// Code's Copilot reads without `context_length` in `model_info`.
const OLLAMA_DEFAULT_CONTEXT: u64 = 32768;

/// The architecture `model_info`'s keys are named after.
const OLLAMA_ARCH: &str = "magpie";

/// What an entry's modified_at says, since no model here was pulled.
const OLLAMA_MODIFIED: &str = "2025-01-01T00:00:00Z";

/// The model ids a caller is shown, in the catalog's own order: each id
/// paired with the owner's label and the window the provider knows, when
/// it knows one.
fn ollama_entries(state: &AppState) -> Vec<(String, String, u64)> {
    let config = state.config.current();
    config
        .catalog()
        .into_iter()
        .map(|(id, owner)| {
            // `group/<id>` names no provider's window; a found group's
            // `auto/<model>` takes its members' widest known one.
            // `provider/<model>` asks its provider's contexts map.
            let window = id
                .split_once('/')
                .and_then(|(provider_id, model)| {
                    config
                        .provider(provider_id)
                        .map(|p| p.window_of(model))
                        .or_else(|| {
                            if provider_id != "auto" {
                                return None;
                            }
                            config.found_group(model).map(|group| {
                                group
                                    .routes
                                    .iter()
                                    .map(|route| {
                                        route
                                            .split_once('/')
                                            .map(|(pid, m)| {
                                                config
                                                    .provider(pid)
                                                    .map(|p| p.window_of(m))
                                                    .unwrap_or(0)
                                            })
                                            .unwrap_or(0)
                                    })
                                    .max()
                                    .unwrap_or(0)
                            })
                        })
                })
                .unwrap_or(0);
            (id, owner, window)
        })
        .collect()
}

/// GET /api/version — Ollama's build number, so a client that requires a
/// minimum takes the gateway as new enough.
async fn ollama_version() -> Json<Value> {
    Json(json!({"version": OLLAMA_VERSION}))
}

/// GET /api/tags — every model the caller is shown, in Ollama's shape:
/// `model` is the id it sends back to chat with.
async fn ollama_tags(State(state): State<Arc<AppState>>) -> Json<Value> {
    let models: Vec<Value> = ollama_entries(&state)
        .into_iter()
        .map(|(id, owner, _)| {
            json!({
                "name": id,
                "model": id,
                "modified_at": OLLAMA_MODIFIED,
                "size": 0,
                "digest": ollama_digest(&id),
                "details": {"family": OLLAMA_ARCH, "parameter_size": "", "quantization_level": ""},
                "capabilities": ["completion", "tools"],
                "display_name": owner,
            })
        })
        .collect();
    Json(json!({"models": models}))
}

/// POST /api/show — one model as Ollama tells it: a client reads what it
/// takes from `capabilities`, its window from
/// `model_info["<arch>.context_length"]` (32768 without it) and its name
/// from `general.basename`.
async fn ollama_show(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("invalid request: {error}")}),
            );
        }
    };
    // `model` is what current clients send; `name` what clients before
    // Ollama 0.5 sent.
    let id = parsed
        .get("model")
        .or_else(|| parsed.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    if id.is_empty() {
        return err_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "model is required"}),
        );
    }
    let Some((_, owner, window)) = ollama_entries(&state)
        .into_iter()
        .find(|(entry, _, _)| *entry == id)
    else {
        return err_response(
            StatusCode::NOT_FOUND,
            json!({"error": format!("model '{id}' not found")}),
        );
    };
    let window = if window == 0 {
        OLLAMA_DEFAULT_CONTEXT
    } else {
        window
    };
    // Every key is sent: the ollama CLI before 0.7 reads parameter_count,
    // context_length and embedding_length without checking and panics on
    // one missing.
    Json(json!({
        "modelfile": "",
        "parameters": "",
        "template": "",
        "details": {"family": OLLAMA_ARCH, "parameter_size": "", "quantization_level": ""},
        "model_info": {
            "general.architecture": OLLAMA_ARCH,
            "general.basename": owner,
            "general.parameter_count": 0,
            format!("{OLLAMA_ARCH}.context_length"): window,
            format!("{OLLAMA_ARCH}.embedding_length"): 0,
        },
        "capabilities": ["completion", "tools"],
        "modified_at": OLLAMA_MODIFIED,
    }))
    .into_response()
}

/// A stable stand-in for a pulled model's digest: the id's own hash, as
/// Ollama's tags carry.
fn ollama_digest(id: &str) -> String {
    let sum = Sha256::digest(id.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in &sum[..16] {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("sha256:{hex}")
}

/// A resolved route: one provider/model and the ordered key pool to try
/// before moving on to the next route.
#[derive(Debug, Clone)]
struct UpstreamRoute {
    provider: Arc<ProviderConfig>,
    model: String,
    /// The effort fixed on the group member (`mock/m:high`), `""` for
    /// none: asked for whatever the agent asked and whatever the turn
    /// picked. The vendor is asked for `model`, the effort riding here.
    effort: String,
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
    /// The key as a conversation's stick names it, without the secret.
    who: String,
}

impl UpstreamCandidate {
    fn new(route: &UpstreamRoute, key_slot: usize) -> Self {
        Self {
            provider: route.provider.clone(),
            model: route.model.clone(),
            key_slot,
            who: route
                .api_keys
                .iter()
                .find(|(slot, _)| *slot == key_slot)
                .map(|(_, key)| who_of(&route.provider.id, key))
                .unwrap_or_else(|| who_of(&route.provider.id, "")),
        }
    }
}

/// The why a request was turned away by a key's queue or minute, worded as
/// the reference's `laneMessage` words it.
fn lane_failure(
    provider: &ProviderConfig,
    error: impl std::fmt::Display,
    retry_after: Option<u64>,
) -> ProxyFailure {
    let label = format!(
        "{}/{}",
        provider.id,
        provider.models.first().map_or("", |m| m.as_str())
    );
    let message = format!("{label}: {error}");
    ProxyFailure::lane(StatusCode::TOO_MANY_REQUESTS, message, retry_after)
}

/// Whether a request for `who` under `provider` would go at once: a slot
/// free in its lane and room in its minute (the reference's `ready`).
fn candidate_ready(state: &AppState, provider: &ProviderConfig, who: &str) -> bool {
    lanes::ready(
        &state.lanes,
        &state.rpms,
        who,
        provider.lane_limit(),
        provider.rpm_limit(),
    )
}

/// Reorder `routes` so that wherever a route's first busy key would queue,
/// a later route/key with a slot free now goes first — another key of the
/// same provider, or another member of the same group (the reference's
/// `laneMate`, #892). Fallback providers are never moved ahead of a full
/// one; they only answer when the whole first provider refused.
fn give_way_to_free_lanes(state: &AppState, routes: &mut Vec<UpstreamRoute>) {
    let mut gave: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Within a route: when its first key's lane is full, a later key of
    // the same provider with a slot free goes first.
    for route in routes.iter_mut() {
        let mut gave_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut i = 0;
        while i < route.api_keys.len() {
            let who = who_of(&route.provider.id, &route.api_keys[i].1);
            if gave_keys.contains(&who) || candidate_ready(state, &route.provider, &who) {
                i += 1;
                continue;
            }
            let mut found = None;
            for j in (i + 1)..route.api_keys.len() {
                let j_who = who_of(&route.provider.id, &route.api_keys[j].1);
                if j_who != who && candidate_ready(state, &route.provider, &j_who) {
                    found = Some(j);
                    break;
                }
            }
            match found {
                Some(j) => {
                    gave_keys.insert(who);
                    let key = route.api_keys.remove(i);
                    route.api_keys.insert(j, key);
                }
                None => i += 1,
            }
        }
    }
    // Across routes: within a group, a member whose first key is full gives
    // way to a later member with a slot free.
    let mut i = 0;
    while i < routes.len() {
        let (first_who, first_ready, first_group, first_provider) = {
            let route = &routes[i];
            let who = route
                .api_keys
                .first()
                .map(|(_, key)| who_of(&route.provider.id, key))
                .unwrap_or_else(|| who_of(&route.provider.id, ""));
            (
                who,
                candidate_ready(
                    state,
                    &route.provider,
                    &who_of(
                        &route.provider.id,
                        route
                            .api_keys
                            .first()
                            .map(|(_, k)| k.as_str())
                            .unwrap_or(""),
                    ),
                ),
                route.group_id.is_some(),
                route.provider.id.clone(),
            )
        };
        if gave.contains(&first_who) || first_ready {
            i += 1;
            continue;
        }
        let mut found = None;
        for j in (i + 1)..routes.len() {
            let (j_who, j_provider, j_group) = {
                let m = &routes[j];
                let who = m
                    .api_keys
                    .first()
                    .map(|(_, key)| who_of(&m.provider.id, key))
                    .unwrap_or_else(|| who_of(&m.provider.id, ""));
                (who, m.provider.id.clone(), m.group_id.is_some())
            };
            if j_who == first_who {
                continue;
            }
            // In a group any member may give way; outside one only another
            // key of the same provider may.
            if !first_group && j_provider != first_provider {
                continue;
            }
            let _ = j_group;
            if candidate_ready(state, &routes[j].provider, &j_who) {
                found = Some(j);
                break;
            }
        }
        match found {
            Some(j) => {
                gave.insert(first_who);
                let route = routes.remove(i);
                routes.insert(j, route);
                // Do not advance: the moved-in route is asked next.
            }
            None => i += 1,
        }
    }
}

/// The agent-facing protocol a request arrived in. Beside decoding, this
/// chooses the reply encoder and the error body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientWire {
    Chat { include_usage: bool },
    Anthropic,
    Responses,
    Gemini,
}

impl ClientWire {
    fn error_body(self, status: StatusCode, message: &str) -> Value {
        let kind = error_kind(status.as_u16());
        self.error_body_kind(status, message, kind)
    }

    fn error_body_kind(self, status: StatusCode, message: &str, kind: &str) -> Value {
        match self {
            Self::Chat { .. } => upstream::chat_error_kind(status, message, kind),
            Self::Anthropic => upstream::anthropic_error_kind(message, kind),
            Self::Responses => upstream::responses_error_kind(status, message, kind),
            Self::Gemini => upstream::gemini_error_reason(
                status,
                message,
                upstream::gemini_reason(status.as_u16()),
            ),
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

    fn encode_response(
        self,
        ir: &magpie_core::ir::CompletionResponse,
        req: &CompletionRequest,
    ) -> Value {
        match self {
            Self::Chat { .. } => chat::encode_response(ir),
            Self::Anthropic => anthropic::encode_response(ir),
            Self::Responses => responses::encode_response_for_request(ir, req),
            Self::Gemini => gemini::encode_response(ir),
        }
    }
}

/// The agent-facing stream encoders, behind one small interface so the
/// upstream translation loop can be shared by every endpoint.
enum ClientStreamEncoder {
    Chat(chat::ChatChunkEncoder),
    Anthropic(anthropic::AnthropicStreamEncoder),
    Responses(Box<responses::ResponsesStreamEncoder>),
    Gemini(gemini::GeminiStreamEncoder),
}

impl ClientStreamEncoder {
    fn new(wire: ClientWire, model: &str) -> Self {
        match wire {
            ClientWire::Chat { include_usage } => {
                Self::Chat(chat::ChatChunkEncoder::new(model, include_usage))
            }
            ClientWire::Anthropic => Self::Anthropic(anthropic::AnthropicStreamEncoder::new(model)),
            ClientWire::Responses => {
                Self::Responses(Box::new(responses::ResponsesStreamEncoder::new(model)))
            }
            ClientWire::Gemini => Self::Gemini(gemini::GeminiStreamEncoder::new(model)),
        }
    }

    fn for_request(wire: ClientWire, req: &CompletionRequest) -> Self {
        match wire {
            ClientWire::Responses => Self::Responses(Box::new(
                responses::ResponsesStreamEncoder::for_request(&req.model, req),
            )),
            _ => Self::new(wire, &req.model),
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
    responses: responses::ResponsesStreamDecoder,
    gemini: gemini::GeminiStreamDecoder,
    gemini_reply: GeminiReply,
    gemini_pending_usage: Option<Usage>,
    answered: bool,
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
) -> Result<
    (
        CompletionRequest,
        UpstreamRoute,
        Option<GroupRoutePlan>,
        (String, Affinity, bool),
    ),
    (StatusCode, Value),
> {
    let creq = wire.decode_request(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            wire.error_body(StatusCode::BAD_REQUEST, &e.to_string()),
        )
    })?;

    let (provider, upstream_model, fixed_effort, group_plan, group_route_index) =
        if let Some(group) = config.group_by_ref(&creq.model) {
            // Both spellings land on the internal id, so rotation,
            // affinity and usage count them as one group.
            let group_id = group.id.clone();
            let routes = key_rotation.group_order(
                &group_id,
                group.routing,
                &group.routes,
                &group.route_weights(),
                &group.pick,
            );
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
            // The vendor is asked for the model's own id; a fixed effort
            // rides the route for the send.
            let Some((provider, upstream_model, effort)) = config.resolve_member(&route) else {
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
                effort,
                Some(GroupRoutePlan {
                    id: group_id.clone(),
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
            (provider, upstream_model, String::new(), None, None)
        };
    let affinity = if let Some(plan) = group_plan.as_ref() {
        let group = config.group(&plan.id).expect("group just resolved");
        (
            format!("group/{}", plan.id),
            group.affinity,
            group.routing == crate::config::RoutingPolicy::Rotate,
        )
    } else {
        (
            format!("{}/{}", provider.id, upstream_model),
            provider.affinity,
            provider.routing == crate::config::RoutingPolicy::Rotate,
        )
    };

    Ok((
        creq,
        UpstreamRoute {
            provider: Arc::new(provider.clone()),
            model: upstream_model,
            effort: fixed_effort,
            api_keys: provider.api_key_pool().into_iter().enumerate().collect(),
            group_id: group_plan.as_ref().map(|plan| plan.id.clone()),
            group_route_index,
        },
        group_plan,
        affinity,
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
        api_keys: key_rotation.rotate_routed(
            &initial.provider.id,
            initial.provider.key_generation,
            initial.provider.routing,
            &initial_keys,
            &initial.provider.key_weights(),
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
    let mut seen_group_routes = HashSet::from([(
        initial.provider.id.clone(),
        initial.model.clone(),
        initial.effort.clone(),
    )]);

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
        // The vendor is asked for the model's own id; a fixed effort rides
        // the route for the send, and tells same-model members apart.
        let Some((provider, model, effort)) = config.resolve_member(&reference) else {
            tracing::warn!("fallback `{reference}` does not resolve; skipping");
            continue;
        };
        if let Some(route_index) = group_route_index {
            if !seen_group_routes.insert((provider.id.clone(), model.clone(), effort.clone())) {
                continue;
            }
            let api_keys = key_rotation.rotate_routed(
                &provider.id,
                provider.key_generation,
                provider.routing,
                &provider.api_key_pool(),
                &provider.key_weights(),
            );
            out.push(UpstreamRoute {
                provider: Arc::new(provider.clone()),
                model,
                effort,
                api_keys,
                group_id: group_plan.map(|plan| plan.id.clone()),
                group_route_index: Some(route_index),
            });
        } else {
            if !seen_providers.insert(provider.id.clone()) {
                continue;
            }
            let api_keys = key_rotation.rotate_routed(
                &provider.id,
                provider.key_generation,
                provider.routing,
                &provider.api_key_pool(),
                &provider.key_weights(),
            );
            out.push(UpstreamRoute {
                provider: Arc::new(provider.clone()),
                model,
                effort,
                api_keys,
                group_id: None,
                group_route_index: None,
            });
        }
    }
    out
}

/// Move the seat a conversation is kept on, and its route, to the front of the
/// order the request will try. Routing still supplies everyone else: only the
/// front changes.
fn apply_affinity(routes: &mut Vec<UpstreamRoute>, decision: &Decision) {
    let Some((route_slot, key_slot)) = decision.first else {
        return;
    };
    let route_at = decision
        .first_route
        .filter(|index| *index < routes.len())
        .or_else(|| {
            routes
                .iter()
                .position(|route| route.api_keys.iter().any(|(slot, _)| *slot == route_slot))
        });
    let Some(route_at) = route_at else {
        return;
    };
    if route_at > 0 {
        let route = routes.remove(route_at);
        routes.insert(0, route);
    }
    let route = &mut routes[0];
    if let Some(key_at) = route
        .api_keys
        .iter()
        .position(|(slot, _)| *slot == key_slot)
        && key_at > 0
    {
        let key = route.api_keys.remove(key_at);
        route.api_keys.insert(0, key);
    }
}

/// Statuses that make the next key in the same provider worth one attempt.
fn key_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 402 | 403 | 429)
}

/// Leave out, before any is asked, the routes whose model is known to take
/// less than the request (ported from the reference's `0f398c48`). A member
/// so small would only refuse it, and a refusal that doesn't say why reached
/// the agent as the vendor's error while the next member had room. This is a
/// check before sending, not a retry.
///
/// The affinity decision names the seat a conversation is stuck to by its
/// index in this route list (`first_route`) plus that route's pool slot
/// (`first`). When routes ahead of it are left out its index moves with the
/// route; when its own route is left out the conversation cannot stay on it
/// anyway, so the pin is cleared and the remaining order stands.
fn leave_out_routes_without_room(
    routes: &mut Vec<UpstreamRoute>,
    decision: &mut Decision,
    creq: &CompletionRequest,
) {
    let windows: Vec<u64> = routes
        .iter()
        .map(|route| route.provider.window_of(&route.model))
        .collect();
    let Some(keep) = roomy::holds(&windows, roomy::estimate(creq)) else {
        return;
    };
    let mut index = 0;
    routes.retain(|_| {
        let kept = keep[index];
        index += 1;
        kept
    });
    match decision.first_route {
        Some(at) if keep.get(at) == Some(&false) => {
            decision.first = None;
            decision.first_route = None;
        }
        Some(at) => {
            decision.first_route = Some(keep[..at].iter().filter(|kept| **kept).count());
        }
        None => {}
    }
}

/// Count what a key just answered against it for the provider's `usage`
/// policy, and wake it from any cooldown. Providers report cumulative counters
/// for each attempt, so `tracker.total_usage()` is the whole logical reply; a
/// reply that named no amount still counts as one request served.
fn record_key_served(
    state: &AppState,
    provider: &ProviderConfig,
    key_slot: usize,
    tracker: &RequestTracker,
) {
    let usage = tracker.total_usage();
    let tokens = usage
        .input
        .unwrap_or(0)
        .saturating_add(usage.output.unwrap_or(0))
        .saturating_add(usage.cache_read.unwrap_or(0))
        .saturating_add(usage.cache_write.unwrap_or(0));
    state
        .key_rotation
        .record_served(&provider.id, provider.key_generation, key_slot, tokens);
}

/// Quota and rate-limit responses should influence later key selection.
fn quota_limited_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 402 | 429)
}

/// A Claude refusing a sampling parameter: "`temperature` is deprecated
/// for this model" (and `top_p`, `top_k` — the reference
/// `samplingDeprecated`, #1454).
fn sampling_refused(status: reqwest::StatusCode, body: &Value) -> bool {
    if status.as_u16() != 400 {
        return false;
    }
    let text = body.to_string().to_lowercase();
    ["temperature", "top_p", "top_k"].iter().any(|param| {
        text.contains(&format!("{param}` is deprecated for this model"))
            || text.contains(&format!("{param} is deprecated for this model"))
            || text.contains(&format!("{param}` is not supported for this model"))
            || text.contains(&format!("{param} is not supported for this model"))
    })
}

/// Whether an upstream status is worth spending another route on.
/// `text` carries the raw human-readable error: HTML block pages never
/// parse into `body`, so the blocked-page wording arrives here instead.
fn route_retryable_status(status: reqwest::StatusCode, body: &Value, text: &str) -> bool {
    let code = status.as_u16();
    // A conversation too long for the model stays with the route it
    // started on: no other member fits it either, and the agent should
    // compact it itself. Short-circuit first, so a long overflow message
    // never reads as an unknown shape or unserved model.
    if context_overflowed(code, body) {
        return false;
    }
    if matches!(code, 401 | 402 | 403 | 404 | 408 | 429 | 500..=599) {
        return true;
    }
    // Model-retired 410s and model-refusal shapes describe the model,
    // not this key or route: another provider of the same model may
    // still serve it. A firewall block page describes this address, so
    // another provider goes another way at any status. A shape the
    // vendor's API can't read is worth handing to the next route, too.
    // A refused conversation ("unapproved channel") or an unavailable
    // response-protection layer ("response protection is unavailable")
    // is that vendor's read on the request or its policy backend, not a
    // broken credential or a busy endpoint: the next route may serve it
    // at any status, and the refusing one never rests.
    model_retired(code, body)
        || model_refused(code, body)
        || shape_refused(code, body)
        || prompt_refused(code, body)
        || protection_refused(code, body)
        || zdr_refused(code, body)
        || edge_blocked(body)
        || edge_blocked_text(text)
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
    Decoded {
        ir: magpie_core::ir::CompletionResponse,
        /// The vendor ignored `stream: true` and answered whole while the
        /// client reads SSE; the caller replays the reply as a stream.
        whole_as_stream: bool,
    },
    Stream {
        events: UpstreamEvents,
        empty_retries: usize,
        /// Held failures exhausted routing and must not gain a relay retry.
        reask_allowed: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    Upstream,
    EmptyReply,
    Refusal,
}

#[derive(Clone)]
struct StreamFailure {
    kind: FailureKind,
    status: reqwest::StatusCode,
    body: Value,
}

struct UpstreamEvent {
    event: StreamEvent,
    failure: Option<StreamFailure>,
    usage_observed: bool,
    /// Account deferred Gemini usage without publishing a premature completion.
    usage_hidden: bool,
    /// Gemini image/search output counts even where the stream IR cannot carry it.
    answer_observed: bool,
    /// True only for the loop guard's reasoning terminal. This is an internal
    /// control signal; ordinary vendor errors must never trigger a reask.
    loop_reasoning: bool,
}
impl From<StreamEvent> for UpstreamEvent {
    fn from(event: StreamEvent) -> Self {
        let failure = matches!(event, StreamEvent::Error { .. }).then(|| StreamFailure {
            kind: FailureKind::Upstream,
            status: reqwest::StatusCode::BAD_GATEWAY,
            body: Value::Null,
        });
        Self {
            event,
            failure,
            usage_observed: false,
            usage_hidden: false,
            answer_observed: false,
            loop_reasoning: false,
        }
    }
}

/// Send to the initial provider's key pool, then its fallback routes. All
/// routing decisions happen before a successful response body is streamed, so
/// an agent never receives a half-finished answer from an abandoned provider.
/// Ask a classifier model through the gateway itself which of `intents` a
/// message is — or, when `effort`, how hard the turn is to think about —
/// as a client would. Every model goes through the gateway — lite has no
/// decision providers answering a separate protocol — with the router's
/// User-Agent, so the call shows in the usage as lite's own. Anything but
/// a numbered answer is the classifier being up but unanswering, never
/// rested for.
///
/// The gateway asking itself cannot name `proxy_parsed` here: its future
/// reaches `send_with_fallback`, which reaches this call — naming the
/// concrete future on both sides is an unprovable `Send` cycle (E0391).
/// The call crosses a `dyn` boundary instead, which erases it.
pub(crate) trait SelfCall: Send + Sync {
    fn call(
        &self,
        headers: HeaderMap,
        body: Value,
        bytes: Bytes,
        wire: ClientWire,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>>;
}
/// How the gateway calls itself: the router's entry point with shared
/// state, behind [`SelfCall`] so no future type names another.
#[derive(Clone)]
struct GatewayCaller {
    state: Arc<AppState>,
}
impl SelfCall for GatewayCaller {
    fn call(
        &self,
        headers: HeaderMap,
        body: Value,
        bytes: Bytes,
        wire: ClientWire,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
        Box::pin(proxy_parsed(self.state.clone(), headers, body, bytes, wire))
    }
}
async fn ask_classifier(
    caller: std::sync::Arc<dyn SelfCall>,
    model: String,
    intents: Vec<String>,
    prev: crate::classify::Before,
    effort: bool,
    text: String,
) -> Result<crate::classify::Verdict, crate::classify::AskError> {
    use crate::classify::{AskError, CLASSIFY_TIMEOUT, Verdict};
    let body = if effort {
        crate::classify::effort_body(&model, &prev.effort, &text)
    } else {
        crate::classify::classify_body(&model, &intents, &prev, &text)
    };
    let bytes =
        Bytes::from(serde_json::to_vec(&body).map_err(|e| AskError::Failed(e.to_string()))?);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(crate::classify::ROUTER_AGENT),
    );
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    // The gateway asking itself: the future type would otherwise name
    // `send_with_fallback` through `proxy_parsed`, which names this call
    // again — an unprovable `Send` cycle (E0391). Erase it at the box.
    let response = tokio::time::timeout(
        CLASSIFY_TIMEOUT,
        caller.call(
            headers,
            body,
            bytes,
            ClientWire::Chat {
                include_usage: false,
            },
        ),
    )
    .await
    .map_err(|_| AskError::Failed(format!("the classifier {model} took longer than 8s")))?;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 8 * 1024 * 1024)
        .await
        .map_err(|e| AskError::Failed(format!("the classifier {model} gave no body: {e}")))?;
    if !parts.status.is_success() {
        let mut short: String = String::from_utf8_lossy(&bytes).chars().take(120).collect();
        if bytes.len() > 120 {
            short.push('…');
        }
        return Err(AskError::Failed(format!(
            "the classifier {model} answered {}: {short}",
            parts.status.as_u16()
        )));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| AskError::Failed(e.to_string()))?;
    let answer = match value.pointer("/choices/0/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => {
            return Err(AskError::Failed(format!(
                "the classifier {model} answered without text"
            )));
        }
    };
    if effort {
        crate::classify::read_effort(&answer)
            .map(|(effort, score)| Verdict {
                effort,
                score,
                ..Default::default()
            })
            .map_err(AskError::Answered)
    } else {
        crate::classify::read_intent(&answer, &intents)
            .map(|intent| Verdict {
                intent,
                ..Default::default()
            })
            .map_err(AskError::Answered)
    }
}
async fn send_with_fallback(
    state: &Arc<AppState>,
    config: &Config,
    creq: &CompletionRequest,
    wire: ClientWire,
    initial: UpstreamRoute,
    group_plan: Option<&GroupRoutePlan>,
    tracker: &mut RequestTracker,
    liveness: &StreamLiveness,
    conversation: &str,
    agent: &str,
    affinity_scope: (String, Affinity, bool),
) -> Result<(UpstreamReply, UpstreamCandidate, Decision), (ProxyFailure, UpstreamCandidate, Decision)>
{
    let mut routes = upstream_routes(
        config,
        &state.key_rotation,
        initial,
        &creq.model,
        group_plan,
    );
    // A group's rules: a pause leaves its member out for now; a matching
    // rule's member goes first, the members the rules after it also name
    // behind it, before the group's others (the reference's `ruleFor` /
    // `applyRule`). The decision is made as a user's turn begins and held
    // for the requests within it.
    // The reasoning picked for the turn, sent by the requests the agent
    // asked to reason for (a group with `effort: auto`).
    let mut turn_pick = String::new();
    if let Some(plan) = group_plan
        && let Some(group) = config.group(&plan.id)
        && (!group.rules.is_empty() || group.effort == "auto")
    {
        // A pause by folder needs the folder the request says it is from.
        let by_folder = group
            .rules
            .iter()
            .any(|r| r.pause && (!r.folders.is_empty() || !r.except_folders.is_empty()));
        let folder = if by_folder {
            crate::rules::work_dir(creq)
        } else {
            String::new()
        };
        let kept = crate::rules::paused_out(
            &group.rules,
            routes
                .iter()
                .map(|r| format!("{}/{}", r.provider.id, r.model))
                .collect(),
            agent,
            &folder,
            crate::rules::local_clock(),
        );
        if kept.is_empty() && !routes.is_empty() {
            let candidate = UpstreamCandidate::new(&routes[0], 0);
            return Err((
                ProxyFailure::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "every model of group/{} is paused now by its rules; \
                         change the rules or ask again when they end",
                        plan.id
                    ),
                ),
                candidate,
                Decision {
                    key: format!("group/{}|{conversation}", plan.id),
                    trace: crate::affinity::Trace::default(),
                    first: None,
                    first_route: None,
                },
            ));
        }
        routes.retain(|r| kept.contains(&format!("{}/{}", r.provider.id, r.model)));
        let windows: HashMap<String, u64> = routes
            .iter()
            .map(|r| {
                (
                    format!("{}/{}", r.provider.id, r.model),
                    r.provider.window_of(&r.model),
                )
            })
            .collect();
        // A rule with an intent waits on what the group's classifier says
        // the turn's message is — asked once, as the turn begins (the
        // reference's `ruleFor` classifier call). Within the turn the
        // stored answer is gone by instead; a compaction is decided on
        // its own, its classifier never asked. When it cannot say, no
        // intent matches and the turn goes by the other conditions. What
        // was asked and said goes on the request's record.
        // A manual group sends every request to its picked member; its
        // rules wait until another member is picked, though they are kept
        // (the reference's `Live`: rules nilled, effort kept — a manual
        // group with `effort: auto` still has the turn judged).
        let rules: &[crate::rules::Rule] = if group.routing == crate::config::RoutingPolicy::Manual
        {
            &[]
        } else {
            &group.rules
        };
        let mut intent = String::new();
        let mut classified: Option<crate::rules::Classified> = None;
        let within_turn = crate::affinity::turn_of(creq).1;
        if !group.classifier.is_empty() && !within_turn && !crate::rules::compacting(creq) {
            let query = crate::rules::rule_query(rules, &plan.id, creq, agent, conversation);
            let intents = crate::rules::intents(rules, &query);
            // A group with `effort: auto` has the classifier judge the turn
            // when the agent asked for reasoning; its requests then ask
            // their model for the pick (the reference's `ruleFor` effort).
            let want_effort = group.effort == "auto" && !query.effort.is_empty();
            if !intents.is_empty() || want_effort {
                // A short follow-up carrying on is of its turn's kind.
                let after = crate::rules::turn_intent(&plan.id, conversation, creq);
                let after = if intents.iter().any(|i| i == &after) {
                    after
                } else {
                    String::new()
                };
                // ... and needs the reasoning its turn before had.
                let after_effort = if want_effort {
                    crate::rules::turn_effort(&plan.id, conversation, creq)
                } else {
                    String::new()
                };
                let text = crate::classify::user_text(creq);
                if !text.is_empty() {
                    let (user, reply) = crate::classify::turn_before(creq);
                    let prev = crate::classify::Before {
                        intent: after.clone(),
                        effort: after_effort.clone(),
                        user,
                        reply,
                    };
                    let caller: std::sync::Arc<dyn SelfCall> = std::sync::Arc::new(GatewayCaller {
                        state: state.clone(),
                    });
                    let ask_intent: crate::classify::AskFn = Box::new({
                        let caller = caller.clone();
                        move |m, i, p, _, t| {
                            let c = caller.clone();
                            Box::pin(ask_classifier(c, m, i, p, false, t))
                                as crate::classify::AskFuture
                        }
                    });
                    let ask_effort: crate::classify::AskFn = Box::new({
                        let caller = caller.clone();
                        move |m, _, p, _, t| {
                            let c = caller.clone();
                            Box::pin(ask_classifier(c, m, Vec::new(), p, true, t))
                                as crate::classify::AskFuture
                        }
                    });
                    let asked_at = std::time::Instant::now();
                    // The two questions go at once, as the reference's
                    // `askClassifier` asks them; either rests the
                    // classifier on its own failure.
                    let (intent_out, effort_out) = tokio::join!(
                        async {
                            if intents.is_empty() {
                                None
                            } else {
                                Some(
                                    crate::classify::classify(
                                        ask_intent,
                                        &group.classifier,
                                        &intents,
                                        &prev,
                                        false,
                                        &text,
                                    )
                                    .await,
                                )
                            }
                        },
                        async {
                            if !want_effort {
                                None
                            } else {
                                let none: Vec<String> = Vec::new();
                                Some(
                                    crate::classify::classify(
                                        ask_effort,
                                        &group.classifier,
                                        &none,
                                        &prev,
                                        true,
                                        &text,
                                    )
                                    .await,
                                )
                            }
                        },
                    );
                    let ms = asked_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    let mut cached = true;
                    let mut error = String::new();
                    let mut intent_err: Option<String> = None;
                    if let Some((v, asked_cached, err)) = intent_out {
                        cached &= asked_cached;
                        if let Some(err) = err {
                            intent_err = Some(err.clone());
                            error = err;
                        }
                        intent = v.intent;
                    }
                    let mut score = 0.0;
                    if let Some((v, asked_cached, err)) = effort_out {
                        cached &= asked_cached;
                        if let Some(err) = err {
                            if !error.is_empty() {
                                error.push_str("; ");
                            }
                            error.push_str(&err);
                        }
                        turn_pick = v.effort;
                        score = v.score;
                    }
                    let mut fallback = String::new();
                    if intent.is_empty() && intent_err.is_some() && !after.is_empty() {
                        // Timed out, failed or resting — not answered none:
                        // the turn before's intent, still one of this
                        // turn's, is a better guess than none (#1826).
                        intent = after.clone();
                        fallback = "previous".to_owned();
                    }
                    classified = Some(crate::rules::Classified {
                        by: group.classifier.clone(),
                        intents: intents.clone(),
                        intent: intent.clone(),
                        after,
                        effort: turn_pick.clone(),
                        score,
                        after_effort,
                        cached,
                        ms,
                        error,
                        fallback,
                    });
                } else {
                    classified = Some(crate::rules::Classified {
                        by: group.classifier.clone(),
                        intents: intents.clone(),
                        error: "the message has no words to classify".to_owned(),
                        ..Default::default()
                    });
                }
            }
        }
        tracker.set_classified(classified.clone());
        if let Some(mut hit) = crate::rules::rule_for(
            &plan.id,
            rules,
            creq,
            agent,
            conversation,
            &windows,
            &intent,
            &turn_pick,
        ) {
            hit.classified = classified;
            // The turn's pick rides with the requests within it, whether or
            // not a rule put a member first.
            if !hit.pick.is_empty() {
                turn_pick = hit.pick.clone();
            }
            if !hit.use_.is_empty() {
                // Put the rule's member first, then those the rules after it
                // also name (`then`), each once, the rest behind in order.
                // Members match as named, effort and all: the same model at
                // two efforts is two members (parsed both sides, so an
                // unvalidated file spells them either way).
                let member = |id: &str| {
                    let (base, effort) = config.member_effort(id);
                    (base, effort)
                };
                let mut rank: HashMap<(String, String), usize> = HashMap::new();
                rank.insert(member(&hit.use_), 0);
                for (i, id) in hit.then.iter().enumerate() {
                    rank.entry(member(id)).or_insert(i + 1);
                }
                routes.sort_by_key(|r| {
                    rank.get(&(format!("{}/{}", r.provider.id, r.model), r.effort.clone()))
                        .copied()
                        .unwrap_or(usize::MAX)
                });
                // The rule's effort rides its member first: the member is
                // asked for it — unless the member fixes its own, and only
                // the first try (failover members are unaffected). It beats
                // the turn's pick and what the agent asked, like a fixed
                // effort does.
                if !hit.use_effort.is_empty()
                    && let Some(first) = routes.first_mut()
                    && first.effort.is_empty()
                    && member(&hit.use_).0 == format!("{}/{}", first.provider.id, first.model)
                {
                    first.effort = crate::rules::fit_level(&hit.use_effort);
                }
            }
        }
    }
    // The turn's requests ask their model for its pick, where the agent
    // asked for reasoning at all — fitted to what a model is known to take,
    // as the reference sends its pick (withEffort/fitLevel). Within the
    // turn the stored pick rides along with the tool rounds going back.
    let picked_req: Option<CompletionRequest> =
        if !turn_pick.is_empty() && creq.effort.is_some_and(|e| e.asked()) {
            magpie_core::effort::Effort::parse(&crate::rules::fit_level(&turn_pick)).map(|level| {
                let mut owned = (*creq).clone();
                owned.effort = Some(level);
                owned
            })
        } else {
            None
        };
    let creq: &CompletionRequest = picked_req.as_ref().unwrap_or(creq);
    let mut decision = None;
    if let Some(decided) = {
        let (scope, mode, rotate) = affinity_scope.clone();
        let seats = routes
            .iter()
            .enumerate()
            .flat_map(|(route_index, route)| {
                route
                    .api_keys
                    .iter()
                    .enumerate()
                    .map(move |(key, (slot, api_key))| Seat {
                        who: who_of(&route.provider.id, api_key),
                        model: route.model.clone(),
                        route_index,
                        route: *slot,
                        key: key,
                        resting: state.key_rotation.cooling(
                            &route.provider.id,
                            route.provider.key_generation,
                            *slot,
                        ),
                    })
            })
            .collect::<Vec<_>>();
        if !seats.is_empty() {
            let decided = state.affinity.affine_conversation(
                &scope,
                mode,
                rotate,
                conversation,
                creq,
                &seats,
            );
            apply_affinity(&mut routes, &decided);
            Some(decided)
        } else {
            None
        }
    } {
        decision = Some(decided);
    }
    let mut decision = decision.expect("a route always carries its affinity");
    leave_out_routes_without_room(&mut routes, &mut decision, creq);
    // A key or account whose lane is full gives way to one with a slot
    // free now: another key of its provider, or another member of its
    // routing group (the reference's laneMate, #892). A fallback provider
    // is never asked for want of a slot.
    give_way_to_free_lanes(state, &mut routes);
    let last_route = routes.len().saturating_sub(1);
    let no_route = routes
        .first()
        .map(|route| {
            let key_slot = route.api_keys.first().map(|(slot, _)| *slot).unwrap_or(0);
            UpstreamCandidate::new(route, key_slot)
        })
        .expect("initial route");
    // Providers that already answered this request with a plain 403 — one
    // that says nothing of why. A second key of the same provider answering
    // alike means the request itself is refused, not each key (d5bc9f00,
    // plugins#243): stop walking its pool so one refusal doesn't bench every
    // key in turn.
    let mut forbade: std::collections::HashSet<String> = std::collections::HashSet::new();

    'routes: for (route_index, route) in routes.into_iter().enumerate() {
        if let (Some(group_id), Some(group_route_index)) =
            (&route.group_id, route.group_route_index)
            && let Some(group) = config.group(group_id)
        {
            state.key_rotation.record_group_attempt(
                group_id,
                group.routing,
                &group.routes,
                &group.route_weights(),
                group_route_index,
            );
        }
        'keys: for (attempt, (key_slot, api_key)) in route.api_keys.iter().enumerate() {
            let candidate = UpstreamCandidate::new(&route, *key_slot);
            tracker.begin_attempt(&route.provider.id, &route.model, *key_slot);
            // A key or account with `max_concurrency` is asked once one of
            // its slots is free, in turn; one with `max_rpm` then waits for
            // room in its minute. Its queue full, or its wait out, nobody
            // failed and nobody rests: the next key or member is asked,
            // else the agent is told with a 429 and Retry-After (#892).
            let who = who_of(&route.provider.id, api_key);
            let slot = match state
                .lanes
                .take(
                    &who,
                    route.provider.lane_limit(),
                    route.provider.queue_bound(),
                    route.provider.queue_wait(),
                )
                .await
            {
                Ok(slot) => slot,
                Err(error) => {
                    let failure = lane_failure(&route.provider, error, None);
                    tracker.record_attempt(&route.provider.id, *key_slot, "queue_refused");
                    if attempt + 1 < route.api_keys.len() {
                        continue 'keys;
                    }
                    if route_index != last_route && route.group_id.is_none() {
                        break 'keys;
                    }
                    if route_index != last_route {
                        break 'keys;
                    }
                    return Err((failure, candidate, decision.clone()));
                }
            };
            if let Err(error) = state
                .rpms
                .wait(
                    &who,
                    route.provider.rpm_limit(),
                    lanes::rpm_longest(&route.provider),
                )
                .await
            {
                drop(slot);
                let retry_after = error.retry_after();
                let failure = lane_failure(&route.provider, error, Some(retry_after));
                tracker.record_attempt(&route.provider.id, *key_slot, "rpm_refused");
                if attempt + 1 < route.api_keys.len() {
                    continue 'keys;
                }
                if route_index != last_route {
                    break 'keys;
                }
                return Err((failure, candidate, decision.clone()));
            }
            let folds_roots = object_root::folds_roots(&route.provider, &route.model);
            let mut empty_retries = 0;
            let mut optional_retry: Option<Vec<&'static str>> = None;
            let mut sampling_retry = false;
            let mut root_union_retry = false;
            // The room a window left for the reply, once a refusal has
            // said it: the same candidate is asked again under it, and it
            // is asked no more.
            let mut room_cap: Option<u64> = None;
            let mut original_root_union_refusal: Option<(Value, String)> = None;
            // A Chat endpoint refusing tools with reasoning was asked
            // again with `reasoning_effort: "none"` (#176); when `none`
            // is refused too, that refusal stands as the answer (#2670).
            let mut tools_without_effort_retry = false;
            let mut tools_refusal: Option<Value> = None;
            // The client's own Chat/Responses conversation ends with the
            // assistant's message: a turn already said, not one to go on
            // from — an upstream model that takes no prefill is asked with
            // a user turn after it (the reference's `userLast`, #1447).
            let client_last_is_turn =
                matches!(wire, ClientWire::Chat { .. } | ClientWire::Responses)
                    && crate::continuation::ends_with_assistant(creq);
            let mut turn_after = false;
            'candidate: loop {
                liveness.activity.attempt();
                // A member fixed at an effort asks for it — whatever the
                // agent asked and whatever the turn picked (the reference's
                // `withFixedEffort`), fitted as the turn's pick is.
                let fixed = if route.effort.is_empty() {
                    None
                } else {
                    magpie_core::effort::Effort::parse(&crate::rules::fit_level(&route.effort)).map(
                        |level| {
                            let mut owned = (*creq).clone();
                            owned.effort = Some(level);
                            owned
                        },
                    )
                };
                let send = fixed.as_ref().unwrap_or(creq);
                let mut body = match upstream::build_request(&route.provider, send, &route.model) {
                    Ok(body) => body,
                    Err(error) => {
                        // A local conversion error is not a bad credential or
                        // upstream outage. Skip the entire key pool, with no
                        // cooldown or network call, and try a compatible route.
                        // Do not count a credential attempt: no HTTP request was sent.
                        if route_index != last_route {
                            continue 'routes;
                        }
                        return Err((
                            ProxyFailure::new(StatusCode::BAD_REQUEST, error.to_string()),
                            candidate,
                            decision.clone(),
                        ));
                    }
                };
                // A vendor that turned an optional field away once gets a
                // request without it from the start.
                if body.as_object().is_some() {
                    let unfit: Vec<&str> = optional_fields::OPTIONAL_FIELDS
                        .iter()
                        .copied()
                        .filter(|field| {
                            body.get(*field).is_some()
                                && unfit_fields::unfit(
                                    &route.provider.id,
                                    &route.model,
                                    route.provider.protocol.as_str(),
                                    field,
                                )
                        })
                        .collect();
                    if !unfit.is_empty() {
                        body = optional_fields::without_fields(&body, &unfit);
                        tracing::debug!(
                            provider = %route.provider.id,
                            model = %route.model,
                            fields = ?unfit,
                            "omitting optional fields an upstream refused"
                        );
                    }
                }
                if let Some(fields) = &optional_retry {
                    body = optional_fields::without_fields(&body, fields);
                }
                if sampling_retry {
                    body.as_object_mut()
                        .expect("request body object")
                        .remove("temperature");
                    body.as_object_mut()
                        .expect("request body object")
                        .remove("top_p");
                    body.as_object_mut()
                        .expect("request body object")
                        .remove("top_k");
                }
                if tools_without_effort_retry && let Some(object) = body.as_object_mut() {
                    object.insert("reasoning_effort".into(), json!("none"));
                }
                if (folds_roots || root_union_retry) && object_root::fold_tool_roots(&mut body) {
                    tracing::debug!(
                        provider = %route.provider.id,
                        model = %route.model,
                        "folding tool roots to an object"
                    );
                }
                if matches!(
                    route.provider.protocol,
                    Protocol::Chat | Protocol::Responses
                ) && client_last_is_turn
                    && (turn_after
                        || unfit_fields::unfit(
                            &route.provider.id,
                            &route.model,
                            route.provider.protocol.as_str(),
                            &crate::continuation::prefill_refused_field(&route.model),
                        ))
                {
                    if let Some(next) =
                        crate::continuation::user_last_body(route.provider.protocol, &body)
                    {
                        body = next;
                        turn_after = true;
                    }
                }
                if let Some(room) = room_cap {
                    // The reply asked for with the prompt passed the
                    // window: ask the same candidate again for what the
                    // window has left.
                    reply_room::with_reply_cap(&mut body, route.provider.protocol, room);
                }
                tracing::debug!(
                    provider = %route.provider.id,
                    model = %route.model,
                    key_slot,
                    route_index,
                    "routing upstream"
                );

                let first_token =
                    GroupFirstToken::new(if creq.stream && route_index != last_route {
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
                        tracker.record_attempt(
                            &route.provider.id,
                            *key_slot,
                            timeout.attempt_status(),
                        );
                        if route_index == last_route {
                            return Err((
                                ProxyFailure::new(StatusCode::GATEWAY_TIMEOUT, timeout.message()),
                                candidate,
                                decision.clone(),
                            ));
                        }
                        // No response headers: an endpoint timeout is not a bad key.
                        break 'keys;
                    }
                    Ok(Err(e)) => {
                        tracker.record_attempt(&route.provider.id, *key_slot, "network_error");
                        let message = format!("cannot reach {}: {e}", route.provider.id);
                        tracing::warn!("{message}");
                        if route_index == last_route {
                            return Err((
                                ProxyFailure::new(StatusCode::BAD_GATEWAY, message),
                                candidate,
                                decision.clone(),
                            ));
                        }
                        // A connection-level failure belongs to the endpoint, not
                        // to one credential, so try the next route directly.
                        break 'keys;
                    }
                };

                let status = resp.status();
                liveness.activity.headers(status.is_success());
                if status.is_success()
                    && let Some(fields) = optional_retry.take()
                {
                    // The retried ask accepted the request: this vendor and
                    // model do not take these fields at all.
                    for field in fields {
                        unfit_fields::refuse(
                            &route.provider.id,
                            &route.model,
                            route.provider.protocol.as_str(),
                            field,
                        );
                    }
                }
                if status.is_success() && root_union_retry {
                    unfit_fields::refuse(
                        &route.provider.id,
                        &route.model,
                        route.provider.protocol.as_str(),
                        "root_union",
                    );
                }
                if status.is_success() {
                    // A vendor that ignored `stream: true` answers with a
                    // whole JSON body, not an SSE stream. It can no longer go
                    // to the agent as one: replay it as the client's stream.
                    let content_type = resp
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let streamed = creq.stream && content_type.contains("text/event-stream");
                    if !creq.stream || !streamed {
                        let value: Value = match resp.json().await {
                            Ok(value) => value,
                            Err(error) => {
                                tracker.record_attempt(
                                    &route.provider.id,
                                    *key_slot,
                                    "bad_upstream_body",
                                );
                                return Err((
                                    ProxyFailure::new(
                                        StatusCode::BAD_GATEWAY,
                                        format!("bad upstream body: {error}"),
                                    ),
                                    candidate,
                                    decision.clone(),
                                ));
                            }
                        };
                        let protocol = route.provider.protocol;
                        tracker.observe_usage(&decode_upstream_response_usage(protocol, &value));
                        if stream_refusal::policy_error(&value)
                            || (stream_refusal::filtered(protocol, &value)
                                && !stream_refusal::has_answer(protocol, &value, true))
                        {
                            tracker.record_attempt(&route.provider.id, *key_slot, "refusal");
                            if route_index != last_route {
                                break 'keys;
                            }
                            return Err((ProxyFailure::refusal(), candidate, decision.clone()));
                        }
                        if value.get("error").is_some() {
                            tracker.record_attempt(&route.provider.id, *key_slot, "upstream_error");
                            return Err((
                                ProxyFailure::new(
                                    StatusCode::BAD_GATEWAY,
                                    "upstream returned an error instead of a reply",
                                ),
                                candidate,
                                decision.clone(),
                            ));
                        }
                        if protocol == Protocol::Gemini {
                            for event in gemini::GeminiStreamDecoder::new().decode(&value) {
                                if let StreamEvent::Usage(usage) = event {
                                    tracker.observe_usage(&usage);
                                }
                            }
                            let mut reply = GeminiReply::default();
                            reply.observe(&value);
                            if reply.is_empty() {
                                tracker.record_attempt(
                                    &route.provider.id,
                                    *key_slot,
                                    "empty_reply",
                                );
                                if empty_retries == 0 {
                                    empty_retries += 1;
                                    tracker.begin_reask();
                                    tokio::time::sleep(Duration::from_secs(1)).await;
                                    continue 'candidate;
                                }
                                if attempt + 1 < route.api_keys.len() {
                                    continue 'keys;
                                }
                                if route_index != last_route {
                                    break 'keys;
                                }
                                return Err((
                                    ProxyFailure::new(StatusCode::BAD_GATEWAY, EMPTY_REPLY),
                                    candidate,
                                    decision.clone(),
                                ));
                            }
                        }
                        let mut ir = match decode_upstream_response(protocol, &value) {
                            Ok(ir) => ir,
                            Err(error) => {
                                tracker.record_attempt(
                                    &route.provider.id,
                                    *key_slot,
                                    "upstream_decode_error",
                                );
                                return Err((
                                    ProxyFailure::new(StatusCode::BAD_GATEWAY, error.to_string()),
                                    candidate,
                                    decision.clone(),
                                ));
                            }
                        };
                        // Grok writes a whole number in a call's arguments as
                        // 2500.0; a client that types its arguments strictly
                        // reads 2500, which is the same value in JSON.
                        if crate::grok_integral::grok_served(&creq.model, &route.model) {
                            crate::grok_integral::whole_reply(&mut ir);
                        }
                        tracker.record_attempt(&route.provider.id, *key_slot, "ok");
                        return Ok((
                            UpstreamReply::Decoded {
                                ir,
                                whole_as_stream: creq.stream,
                            },
                            candidate,
                            decision.clone(),
                        ));
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
                            break 'keys;
                        }
                    };
                    if let Some(ref failure) = failure {
                        tracker.record_attempt(
                            &route.provider.id,
                            *key_slot,
                            match failure.kind {
                                FailureKind::EmptyReply => "empty_reply",
                                FailureKind::Refusal => "refusal",
                                FailureKind::Upstream => "upstream_stream_error",
                            },
                        );
                        if failure.kind == FailureKind::Refusal {
                            drop(events);
                            if route_index != last_route {
                                break 'keys;
                            }
                            return Err((ProxyFailure::refusal(), candidate, decision.clone()));
                        }
                        if failure.kind == FailureKind::EmptyReply && empty_retries == 0 {
                            drop(events);
                            empty_retries += 1;
                            tracker.begin_reask();
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue 'candidate;
                        }
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
                        // A mate key of this provider already gave a plain
                        // 403 on this request: it is the request refused,
                        // not each key — stop the pool without resting this
                        // one either. On the last key of the last route its
                        // own 403 is the answer.
                        if plain_forbidden(failure.status.as_u16(), &failure.body)
                            && !forbade.insert(route.provider.id.clone())
                            && (attempt + 1 < route.api_keys.len() || route_index != last_route)
                        {
                            drop(events);
                            break 'keys;
                        }
                        if attempt + 1 < route.api_keys.len()
                            && (failure.kind == FailureKind::EmptyReply
                                || key_retryable_status(failure.status))
                        {
                            drop(events);
                            continue 'keys;
                        }
                        if route_index != last_route
                            && route_retryable_status(failure.status, &failure.body, "")
                        {
                            drop(events);
                            if model_retired(failure.status.as_u16(), &failure.body) {
                                retired_store::retire(
                                    &route.provider.id,
                                    &route.model,
                                    std::time::SystemTime::now(),
                                );
                            }
                            break 'keys;
                        }
                    } else {
                        tracker.record_attempt(&route.provider.id, *key_slot, "ok");
                    }
                    return Ok((
                        UpstreamReply::Stream {
                            events,
                            empty_retries,
                            reask_allowed: failure.is_none(),
                        },
                        candidate,
                        decision.clone(),
                    ));
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
                                decision.clone(),
                            ));
                        }
                        break 'keys;
                    }
                };
                if stream_refusal::policy_error(&error.body) {
                    tracker.record_attempt(&route.provider.id, *key_slot, "refusal");
                    if route_index != last_route {
                        break 'keys;
                    }
                    return Err((ProxyFailure::refusal(), candidate, decision.clone()));
                }
                tracker.record_attempt(&route.provider.id, *key_slot, &status.as_u16().to_string());
                // A vendor that names a union at a tool's parameter root
                // gets one more ask with every root folded to an object.
                if optional_fields::bad_request(status.as_u16())
                    && !root_union_retry
                    && object_root::refusal(&error.body)
                    && object_root::fold_tool_roots(&mut body)
                {
                    original_root_union_refusal = Some((error.body.clone(), error.message.clone()));
                    root_union_retry = true;
                    continue 'candidate;
                }
                // A vendor that names an optional request field in a 400/422
                // gets one more ask without it, on the same credential. An
                // invalid value must reach the client instead.
                if optional_fields::bad_request(status.as_u16())
                    && route.provider.protocol != Protocol::Anthropic
                    && optional_retry.is_none()
                {
                    let refused = optional_fields::refused_optional(
                        status.as_u16(),
                        &body,
                        &error.body.get("error").unwrap_or(&error.body).clone(),
                    );
                    if !refused.is_empty() {
                        optional_retry = Some(refused);
                        continue 'candidate;
                    }
                }
                // A Claude that refuses a sampling parameter by a wording
                // no other rule reads (#1454, as the reference
                // `samplingDeprecated`) is asked once more without
                // temperature and top_p, on the same credential.
                if sampling_refused(status, &error.body)
                    && route.provider.protocol == Protocol::Anthropic
                    && !sampling_retry
                    && (creq.temperature.is_some() || creq.top_p.is_some())
                {
                    sampling_retry = true;
                    continue 'candidate;
                }
                // A Chat endpoint that refuses function tools with
                // reasoning but names `none` is asked once more that way,
                // when no Responses route of this member can take them
                // (#176). When `none` is refused too, naming the levels
                // the model takes, that refusal is not a retry at the
                // model's lowest level — the same by the upstream's own
                // word — but the endpoint refusal it is (#2670): the
                // original tools refusal stands as the answer, a group's
                // next member is asked and the key doesn't rest.
                let mut error = error;
                // If folding the roots exposed a different shape refusal,
                // the agent still sees the union refusal that made us try.
                if root_union_retry
                    && shape_refused(status.as_u16(), &error.body)
                    && let Some((refusal_body, refusal_message)) =
                        original_root_union_refusal.take()
                {
                    error.body = refusal_body;
                    error.message = refusal_message;
                }
                if status == StatusCode::BAD_REQUEST {
                    if let Some(refusal) = &tools_refusal {
                        if crate::wrong_endpoint::effort_levels_named(&error.body) {
                            unfit_fields::refuse(
                                &route.provider.id,
                                &route.model,
                                route.provider.protocol.as_str(),
                                "reasoning_off",
                            );
                            error.body = refusal.clone();
                            if let Some(message) = refusal
                                .pointer("/error/message")
                                .or_else(|| refusal.pointer("/message"))
                                .and_then(Value::as_str)
                            {
                                error.message = format!("upstream {status}: {message}");
                            }
                        }
                    } else if !tools_without_effort_retry
                        && body
                            .get("reasoning_effort")
                            .and_then(Value::as_str)
                            .is_some_and(|level| level != "none")
                        && crate::wrong_endpoint::tools_without_effort(&error.body)
                        && !unfit_fields::unfit(
                            &route.provider.id,
                            &route.model,
                            route.provider.protocol.as_str(),
                            "reasoning_off",
                        )
                    {
                        tools_refusal = Some(error.body.clone());
                        tools_without_effort_retry = true;
                        continue 'candidate;
                    }
                }
                // A model that takes no prefill — it refuses a
                // conversation ending with the assistant's message —
                // is remembered for it alone, and the client's own such
                // conversation is asked once more with a user turn after
                // it (the reference's `refusesPrefill`/`userLast`, #1447).
                if crate::continuation::refuses_prefill(status.as_u16(), &error.body)
                    && crate::continuation::ends_with_assistant(creq)
                {
                    unfit_fields::refuse(
                        &route.provider.id,
                        &route.model,
                        route.provider.protocol.as_str(),
                        &crate::continuation::prefill_refused_field(&route.model),
                    );
                    if client_last_is_turn
                        && !turn_after
                        && matches!(
                            route.provider.protocol,
                            Protocol::Chat | Protocol::Responses
                        )
                    {
                        turn_after = true;
                        continue 'candidate;
                    }
                }
                // The window held the prompt but not the reply asked for
                // with it: DeepSeek counts `max_tokens` in its 1M window,
                // so a client asking its full 384K for a ~700K
                // conversation was refused there as too long, where every
                // other member of its group answered. The same candidate
                // is asked again for what the window has left — same key,
                // same model, nobody rests. Room too small to be worth a
                // reply still reaches the agent as the overflow it is.
                if room_cap.is_none()
                    && optional_fields::bad_request(status.as_u16())
                    && let Some(left) = reply_room::reply_left(&error.message)
                        .or_else(|| reply_room::reply_left(&error.body.to_string()))
                    && left >= reply_room::LEAST_REPLY_ROOM
                    && reply_room::with_reply_cap(&mut body, route.provider.protocol, left)
                {
                    tracing::debug!(
                        provider = %route.provider.id,
                        model = %route.model,
                        left,
                        "asking again for the reply the window leaves"
                    );
                    room_cap = Some(left);
                    continue 'candidate;
                }
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
                let mut detail = error.message;
                tracing::warn!(
                    "upstream {} key {}: {detail}",
                    route.provider.id,
                    key_slot + 1
                );
                // A mate key of this provider already gave a plain 403 on
                // this request: it is the request refused, not each key —
                // stop the pool without resting this one either. On the
                // last key of the last route its own 403 is the answer.
                if plain_forbidden(status.as_u16(), &error.body)
                    && !forbade.insert(route.provider.id.clone())
                    && (attempt + 1 < route.api_keys.len() || route_index != last_route)
                {
                    break 'keys;
                }
                if attempt + 1 < route.api_keys.len() && key_retryable_status(status) {
                    continue 'keys;
                }
                if route_index != last_route && route_retryable_status(status, &error.body, &detail)
                {
                    if model_retired(status.as_u16(), &error.body) {
                        retired_store::retire(
                            &route.provider.id,
                            &route.model,
                            std::time::SystemTime::now(),
                        );
                    }
                    break 'keys;
                }
                if edge_blocked(&error.body) || edge_blocked_text(&detail) {
                    detail = format!("{detail} — {BLOCKED_HINT}");
                }
                if crate::wrong_endpoint::wrong_endpoint(status.as_u16(), &error.body) {
                    let asked = match route.provider.protocol {
                        Protocol::Chat => "/v1/chat/completions",
                        Protocol::Responses => "/v1/responses",
                        Protocol::Anthropic => "/v1/messages",
                        Protocol::Gemini => "generateContent",
                    };
                    detail =
                        wrong_api_note(&route.provider, &route.model, &error.body, asked, detail);
                }
                let status =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                return Err((
                    ProxyFailure::new(status, detail),
                    candidate,
                    decision.clone(),
                ));
            }
        }
    }

    Err((
        ProxyFailure::new(StatusCode::BAD_GATEWAY, "no upstream route"),
        no_route,
        decision,
    ))
}

/// Re-ask an empty reply or reasoning-only loop on the exact candidate that already
/// committed the response. This deliberately bypasses fallback and key
/// rotation: it is a continuation of the same upstream attempt, not a new
/// routing decision.
async fn reask_stream(
    state: &AppState,
    config: &Config,
    creq: &CompletionRequest,
    candidate: &UpstreamCandidate,
    tracker: &mut RequestTracker,
    liveness: &StreamLiveness,
    pause: Duration,
    continuation: Option<&crate::continuation::Continuation>,
) -> Result<UpstreamEvents, String> {
    tracker.begin_reask();
    tokio::time::sleep(pause).await;

    let api_key = candidate
        .provider
        .api_key_pool()
        .into_iter()
        .nth(candidate.key_slot)
        .ok_or_else(|| "reask key is no longer available".to_owned())?;
    let request = match continuation {
        Some(cont) => cont.request(creq),
        None => creq.clone(),
    };
    let mut body = upstream::build_request(&candidate.provider, &request, &candidate.model)
        .map_err(|error| error.to_string())?;
    if let Some(cont) = continuation {
        cont.mark(&mut body);
    }
    let clock = StreamClock::new(
        config.stream_first_content_timeout_ms,
        config.stream_idle_timeout_ms,
    );
    liveness.activity.attempt();
    let response = match clock
        .headers(upstream::send_at(
            &state.http,
            &candidate.provider,
            &api_key,
            &body,
            &candidate.model,
            true,
            continuation.map(|c| c.base_url(&candidate.provider)),
        ))
        .await
    {
        Ok(Ok(response)) => response,
        Err(timeout) => {
            tracker.record_attempt(
                &candidate.provider.id,
                candidate.key_slot,
                timeout.attempt_status(),
            );
            return Err(timeout.message().to_owned());
        }
        Ok(Err(error)) => {
            tracker.record_attempt(&candidate.provider.id, candidate.key_slot, "network_error");
            return Err(format!("reask could not reach upstream: {error}"));
        }
    };

    let status = response.status();
    liveness.activity.headers(status.is_success());
    if !status.is_success() {
        let detail = match clock.headers(upstream::error_detail(response)).await {
            Ok(detail) => detail,
            Err(timeout) => {
                tracker.record_attempt(
                    &candidate.provider.id,
                    candidate.key_slot,
                    timeout.attempt_status(),
                );
                return Err(timeout.message().to_owned());
            }
        };
        if stream_refusal::policy_error(&detail.body) {
            tracker.record_attempt(&candidate.provider.id, candidate.key_slot, "refusal");
            return Err(REFUSAL.to_owned());
        }
        // A model that takes no prefill turned the continuation's ask
        // away: remember it so the reply isn't asked to go on again, and
        // end it with what cut it, not with a refusal of an ask the
        // client never made (the reference's `refusesPrefill`, #1447).
        if continuation.is_some()
            && crate::continuation::refuses_prefill(status.as_u16(), &detail.body)
        {
            tracker.record_attempt(
                &candidate.provider.id,
                candidate.key_slot,
                "prefill_refused",
            );
            unfit_fields::refuse(
                &candidate.provider.id,
                &candidate.model,
                candidate.provider.protocol.as_str(),
                &crate::continuation::prefill_refused_field(&candidate.model),
            );
            return Err(crate::continuation::PREFILL_REFUSED.to_owned());
        }
        tracker.record_attempt(
            &candidate.provider.id,
            candidate.key_slot,
            &status.as_u16().to_string(),
        );
        let mut message = detail.message;
        if edge_blocked(&detail.body) || edge_blocked_text(&message) {
            message = format!("{message} — {BLOCKED_HINT}");
        }
        return Err(message);
    }

    let (events, failure) = prefetch_stream(
        response,
        candidate.provider.protocol,
        tracker,
        clock,
        liveness,
        config.stream_loop_guard,
    )
    .await;
    tracker.record_attempt(
        &candidate.provider.id,
        candidate.key_slot,
        match failure.as_ref().map(|f| f.kind) {
            Some(FailureKind::Refusal) => "refusal",
            Some(_) => "upstream_stream_error",
            None => "ok",
        },
    );
    Ok(events)
}

/// Add the reference gateway's `wrongAPINote` (#1215, #1308, #2670) to an
/// upstream error that says the model is served on Responses, when this
/// provider has no Responses URL: the agent is told which URL to set
/// instead of only the upstream's complaint.
fn wrong_api_note(
    provider: &ProviderConfig,
    model: &str,
    body: &Value,
    asked: &str,
    mut detail: String,
) -> String {
    if !crate::wrong_endpoint::wrong_endpoint(400, body)
        || !crate::wrong_endpoint::asks_responses(body)
        || provider.protocol == Protocol::Responses
    {
        return detail;
    }
    detail.push_str(&format!(
        " (asked at {asked}; {} is served on /v1/responses, which {} has no URL for)",
        model, provider.name
    ));
    detail
}

struct ProxyFailure {
    kind: FailureKind,
    status: StatusCode,
    message: String,
    retry_after: Option<u64>,
}

impl ProxyFailure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Upstream,
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    fn refusal() -> Self {
        Self {
            kind: FailureKind::Refusal,
            status: StatusCode::BAD_REQUEST,
            message: REFUSAL.into(),
            retry_after: None,
        }
    }

    /// A request turned away by a key's queue or minute, telling the agent
    /// when it may ask again (the reference's `laneMessage` + Retry-After).
    fn lane(status: StatusCode, message: impl Into<String>, retry_after: Option<u64>) -> Self {
        Self {
            kind: FailureKind::Upstream,
            status,
            message: message.into(),
            retry_after,
        }
    }

    fn history_status(&self) -> &'static str {
        if self.kind == FailureKind::Refusal {
            "refusal"
        } else {
            "upstream_error"
        }
    }

    fn response(self, wire: ClientWire) -> Response {
        let (status, kind, message) = client_error(self.status, &self.message, wire);
        let mut response = err_response(status, wire.error_body_kind(status, &message, kind));
        if let Some(seconds) = self.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                header::HeaderValue::from_str(&seconds.to_string())
                    .unwrap_or_else(|_| header::HeaderValue::from_static("1")),
            );
        }
        response
    }
}

/// The status, error type and message a client sees for an upstream
/// failure, as the reference's `writeErrorSaid` says them.
///
/// An overflow the vendor worded reaches an agent as a 400
/// `context_length_exceeded`, whatever status it came in (a 413 body
/// limit or a 422), so it compacts and tries again rather than stopping.
/// Anthropic's own 400 gets its "prompt is too long" prefix. A 413 is
/// `request_too_large` in every shape, which pi reads as an overflow; it
/// also says this was a request-body byte limit, not the model's context.
fn client_error(
    status: StatusCode,
    message: &str,
    wire: ClientWire,
) -> (StatusCode, &'static str, String) {
    let kind = error_kind(status.as_u16());
    if crate::model_gone::context_overflowed(status.as_u16(), &json!({"message": message})) {
        let mut message = message.to_owned();
        if wire == ClientWire::Anthropic && !message.to_lowercase().contains("prompt is too long") {
            message = format!("prompt is too long: {message}");
        }
        return (StatusCode::BAD_REQUEST, "invalid_request_error", message);
    }
    let mut message = message.to_owned();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        const HINT: &str = "This is a request-body byte limit, not the model's context: compact the conversation (/compact in Codex or Claude Code), or start a new one with a short summary; fewer images or smaller tool results also help.";
        if !message.contains(HINT) {
            message.push_str(" — ");
            message.push_str(HINT);
        }
    }
    (status, kind, message)
}

/// The error type a client sees for an upstream status, as the
/// reference's `writeErrorSaid` names it. Every client shape carries it.
fn error_kind(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

fn err_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

#[cfg(test)]
std::thread_local! {
    // Counts client request bodies decoded by this crate's handlers, so a test
    // can prove one request decodes its body once.
    static CLIENT_BODY_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn client_body_decodes() -> usize {
    CLIENT_BODY_DECODES.with(|count| count.get())
}

#[cfg(test)]
fn reset_client_body_decodes() {
    CLIENT_BODY_DECODES.with(|count| count.set(0));
}

/// Decode one client request body.
///
/// Every entry point decodes through here so a request body is parsed once on
/// its way in. A Chat or Gemini request used to parse the same payload twice:
/// once for a request option and once in the shared proxy, which at a couple
/// of megabytes costs real CPU per request.
fn decode_client_body(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    #[cfg(test)]
    CLIENT_BODY_DECODES.with(|count| count.set(count.get() + 1));
    serde_json::from_slice(bytes)
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

/// A whole reply from a vendor that ignored `stream: true`, as the stream
/// events that say it. The agent asked for a stream and already has the
/// stream's headers, so a whole reply can no longer go to it as one.
fn whole_as_stream_events(reply: &CompletionResponse) -> Vec<StreamEvent> {
    let mut out = vec![StreamEvent::Start {
        model: reply.model.clone(),
    }];
    let mut calls = 0usize;
    for part in &reply.message.parts {
        match part.kind() {
            PartKind::Text => {
                if !part.text.is_empty() {
                    out.push(StreamEvent::TextDelta {
                        text: part.text.clone(),
                    });
                }
                for citation in &part.citations {
                    out.push(StreamEvent::Citation {
                        citation: citation.clone(),
                    });
                }
            }
            PartKind::Thinking => {
                if !part.text.is_empty() {
                    out.push(StreamEvent::ThinkingDelta {
                        text: part.text.clone(),
                    });
                }
                // Native signatures only re-emit to their own protocol's
                // encoder; the Anthropic encoder filters by protocol itself.
                if !part.signature.is_empty() {
                    out.push(StreamEvent::Signature {
                        protocol: SignatureProtocol::Anthropic,
                        target: SignatureTarget::Thinking,
                        signature: part.signature.clone(),
                    });
                }
            }
            PartKind::ToolCall => {
                let index = calls;
                calls += 1;
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: part.id.clone(),
                    name: part.name.clone(),
                    namespace: part.namespace.clone(),
                });
                let args = if part.args.is_null() {
                    "{}".to_owned()
                } else {
                    part.args.to_string()
                };
                if !args.is_empty() {
                    out.push(StreamEvent::ToolCallDelta { index, args });
                }
                out.push(StreamEvent::ToolCallEnd { index });
            }
            PartKind::Image => {
                if !part.data.is_empty() || !part.url.is_empty() {
                    out.push(StreamEvent::Image {
                        image: Box::new(part.clone()),
                    });
                }
            }
            // A completed upstream search replays as its public result;
            // other kinds have no conversation-facing stream shape.
            PartKind::Search => {
                if let Some(search) = part.search.as_ref().filter(|s| s.is_public_result()) {
                    out.push(StreamEvent::Search {
                        search: search.clone(),
                    });
                }
            }
            PartKind::ToolResult => {}
        }
    }
    out.push(StreamEvent::Stop(reply.stop));
    out.push(StreamEvent::Usage(reply.usage));
    out.push(StreamEvent::Done);
    out
}

fn decode_upstream_chunk(
    protocol: Protocol,
    data: &Value,
    decoders: &mut UpstreamDecoders,
) -> Vec<StreamEvent> {
    match protocol {
        Protocol::Chat => decoders.chat.decode(data),
        Protocol::Anthropic => decoders.anthropic.decode(data),
        Protocol::Responses => decoders.responses.decode(data),
        Protocol::Gemini => decoders.gemini.decode(data),
    }
}

/// Releases the in-flight opening count when a request ends, including when
/// its future or response stream is dropped before a terminal event.
struct AffinityOpening {
    state: Arc<AppState>,
    key: String,
}

impl Drop for AffinityOpening {
    fn drop(&mut self) {
        self.state.affinity.opened(&self.key);
    }
}

/// A single handler implementation for every agent-facing protocol.
async fn proxy(
    state: Arc<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
    wire: ClientWire,
) -> Response {
    match decode_client_body(&bytes) {
        Ok(body) => proxy_parsed(state, headers, body, bytes, wire).await,
        Err(e) => {
            let message = format!("invalid JSON: {e}");
            err_response(
                StatusCode::BAD_REQUEST,
                wire.error_body(StatusCode::BAD_REQUEST, &message),
            )
        }
    }
}

/// The shared proxy for a request body that its handler already decoded.
///
/// `bytes` is the exact wire form used for conversation affinity. Keeping it
/// separate from `body` lets Chat and Gemini read request options without
/// decoding the same large JSON payload again here.
pub(crate) async fn proxy_parsed(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: Value,
    bytes: Bytes,
    wire: ClientWire,
) -> Response {
    let config = state.config.current();
    let (creq, initial, group_plan, affinity_scope) =
        match prepare(&config, &state.key_rotation, &body, wire) {
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
    let body_bytes = bytes.to_vec();
    let conversation = crate::affinity::conversation_id_from_value(&headers, &body, &body_bytes);
    let agent = crate::rules::agent_of(
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    );
    let agent = std::sync::Arc::new(agent);
    // Keep ownership through both header waiting and body polling. Dropping
    // either stage cancels the same work and finalizes the same tracker.
    let execute = async move {
        let rule_group = group_plan.as_ref().map(|p| p.id.clone());
        let mut tracker = RequestTracker::new(
            state.requests.clone(),
            wire.name(),
            &creq,
            &initial.provider.id,
            &initial.model,
            0,
        );
        let (reply, candidate, decision) = match send_with_fallback(
            &state,
            &config,
            &creq,
            wire,
            initial,
            group_plan.as_ref(),
            &mut tracker,
            &liveness,
            &conversation,
            &agent,
            affinity_scope.clone(),
        )
        .await
        {
            Ok((resp, candidate, decision)) => {
                tracker.set_route(&candidate.provider.id, &candidate.model, candidate.key_slot);
                retired_store::unretire(
                    &candidate.provider.id,
                    &candidate.model,
                    std::time::SystemTime::now(),
                );
                (resp, candidate, decision)
            }
            Err((response, candidate, decision)) => {
                tracker.set_route(&candidate.provider.id, &candidate.model, candidate.key_slot);
                tracker.finish(response.history_status(), &Usage::default());
                // `affine` counted this conversation while its first request
                // was out. A failed request has no stream to own the guard, so
                // end that count here or it would hold a seat until expiry.
                state.affinity.opened(&decision.key);
                return Err(response);
            }
        };
        // `least_held` counts an unanswered conversation while its first
        // request is out. Keep that count alive through both the header wait
        // and the body stream; a plain `opened` call would miss cancellation
        // and early client disconnects.
        let mut opening = Some(AffinityOpening {
            state: state.clone(),
            key: decision.key.clone(),
        });
        let (mut events, mut empty_retries, reask_allowed) = match reply {
            UpstreamReply::Decoded {
                mut ir,
                whole_as_stream,
            } => {
                tracker.observe_usage(&ir.usage);
                ir.usage = tracker.total_usage();
                // The key that answered is credited with the reply's tokens,
                // whether the client gets JSON or the answer is replayed as a
                // stream below.
                record_key_served(&state, &candidate.provider, candidate.key_slot, &tracker);
                // A whole answer is the reply the key served: remember it as
                // the conversation's answerer before the reply goes out.
                state.affinity.answered(
                    &decision.key,
                    &candidate.who,
                    &candidate.model,
                    decision.trace.turn,
                    tracker.total_usage().cache_read.unwrap_or(0),
                );
                // What the vendor counted the conversation's last request as:
                // the floor a `tokens` rule goes by next turn.
                if let Some(group_id) = rule_group.as_deref() {
                    crate::rules::rule_answered(
                        group_id,
                        &conversation,
                        &creq,
                        &tracker.total_usage(),
                    );
                }
                tracker.finish("ok", &Usage::default());
                if !whole_as_stream {
                    // A whole JSON reply has no body stream to own the guard.
                    drop(opening.take());
                    return Ok(ok_json(wire.encode_response(&ir, &creq)));
                }
                // The vendor ignored `stream: true` and answered whole. The
                // client already holds the stream's headers, so replay the
                // reply as the client dialect's stream.
                (
                    Box::pin(futures::stream::iter(
                        whole_as_stream_events(&ir)
                            .into_iter()
                            .map(UpstreamEvent::from),
                    )) as UpstreamEvents,
                    0usize,
                    false,
                )
            }
            UpstreamReply::Stream {
                events,
                empty_retries,
                reask_allowed,
            } => (events, empty_retries, reask_allowed),
        };

        let reask_candidate = candidate.clone();
        // A reply the upstream's stream cut mid-way is asked of the same
        // candidate again, going on from what the client already has —
        // only where the upstream goes on from a reply's part sent back
        // (the reference's `continuation.go`).
        let mut cont = crate::continuation::Continuation::new({
            let mode =
                crate::continuation::prefill_how(&reask_candidate.provider, &reask_candidate.model);
            if !mode.is_empty()
                && unfit_fields::unfit(
                    &reask_candidate.provider.id,
                    &reask_candidate.model,
                    reask_candidate.provider.protocol.as_str(),
                    &crate::continuation::prefill_refused_field(&reask_candidate.model),
                )
            {
                // The model turned a prefill away before: don't ask it to
                // go on.
                ""
            } else {
                mode
            }
        });
        // Grok writes a whole number in a call's arguments as 2500.0, which a
        // client that types its arguments strictly turns away. The rule is
        // per reply, and follows the model the client named or the one the
        // route actually sends to.
        let mut integral = crate::grok_integral::GrokIntegral::new(
            crate::grok_integral::grok_served(&creq.model, &candidate.model),
        );
        let opening = opening.expect("a stream reply keeps its opening guard");
        let out = async_stream::stream! {
            let _opening = opening;
            let mut encoder = ClientStreamEncoder::for_request(wire, &creq);
            let mut reasked = false;
            let mut reasoning_reasked = false;
            let mut continued = empty_retries > 0;
            let mut stream_retries = 0usize;
            let mut usage_since_retry = false;
            let empty_limit = if group_plan.is_some() { 1 } else { 2 };
            let mut emitted_answer = false;
            'attempt: loop {
                while let Some(item) = events.next().await {
                    let mut event = item.event;
                    // A delta that held only a fraction of zeros has nothing
                    // to hand on: the next piece decides what the number is.
                    let held = !integral.event(&mut event);
                    if let StreamEvent::Usage(usage) = &mut event {
                        if !item.usage_observed {
                            tracker.observe_usage(usage);
                        }
                        // Providers report cumulative counters for each attempt.
                        // Account raw counters once, but expose the same logical
                        // response total to clients and request history.
                        if continued {
                            *usage = tracker.total_usage();
                        }
                    }

                    // The second upstream sends its own Start frame, but the
                    // client sees one logical response and must not receive a
                    // second response.created/message_start/chunk opening.
                    let suppress_start = reasked && matches!(event, StreamEvent::Start { .. });

                    let answer = event.is_answer();
                    emitted_answer |= answer || item.answer_observed;

                    // A cut mid-reply — the client already has something
                    // of it — is asked to go on from there: hold its
                    // error out of the reply while it may yet go on (the
                    // reference's `cut`), unless the error is what the
                    // client must see as it is: a refusal, a named code
                    // it may act on, or the loop guard's own end; nor is
                    // the ask repeated where it just failed the same way
                    // — a 400, a 404, a 422 the same conversation gets
                    // again.
                    if let StreamEvent::Error { code, .. } = &event
                        && cont.possible()
                        && group_plan.is_none()
                        && code.is_none()
                        && !item.loop_reasoning
                        && !item
                            .failure
                            .as_ref()
                            .is_some_and(|f| f.kind == FailureKind::Refusal)
                        && cont.again(
                            item.failure.as_ref().map_or(0, |f| f.status.as_u16()),
                            false,
                            stream_retries,
                        )
                    {
                        drop(events);
                        reasked = true;
                        continued = true;
                        integral.reset();
                        usage_since_retry = false;
                        stream_retries += 1;
                        match reask_stream(
                            &state,
                            &config,
                            &creq,
                            &reask_candidate,
                            &mut tracker,
                            &liveness,
                            Duration::from_secs(1),
                            Some(&cont),
                        ).await {
                            Ok(next_events) => {
                                events = next_events;
                                continue 'attempt;
                            }
                            Err(message) => {
                                // The prefill itself was turned away: the
                                // reply ends with what cut it, as a reply
                                // that can't go on does.
                                let message = if message == crate::continuation::PREFILL_REFUSED {
                                    match &event {
                                        StreamEvent::Error { message, .. } => message.clone(),
                                        _ => message,
                                    }
                                } else {
                                    message
                                };
                                tracker.finish("upstream_stream_error", &Usage::default());
                                let error = StreamEvent::Error {
                                    message,
                                    code: None,
                                };
                                if let Some(frame) = encoder.encode(&error) {
                                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                                }
                                return;
                            }
                        }
                    }

                    // A continuation's reply doesn't say its framing or
                    // its reasoning again, and a full echo of the text it
                    // was prefilled with is dropped (the reference's
                    // `emit`/`unecho`); what the model says instead of
                    // echoing goes whole.
                    if cont.resume {
                        match &mut event {
                            StreamEvent::ThinkingDelta { .. }
                            | StreamEvent::ThinkingStart { .. }
                            | StreamEvent::Sealed { .. }
                            | StreamEvent::Signature { .. } => continue,
                            StreamEvent::TextDelta { text } => {
                                *text = cont.unecho(std::mem::take(text));
                                if text.is_empty() {
                                    continue;
                                }
                            }
                            _ => {}
                        }
                    }
                    cont.add(&event);

                    let retry_reasoning = item.loop_reasoning && !reasoning_reasked && !emitted_answer;
                    let retry_empty = reask_allowed && item.failure.as_ref().is_some_and(|f| f.kind == FailureKind::EmptyReply)
                        && empty_retries < empty_limit && !emitted_answer;
                    if retry_reasoning || retry_empty {
                        // Preserve any reasoning already sent; replace only the
                        // internally classified terminal with a bounded
                        // same-candidate reask. Dropping the old stream first
                        // is what cancels its response body.
                        drop(events);
                        reasked = true;
                        continued = true;
                        // The reasked reply starts its calls over; a fraction
                        // held from the abandoned one must not leak into it.
                        integral.reset();
                        usage_since_retry = false;
                        reasoning_reasked |= retry_reasoning;
                        empty_retries += usize::from(retry_empty);
                        let pause = Duration::from_secs(if retry_empty { 1 << (empty_retries - 1) } else { 1 });
                        match reask_stream(
                            &state,
                            &config,
                            &creq,
                            &reask_candidate,
                            &mut tracker,
                            &liveness,
                            pause,
                            None,
                        ).await {
                            Ok(next_events) => {
                                events = next_events;
                                continue 'attempt;
                            }
                            Err(message) => {
                                tracker.finish("upstream_stream_error", &Usage::default());
                                let error = StreamEvent::Error {
                                    message,
                                    code: None,
                                };
                                if let Some(frame) = encoder.encode(&error) {
                                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                                }
                                return;
                            }
                        }
                    }

                    let terminal_status = match &event {
                        StreamEvent::Done => Some("ok"),
                        StreamEvent::Error { .. } => Some(if item.failure.as_ref().is_some_and(|f| f.kind == FailureKind::Refusal) { "refusal" } else { "upstream_stream_error" }),
                        _ => None,
                    };
                    if let Some(status) = terminal_status {
                        if matches!(event, StreamEvent::Done) && continued && !usage_since_retry
                            && let Some(frame) = encoder.encode(&StreamEvent::Usage(tracker.total_usage()))
                        {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                        // Release the HTTP response before yielding the final frame,
                        // even if the downstream is backpressured or stops polling.
                        drop(events);
                        // A stream that reached its own Done is the reply the key
                        // served; credit it the whole logical reply. A reply
                        // replayed from a whole upstream answer already finished
                        // above and must not be counted a second time.
                        if status == "ok" && !tracker.is_finished() {
                            record_key_served(
                                &state,
                                &reask_candidate.provider,
                                reask_candidate.key_slot,
                                &tracker,
                            );
                            // A stream that reached its own Done answered the
                            // conversation, and the key that served it keeps it.
                            state.affinity.answered(
                                &decision.key,
                                &reask_candidate.who,
                                &reask_candidate.model,
                                decision.trace.turn,
                                tracker.total_usage().cache_read.unwrap_or(0),
                            );
                            if let Some(group_id) = rule_group.as_deref() {
                                crate::rules::rule_answered(
                                    group_id,
                                    &conversation,
                                    &creq,
                                    &tracker.total_usage(),
                                );
                            }
                        } else if status != "ok" {
                            // The reply had begun and broke off: not an
                            // answer, so the agent's retry goes by routing
                            // again, not back to the one that just failed it.
                            state
                                .affinity
                                .unanswered(&decision.key, &reask_candidate.who);
                        }
                        tracker.finish(status, &Usage::default());
                        if !suppress_start && let Some(frame) = encoder.encode(&event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                        return;
                    }
                    if !held && !item.usage_hidden && !suppress_start && let Some(frame) = encoder.encode(&event) {
                        usage_since_retry |= matches!(event, StreamEvent::Usage(_));
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
                let event = StreamEvent::Error {
                    message: error.message,
                    code: None,
                };
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

fn decode_upstream_response_usage(protocol: Protocol, value: &Value) -> Usage {
    match protocol {
        Protocol::Chat => chat::response_usage(value),
        Protocol::Anthropic => anthropic::response_usage(value),
        Protocol::Responses => responses::response_usage(value.get("response").unwrap_or(value)),
        Protocol::Gemini => gemini::response_usage(value),
    }
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
        return if protocol == Protocol::Gemini {
            vec![
                StreamEvent::Error {
                    message: "bad Gemini upstream stream payload".into(),
                    code: None,
                }
                .into(),
            ]
        } else {
            Vec::new()
        };
    };
    if protocol == Protocol::Gemini {
        decoders.gemini_reply.observe(&value);
    }
    let decoded = decode_upstream_chunk(protocol, &value, decoders);
    decoders.answered |= stream_refusal::has_answer(protocol, &value, false)
        || decoded.iter().any(stream_refusal::event_answers);
    let refused = stream_refusal::policy_error(&value)
        || (stream_refusal::filtered(protocol, &value) && !decoders.answered);
    let mut out = Vec::new();
    if protocol == Protocol::Gemini
        && (refused || !decoders.gemini_reply.is_empty())
        && let Some(usage) = decoders.gemini_pending_usage.take()
    {
        out.push(StreamEvent::Usage(usage).into());
    }
    if refused {
        out.push(StreamEvent::Usage(decode_upstream_response_usage(protocol, &value)).into());
        // Usage can follow Stop in the decoded frame. Account it before the
        // refusal; neither Stop nor Done may finish a refused logical turn.
        out.extend(
            decoded
                .into_iter()
                .filter(|event| matches!(event, StreamEvent::Usage(_)))
                .map(UpstreamEvent::from),
        );
        let mut error = UpstreamEvent::from(StreamEvent::Error {
            message: REFUSAL.into(),
            code: None,
        });
        error.failure = Some(StreamFailure {
            kind: FailureKind::Refusal,
            status: StatusCode::BAD_REQUEST,
            body: Value::Null,
        });
        out.push(error);
        return out;
    }
    for event in decoded {
        let empty = protocol == Protocol::Gemini && decoders.gemini_reply.is_empty();
        if empty && matches!(event, StreamEvent::Stop(_)) {
            // Do not let any client encoder finish a turn that will be reasked.
            continue;
        }
        if empty && matches!(event, StreamEvent::Done) {
            out.push(empty_reply_event());
            continue;
        }
        let hidden = empty && matches!(event, StreamEvent::Usage(_));
        if hidden && let StreamEvent::Usage(usage) = &event {
            decoders
                .gemini_pending_usage
                .get_or_insert_default()
                .merge(usage);
        }
        let failure = matches!(event, StreamEvent::Error { .. }).then(|| StreamFailure {
            kind: FailureKind::Upstream,
            status: crate::stream_status::error_status(&value),
            body: value.clone(),
        });
        out.push(UpstreamEvent {
            event,
            failure,
            usage_observed: false,
            usage_hidden: hidden,
            answer_observed: protocol == Protocol::Gemini && decoders.gemini_reply.has_answer(),
            loop_reasoning: false,
        });
    }
    out
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
            StreamEvent::Start { .. }
            | StreamEvent::Usage(_)
            | StreamEvent::ThinkingStart { .. } => false,
            StreamEvent::SearchUpdate { search } => search.is_answer(),
            StreamEvent::TextDelta { text } | StreamEvent::ThinkingDelta { text } => {
                !text.is_empty()
            }
            _ => true,
        };
        metadata_bytes += std::mem::size_of_val(&item);
        if let StreamEvent::Start { model } = &item.event {
            metadata_bytes += model.len();
        }
        if let StreamEvent::ThinkingStart { id } = &item.event {
            metadata_bytes = metadata_bytes.saturating_add(id.len());
        }
        if let StreamEvent::SearchUpdate { search } = &item.event {
            metadata_bytes = metadata_bytes.saturating_add(
                search
                    .gemini_grounding
                    .as_ref()
                    .map_or(0, |metadata| metadata.to_string().len()),
            );
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

fn empty_reply_event() -> UpstreamEvent {
    UpstreamEvent {
        event: StreamEvent::Error {
            message: EMPTY_REPLY.into(),
            code: None,
        },
        failure: Some(StreamFailure {
            kind: FailureKind::EmptyReply,
            status: StatusCode::BAD_GATEWAY,
            body: Value::Null,
        }),
        usage_observed: false,
        usage_hidden: false,
        answer_observed: false,
        loop_reasoning: false,
    }
}

fn stream_timeout_event(timeout: StreamTimeout) -> UpstreamEvent {
    UpstreamEvent {
        event: StreamEvent::Error {
            message: timeout.message().into(),
            code: None,
        },
        failure: Some(StreamFailure {
            kind: FailureKind::Upstream,
            status: StatusCode::GATEWAY_TIMEOUT,
            body: Value::Null,
        }),
        usage_observed: false,
        usage_hidden: false,
        answer_observed: false,
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
                    yield StreamEvent::Error {
                        message: "upstream stream read failed".into(),
                        code: None,
                    }
                    .into();
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
                        yield StreamEvent::Error {
                            message: error.to_string(),
                            code: None,
                        }
                        .into();
                        return;
                    }
                };
                for event in decode_upstream_data(protocol, &data, &mut decoders) {
                    clock.observe(&event.event);
                    match &event.event {
                        StreamEvent::Start { .. } => opening_held = true,
                        StreamEvent::Usage(_) | StreamEvent::ThinkingStart { .. } => {},
                        StreamEvent::TextDelta { text } | StreamEvent::ThinkingDelta { text } if text.is_empty() => {},
                        _ => hold = None,
                    }
                    chat_stopped |= protocol == Protocol::Chat && matches!(event.event, StreamEvent::Stop(_));
                    let trip = loop_guard.as_mut().and_then(|guard| guard.observe(&event.event));
                    yield event.into();
                    if let Some(trip) = trip {
                        yield UpstreamEvent {
                            event: StreamEvent::Error {
                                message: trip.message().into(),
                                code: None,
                            },
                            failure: Some(StreamFailure {
                                kind: FailureKind::Upstream,
                                status: StatusCode::BAD_GATEWAY,
                                body: Value::Null,
                            }),
                            usage_observed: false,
                            usage_hidden: false,
                            answer_observed: false,
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
                            event: StreamEvent::Error {
                                message: trip.message().into(),
                                code: None,
                            },
                            failure: Some(StreamFailure {
                                kind: FailureKind::Upstream,
                                status: StatusCode::BAD_GATEWAY,
                                body: Value::Null,
                            }),
                            usage_observed: false,
                            usage_hidden: false,
                            answer_observed: false,
                            loop_reasoning: trip.is_reasoning(),
                        }.into();
                        return;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                yield StreamEvent::Error {
                    message: error.to_string(),
                    code: None,
                }
                .into();
                return;
            }
        }
        // Some Chat-compatible endpoints close cleanly after finish_reason,
        // omitting [DONE]. Do not end at Stop: a later usage frame is valid.
        if protocol == Protocol::Gemini && decoders.gemini_reply.is_empty() {
            yield empty_reply_event().into();
        } else if chat_stopped
            || (protocol == Protocol::Gemini && decoders.gemini_reply.has_answer())
        {
            // Unlike Anthropic/Responses, Gemini may end an answered
            // reply by clean EOF without a separate finish marker.
            yield StreamEvent::Done.into();
        } else {
            yield StreamEvent::Error {
                message: "upstream stream ended before a completion marker".into(),
                code: None,
            }.into();
        }
    }
}

/// POST /v1/chat/completions — the OpenAI Chat dialect.
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let value: Value = match decode_client_body(&body) {
        Ok(value) => value,
        Err(e) => {
            let message = format!("invalid JSON: {e}");
            let wire = ClientWire::Chat {
                include_usage: false,
            };
            return err_response(
                StatusCode::BAD_REQUEST,
                wire.error_body(StatusCode::BAD_REQUEST, &message),
            );
        }
    };
    let include_usage = value
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    proxy_parsed(
        state,
        headers,
        value,
        body,
        ClientWire::Chat { include_usage },
    )
    .await
}

/// POST /v1/messages — the Anthropic Messages dialect.
async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy(state, headers, body, ClientWire::Anthropic).await
}

/// POST /v1/responses — the OpenAI Responses dialect Codex speaks.
async fn responses_api(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy(state, headers, body, ClientWire::Responses).await
}

/// POST /v1beta/models/{model}:generateContent and its streaming sibling.
/// The wildcard also accepts gateway model ids containing a slash.
async fn gemini_generate(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
    headers: HeaderMap,
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

    let mut value: Value = match decode_client_body(&body) {
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
    // The proxy no longer re-encodes the decoded body; serialize the edited
    // value once for conversation affinity.
    let bytes = Bytes::from(serde_json::to_vec(&value).expect("JSON values always serialize"));
    proxy_parsed(state, headers, value, bytes, wire).await
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
            contexts: Default::default(),
            fallback: vec![],
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
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
                            rules: Vec::new(),
                            classifier: String::new(),
                            effort: String::new(),
                            pick: String::new(),
                            auto: false,
                            weights: Vec::new(),
                            routing: RoutingPolicy::Order,
                            first_token_timeout_ms: 0,
                            affinity: Default::default(),
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
                    affinity: AffinityStore::default(),
                    lanes: lanes::Lanes::default(),
                    rpms: lanes::Rpms::default(),
                });
                let requested = if grouped {
                    "group/daily"
                } else {
                    "primary/first-model"
                };
                let body = Bytes::from(json!({"model":requested,"stream":stream,"messages":[{"role":"user","content":"private prompt"}]}).to_string());
                let task = tokio::spawn(proxy(
                    state,
                    HeaderMap::default(),
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

    /// A state with no provider for the model, so a decoded request is refused
    /// after routing and no upstream is contacted.
    fn decode_counting_state() -> Arc<AppState> {
        Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(Config::default(), None),
            http: upstream::http_client().unwrap(),
            requests: Arc::new(Mutex::new(crate::usage::RequestLog::default())),
            key_rotation: Default::default(),
            affinity: AffinityStore::default(),
            lanes: lanes::Lanes::default(),
            rpms: lanes::Rpms::default(),
        })
    }

    /// A Chat or Gemini handler reads a request option from the body and then
    /// hands the same value to the shared proxy. The body used to be parsed
    /// once for the option and again in the proxy, which on a multi-megabyte
    /// prompt is real CPU per request.
    #[tokio::test]
    async fn a_client_request_body_is_decoded_once() {
        let state = decode_counting_state();

        reset_client_body_decodes();
        let chat = chat_completions(
            State(state.clone()),
            HeaderMap::default(),
            Bytes::from_static(
                br#"{"model":"missing/model","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;
        assert_eq!(chat.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            client_body_decodes(),
            1,
            "a Chat request body was decoded more than once"
        );

        reset_client_body_decodes();
        let gemini = gemini_generate(
            State(state),
            Path("missing/model:generateContent".to_owned()),
            HeaderMap::default(),
            Bytes::from_static(br#"{"contents":[{"parts":[{"text":"hi"}]}]}"#),
        )
        .await;
        assert_eq!(gemini.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            client_body_decodes(),
            1,
            "a Gemini request body was decoded more than once"
        );
    }

    /// Decoding a body in the handler must keep each protocol's malformed-JSON
    /// shape, not fall back to a shared Chat-shaped refusal.
    #[tokio::test]
    async fn malformed_client_bodies_keep_their_protocol_error_shape() {
        let state = decode_counting_state();

        let chat = chat_completions(
            State(state.clone()),
            HeaderMap::default(),
            Bytes::from_static(b"{"),
        )
        .await;
        assert_eq!(chat.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(chat.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value["error"]["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("invalid JSON:"))
        );

        let gemini = gemini_generate(
            State(state),
            Path("mock/mock-model:generateContent".to_owned()),
            HeaderMap::default(),
            Bytes::from_static(b"{"),
        )
        .await;
        assert_eq!(gemini.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(gemini.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["status"], "INVALID_ARGUMENT");
    }
}
