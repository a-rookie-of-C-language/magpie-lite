//! The gateway itself: a local HTTP server that speaks OpenAI Chat
//! Completions and Anthropic Messages to agents, translates each request
//! through the IR, and forwards it to whichever provider serves the model.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use magpie_core::ir::CompletionRequest;
use magpie_core::stream::{SseParser, StreamEvent};
use magpie_core::{anthropic, chat};
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
        .route("/v1/models", get(list_models))
        .route("/health", get(health))
        .with_state(state)
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

/// Decode the body, resolve the route, and build the translated upstream
/// body. Shared by both wire endpoints.
fn prepare(
    state: &AppState,
    body: &Value,
    decode: fn(&Value) -> magpie_core::Result<CompletionRequest>,
) -> Result<(CompletionRequest, Arc<ProviderConfig>, String, Value), (StatusCode, Value)> {
    let creq = decode(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": e.to_string()}}),
        )
    })?;
    let Some((provider, upstream_model)) = state.config.resolve(&creq.model) else {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": {"message": format!("no provider serves model `{}`; add one with `magpie-lite provider add`", creq.model)}}),
        ));
    };
    let provider = Arc::new(provider.clone());
    let upstream_body = upstream::build_request(&provider, &creq, &upstream_model);
    Ok((creq, provider, upstream_model, upstream_body))
}

async fn send_or_error(
    state: &AppState,
    provider: &ProviderConfig,
    body: &Value,
    error_json: fn(reqwest::StatusCode, &str) -> Value,
) -> Result<reqwest::Response, Response> {
    match upstream::send(&state.http, provider, body).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            let msg = format!("cannot reach {}: {e}", provider.id);
            Err(err_response(
                StatusCode::BAD_GATEWAY,
                error_json(StatusCode::BAD_GATEWAY, &msg),
            ))
        }
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

/// POST /v1/chat/completions — the OpenAI-dialect endpoint.
async fn chat_completions(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                upstream::chat_error(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")),
            );
        }
    };
    let include_usage = v
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (creq, provider, _upstream_model, upstream_body) =
        match prepare(&state, &v, chat::decode_request) {
            Ok(x) => x,
            Err((status, body)) => return (status, Json(body)).into_response(),
        };

    let resp = match send_or_error(&state, &provider, &upstream_body, upstream::chat_error).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    if !resp.status().is_success() {
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let detail = upstream::error_detail(resp).await;
        return err_response(status, upstream::chat_error(status, &detail));
    }

    if !creq.stream {
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return err_response(
                    StatusCode::BAD_GATEWAY,
                    upstream::chat_error(
                        StatusCode::BAD_GATEWAY,
                        &format!("bad upstream body: {e}"),
                    ),
                );
            }
        };
        let ir = match provider.protocol {
            Protocol::Chat => chat::decode_response(&body),
            Protocol::Anthropic => anthropic::decode_response(&body),
        }
        .unwrap_or_else(|e| {
            tracing::warn!("decoding upstream reply: {e}");
            magpie_core::ir::CompletionResponse::default()
        });
        return ok_json(chat::encode_response(&ir));
    }

    // Streaming: translate each upstream SSE frame into chat chunks.
    let client_model = creq.model.clone();
    let protocol = provider.protocol;
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut parser = SseParser::new();
        let mut dec = anthropic::AnthropicStreamDecoder::new();
        let mut enc = chat::ChatChunkEncoder::new(&client_model, include_usage);
        if let Some(frame) = enc.encode(&StreamEvent::Start { model: client_model.clone() }) {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
        }
        let mut byte_stream = byte_stream;
        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let frame = enc.encode(&StreamEvent::Error { message: format!("upstream read: {e}") });
                    if let Some(frame) = frame {
                        yield Ok(Bytes::from(frame));
                    }
                    break;
                }
            };
            parser.extend(&chunk);
            while let Some(data) = parser.next_data() {
                if data == "[DONE]" {
                    continue; // the encoder emits its own [DONE] at the end
                }
                let v: Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let events = match protocol {
                    Protocol::Chat => chat::decode_chunk(&v),
                    Protocol::Anthropic => dec.decode(&v),
                };
                for ev in events {
                    if let Some(frame) = enc.encode(&ev) {
                        yield Ok(Bytes::from(frame));
                    }
                }
            }
        }
        if let Some(data) = parser.flush() {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                let events = match protocol {
                    Protocol::Chat => chat::decode_chunk(&v),
                    Protocol::Anthropic => dec.decode(&v),
                };
                for ev in events {
                    if let Some(frame) = enc.encode(&ev) {
                        yield Ok(Bytes::from(frame));
                    }
                }
            }
        }
        if let Some(frame) = enc.encode(&StreamEvent::Done) {
            yield Ok(Bytes::from(frame));
        }
    };
    sse_response(Body::from_stream(out))
}

/// POST /v1/messages — the Anthropic-dialect endpoint.
async fn anthropic_messages(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                upstream::anthropic_error(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")),
            );
        }
    };
    let (creq, provider, _upstream_model, upstream_body) =
        match prepare(&state, &v, anthropic::decode_request) {
            Ok(x) => x,
            Err((status, body)) => {
                let detail = body
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("bad request")
                    .to_owned();
                return err_response(status, upstream::anthropic_error(status, &detail));
            }
        };

    let resp =
        match send_or_error(&state, &provider, &upstream_body, upstream::anthropic_error).await {
            Ok(r) => r,
            Err(e) => return e,
        };
    if !resp.status().is_success() {
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let detail = upstream::error_detail(resp).await;
        return err_response(status, upstream::anthropic_error(status, &detail));
    }

    if !creq.stream {
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return err_response(
                    StatusCode::BAD_GATEWAY,
                    upstream::anthropic_error(
                        StatusCode::BAD_GATEWAY,
                        &format!("bad upstream body: {e}"),
                    ),
                );
            }
        };
        let ir = match provider.protocol {
            Protocol::Chat => chat::decode_response(&body),
            Protocol::Anthropic => anthropic::decode_response(&body),
        }
        .unwrap_or_else(|e| {
            tracing::warn!("decoding upstream reply: {e}");
            magpie_core::ir::CompletionResponse::default()
        });
        return ok_json(anthropic::encode_response(&ir));
    }

    // Streaming: translate each upstream SSE frame into Anthropic events.
    let client_model = creq.model.clone();
    let protocol = provider.protocol;
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut parser = SseParser::new();
        let mut dec = anthropic::AnthropicStreamDecoder::new();
        let mut enc = anthropic::AnthropicStreamEncoder::new(&client_model);
        if let Some(frame) = enc.encode(&StreamEvent::Start { model: client_model.clone() }) {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
        }
        let mut byte_stream = byte_stream;
        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let frame = enc.encode(&StreamEvent::Error { message: format!("upstream read: {e}") });
                    if let Some(frame) = frame {
                        yield Ok(Bytes::from(frame));
                    }
                    break;
                }
            };
            parser.extend(&chunk);
            while let Some(data) = parser.next_data() {
                let v: Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let events = match protocol {
                    Protocol::Chat => chat::decode_chunk(&v),
                    Protocol::Anthropic => dec.decode(&v),
                };
                for ev in events {
                    if let Some(frame) = enc.encode(&ev) {
                        yield Ok(Bytes::from(frame));
                    }
                }
            }
        }
        if let Some(data) = parser.flush() {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                let events = match protocol {
                    Protocol::Chat => chat::decode_chunk(&v),
                    Protocol::Anthropic => dec.decode(&v),
                };
                for ev in events {
                    if let Some(frame) = enc.encode(&ev) {
                        yield Ok(Bytes::from(frame));
                    }
                }
            }
        }
        if let Some(frame) = enc.encode(&StreamEvent::Done) {
            yield Ok(Bytes::from(frame));
        }
    };
    sse_response(Body::from_stream(out))
}
