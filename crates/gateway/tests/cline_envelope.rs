//! Cline's whole-reply envelope over the wire.
//!
//! Cline's API answers a request that is not streamed with the
//! completion inside `{"success":true,"data":…}` (the reference's
//! `clineUnwrapped`, 6094S on Discord: Alma's tool-model calls read
//! no choices and failed). These tests send through lite's real
//! `upstream::send` path against a mock and unwrap with a Cline base:
//! the agent sees the completion itself, a refusal becomes the
//! agent-API error shape, and another host's body passes as it came.
//! Streams never reach the unwrap — only the whole-reply branch
//! calls it — and a translated request is asked of Cline as a
//! stream, which comes plain.
use std::sync::{Arc, Mutex};

use axum::{Json, Router, routing::post};
use magpie_gateway::{Protocol, ProviderConfig, RoutingPolicy, cline, upstream};
use serde_json::{Value, json};

fn completion() -> Value {
    json!({"id":"gen_1","object":"chat.completion","model":"deepseek/deepseek-v4.1-flash",
        "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":7}})
}

async fn bind(body: Value) -> (String, tokio::task::JoinHandle<()>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_request = seen.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(incoming): Json<Value>| {
            let seen = seen_request.clone();
            let body = body.clone();
            async move {
                seen.lock().unwrap().push(incoming);
                Json(body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _ = seen;
    (url, task)
}

fn provider(id: &str, upstream: &str) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "k".into(),
        api_keys: Vec::new(),
        models: vec!["up-model".into()],
        contexts: Default::default(),
        outputs: Default::default(),
        free_models: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: RoutingPolicy::Order,
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

const CLINE_BASE: &str = "https://api.cline.bot/api/v1";

async fn round_trip(envelope: Value) -> Value {
    let (url, _task) = bind(envelope).await;
    let http = upstream::http_client().unwrap();
    let p = provider("mock", &url);
    let ask = json!({"model":"up-model","messages":[{"role":"user","content":"Say pong"}],
        "max_tokens":64,"stream":false});
    let resp = upstream::send(&http, &p, "k", &ask, "up-model", false)
        .await
        .expect("send");
    assert!(resp.status().is_success(), "{}", resp.status());
    let value: Value = resp.json().await.expect("json");
    cline::unwrap_envelope(CLINE_BASE, value)
}

#[tokio::test]
async fn completion_reaches_the_agent_outside_the_envelope() {
    let out = round_trip(json!({"success": true, "data": completion()})).await;
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["content"], "pong");
    assert_eq!(out["usage"]["total_tokens"], 7);
    assert!(out.get("success").is_none(), "{out}");
}

#[tokio::test]
async fn refusal_reaches_the_agent_as_an_error() {
    let out = round_trip(json!({"success": false, "error": "Insufficient balance"})).await;
    assert_eq!(out, json!({"error": {"message": "Insufficient balance"}}));
}

#[tokio::test]
async fn another_hosts_body_passes_as_it_came() {
    let (url, _task) = bind(json!({"success": true, "data": completion()})).await;
    let http = upstream::http_client().unwrap();
    let p = provider("mock", &url);
    let ask = json!({"model":"up-model","messages":[{"role":"user","content":"Say pong"}],
        "max_tokens":64,"stream":false});
    let resp = upstream::send(&http, &p, "k", &ask, "up-model", false)
        .await
        .expect("send");
    let value: Value = resp.json().await.expect("json");
    let out = cline::unwrap_envelope("https://api.openai.com/v1", value.clone());
    assert_eq!(out, value);
}
