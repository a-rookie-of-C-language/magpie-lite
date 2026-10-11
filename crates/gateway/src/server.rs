//! The gateway itself: a local HTTP server that speaks OpenAI Chat
//! Completions, OpenAI Responses, Anthropic Messages, and Google Gemini to
//! agents, translates each request through the IR, and forwards it to
//! whichever provider serves the model.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use magpie_core::ir::CompletionRequest;
use magpie_core::stream::{SseParser, StreamEvent};
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

use crate::config::ProviderConfig;
use crate::config::{Config, Protocol};
use crate::upstream;

/// Shared handler state.
pub struct AppState {
    /// The loaded configuration.
    pub config: Config,
    /// The pooled upstream client.
    pub http: reqwest::Client,
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
        // Every model route above proves it knows the configured local
        // token before a request can reach a provider. The health probe
        // below is added after the layer and stays public.
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .route("/health", get(health))
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
        .is_some_and(|present| constant_time_eq(present, &state.config.token));
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
    let port = port.unwrap_or(config.port);
    let http = upstream::http_client()?;
    let state = Arc::new(AppState { config, http });
    let app = build_router(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("magpie-lite gateway listening on http://{addr}");
    println!("magpie-lite gateway listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let data: Vec<Value> = state
        .config
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

/// A resolved place to send one attempt: a provider and its spelling of
/// the model. The same IR is re-encoded for every candidate.
#[derive(Debug, Clone)]
struct UpstreamCandidate {
    provider: Arc<ProviderConfig>,
    model: String,
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
    state: &AppState,
    body: &Value,
    wire: ClientWire,
) -> Result<(CompletionRequest, UpstreamCandidate), (StatusCode, Value)> {
    let creq = wire.decode_request(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            wire.error_body(StatusCode::BAD_REQUEST, &e.to_string()),
        )
    })?;
    let Some((provider, upstream_model)) = state.config.resolve(&creq.model) else {
        let message = format!(
            "no provider serves model `{}`; add one with `magpie-lite provider add`",
            creq.model
        );
        return Err((
            StatusCode::NOT_FOUND,
            wire.error_body(StatusCode::NOT_FOUND, &message),
        ));
    };
    Ok((
        creq,
        UpstreamCandidate {
            provider: Arc::new(provider.clone()),
            model: upstream_model,
        },
    ))
}

/// The initial provider followed by its fallback references, each resolved
/// with the ordinary model rules. Chains are deliberately one level deep:
/// `fallback` says where this model goes next, not how to build a graph.
fn upstream_candidates(config: &Config, initial: UpstreamCandidate) -> Vec<UpstreamCandidate> {
    let mut out = vec![initial.clone()];
    let mut seen = HashSet::from([initial.provider.id.clone()]);

    for reference in &initial.provider.fallback {
        let Some((provider, model)) = config.resolve(reference) else {
            tracing::warn!("fallback `{reference}` does not resolve; skipping");
            continue;
        };
        if !seen.insert(provider.id.clone()) {
            continue;
        }
        out.push(UpstreamCandidate {
            provider: Arc::new(provider.clone()),
            model,
        });
    }
    out
}

/// Whether an upstream status is worth spending another provider on.
fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        402 | 403 | 408 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

/// Send to the initial provider, then its fallbacks. All routing decisions
/// happen before a successful response body is streamed, so an agent never
/// receives a half-finished answer from an abandoned provider.
async fn send_with_fallback(
    state: &AppState,
    creq: &CompletionRequest,
    initial: UpstreamCandidate,
    wire: ClientWire,
) -> Result<(reqwest::Response, Protocol), Box<Response>> {
    let candidates = upstream_candidates(&state.config, initial);
    let last = candidates.len().saturating_sub(1);

    for (attempt, candidate) in candidates.into_iter().enumerate() {
        let body = upstream::build_request(&candidate.provider, creq, &candidate.model);
        tracing::debug!(
            provider = %candidate.provider.id,
            model = %candidate.model,
            attempt,
            "routing upstream"
        );

        let resp = match upstream::send(
            &state.http,
            &candidate.provider,
            &body,
            &candidate.model,
            creq.stream,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                let message = format!("cannot reach {}: {e}", candidate.provider.id);
                tracing::warn!("{message}");
                if attempt == last {
                    return Err(Box::new(err_response(
                        StatusCode::BAD_GATEWAY,
                        wire.error_body(StatusCode::BAD_GATEWAY, &message),
                    )));
                }
                continue;
            }
        };

        let status = resp.status();
        if status.is_success() {
            return Ok((resp, candidate.provider.protocol));
        }
        let detail = upstream::error_detail(resp).await;
        tracing::warn!("upstream {}: {detail}", candidate.provider.id);
        if attempt != last && retryable_status(status) {
            continue;
        }
        let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return Err(Box::new(err_response(
            status,
            wire.error_body(status, &detail),
        )));
    }

    Err(Box::new(err_response(
        StatusCode::BAD_GATEWAY,
        wire.error_body(StatusCode::BAD_GATEWAY, "no upstream route"),
    )))
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
        Protocol::Chat => chat::decode_chunk(data),
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
    let (creq, initial) = match prepare(&state, &body, wire) {
        Ok(x) => x,
        Err((status, body)) => return err_response(status, body),
    };

    let (resp, protocol) = match send_with_fallback(&state, &creq, initial, wire).await {
        Ok(x) => x,
        Err(response) => return *response,
    };

    if !creq.stream {
        let upstream_body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                let message = format!("bad upstream body: {e}");
                return err_response(
                    StatusCode::BAD_GATEWAY,
                    wire.error_body(StatusCode::BAD_GATEWAY, &message),
                );
            }
        };
        let ir = decode_upstream_response(protocol, &upstream_body).unwrap_or_else(|e| {
            tracing::warn!("decoding upstream reply: {e}");
            magpie_core::ir::CompletionResponse::default()
        });
        return ok_json(wire.encode_response(&ir));
    }

    let client_model = creq.model.clone();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut parser = SseParser::new();
        let mut decoders = UpstreamDecoders::new();
        let mut encoder = ClientStreamEncoder::new(wire, &client_model);
        let mut byte_stream = byte_stream;

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let event = StreamEvent::Error {
                        message: format!("upstream read: {e}"),
                    };
                    if let Some(frame) = encoder.encode(&event) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                    }
                    break;
                }
            };
            parser.extend(&chunk);

            while let Some(data) = parser.next_data() {
                if data == "[DONE]" {
                    continue;
                }
                let value: Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for event in decode_upstream_chunk(protocol, &value, &mut decoders) {
                    if let Some(frame) = encoder.encode(&event) {
                        yield Ok(Bytes::from(frame));
                    }
                }
            }
        }

        if let Some(data) = parser.flush()
            && let Ok(value) = serde_json::from_str::<Value>(&data)
        {
            for event in decode_upstream_chunk(protocol, &value, &mut decoders) {
                if let Some(frame) = encoder.encode(&event) {
                    yield Ok(Bytes::from(frame));
                }
            }
        }
        if let Some(frame) = encoder.encode(&StreamEvent::Done) {
            yield Ok(Bytes::from(frame));
        }
    };
    sse_response(Body::from_stream(out))
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
