//! A group member whose model is known to take less than the request is not
//! asked it, before anything is sent.
//!
//! Ports the reference gateway's `roomy` check (`0f398c48`). The reported
//! case: OpenCode Zen's free DeepSeek V4.1 Flash (200K) first in a group and a
//! 1M subscription of it second, so the first 200K go free and past that go to
//! the subscription. The free member used to be asked a 220K request, refuse
//! it without saying why, and reach the agent as that error while the paid
//! member had room — spending a request against the free tier's limits too.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, KeyRotator, Protocol, ProviderConfig,
    RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

/// A fake vendor that records how many requests reached it and answers a
/// whole Chat reply. `refuse` makes it answer a bare "Invalid request
/// parameters" 400, the wording that doesn't say the window overflowed.
#[derive(Clone)]
struct Member {
    asked: Arc<Mutex<usize>>,
    refuse: bool,
}

impl Member {
    fn new(refuse: bool) -> Self {
        Self {
            asked: Arc::new(Mutex::new(0)),
            refuse,
        }
    }

    fn asked(&self) -> usize {
        *self.asked.lock().unwrap()
    }
}

async fn chat(State(member): State<Member>, _body: Bytes) -> Response {
    *member.asked.lock().unwrap() += 1;
    if member.refuse {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "Invalid request parameters"}})),
        )
            .into_response();
    }
    axum::Json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

async fn upstream(member: Member) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/chat/completions", post(chat))
                .with_state(member),
        )
        .await
        .unwrap()
    });
    format!("http://{addr}/v1")
}

fn provider(id: &str, url: String, window: u64) -> ProviderConfig {
    let mut contexts = BTreeMap::new();
    if window > 0 {
        contexts.insert("mock-model".to_owned(), window);
    }
    ProviderConfig {
        id: id.into(),
        name: id.to_uppercase(),
        protocol: Protocol::Chat,
        base_url: url,
        api_key: "fixture-key".into(),
        api_keys: Vec::new(),
        models: vec!["mock-model".into()],
        contexts,
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
        key_limits: Vec::new(),
    }
}

/// A gateway whose group `g` has `free/mock-model` then `paid/mock-model`.
async fn gateway(free: ProviderConfig, paid: ProviderConfig) -> String {
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
        default_provider: "free".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: vec![free, paid],
        group: vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes: vec!["free/mock-model".into(), "paid/mock-model".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s: None,
        }],
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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    format!("http://{addr}")
}

async fn ask(gateway: &str, text: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/g",
            "messages": [{"role": "user", "content": text}]
        }))
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

/// About 2200 tokens at the reference's four bytes a token.
fn past_the_free_window() -> String {
    "word ".repeat(1760)
}

#[tokio::test]
async fn a_member_too_small_is_not_asked_the_request() {
    let free = Member::new(true);
    let paid = Member::new(false);
    let gw = gateway(
        provider("free", upstream(free.clone()).await, 2000),
        provider("paid", upstream(paid.clone()).await, 10_000),
    )
    .await;

    let (status, body) = ask(&gw, &past_the_free_window()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        free.asked(),
        0,
        "the free member was asked a request it cannot hold"
    );
    assert_eq!(paid.asked(), 1, "the paid member did not answer");
}

#[tokio::test]
async fn a_request_within_the_free_window_still_goes_to_it_first() {
    let free = Member::new(false);
    let paid = Member::new(false);
    let gw = gateway(
        provider("free", upstream(free.clone()).await, 2000),
        provider("paid", upstream(paid.clone()).await, 10_000),
    )
    .await;

    let (status, body) = ask(&gw, "hi").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(free.asked(), 1);
    assert_eq!(paid.asked(), 0);
}

#[tokio::test]
async fn a_conversation_kept_on_the_small_member_moves_once_it_outgrows_it() {
    let free = Member::new(false);
    let paid = Member::new(false);
    let gw = gateway(
        provider("free", upstream(free.clone()).await, 2000),
        provider("paid", upstream(paid.clone()).await, 10_000),
    )
    .await;

    // The first turn fits, and the free member answers it.
    let (status, body) = ask(&gw, "hi").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(free.asked(), 1);
    assert_eq!(paid.asked(), 0);

    // The same conversation, grown past the free member's window, goes to the
    // paid member. The gateway names the conversation by its messages, so a
    // later request that carries the history is the same one.
    let client = reqwest::Client::new();
    let history = json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
        {"role": "user", "content": past_the_free_window()}
    ]);
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model": "group/g", "messages": history}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        free.asked(),
        1,
        "the grown conversation was still asked the small member"
    );
    assert_eq!(paid.asked(), 1);
}

#[tokio::test]
async fn a_window_not_known_and_a_hopeless_request_are_left_alone() {
    // No window on either member: the first is asked as before.
    let unknown = Member::new(false);
    let unknown_second = Member::new(false);
    let gw = gateway(
        provider("free", upstream(unknown.clone()).await, 0),
        provider("paid", upstream(unknown_second.clone()).await, 0),
    )
    .await;
    let (status, body) = ask(&gw, &past_the_free_window()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(unknown.asked(), 1);
    assert_eq!(unknown_second.asked(), 0);

    // No member holds the request: the first is asked, and its refusal reaches
    // the agent as one, to compact.
    let small = Member::new(true);
    let smaller = Member::new(true);
    let gw = gateway(
        provider("free", upstream(small.clone()).await, 2000),
        provider("paid", upstream(smaller.clone()).await, 1500),
    )
    .await;
    let (status, _body) = ask(&gw, &past_the_free_window()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(small.asked(), 1);
    assert_eq!(smaller.asked(), 0);
}
