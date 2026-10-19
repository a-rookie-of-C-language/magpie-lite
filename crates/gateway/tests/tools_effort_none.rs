//! A Chat-only relay that refuses function tools with reasoning, and
//! `reasoning_effort: "none"` too, moves a group on instead of handing
//! its `none` refusal to the agent (#2670, ported from the reference).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, Protocol, ProviderConfig, RoutingPolicy,
    build_router,
};
use reqwest::Client;
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Relay {
    efforts: Arc<Mutex<Vec<String>>>,
    /// `none` is accepted (the relay takes tools without reasoning).
    none_ok: bool,
    /// Every request is answered.
    serves: bool,
}

impl Relay {
    fn asked(&self) -> Vec<String> {
        self.efforts.lock().unwrap().clone()
    }

    fn forget(&self) {
        self.efforts.lock().unwrap().clear();
    }
}

async fn chat(State(relay): State<Relay>, Json(body): Json<Value>) -> axum::response::Response {
    use axum::response::IntoResponse;

    let effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    relay.efforts.lock().unwrap().push(effort.clone());
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    if !relay.serves {
        if effort == "none" && !relay.none_ok {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": {
                    "message": "Unsupported value: 'none' is not supported with the 'gpt-6.1-sol' model. Supported values are: 'low', 'medium', 'high', 'xhigh', and 'max'.",
                    "type": "invalid_request_error"
                }})),
            )
                .into_response();
        }
        if tools && effort != "none" {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": {
                    "message": "Function tools with reasoning_effort are not supported for gpt-6.1-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
                    "type": "invalid_request_error"
                }})),
            )
                .into_response();
        }
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if !stream {
        return Json(json!({
            "id": "c1", "object": "chat.completion", "model": "gpt-6.1-sol",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok from relay"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        }))
        .into_response();
    }
    let frames = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok from relay\"}}]}\n\n\
data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n\
data: [DONE]\n\n";
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(frames))
        .unwrap()
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

fn provider(id: &str, name: &str, base: &str) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        name: name.into(),
        protocol: Protocol::Chat,
        base_url: format!("{base}/v1"),
        api_key: "k".into(),
        api_keys: Vec::new(),
        models: vec!["gpt-6.1-sol".into()],
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
    }
}

async fn gateway(
    id: &str,
    relay: Relay,
    other: Option<Relay>,
    group: bool,
) -> (Gateway, Relay, Option<Relay>) {
    let relay_state = relay.clone();
    let (relay_url, relay_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(relay),
    )
    .await;
    let (mut providers, mut tasks) = (Vec::new(), vec![relay_task]);
    providers.push(provider(id, "RELAY", &relay_url));
    let mut other_state = None;
    if let Some(other) = &other {
        let (other_url, other_task) = bind(
            Router::new()
                .route("/v1/chat/completions", post(chat))
                .with_state(other.clone()),
        )
        .await;
        tasks.push(other_task);
        providers.push(provider(&format!("{id}-other"), "OTHER", &other_url));
        other_state = Some(other.clone());
    }
    let groups = if group {
        vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes: if other.is_some() {
                vec![
                    format!("{id}/gpt-6.1-sol"),
                    format!("{id}-other/gpt-6.1-sol"),
                ]
            } else {
                vec![format!("{id}/gpt-6.1-sol")]
            },
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            pick: String::new(),
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
        }]
    } else {
        Vec::new()
    };
    let config = Config {
        token: "magpie-lite".into(),
        default_provider: id.into(),
        provider: providers,
        group: groups,
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    tasks.push(gateway_task);
    (Gateway { url, _tasks: tasks }, relay_state, other_state)
}

fn codex_tools(model: &str, effort: &str) -> Value {
    json!({
        "model": model,
        "stream": true,
        "reasoning": {"effort": effort},
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}],
        "input": "hi"
    })
}

#[tokio::test]
async fn a_none_refused_too_is_not_asked_again_at_the_lowest_level() {
    let (gateway, relay, _) = gateway(
        "none-refused",
        Relay {
            none_ok: false,
            ..Relay::default()
        },
        None,
        false,
    )
    .await;
    let client = Client::new();
    for (i, (effort, want)) in [
        ("xhigh", vec!["xhigh", "none"]),
        ("high", vec!["high"]),
        ("high", vec!["high"]),
    ]
    .into_iter()
    .enumerate()
    {
        relay.forget();
        let response = client
            .post(format!("{}/v1/responses", gateway.url))
            .bearer_auth("magpie-lite")
            .json(&codex_tools("none-refused/gpt-6.1-sol", effort))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_REQUEST,
            "request {}",
            i + 1
        );
        assert!(
            body.contains("Function tools with reasoning_effort"),
            "request {}: {body}",
            i + 1
        );
        assert!(
            !body.contains("'none' is not supported"),
            "request {} handed back the none refusal: {body}",
            i + 1
        );
        assert!(
            body.contains("/v1/responses, which RELAY has no URL for"),
            "request {}: {body}",
            i + 1
        );
        assert_eq!(relay.asked(), want, "request {}", i + 1);
    }
}

#[tokio::test]
async fn a_group_moves_on_when_the_chat_only_member_refuses_tools_with_none() {
    let (gateway, relay, other) = gateway(
        "group-none-refused",
        Relay {
            none_ok: false,
            ..Relay::default()
        },
        Some(Relay {
            serves: true,
            ..Relay::default()
        }),
        true,
    )
    .await;
    let other = other.expect("other");
    let client = Client::new();
    for (i, (effort, want)) in [
        ("xhigh", vec!["xhigh", "none"]),
        ("high", vec!["high"]),
        ("high", vec!["high"]),
    ]
    .into_iter()
    .enumerate()
    {
        relay.forget();
        other.forget();
        let response = client
            .post(format!("{}/v1/responses", gateway.url))
            .bearer_auth("magpie-lite")
            .json(&codex_tools("group/g", effort))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "request {}: {body}", i + 1);
        assert!(body.contains("ok from relay"), "request {}: {body}", i + 1);
        assert_eq!(relay.asked(), want, "request {}", i + 1);
        assert_eq!(other.asked().len(), 1, "request {}", i + 1);
    }
}

#[tokio::test]
async fn a_chat_client_relay_asks_none_once_and_not_again() {
    let (gateway, relay, _) = gateway(
        "chat-none-refused",
        Relay {
            none_ok: false,
            ..Relay::default()
        },
        None,
        false,
    )
    .await;
    let client = Client::new();
    for (i, want) in [vec!["high", "none"], vec!["high"]].into_iter().enumerate() {
        relay.forget();
        let response = client
            .post(format!("{}/v1/chat/completions", gateway.url))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "chat-none-refused/gpt-6.1-sol",
                "stream": true,
                "reasoning_effort": "high",
                "tools": [{"type": "function", "function": {"name": "shell", "parameters": {"type": "object"}}}],
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "request {}",
            i + 1
        );
        assert_eq!(relay.asked(), want, "request {}", i + 1);
    }
}

#[tokio::test]
async fn a_relay_that_takes_none_answers_every_tools_request() {
    let (gateway, relay, _) = gateway(
        "none-taken",
        Relay {
            none_ok: true,
            ..Relay::default()
        },
        None,
        false,
    )
    .await;
    let client = Client::new();
    for (i, effort) in ["xhigh", "high", "high"].into_iter().enumerate() {
        relay.forget();
        let response = client
            .post(format!("{}/v1/responses", gateway.url))
            .bearer_auth("magpie-lite")
            .json(&codex_tools("none-taken/gpt-6.1-sol", effort))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "request {}",
            i + 1
        );
        assert_eq!(relay.asked(), vec![effort, "none"], "request {}", i + 1);
    }
}
