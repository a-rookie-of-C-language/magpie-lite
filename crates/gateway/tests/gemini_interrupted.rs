//! An interrupted tool call is answered on the way to Google's APIs, not
//! refused by them (the reference's `pairFunctionTurns`, `440fded2`,
//! #1560).
//!
//! A client replaying a conversation whose turn was interrupted with the
//! tool call still out had every later turn refused by Google with 400
//! "Please ensure that the number of function response parts is equal to
//! the number of function call parts of the function call turn". The
//! gateway answers the pending call with a synthetic result before the
//! request goes out, beside the client's own answers when the next turn
//! holds some and otherwise in a turn of its own — and never in a turn
//! mixing a functionResponse with text, which Vertex answers "Requests
//! ending with a model turn are not supported" (#1533).
//!
//! The fake below refuses both shapes with Google's literal 400s, so the
//! loopback proves the wire the gateway sends is accepted, and the direct
//! posts prove the fake would catch a regression.

use std::sync::{Arc, Mutex};

use axum::routing::post;
use axum::{Router, body::Bytes, extract::State, http::StatusCode, response::IntoResponse};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig,
    RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const TOKEN: &str = "INTERRUPTED_GATEWAY_TOKEN";
const KEY: &str = "INTERRUPTED_PROVIDER_KEY";
const ANSWER: &str = "done";

const MIXED_TURN: &str = "Requests ending with a model turn are not supported.";
const UNBALANCED: &str = "Please ensure that the number of function response parts is equal to the number of function call parts of the function call turn.";

#[derive(Clone)]
struct Fake {
    seen: Arc<Mutex<Vec<Value>>>,
}

fn gemini_error(message: &str) -> serde_json::Value {
    json!({"error": {"code": 400, "message": message, "status": "INVALID_ARGUMENT"}})
}

/// A fake Google endpoint: it answers what Google's APIs answer — a turn
/// mixing a functionResponse with anything else, or calls no later
/// response answers, are refused with the literal 400s.
async fn generate_content(State(fake): State<Fake>, body: Bytes) -> impl IntoResponse {
    let request: Value = serde_json::from_slice(&body).expect("upstream body is JSON");
    fake.seen.lock().unwrap().push(request.clone());
    let contents = request["contents"].as_array().cloned().unwrap_or_default();
    let mut calls: Vec<(String, String)> = Vec::new();
    let mut answered_ids: Vec<String> = Vec::new();
    let mut answered_names: Vec<String> = Vec::new();
    for content in &contents {
        let parts = content["parts"].as_array().cloned().unwrap_or_default();
        if content["role"] == "user"
            && parts.iter().any(|p| p.get("functionResponse").is_some())
            && parts.iter().any(|p| p.get("functionResponse").is_none())
        {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(gemini_error(MIXED_TURN)),
            )
                .into_response();
        }
        for part in &parts {
            if let Some(call) = part.get("functionCall") {
                calls.push((
                    call.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    call.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ));
            }
            if let Some(response) = part.get("functionResponse") {
                let id = response.get("id").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    answered_names.push(
                        response
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    );
                } else {
                    answered_ids.push(id.to_owned());
                }
            }
        }
    }
    for (id, name) in &calls {
        let answered = if id.is_empty() {
            answered_names.iter().any(|n| n == name)
        } else {
            answered_ids.iter().any(|a| a == id)
        };
        if !answered {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(gemini_error(UNBALANCED)),
            )
                .into_response();
        }
    }
    axum::Json(json!({"responseId":"ok","modelVersion":"mock-model",
        "candidates":[{"index":0,"content":{"role":"model","parts":[{"text":ANSWER}]},
        "finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}))
    .into_response()
}

struct Fixture {
    gateway: String,
    upstream: String,
    seen: Arc<Mutex<Vec<Value>>>,
    _task: Vec<tokio::task::JoinHandle<()>>,
}

async fn fixture() -> Fixture {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1beta/models/{*model}", post(generate_content))
        .with_state(Fake { seen: seen.clone() });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let upstream_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config {
        token: TOKEN.into(),
        default_provider: "mock".into(),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Gemini,
            base_url: format!("{upstream}/v1beta"),
            api_key: KEY.into(),
            api_keys: Vec::new(),
            models: vec!["mock-model".into()],
            contexts: Default::default(),
            outputs: Default::default(),
            free_models: Default::default(),
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
            pin_upstream: false,
            key_limits: Vec::new(),
        }],
        ..Config::default()
    };
    let gateway_app = build_router(Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(RequestLog::default())),
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    }));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    let gateway_task =
        tokio::spawn(async move { axum::serve(listener, gateway_app).await.unwrap() });
    Fixture {
        gateway,
        upstream,
        seen,
        _task: vec![upstream_task, gateway_task],
    }
}

/// A Chat history interrupted with the call still out: the user speaks on
/// without the call's answer, and the Gemini upstream is asked with the
/// call answered — no mixed turn, no unbalanced turn — and answers.
#[tokio::test]
async fn interrupted_call_is_answered_before_google_sees_it() {
    let fix = fixture().await;
    let reply = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", fix.gateway))
        .bearer_auth(TOKEN)
        .json(
            &json!({"model": "mock/mock-model", "stream": false, "messages": [
                {"role": "user", "content": "read the file"},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "read", "arguments": "{\"path\":\"/x\"}"}}
                ]},
                {"role": "user", "content": "continue"},
            ]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        reply.status(),
        StatusCode::OK,
        "{}",
        reply.text().await.unwrap()
    );
    let seen = fix.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "asked once and answered");
    let contents = seen[0]["contents"].as_array().unwrap();
    let responses: Vec<&Value> = contents
        .iter()
        .flat_map(|c| c["parts"].as_array().unwrap())
        .filter(|p| p.get("functionResponse").is_some())
        .collect();
    assert_eq!(
        responses.len(),
        1,
        "the pending call is answered: {contents:?}"
    );
    assert_eq!(responses[0]["functionResponse"]["id"], "call_1");
    assert!(
        responses[0]["functionResponse"]["response"]["output"]
            .as_str()
            .unwrap()
            .contains("interrupted"),
        "the synthetic answer says so: {responses:?}"
    );
    let texts: Vec<&str> = contents
        .iter()
        .flat_map(|c| c["parts"].as_array().unwrap())
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"continue"),
        "the user still speaks: {texts:?}"
    );
    for content in contents {
        let parts = content["parts"].as_array().unwrap();
        assert!(
            !parts.iter().any(|p| p.get("functionResponse").is_some())
                || parts.iter().all(|p| p.get("functionResponse").is_some()),
            "no turn mixes the answer with words: {content:?}"
        );
    }
}

/// The fake refuses the two shapes with Google's literal 400s, so the
/// loopback above would catch a regression that sent them.
#[tokio::test]
async fn the_fake_refuses_mixed_and_unbalanced_shapes_literally() {
    let fix = fixture().await;
    let url = format!("{}/v1beta/models/mock-model:generateContent", fix.upstream);
    let mixed = reqwest::Client::new()
        .post(&url)
        .json(&json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"id": "c", "name": "read", "args": {}}}]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "c", "name": "read", "response": {"output": "x"}}},
                {"text": "and now summarize"},
            ]},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(mixed.status(), StatusCode::BAD_REQUEST);
    assert!(mixed.text().await.unwrap().contains(MIXED_TURN));
    let unbalanced = reqwest::Client::new()
        .post(&url)
        .json(&json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"id": "c", "name": "read", "args": {}}}]},
            {"role": "user", "parts": [{"text": "continue"}]},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(unbalanced.status(), StatusCode::BAD_REQUEST);
    assert!(unbalanced.text().await.unwrap().contains(UNBALANCED));
}
