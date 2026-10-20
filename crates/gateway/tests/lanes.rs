//! Per-key concurrency slots and requests-per-minute windows.
//!
//! Ports the reference gateway's `concurrency.go`/`rpm.go` (#892): a
//! provider's `max_concurrency` is how many requests may be out at the
//! vendor at once on each of its keys; one more queues, in turn, and is
//! never passed to a fallback for want of a slot. `queue_limit` bounds the
//! waiters and `queue_wait_s` how long one waits; past either, a 429 with
//! Retry-After. `max_rpm` caps what a key sends in a rolling minute.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// A fake vendor that counts what it is serving now, sleeps `hold`, then
/// answers a whole Chat reply.
#[derive(Clone)]
struct Member {
    serving: Arc<AtomicUsize>,
    asked: Arc<AtomicUsize>,
    hold: Duration,
}

impl Member {
    fn new(hold: Duration) -> Self {
        Self {
            serving: Arc::new(AtomicUsize::new(0)),
            asked: Arc::new(AtomicUsize::new(0)),
            hold,
        }
    }
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

async fn chat(State(member): State<Member>, _body: Bytes) -> Response {
    member.asked.fetch_add(1, Ordering::SeqCst);
    member.serving.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(member.hold).await;
    member.serving.fetch_sub(1, Ordering::SeqCst);
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

fn provider(id: &str, url: String) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        name: id.to_uppercase(),
        protocol: Protocol::Chat,
        base_url: url,
        api_key: "fixture-key".into(),
        api_keys: Vec::new(),
        models: vec!["mock-model".into()],
        contexts: BTreeMap::new(),
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

/// A gateway over the providers given, grouped `g` over all of them.
async fn gateway(providers: Vec<ProviderConfig>) -> (String, Arc<AppState>) {
    let routes = providers
        .iter()
        .map(|p| format!("{}/mock-model", p.id))
        .collect::<Vec<_>>();
    let default_provider = providers.first().map(|p| p.id.clone()).unwrap_or_default();
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
        default_provider,
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: providers,
        group: vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes,
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
    tokio::spawn({
        let state = state.clone();
        async move { axum::serve(listener, build_router(state)).await.unwrap() }
    });
    (format!("http://{addr}"), state)
}

fn ask(
    gateway: String,
    model: &str,
) -> impl std::future::Future<Output = (StatusCode, reqwest::header::HeaderMap, Value)> {
    let gateway = gateway.to_owned();
    let model = model.to_owned();
    async move {
        let resp = reqwest::Client::new()
            .post(format!("{gateway}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap();
        (
            status,
            headers,
            serde_json::from_str(&body).unwrap_or(Value::String(body)),
        )
    }
}

/// One request at a time on the key: the second waits its turn and is then
/// sent to the same vendor, not to a fallback.
#[tokio::test]
async fn a_full_lane_queues_the_next_request_until_the_first_is_done() {
    let member = Member::new(Duration::from_millis(300));
    let mut only = provider("solo", upstream(member.clone()).await);
    only.max_concurrency = Some(1);
    let (gw, _state) = gateway(vec![only]).await;

    let first = tokio::spawn(ask(gw.clone(), "solo/mock-model"));
    // Let the first reach the vendor before the second queues.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let second = tokio::spawn(ask(gw.clone(), "solo/mock-model"));

    let (s1, _h1, b1) = first.await.unwrap();
    let (s2, _h2, b2) = second.await.unwrap();
    assert_eq!(s1, StatusCode::OK, "{b1}");
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(member.asked(), 2, "both requests reached the one vendor");
}

/// `/stats` shows live concurrency lanes: while one request holds the
/// key's only slot and another queues behind it, the lane reads busy and
/// waiting with its bound; once both are done it is gone.
#[tokio::test]
async fn stats_show_live_lanes_while_a_request_queues() {
    async fn stats(gw: &str) -> Value {
        reqwest::Client::new()
            .get(format!("{gw}/stats"))
            .bearer_auth("magpie-lite")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    let member = Member::new(Duration::from_millis(300));
    let mut only = provider("solo", upstream(member.clone()).await);
    only.max_concurrency = Some(1);
    only.max_rpm = Some(60);
    let (gw, state) = gateway(vec![only]).await;

    let who = magpie_gateway::affinity::who_of("solo", "fixture-key");
    let hold = state
        .lanes
        .take(&who, 1, 0, Duration::ZERO)
        .await
        .expect("hold the lane");
    let queued = tokio::spawn(ask(gw.clone(), "solo/mock-model"));
    // Let the queued request arrive before reading `/stats`.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        stats(&gw).await["lanes"],
        json!([{"who": &who, "busy": 1, "waiting": 1, "limit": 1}]),
        "one held, one queued"
    );
    // Nothing reached the vendor yet: the slot is held and the request
    // still waits for it, so no minute is counted.
    let rpm = stats(&gw).await["rpm"].clone();
    assert!(rpm.is_null() || rpm == json!([]), "{rpm}");
    drop(hold);
    let (status, _headers, body) = queued.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stats(&gw).await["rpm"],
        json!([{"who": &who, "used": 1}]),
        "one sent this minute"
    );
    // The lane is gone once nothing holds or waits on it (an empty lane
    // list is omitted from `/stats`, like other empty details).
    let mut lanes = Value::Null;
    for _ in 0..20 {
        lanes = stats(&gw).await["lanes"].clone();
        if lanes.is_null() || lanes == json!([]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(lanes.is_null() || lanes == json!([]), "{lanes}");
}

/// Two keys on a provider, the first's lane full: the second is asked first
/// (the reference's laneMate) instead of the request waiting.
#[tokio::test]
async fn a_full_key_gives_way_to_its_provider_mate_with_a_free_slot() {
    let member = Member::new(Duration::from_millis(400));
    let url = upstream(member.clone()).await;
    let mut p = provider("pair", url);
    p.api_key = String::new();
    p.api_keys = vec!["key-one".into(), "key-two".into()];
    p.routing = RoutingPolicy::Order;
    p.max_concurrency = Some(1);
    let (gw, state) = gateway(vec![p]).await;

    // Hold key-one's slot so a request routed to it is full.
    let who_one = magpie_gateway::affinity::who_of("pair", "key-one");
    let _hold = state
        .lanes
        .take(&who_one, 1, 0, Duration::ZERO)
        .await
        .expect("key-one's first slot");

    let (status, _headers, body) = ask(gw.clone(), "pair/mock-model").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The request answered quickly rather than waiting on key-one's 400ms
    // slot: the routing rotated or gave way to a free key.
    assert!(member.asked() >= 1);
}

/// The queue's bound reached, a request is turned away with a 429 at once
/// and nobody rests.
#[tokio::test]
async fn a_queue_past_its_bound_is_turned_away_with_a_429() {
    let member = Member::new(Duration::from_millis(600));
    let mut only = provider("solo", upstream(member.clone()).await);
    only.max_concurrency = Some(1);
    only.queue_limit = Some(1);
    let (gw, _state) = gateway(vec![only]).await;

    // Occupy the slot and the one queue place.
    let first = tokio::spawn(ask(gw.clone(), "solo/mock-model"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let queued = tokio::spawn(ask(gw.clone(), "solo/mock-model"));
    tokio::time::sleep(Duration::from_millis(60)).await;

    // A third is over the bound: a 429 at once.
    let (status, _headers, body) = ask(gw.clone(), "solo/mock-model").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    drop(first.await.unwrap());
    drop(queued.await.unwrap());
    assert_eq!(member.asked(), 2, "the refused request never went upstream");
}

/// A key's own override caps it while the provider stays unlimited: with
/// no `max_concurrency` and `key_limits` of [1], the key's lane still
/// queues one and turns the next away with a 429 — the override, not the
/// provider, set the slot count.
#[tokio::test]
async fn a_per_key_override_caps_its_key_while_the_provider_stays_unlimited() {
    let member = Member::new(Duration::from_millis(600));
    let mut only = provider("solo", upstream(member.clone()).await);
    only.key_limits = vec![1];
    only.queue_limit = Some(1);
    let (gw, _state) = gateway(vec![only]).await;

    // Occupy the key's one slot and its one queue place.
    let first = tokio::spawn(ask(gw.clone(), "solo/mock-model"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let queued = tokio::spawn(ask(gw.clone(), "solo/mock-model"));
    tokio::time::sleep(Duration::from_millis(60)).await;

    // A third is over the bound: a 429 at once.
    let (status, _headers, body) = ask(gw.clone(), "solo/mock-model").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    drop(first.await.unwrap());
    drop(queued.await.unwrap());
    assert_eq!(member.asked(), 2, "the refused request never went upstream");
}

/// A minute's worth sent on the key, the next waits for room in it; past
/// the wait it may keep, a 429 with Retry-After is the answer.
#[tokio::test]
async fn a_minutes_worth_sent_is_answered_with_retry_after() {
    let member = Member::new(Duration::ZERO);
    let mut only = provider("solo", upstream(member.clone()).await);
    only.max_rpm = Some(1);
    // A shorter bound than the minute's wait, so the next is refused.
    only.queue_wait_s = Some(1);
    let (gw, _state) = gateway(vec![only]).await;

    let (s1, _h1, b1) = ask(gw.clone(), "solo/mock-model").await;
    assert_eq!(s1, StatusCode::OK, "{b1}");

    let (status, headers, body) = ask(gw.clone(), "solo/mock-model").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(
        headers.get("retry-after").is_some(),
        "a minute-bound refusal carries Retry-After"
    );
}

/// Waiting is not failing: a queued request on one key is never passed to a
/// fallback for want of a slot.
#[tokio::test]
async fn a_queued_request_is_never_sent_to_a_fallback_for_want_of_a_slot() {
    let slow = Member::new(Duration::from_millis(300));
    let fast = Member::new(Duration::ZERO);
    let mut primary = provider("primary", upstream(slow.clone()).await);
    primary.max_concurrency = Some(1);
    let fallback = provider("fallback", upstream(fast.clone()).await);
    primary.fallback = vec!["fallback/mock-model".into()];
    let (gw, _state) = gateway_with_fallback(primary, fallback).await;

    let first = tokio::spawn(ask(gw.clone(), "primary/mock-model"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let second = tokio::spawn(ask(gw.clone(), "primary/mock-model"));

    let (s1, _h1, b1) = first.await.unwrap();
    let (s2, _h2, b2) = second.await.unwrap();
    assert_eq!(s1, StatusCode::OK, "{b1}");
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(slow.asked(), 2);
    assert_eq!(
        fast.asked(),
        0,
        "a fallback answered a request only queued on the first"
    );
}

/// A gateway whose first provider names `fallback/mock-model` as its
/// fallback, separate from the group.
async fn gateway_with_fallback(
    primary: ProviderConfig,
    fallback: ProviderConfig,
) -> (String, Arc<AppState>) {
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
        default_provider: primary.id.clone(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: vec![primary, fallback],
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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let state = state.clone();
        async move { axum::serve(listener, build_router(state)).await.unwrap() }
    });
    (format!("http://{addr}"), state)
}
