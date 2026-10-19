//! A reply asked for past what the context window leaves is asked again
//! for what the window leaves, rather than failed as too long.
//!
//! DeepSeek counts the reply asked for (`max_tokens`) in its 1M window
//! with the prompt, so a turn asking the model's whole 384K on a ~620K+
//! conversation was refused there and read as "too long for its model",
//! where every other member of its group answered. An endpoint speaking
//! Anthropic's API words the same refusal its own way.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use magpie_gateway::{
    AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig, RoutingPolicy,
    build_router, usage::RequestLog,
};
use serde_json::{Value, json};

/// A fake vendor that counts the reply asked for in its window with the
/// prompt, as DeepSeek does, and refuses in the words it does when the two
/// together pass it. It records every `max_tokens` it was asked for.
#[derive(Clone)]
struct Window {
    window: u64,
    prompt: u64,
    asked: Arc<Mutex<Vec<i64>>>,
}

impl Window {
    fn new(window: u64, prompt: u64) -> Self {
        Self {
            window,
            prompt,
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn took(&self) -> Vec<i64> {
        std::mem::take(&mut self.asked.lock().unwrap())
    }
}

async fn chat_window(State(window): State<Window>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).expect("upstream body is JSON");
    let asked = request.get("max_tokens").and_then(Value::as_i64);
    window.asked.lock().unwrap().push(asked.unwrap_or(-1));
    let Some(asked) = asked else {
        return refusal(window.window, window.prompt, 0, &request);
    };
    if window.prompt + asked as u64 > window.window {
        return refusal(window.window, window.prompt, asked, &request);
    }
    streamed_answer()
}

/// The vendor's own wording: "This model's maximum context length is N
/// tokens. However, you requested M tokens (P in the messages, C in the
/// completion)."
fn refusal(window: u64, prompt: u64, asked: i64, _request: &Value) -> Response {
    let body = json!({
        "error": {
            "message": format!(
                "This model's maximum context length is {window} tokens. However, you requested {} tokens ({prompt} in the messages, {asked} in the completion). Please reduce the length of the messages or completion.",
                prompt as i64 + asked
            ),
            "type": "invalid_request_error",
            "param": null,
            "code": "invalid_request_error",
        }
    });
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

/// A streamed chat answer, so a client reading SSE isn't handed JSON.
fn streamed_answer() -> Response {
    let body = "data: {\"id\":\"m\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n\
                data: {\"id\":\"m\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: [DONE]\n\n";
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// The Anthropic wording of the same refusal, from an endpoint speaking
/// that API: "input length and `max_tokens` exceed context limit: P + C > W".
async fn anthropic_window(State(window): State<Window>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).expect("upstream body is JSON");
    let asked = request
        .get("max_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    window.asked.lock().unwrap().push(asked);
    if window.prompt + asked as u64 > window.window {
        let body = json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": format!(
                    "input length and `max_tokens` exceed context limit: {} + {asked} > {}, decrease input length or `max_tokens` and try again",
                    window.prompt, window.window
                ),
            }
        });
        return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
    }
    axum::Json(json!({
        "id": "msg", "type": "message", "role": "assistant", "model": "mock-model",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .into_response()
}

async fn serve(protocol: Protocol, window: Window) -> String {
    let app = if protocol == Protocol::Anthropic {
        Router::new()
            .route("/v1/messages", post(anthropic_window))
            .with_state(window)
    } else {
        Router::new()
            .route("/v1/chat/completions", post(chat_window))
            .with_state(window)
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A gateway over the fake vendor, named `deepseek`, with `mock-model` on
/// it and no other route.
async fn gateway(protocol: Protocol, upstream: String) -> String {
    let base_url = match protocol {
        Protocol::Anthropic => upstream.clone(),
        _ => format!("{upstream}/v1"),
    };
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path: None,
        default_provider: "deepseek".into(),
        provider: vec![ProviderConfig {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol,
            base_url,
            api_key: "fixture-key".into(),
            api_keys: Vec::new(),
            models: vec!["mock-model".into()],
            contexts: Default::default(),
            fallback: Vec::new(),
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
        }],
        group: Vec::new(),
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(RequestLog::default())),
        key_rotation: KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn send_request(gateway: &str, path: &str, body: &Value) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{gateway}{path}"))
        .bearer_auth("magpie-lite")
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

/// A client asking DeepSeek for its whole 384K reply on a conversation of
/// 706,784 tokens is refused by DeepSeek as too long, since its 1M window
/// counts the reply asked for. It is asked again for the reply the window
/// has left, and answers. Room too small to be worth a reply still reaches
/// the agent as the overflow, asked once.
#[tokio::test]
async fn a_reply_past_the_window_is_asked_again_for_what_it_leaves() {
    let window = Window::new(1_000_000, 706_784);
    let upstream = serve(Protocol::Chat, window.clone()).await;
    let gw = gateway(Protocol::Chat, upstream).await;

    let messages = json!([{"role": "user", "content": "hi"}]);
    for (name, path, body, want) in [
        (
            "OpenAI's newer field",
            "/v1/chat/completions",
            json!({"model":"deepseek/mock-model","stream":true,"max_completion_tokens":393216,"messages":messages}),
            vec![393216, 293216],
        ),
        (
            "OpenAI's older field",
            "/v1/chat/completions",
            json!({"model":"deepseek/mock-model","stream":true,"max_tokens":393216,"messages":messages}),
            vec![393216, 293216],
        ),
        (
            "a Responses client, translated to the Chat API",
            "/v1/responses",
            json!({"model":"deepseek/mock-model","stream":true,"max_output_tokens":393216,"input":"hi"}),
            vec![393216, 293216],
        ),
        (
            "one that fits is asked once",
            "/v1/chat/completions",
            json!({"model":"deepseek/mock-model","stream":true,"max_tokens":32000,"messages":messages}),
            vec![32000],
        ),
    ] {
        let (status, reply) = send_request(&gw, path, &body).await;
        assert_eq!(status, StatusCode::OK, "{name}: {reply}");
        assert_eq!(window.took(), want, "{name}");
    }
}

/// The prompt alone leaves too little room for a reply worth asking for:
/// the conversation needs compacting, and the agent is told it overflowed,
/// asked once.
#[tokio::test]
async fn too_little_room_reaches_the_agent_as_the_overflow() {
    let window = Window::new(1_000_000, 1_000_000 - 4_000);
    let upstream = serve(Protocol::Chat, window.clone()).await;
    let gw = gateway(Protocol::Chat, upstream).await;

    let (status, reply) = send_request(
        &gw,
        "/v1/chat/completions",
        &json!({"model":"deepseek/mock-model","stream":true,"max_tokens":393216,
            "messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert_eq!(reply["error"]["code"], "context_length_exceeded", "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("maximum context length")),
        "{reply}"
    );
    assert_eq!(window.took(), vec![393216], "asked once only");
}

/// A refusal that says a window, but nothing this rule reads, is not
/// retried: the client's answer is the vendor's own 400.
#[tokio::test]
async fn an_unreadable_window_refusal_is_not_retried() {
    let window = Window::new(1_000_000, 0);
    let upstream = serve(Protocol::Chat, window.clone()).await;
    let gw = gateway(Protocol::Chat, upstream).await;

    // A prompt of 0 leaves the whole window, but `max_tokens` alone can't
    // pass it, so the fake answers; the point of this case is only that a
    // body naming no reply limit is given one rather than refused locally.
    let (status, reply) = send_request(
        &gw,
        "/v1/chat/completions",
        &json!({"model":"deepseek/mock-model","stream":true,
            "messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert_eq!(window.took(), vec![-1]);
}

/// Claude Code and Codex reach an endpoint speaking Anthropic's API with a
/// reply limit of their own (Claude Code's 64000) or the model's whole
/// (Codex names none, so the gateway asks Anthropic's least). Each is
/// asked again for what the window leaves.
#[tokio::test]
async fn an_anthropic_wording_is_read_too() {
    let window = Window::new(1_000_000, 970_000);
    let upstream = serve(Protocol::Anthropic, window.clone()).await;
    let gw = gateway(Protocol::Anthropic, upstream).await;

    let (status, reply) = send_request(
        &gw,
        "/v1/messages",
        &json!({"model":"deepseek/mock-model","max_tokens":64000,
            "messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(window.took(), vec![64000, 30000]);
}
