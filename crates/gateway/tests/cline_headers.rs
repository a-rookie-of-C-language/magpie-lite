//! Cline vendor headers over the wire.
//!
//! The Cline API answers its free models `403 "only available via Cline
//! product surfaces"` unless the request carries the desktop app's
//! headers (the reference's `internal/provider/cline.go`, `ClineClient`
//! and `TestClineModelTestStreams`). These tests pin the contract from
//! lite's side: a fake Cline API that 403s headerless asks and streams
//! an answer to headed ones, and a permissive fake proving other
//! vendors' requests carry no Cline headers.

use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::{Router, extract::State, routing::post};
use magpie_gateway::{Protocol, ProviderConfig, RoutingPolicy, cline, upstream};
use serde_json::{Value, json};

/// Captured request headers per ask.
type SeenHeaders = Arc<Mutex<Vec<Vec<(String, String)>>>>;

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
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

/// A permissive Chat fake that records what headers arrived with the ask.
async fn permissive(seen: SeenHeaders) -> (String, tokio::task::JoinHandle<()>) {
    async fn answer(
        State(seen): State<SeenHeaders>,
        parts: axum::http::request::Parts,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        assert!(body.get("model").is_some(), "body {body}");
        let headers = parts
            .headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        seen.lock().unwrap().push(headers);
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
            .with_state(seen),
    )
    .await
}

/// A request to another vendor's host carries no Cline headers: the hook
/// fires on Cline hosts only, and `upstream::send` is the path that
/// decides.
#[tokio::test]
async fn other_vendors_get_no_cline_headers() {
    let seen: SeenHeaders = Arc::new(Mutex::new(Vec::new()));
    let (url, _task) = permissive(seen.clone()).await;
    let http = upstream::http_client().unwrap();
    let p = provider("other", &url);
    let body = json!({
        "model": "up-model",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 64,
    });
    let response = upstream::send(&http, &p, "k", &body, "up-model", false)
        .await
        .expect("send");
    assert!(response.status().is_success(), "{}", response.status());
    let headers = {
        let guard = seen.lock().unwrap();
        assert_eq!(guard.len(), 1, "one ask, {guard:?}");
        guard[0].clone()
    };
    assert!(
        !headers.iter().any(|(k, _)| k == "x-client-type"),
        "no Cline client type, {headers:?}"
    );
    assert!(
        !headers.iter().any(|(k, _)| k == "x-client-version"),
        "no Cline client version, {headers:?}"
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer k"),
        "bearer auth still rides, {headers:?}"
    );
}

/// A fake Cline API, mirroring the reference's probe fake: a free model
/// asked without the desktop client type is refused as only available
/// via Cline product surfaces; a headed streamed ask with room is
/// answered as Cline's own clients are.
async fn fake_cline() -> (String, tokio::task::JoinHandle<()>) {
    async fn answer(
        parts: axum::http::request::Parts,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::response::Response {
        if parts
            .headers
            .get("x-client-type")
            .and_then(|v| v.to_str().ok())
            != Some("cline-desktop")
        {
            return (
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(
                    json!({"error": {"message": "only available via Cline product surfaces"}}),
                ),
            )
                .into_response();
        }
        let room = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
        if body.get("stream") != Some(&json!(true)) || room < 256 {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": {"message": "empty response content"}})),
            )
                .into_response();
        }
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello!\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            sse,
        )
            .into_response()
    }
    bind(Router::new().route("/v1/chat/completions", post(answer))).await
}

/// The headed request satisfies the fake Cline API over the wire, while
/// the same ask without the headers is refused: the header values lite
/// carries are the ones the API answers.
#[tokio::test]
async fn cline_headers_satisfy_a_fake_cline_api() {
    let (url, _task) = fake_cline().await;
    let http = upstream::http_client().unwrap();
    let body = json!({
        "model": "cline-free/deepseek-v4.1-flash",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "stream": true,
    });
    // Without the desktop headers the free model is refused.
    let bare = http
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("clp_key")
        .json(&body)
        .send()
        .await
        .expect("send");
    assert_eq!(bare.status(), 403, "headerless ask refused");
    // With lite's Cline headers the same ask streams an answer.
    let headed = cline::apply_cline_headers(
        http.post(format!("{url}/v1/chat/completions"))
            .bearer_auth("clp_key"),
        "https://api.cline.bot/api/v1",
    )
    .json(&body)
    .send()
    .await
    .expect("send");
    assert!(headed.status().is_success(), "{}", headed.status());
    let wire = headed.text().await.expect("body");
    assert!(wire.contains("Hello!"), "streams the answer, {wire}");
}
