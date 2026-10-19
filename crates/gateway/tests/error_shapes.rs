//! The client sees the reference gateway's error types and statuses: a
//! 413 is `request_too_large` in every shape (pi compacts on it), and an
//! overflow the vendor worded is a 400 `context_length_exceeded`, so an
//! agent compacts instead of stopping (#2551's sibling, `97f264c7`).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use magpie_gateway::{
    AppState, Config, ConfigStore, Protocol, ProviderConfig, RoutingPolicy, build_router,
};
use reqwest::Client;
use serde_json::{Value, json};

#[derive(Clone)]
struct Scripted {
    reply: Arc<Mutex<Option<(u16, Value)>>>,
}

async fn chat(State(scripted): State<Scripted>) -> axum::response::Response {
    use axum::response::IntoResponse;

    let (status, body) = scripted
        .reply
        .lock()
        .unwrap()
        .clone()
        .expect("a scripted reply");
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(body),
    )
        .into_response()
}

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

struct Gateway {
    url: String,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        for task in &self._tasks {
            task.abort();
        }
    }
}

async fn gateway(status: u16, body: Value) -> Gateway {
    let scripted = Scripted {
        reply: Arc::new(Mutex::new(Some((status, body)))),
    };
    let (upstream, upstream_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(scripted),
    )
    .await;
    let config = Config {
        token: "magpie-lite".into(),
        default_provider: "mock".into(),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Chat,
            base_url: format!("{upstream}/v1"),
            api_key: "k".into(),
            api_keys: Vec::new(),
            models: vec!["m".into()],
            contexts: Default::default(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            key_generation: 0,
        }],
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    Gateway {
        url,
        _tasks: vec![upstream_task, gateway_task],
    }
}

async fn ask(gw: &Gateway, path: &str, body: Value) -> reqwest::Response {
    Client::new()
        .post(format!("{}{path}", gw.url))
        .bearer_auth("magpie-lite")
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_body_byte_limit_is_request_too_large_in_every_shape() {
    let gw = gateway(
        413,
        json!({"error": {"message": "413 Request Entity Too Large"}}),
    )
    .await;
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "mock/m", "messages": [{"role": "user", "content": "hi"}]}),
        ),
        ("/v1/responses", json!({"model": "mock/m", "input": "hi"})),
        (
            "/v1/messages",
            json!({"model": "mock/m", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
        ),
    ] {
        let response = ask(&gw, path, body).await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "{path}"
        );
        let body: Value = response.json().await.unwrap();
        let kind = body
            .pointer("/error/type")
            .or_else(|| body.pointer("/error/status"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            kind == "request_too_large" || kind == "INVALID_ARGUMENT",
            "{path}: {body}"
        );
        let message = body.pointer("/error/message").unwrap().as_str().unwrap();
        assert!(
            message.contains("request-body byte limit"),
            "{path}: {body}"
        );
    }
}

#[tokio::test]
async fn a_worded_overflow_is_a_400_context_length_exceeded() {
    // A vendor's 422 over a conversation that no longer fits reaches an
    // agent the way its own API says it, so it compacts and retries.
    let gw = gateway(
        422,
        json!({"error": {"message": "This model's maximum context length is 8192 tokens"}}),
    )
    .await;
    let response = ask(
        &gw,
        "/v1/chat/completions",
        json!({"model": "mock/m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "context_length_exceeded");
}

#[tokio::test]
async fn an_anthropic_overflow_says_prompt_is_too_long() {
    let gw = gateway(
        400,
        json!({"error": {"message": "input is too long for this model"}}),
    )
    .await;
    let response = ask(
        &gw,
        "/v1/messages",
        json!({"model": "mock/m", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("prompt is too long:"),
        "{body}"
    );
}
