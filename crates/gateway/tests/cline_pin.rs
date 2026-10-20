//! Cline upstream pinning over the wire.
//!
//! A pinning Cline provider serves a DeepSeek model from DeepSeek's own
//! API: the Chat body names it in `providerOptions.gateway.only`
//! (the reference's `internal/gateway/cline_pin.go`). These tests pin
//! the contract from lite's side: a fake upstream records what arrived,
//! and `upstream::send`/`build_request` drive it — a pinned body
//! carries the field intact, while another vendor's send never gains
//! one even with pinning switched on.

use std::sync::{Arc, Mutex};

use axum::{Router, extract::State, routing::post};
use magpie_core::chat;
use magpie_gateway::{ProviderConfig, upstream};
use serde_json::{Value, json};

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

fn provider(id: &str, upstream: &str, pin_upstream: bool) -> ProviderConfig {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "protocol": "chat",
        "base_url": format!("{upstream}/v1"),
        "api_key": "k",
        "models": ["up-model"],
        "pin_upstream": pin_upstream,
    }))
    .unwrap()
}

/// A permissive Chat fake that records the bodies it was sent.
async fn fake(bodies: Arc<Mutex<Vec<Value>>>) -> (String, tokio::task::JoinHandle<()>) {
    async fn answer(
        State(bodies): State<Arc<Mutex<Vec<Value>>>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        bodies.lock().unwrap().push(body);
        axum::Json(json!({
            "id": "mock", "object": "chat.completion", "model": "up-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        }))
    }
    bind(
        Router::new()
            .route("/v1/chat/completions", post(answer))
            .with_state(bodies),
    )
    .await
}

/// A pinned send to another vendor's host gains no `providerOptions`:
/// pinning fires on Cline hosts only, and `upstream::send` is the path
/// that decides.
#[tokio::test]
async fn other_vendors_are_never_pinned() {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let (url, _task) = fake(bodies.clone()).await;
    let http = upstream::http_client().unwrap();
    let p = provider("other", &url, true);
    let body = json!({
        "model": "deepseek-v4-pro",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 64,
    });
    let response = upstream::send(&http, &p, "k", &body, "deepseek-v4-pro", false)
        .await
        .expect("send");
    assert!(response.status().is_success(), "{}", response.status());
    let guard = bodies.lock().unwrap();
    assert_eq!(guard.len(), 1, "one ask, {guard:?}");
    assert!(
        guard[0].get("providerOptions").is_none(),
        "no pin off Cline hosts, {:?}",
        guard[0]
    );
}

/// The body `build_request` makes for a pinning Cline provider carries
/// the pin intact across the wire, and the upstream answers it.
#[tokio::test]
async fn pinned_bodies_survive_the_wire() {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let (url, _task) = fake(bodies.clone()).await;
    let http = upstream::http_client().unwrap();
    // The pin decision reads the provider's own base URL, so the body
    // is built for the Cline host, then posted to the loopback fake.
    let cline: ProviderConfig = serde_json::from_value(json!({
        "id": "cline",
        "protocol": "chat",
        "base_url": "https://api.cline.bot/api/v1",
        "pin_upstream": true,
    }))
    .unwrap();
    let request = chat::decode_request(&json!({"model": "client",
        "messages": [{"role": "user", "content": "hi"}]}))
    .unwrap();
    let body = upstream::build_request(&cline, &request, "deepseek-v4-pro", false).unwrap();
    assert_eq!(
        body["providerOptions"],
        json!({"gateway": {"only": ["deepseek"]}}),
        "built body pins, {body}"
    );
    let response = http
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("k")
        .json(&body)
        .send()
        .await
        .expect("send");
    assert!(response.status().is_success(), "{}", response.status());
    let guard = bodies.lock().unwrap();
    assert_eq!(guard.len(), 1, "one ask, {guard:?}");
    assert_eq!(
        guard[0]["providerOptions"],
        json!({"gateway": {"only": ["deepseek"]}}),
        "pin arrives intact, {:?}",
        guard[0]
    );
}
