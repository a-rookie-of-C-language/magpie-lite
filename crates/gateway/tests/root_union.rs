//! A tool whose parameters have a union at the root is asked again folded
//! when an upstream refuses that shape, and the refusal is remembered for
//! the same provider, model and protocol.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig,
    RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const TOKEN: &str = "root-union-fixture-token";
const KEY: &str = "root-union-fixture-key";
const XAI_REFUSAL: &str = "tool parameter root must be an object type";
const OPENAI_CHAT_REFUSAL: &str = "Invalid schema for function 't': schema must have type 'object' and not have 'oneOf'/'anyOf'/'allOf'/'enum'/'const'/'not' at the top level.";
const OPENAI_RESPONSES_REFUSAL: &str = "Invalid schema for function 't': schema must be a JSON Schema of 'type: \"object\"', got 'type: \"None\"'.";

#[derive(Clone)]
enum Behavior {
    RefuseRootUnion(&'static str),
    RefuseRootUnionOnFirstOnly(&'static str),
    RefuseRootUnionThenShape(&'static str),
    AcceptUnion,
    UnrelatedInvalidSchema,
}

#[derive(Clone)]
struct Upstream {
    protocol: Protocol,
    behavior: Behavior,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn parameters(body: &Value, protocol: Protocol) -> Option<&Value> {
    body.get("tools")?
        .as_array()?
        .iter()
        .find_map(|tool| match protocol {
            Protocol::Chat => tool.pointer("/function/parameters"),
            Protocol::Responses => tool.get("parameters"),
            _ => None,
        })
}

fn has_root_union(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) != Some("object")
        || ["anyOf", "oneOf", "allOf"]
            .iter()
            .any(|key| schema.get(key).is_some())
}

fn answer(protocol: Protocol) -> Response {
    match protocol {
        Protocol::Chat => Json(json!({
            "id": "c1",
            "object": "chat.completion",
            "model": "up-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        }))
        .into_response(),
        Protocol::Responses => Json(json!({
            "id": "r1",
            "object": "response",
            "model": "up-model",
            "status": "completed",
            "output": [{
                "id": "msg",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        }))
        .into_response(),
        _ => unreachable!(),
    }
}

fn refusal(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": "invalid_request_error"}})),
    )
        .into_response()
}

async fn upstream(State(upstream): State<Upstream>, Json(body): Json<Value>) -> Response {
    let schema = parameters(&body, upstream.protocol);
    let root_union = schema.is_some_and(has_root_union);
    let prior = upstream.seen.lock().unwrap().len();
    upstream.seen.lock().unwrap().push(body);
    match upstream.behavior {
        Behavior::RefuseRootUnion(message) if root_union => {
            refusal(StatusCode::BAD_REQUEST, message)
        }
        Behavior::RefuseRootUnionOnFirstOnly(message) if root_union && prior == 0 => {
            refusal(StatusCode::BAD_REQUEST, message)
        }
        Behavior::RefuseRootUnionThenShape(message) if root_union => {
            refusal(StatusCode::BAD_REQUEST, message)
        }
        Behavior::RefuseRootUnionThenShape(_) if prior > 0 => {
            refusal(StatusCode::BAD_REQUEST, "Unknown parameter 'shape_probe'.")
        }
        Behavior::UnrelatedInvalidSchema => refusal(
            StatusCode::BAD_REQUEST,
            "Invalid schema for function 't': In context=('properties', 'a'), 'format' is not permitted.",
        ),
        _ => answer(upstream.protocol),
    }
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(router: Router) -> Server {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { url, task }
}

struct Fixture {
    gateway: Server,
    _upstream: Server,
    seen: Arc<Mutex<Vec<Value>>>,
    client: reqwest::Client,
}

impl Fixture {
    async fn new(protocol: Protocol, provider_id: &str, model: &str, behavior: Behavior) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = serve(Router::new().fallback(post(upstream)).with_state(Upstream {
            protocol,
            behavior,
            seen: seen.clone(),
        }))
        .await;
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(
                Config {
                    token: TOKEN.into(),
                    default_provider: provider_id.into(),
                    provider: vec![ProviderConfig {
                        id: provider_id.into(),
                        name: provider_id.into(),
                        protocol,
                        capabilities: Default::default(),
                        base_url: format!("{}/v1", upstream.url),
                        api_key: KEY.into(),
                        api_keys: Vec::new(),
                        models: vec![model.into()],
                        contexts: Default::default(),
                        outputs: Default::default(),
                        fallback: Vec::new(),
                        key_cooldown_ms: None,
                        routing: RoutingPolicy::Rotate,
                        weights: Vec::new(),
                        affinity: Default::default(),
                        max_concurrency: None,
                        max_rpm: None,
                        queue_limit: None,
                        queue_wait_s: None,
                        key_generation: 0,
                        key_limits: Vec::new(),
                    }],
                    ..Config::default()
                },
                None,
            ),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: Arc::new(Mutex::new(RequestLog::default())),
            key_rotation: KeyRotator::default(),
            affinity: AffinityStore::default(),
            lanes: magpie_gateway::lanes::Lanes::default(),
            rpms: magpie_gateway::lanes::Rpms::default(),
        });
        let gateway = serve(build_router(state)).await;
        Self {
            gateway,
            _upstream: upstream,
            seen,
            client: reqwest::Client::new(),
        }
    }

    fn request(protocol: Protocol, model: &str) -> Value {
        let parameters = json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": {"prompt": {"type": "string"}, "mode": {"type": "string"}},
                    "required": ["prompt"]
                },
                {
                    "type": "object",
                    "properties": {"prompt": {"type": "string"}, "path": {"type": "string"}},
                    "required": ["prompt"]
                }
            ]
        });
        match protocol {
            Protocol::Chat => json!({
                "model": model,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function", "function": {"name": "t", "parameters": parameters}}]
            }),
            Protocol::Responses => json!({
                "model": model,
                "stream": false,
                "input": "hi",
                "tools": [{"type": "function", "name": "t", "parameters": parameters}]
            }),
            _ => unreachable!(),
        }
    }

    async fn ask(&self, protocol: Protocol, model: &str) -> (StatusCode, String) {
        let path = match protocol {
            Protocol::Chat => "/v1/chat/completions",
            Protocol::Responses => "/v1/responses",
            _ => unreachable!(),
        };
        let response = self
            .client
            .post(format!("{}{path}", self.gateway.url))
            .bearer_auth(TOKEN)
            .json(&Self::request(protocol, model))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.text().await.unwrap())
    }
}

fn seen_parameters(seen: &[Value], protocol: Protocol, at: usize) -> &Value {
    parameters(&seen[at], protocol).expect("tool parameters")
}

#[tokio::test]
async fn a_chat_root_union_is_folded_once_and_remembered() {
    let protocol = Protocol::Chat;
    let model = "chat-root-union-model";
    let fixture = Fixture::new(
        protocol,
        "chat-root-union-provider",
        model,
        Behavior::RefuseRootUnion(OPENAI_CHAT_REFUSAL),
    )
    .await;

    let (status, body) = fixture
        .ask(protocol, &format!("chat-root-union-provider/{model}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen_parameters(&seen, protocol, 0)["anyOf"].is_array());
    let folded = seen_parameters(&seen, protocol, 1);
    assert_eq!(folded["type"], "object");
    assert!(folded.get("anyOf").is_none());
    assert_eq!(folded["properties"]["prompt"]["type"], "string");
    assert_eq!(folded["properties"]["mode"]["type"], "string");
    assert_eq!(folded["properties"]["path"]["type"], "string");

    let (status, body) = fixture
        .ask(protocol, &format!("chat-root-union-provider/{model}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(seen_parameters(&seen, protocol, 2).get("anyOf").is_none());
}

#[tokio::test]
async fn openais_responses_wording_is_folded() {
    let protocol = Protocol::Responses;
    let model = "responses-root-union-model";
    let fixture = Fixture::new(
        protocol,
        "responses-root-union-provider",
        model,
        Behavior::RefuseRootUnion(OPENAI_RESPONSES_REFUSAL),
    )
    .await;

    let (status, body) = fixture
        .ask(protocol, &format!("responses-root-union-provider/{model}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen_parameters(&seen, protocol, 0)["anyOf"].is_array());
    assert_eq!(seen_parameters(&seen, protocol, 1)["type"], "object");
}

#[tokio::test]
async fn a_vendor_that_accepts_the_union_is_not_rewritten() {
    let protocol = Protocol::Chat;
    let model = "accepting-root-union-model";
    let fixture = Fixture::new(
        protocol,
        "accepting-root-union-provider",
        model,
        Behavior::AcceptUnion,
    )
    .await;

    let (status, body) = fixture
        .ask(protocol, &format!("accepting-root-union-provider/{model}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(seen_parameters(&seen, protocol, 0)["anyOf"].is_array());
}

#[tokio::test]
async fn xais_wording_is_folded_too() {
    let protocol = Protocol::Responses;
    let model = "xai-wording-root-union-model";
    let fixture = Fixture::new(
        protocol,
        "xai-wording-root-union-provider",
        model,
        Behavior::RefuseRootUnion(XAI_REFUSAL),
    )
    .await;

    let (status, body) = fixture
        .ask(
            protocol,
            &format!("xai-wording-root-union-provider/{model}"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen_parameters(&seen, protocol, 1)["type"], "object");
}

#[tokio::test]
async fn a_successful_folded_retry_is_remembered() {
    let protocol = Protocol::Chat;
    let model = "transient-root-union-model";
    let fixture = Fixture::new(
        protocol,
        "transient-root-union-provider",
        model,
        Behavior::RefuseRootUnionOnFirstOnly(OPENAI_CHAT_REFUSAL),
    )
    .await;

    let model = format!("transient-root-union-provider/{model}");
    let (status, body) = fixture.ask(protocol, &model).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = fixture.ask(protocol, &model).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(seen_parameters(&seen, protocol, 2).get("anyOf").is_none());
}

#[tokio::test]
async fn an_unrelated_invalid_schema_is_not_retried() {
    let protocol = Protocol::Chat;
    let model = "unrelated-root-union-model";
    let fixture = Fixture::new(
        protocol,
        "unrelated-root-union-provider",
        model,
        Behavior::UnrelatedInvalidSchema,
    )
    .await;

    let (status, _) = fixture
        .ask(protocol, &format!("unrelated-root-union-provider/{model}"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fixture.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_folded_retry_that_fails_another_shape_keeps_the_root_union_refusal() {
    let protocol = Protocol::Chat;
    let model = "folded-shape-root-union-model";
    let refusal = OPENAI_CHAT_REFUSAL;
    let fixture = Fixture::new(
        protocol,
        "folded-shape-root-union-provider",
        model,
        Behavior::RefuseRootUnionThenShape(refusal),
    )
    .await;

    let (status, body) = fixture
        .ask(
            protocol,
            &format!("folded-shape-root-union-provider/{model}"),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("schema must have type 'object'"), "{body}");
    assert!(!body.contains("shape_probe"), "{body}");
    assert_eq!(fixture.seen.lock().unwrap().len(), 2);
}
