//! Image recognition: a text-only model is answered from descriptions.
//!
//! Chat-only loopback: a `blind` provider declares no vision, a `seeing`
//! provider describes. With `vision = seeing/mock-model`, a current-turn
//! user image is described through lite itself (the magpie-vision/1 agent)
//! and the blind upstream gets the description with no image bytes. Without
//! a describer, a missing one, or a failing one, the request falls back to
//! the existing refuse/omit behavior; a failed describer rests so the next
//! request does not stall on another call. HTTP `PUT /config/vision` names
//! the describer with revision and idempotency, like the other switches.
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderCapabilities,
    ProviderConfig, RoutingPolicy, build_router, usage::RequestLog,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

const TOKEN: &str = "PRIVATE_VISION_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_VISION_PROVIDER_KEY";
const PNG: &str = "c2NyZWVuc2hvdA==";
const ANSWER: &str = "text only answered from a description";
const DESCRIPTION: &str = "a red square";

#[derive(Clone)]
struct Mock {
    seen: Arc<Mutex<Vec<Value>>>,
    seen_headers: Arc<Mutex<Vec<String>>>,
    fail_describe: bool,
}

fn chat_answer(text: &str) -> Value {
    json!({"id":"v","object":"chat.completion","model":"mock-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}})
}

async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    mock.seen_headers.lock().unwrap().push(agent.clone());
    mock.seen.lock().unwrap().push(body.clone());
    let text = body
        .pointer("/messages/0/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_describe = text.contains("cannot see them");
    if is_describe && mock.fail_describe {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":{"message":"describer is down","type":"server_error"}})),
        )
            .into_response();
    }
    if is_describe {
        // The describer is asked through lite itself: the Chat body carries
        // the vision brief and the image. The magpie-vision/1 User-Agent is
        // the gateway-internal routing marker and is not forwarded upstream.
        assert!(body.to_string().contains("cannot see them"), "{body}");
        return Json(chat_answer(DESCRIPTION)).into_response();
    }
    Json(chat_answer(ANSWER)).into_response()
}

struct Server {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { base, task }
}

fn provider(id: &str, base: &str, vision: Option<bool>) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        capabilities: ProviderCapabilities {
            vision,
            ..Default::default()
        },
        base_url: format!("{base}/v1"),
        api_key: KEY.into(),
        api_keys: Vec::new(),
        models: vec!["mock-model".into()],
        contexts: Default::default(),
        outputs: Default::default(),
        fallback: vec![],
        key_cooldown_ms: None,
        routing: RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        pin_upstream: false,
        key_limits: Vec::new(),
    }
}

struct Fixture {
    gateway: Server,
    _blind_upstream: Server,
    _seeing_upstream: Server,
    _home: tempfile::TempDir,
    blind: Arc<Mutex<Vec<Value>>>,
    seeing: Arc<Mutex<Vec<Value>>>,
}

async fn fixture(vision: Option<&str>, fail_describe: bool) -> Fixture {
    let blind_seen = Arc::new(Mutex::new(Vec::new()));
    let seeing_seen = Arc::new(Mutex::new(Vec::new()));
    let blind_upstream = serve(Router::new().route(
        "/v1/chat/completions",
        post(upstream).with_state(Mock {
            seen: blind_seen.clone(),
            seen_headers: Arc::new(Mutex::new(Vec::new())),
            fail_describe: false,
        }),
    ))
    .await;
    let seeing_upstream = serve(Router::new().route(
        "/v1/chat/completions",
        post(upstream).with_state(Mock {
            seen: seeing_seen.clone(),
            seen_headers: Arc::new(Mutex::new(Vec::new())),
            fail_describe,
        }),
    ))
    .await;
    let home = tempfile::tempdir().unwrap();
    let history = home.path().join("history.jsonl");
    let config = Config {
        port: 0,
        token: TOKEN.into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path: Some(history.clone()),
        default_provider: "blind".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        vision: vision.map(str::to_owned),
        provider: vec![
            provider("blind", &blind_upstream.base, Some(false)),
            provider("seeing", &seeing_upstream.base, None),
        ],
        group: Vec::new(),
    };
    let log = Arc::new(Mutex::new(RequestLog::open(&history).unwrap()));
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: log,
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let gateway = serve(build_router(state)).await;
    Fixture {
        gateway,
        _blind_upstream: blind_upstream,
        _seeing_upstream: seeing_upstream,
        _home: home,
        blind: blind_seen,
        seeing: seeing_seen,
    }
}

fn current_image(model: &str) -> Value {
    json!({"model":model,"stream":false,"messages":[
        {"role":"user","content":[
            {"type":"text","text":"read this"},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}}
        ]}
    ]})
}

async fn ask(url: &str, body: &Value) -> (StatusCode, Value) {
    let reply = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(TOKEN)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = reply.status();
    let value: Value = reply.json().await.unwrap_or(Value::Null);
    (status, value)
}

fn reply_text(value: &Value) -> String {
    value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn vision_hit_describes_and_hides_image_bytes() {
    let fx = fixture(Some("seeing/mock-model"), false).await;
    let (status, value) = ask(&fx.gateway.base, &current_image("blind/mock-model")).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(reply_text(&value), ANSWER);
    // One describe call through lite itself, carrying the image.
    let seeing = fx.seeing.lock().unwrap();
    assert_eq!(seeing.len(), 1, "{seeing:?}");
    assert!(seeing[0].to_string().contains(PNG), "{seeing:?}");
    assert!(
        seeing[0].to_string().contains("cannot see them"),
        "{seeing:?}"
    );
    // The text-only upstream gets the description, never the bytes.
    let blind = fx.blind.lock().unwrap();
    assert_eq!(blind.len(), 1, "{blind:?}");
    let sent = blind[0].to_string();
    assert!(sent.contains("[Image description: a red square]"), "{sent}");
    assert!(
        !sent.contains(PNG),
        "image bytes reached a text-only model: {sent}"
    );
}

#[tokio::test]
async fn vision_unset_falls_back_to_refusal() {
    let fx = fixture(None, false).await;
    let (status, value) = ask(&fx.gateway.base, &current_image("blind/mock-model")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert!(fx.blind.lock().unwrap().is_empty());
    assert!(fx.seeing.lock().unwrap().is_empty());
}

#[tokio::test]
async fn vision_missing_names_the_describer() {
    let fx = fixture(Some("gone/mock-model"), false).await;
    let (status, value) = ask(&fx.gateway.base, &current_image("blind/mock-model")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(message.contains("missing"), "{message}");
    assert!(message.contains("gone/mock-model"), "{message}");
    assert!(fx.blind.lock().unwrap().is_empty());
}

#[tokio::test]
async fn vision_failure_falls_back_and_rests() {
    let fx = fixture(Some("seeing/mock-model"), true).await;
    let body = current_image("blind/mock-model");
    let (first_status, _) = ask(&fx.gateway.base, &body).await;
    assert_eq!(first_status, StatusCode::BAD_REQUEST);
    assert_eq!(fx.seeing.lock().unwrap().len(), 1);
    // The failed describer rests: a second request refuses at once.
    let (second_status, _) = ask(&fx.gateway.base, &body).await;
    assert_eq!(second_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        fx.seeing.lock().unwrap().len(),
        1,
        "rested describer asked again"
    );
    assert!(fx.blind.lock().unwrap().is_empty());
}

#[tokio::test]
async fn vision_http_names_with_revision_and_idempotency() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let upstream = serve(Router::new().route(
        "/v1/chat/completions",
        post(upstream).with_state(Mock {
            seen: Arc::new(Mutex::new(Vec::new())),
            seen_headers: Arc::new(Mutex::new(Vec::new())),
            fail_describe: false,
        }),
    ))
    .await;
    let config = Config {
        port: 0,
        token: TOKEN.into(),
        default_provider: "mock".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        vision: None,
        history_path: None,
        provider: vec![provider("mock", &upstream.base, None)],
        group: Vec::new(),
        ..Config::default()
    };
    config.save_with_revision(&path, 1).unwrap();
    let history = directory.path().join("history.jsonl");
    let store = ConfigStore::new(Config::load(&path).unwrap(), Some(path.clone()));
    let log = Arc::new(Mutex::new(RequestLog::open(&history).unwrap()));
    let state = Arc::new(AppState {
        agents: None,
        config: store,
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: log,
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let gateway = serve(build_router(state)).await;
    let client = reqwest::Client::new();
    async fn snapshot(client: &reqwest::Client, url: &str) -> Value {
        client
            .get(format!("{url}/config"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    let first = snapshot(&client, &gateway.base).await;
    let revision = first["revision"].as_u64().unwrap();
    // A bare name is not provider/model: refused, and the file is unchanged.
    let bare = client
        .put(format!("{}/config/vision", gateway.base))
        .bearer_auth(TOKEN)
        .header("If-Match", revision.to_string())
        .json(&json!({"vision": "bare"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::BAD_REQUEST);
    // Name the describer (mock has unknown vision, so it resolves; a
    // text-only provider is refused by check_vision unit paths).
    let reply = client
        .put(format!("{}/config/vision", gateway.base))
        .bearer_auth(TOKEN)
        .header("If-Match", revision.to_string())
        .header("Idempotency-Key", "vision-1")
        .json(&json!({"vision": "mock/mock-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let after: Value = reply.json().await.unwrap();
    assert_eq!(after["vision"], "mock/mock-model");
    // A stale revision is refused.
    let stale = client
        .put(format!("{}/config/vision", gateway.base))
        .bearer_auth(TOKEN)
        .header("If-Match", revision.to_string())
        .json(&json!({"vision": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
}
