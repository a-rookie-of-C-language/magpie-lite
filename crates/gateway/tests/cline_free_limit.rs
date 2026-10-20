//! Cline's daily limit on one free model rests that model alone.
//!
//! Cline answers a free model's spent day with 429 "Daily free limit
//! reached on model X. Try again in 23h 59m" (the reference's
//! per-model rest, #1398): the model alone rests until the time the
//! refusal names while its provider's other models still answer, and
//! the model out of its day is answered with its refusal without
//! asking Cline again. Another vendor's same words, and a Cline 429
//! that doesn't name the model's limit, rest the key as before.
//!
//! A loopback fake can't live on a Cline host, so these tests drive
//! the gateway with the rest the refusal would lay down (`model_rest`
//! is the hook's whole state): the skip, the replay and the key
//! fallback. The host gate itself — a Cline host's refusal rests the
//! model, another vendor's same words rest its key — is covered by
//! `cline::free_limit_rest`'s unit tests, which run against the real
//! host names.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, routing::post};
use magpie_gateway::{AppState, Config, ProviderConfig, RoutingPolicy, build_router, model_rest};
use reqwest::StatusCode;
use serde_json::{Value, json};

const FREE_LIMIT: &str =
    "Daily free limit reached on model deepseek/deepseek-v4-flash. Try again in 23h 59m";

async fn bind(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

/// A Chat fake that records the models it was asked for and answers
/// `status` with `body` every time.
async fn fake(asked: Arc<Mutex<Vec<String>>>, status: StatusCode, body: Value) -> String {
    #[derive(Clone)]
    struct Fake {
        asked: Arc<Mutex<Vec<String>>>,
        status: StatusCode,
        body: Value,
    }
    async fn answer(
        axum::extract::State(Fake {
            asked,
            status,
            body,
        }): axum::extract::State<Fake>,
        Json(incoming): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        asked.lock().unwrap().push(
            incoming
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned(),
        );
        (status, Json(body))
    }
    bind(
        Router::new()
            .route("/v1/chat/completions", post(answer))
            .with_state(Fake {
                asked,
                status,
                body,
            }),
    )
    .await
}

fn completion(model: &str) -> Value {
    json!({"id":"gen_1","object":"chat.completion","model":model,
        "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}})
}

async fn gateway(id: &str, upstream: &str, models: &[&str]) -> String {
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        provider: vec![ProviderConfig {
            id: id.into(),
            name: id.into(),
            protocol: magpie_gateway::Protocol::Chat,
            base_url: format!("{upstream}/v1"),
            api_key: "k".into(),
            api_keys: Vec::new(),
            models: models.iter().map(|model| model.to_string()).collect(),
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
        }],
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: magpie_gateway::ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    bind(build_router(state)).await
}

async fn ask(gw: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": model,
            "stream": false,
            "messages": [{"role": "user", "content": "Say pong"}],
        }))
        .send()
        .await
        .unwrap()
}

/// The model out of its day is answered with its refusal without
/// asking the vendor again, while the provider's other models still
/// answer.
#[tokio::test]
async fn rested_model_is_answered_without_asking_and_others_still_answer() {
    model_rest::reset();
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake(
        asked.clone(),
        StatusCode::OK,
        completion("cline-free/mimo-v2.6-flash"),
    )
    .await;
    let gw = gateway(
        "cline",
        &upstream,
        &["cline-free/deepseek-v4-flash", "cline-free/mimo-v2.6-flash"],
    )
    .await;
    model_rest::rest(
        "cline",
        "cline-free/deepseek-v4-flash",
        Duration::from_secs(3600),
        FREE_LIMIT,
    );

    let refused = ask(&gw, "cline/cline-free/deepseek-v4-flash").await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = refused
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .expect("rested model names when it is back")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(retry_after > 0, "back in {retry_after}s");
    let body = refused.text().await.unwrap();
    assert!(
        body.contains("Daily free limit"),
        "its own refusal answers, {body}"
    );
    assert!(
        asked.lock().unwrap().is_empty(),
        "the vendor was not asked again"
    );

    let served = ask(&gw, "cline/cline-free/mimo-v2.6-flash").await;
    assert_eq!(
        served.status(),
        StatusCode::OK,
        "{}",
        served.text().await.unwrap()
    );
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        ["cline-free/mimo-v2.6-flash"],
        "only the other model was asked"
    );
    model_rest::reset();
}

/// Another vendor's same words rest its key, never the model: the next
/// ask for the same model still reaches the vendor.
#[tokio::test]
async fn another_vendors_same_words_rest_its_key() {
    model_rest::reset();
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake(
        asked.clone(),
        StatusCode::TOO_MANY_REQUESTS,
        json!({"error": {"message": FREE_LIMIT}}),
    )
    .await;
    let gw = gateway("relay", &upstream, &["relay-a", "relay-b"]).await;

    let first = ask(&gw, "relay/relay-a").await;
    assert_eq!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        model_rest::resting("relay", "relay-a").is_none(),
        "no model rest off another vendor's host"
    );
    let second = ask(&gw, "relay/relay-a").await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        asked.lock().unwrap().len(),
        2,
        "the key path asks again instead of replaying"
    );
    model_rest::reset();
}

/// A Cline 429 that doesn't name the model's limit rests the key too.
#[tokio::test]
async fn plain_cline_rate_limit_rests_the_key() {
    model_rest::reset();
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream = fake(
        asked.clone(),
        StatusCode::TOO_MANY_REQUESTS,
        json!({"error": {"message": "Rate limit exceeded"}}),
    )
    .await;
    // Its own model name: the other tests' rests never touch it, and its
    // own key path lays none.
    let gw = gateway("cline", &upstream, &["cline-free/solo-v1"]).await;
    let first = ask(&gw, "cline/cline-free/solo-v1").await;
    assert_eq!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        model_rest::resting("cline", "cline-free/solo-v1").is_none(),
        "a limit that names no model rests no model"
    );
    let second = ask(&gw, "cline/cline-free/solo-v1").await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        asked.lock().unwrap().len(),
        2,
        "the key path asks again instead of replaying"
    );
    model_rest::reset();
}
