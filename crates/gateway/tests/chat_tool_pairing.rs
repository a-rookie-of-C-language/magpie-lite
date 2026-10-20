//! Real HTTP requests from all four dialects to a strict local Chat provider.
//! Fixtures only: no live Agent configuration or external inference calls.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, GroupConfig, KeyRotator, Protocol,
    ProviderConfig, RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const PRIVATE: &str = "PRIVATE_COMPACTED_TOOL_RESULT";
const TOKEN: &str = "PRIVATE_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_PROVIDER_KEY";

struct Server {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { base, task }
}

// Enforce complete, adjacent, ordered, nonempty and single-use call/output ids.
fn valid(messages: &[Value]) -> bool {
    let mut expected = std::collections::VecDeque::new();
    let mut used = HashSet::new();
    for message in messages {
        if message["role"] == "tool" {
            let Some(id) = message["tool_call_id"].as_str().filter(|id| !id.is_empty()) else {
                return false;
            };
            if expected.pop_front().as_deref() != Some(id) {
                return false;
            }
        } else {
            if !expected.is_empty() {
                return false;
            }
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) else {
                        return false;
                    };
                    if !used.insert(id.to_owned()) {
                        return false;
                    }
                    expected.push_back(id.to_owned());
                }
            }
        }
    }
    expected.is_empty()
}

async fn strict_chat(
    State(seen): State<Arc<Mutex<Vec<Value>>>>,
    Json(request): Json<Value>,
) -> Response {
    seen.lock().unwrap().push(request.clone());
    if !valid(request["messages"].as_array().unwrap()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"invalid tool transcript"}})),
        )
            .into_response();
    }
    if request["stream"] == true {
        let wire = concat!(
            "data: {\"id\":\"paired\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"paired answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"paired\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        return ([("content-type", "text/event-stream")], wire).into_response();
    }
    Json(
        json!({"id":"paired","model":"mock-model","choices":[{"index":0,
        "message":{"role":"assistant","content":"paired answer"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":2}}),
    )
    .into_response()
}

#[derive(Clone, Copy)]
enum Piece {
    Text(&'static str),
    Call(&'static str),
    Result(&'static str, &'static str),
}

fn transcript(ambiguous: bool) -> Vec<(bool, Vec<Piece>)> {
    use Piece::*;
    if ambiguous {
        return vec![
            (
                true,
                vec![Call("PRIVATE_AMBIGUOUS_ID"), Call("PRIVATE_AMBIGUOUS_ID")],
            ),
            (false, vec![Result("PRIVATE_AMBIGUOUS_ID", PRIVATE)]),
        ];
    }
    vec![
        (false, vec![Result("orphan", PRIVATE)]),
        (true, vec![Text("parallel "), Call("A")]),
        (true, vec![Text("work"), Call("B")]),
        (false, vec![Result("B", "two"), Result("B", "duplicate")]),
        (false, vec![Result("A", "one"), Text("attachment")]),
        (true, vec![Call("C")]),
        (false, vec![Text("interrupted")]),
        (false, vec![Result("C", "late real result")]),
        (true, vec![Call("A")]), // a later exchange reuses an Agent's id
        (false, vec![Result("A", "second exchange")]),
        (false, vec![Text("thanks")]),
    ]
}

// Construct native bodies directly, not through the codecs under test.
fn request(protocol: Protocol, model: &str, streaming: bool, ambiguous: bool) -> Value {
    use Piece::*;
    let turns = transcript(ambiguous);
    match protocol {
        Protocol::Anthropic => json!({"model":model,"stream":streaming,"max_tokens":64,
            "messages":turns.iter().map(|(assistant,pieces)| json!({
                "role":if *assistant { "assistant" } else { "user" },
                "content":pieces.iter().map(|piece| match piece {
                    Text(s) => json!({"type":"text","text":s}),
                    Call(id) => json!({"type":"tool_use","id":id,"name":"run","input":{"cmd":id}}),
                    Result(id,s) => json!({"type":"tool_result","tool_use_id":id,"content":s}),
                }).collect::<Vec<_>>()
            })).collect::<Vec<_>>() }),
        Protocol::Gemini => json!({"contents":turns.iter().map(|(assistant,pieces)| json!({
            "role":if *assistant { "model" } else { "user" },
            "parts":pieces.iter().map(|piece| match piece {
                Text(s) => json!({"text":s}),
                Call(id) => json!({"functionCall":{"id":id,"name":"run","args":{"cmd":id}}}),
                Result(id,s) => json!({"functionResponse":{"id":id,"name":"run","response":{"output":s}}}),
            }).collect::<Vec<_>>()
        })).collect::<Vec<_>>() }),
        Protocol::Responses => {
            let mut input = Vec::new();
            for (assistant, pieces) in turns {
                for piece in pieces {
                    input.push(match piece {
                        Text(s) => json!({"type":"message","role":if assistant {"assistant"} else {"user"},"content":s}),
                        Call(id) => json!({"type":"function_call","call_id":id,"name":"run","arguments":json!({"cmd":id}).to_string()}),
                        Result(id,s) => json!({"type":"function_call_output","call_id":id,"output":s}),
                    });
                }
            }
            json!({"model":model,"stream":streaming,"input":input})
        }
        Protocol::Chat => {
            let mut messages = Vec::new();
            for (assistant, pieces) in turns {
                let mut content = String::new();
                let mut calls = Vec::new();
                for piece in pieces {
                    match piece {
                        Text(s) => content.push_str(s),
                        Call(id) => calls.push(json!({"id":id,"type":"function","function":{"name":"run","arguments":json!({"cmd":id}).to_string()}})),
                        Result(id,s) => messages.push(json!({"role":"tool","tool_call_id":id,"content":s})),
                    }
                }
                if assistant {
                    messages.push(json!({"role":"assistant","content":if content.is_empty() { Value::Null } else { json!(content) },"tool_calls":calls}));
                } else if !content.is_empty() {
                    messages.push(json!({"role":"user","content":content}));
                }
            }
            json!({"model":model,"stream":streaming,"messages":messages})
        }
    }
}

fn path(protocol: Protocol, model: &str, streaming: bool) -> String {
    match protocol {
        Protocol::Chat => "/v1/chat/completions".into(),
        Protocol::Anthropic => "/v1/messages".into(),
        Protocol::Responses => "/v1/responses".into(),
        Protocol::Gemini => format!(
            "/v1beta/models/{model}:{}",
            if streaming {
                "streamGenerateContent"
            } else {
                "generateContent"
            }
        ),
    }
}

struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    history: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    log: Arc<Mutex<RequestLog>>,
}
async fn fixture() -> Fixture {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = serve(
        Router::new()
            .route("/v1/chat/completions", post(strict_chat))
            .with_state(seen.clone()),
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let history = home.path().join("history.jsonl");
    let config = Config {
        token: TOKEN.into(),
        default_provider: "mock".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        vision: None,
        history_path: Some(history.clone()),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Chat,
            capabilities: Default::default(),
            base_url: format!("{}/v1", upstream.base),
            api_key: KEY.into(),
            api_keys: vec![],
            models: vec!["mock-model".into()],
            contexts: Default::default(),
            outputs: Default::default(),
            free_models: Default::default(),
            fallback: vec![],
            key_cooldown_ms: None,
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
        group: vec![GroupConfig {
            id: "daily".into(),
            name: "Daily".into(),
            routes: vec!["mock/mock-model".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Rotate,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s: None,
        }],
        ..Config::default()
    };
    let log = Arc::new(Mutex::new(RequestLog::open(&history).unwrap()));
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: log.clone(),
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let gateway = serve(build_router(state)).await;
    Fixture {
        gateway,
        _upstream: upstream,
        _home: home,
        history,
        seen,
        log,
    }
}

#[tokio::test]
async fn all_clients_repair_tools_over_direct_and_group_chat_routes() {
    let fixture = fixture().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut completed = 0;
    for protocol in [
        Protocol::Chat,
        Protocol::Anthropic,
        Protocol::Responses,
        Protocol::Gemini,
    ] {
        for model in ["mock/mock-model", "group/daily"] {
            for streaming in [false, true] {
                let response = client
                    .post(format!(
                        "{}{}",
                        fixture.gateway.base,
                        path(protocol, model, streaming)
                    ))
                    .bearer_auth(TOKEN)
                    .json(&request(protocol, model, streaming, false))
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let wire = response.text().await.unwrap();
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{protocol:?}/{model}/stream={streaming}: {wire}"
                );
                assert!(wire.contains("paired answer"), "{protocol:?}: {wire}");
                assert!(!wire.contains("invalid tool transcript"));
                completed += 1;
                let upstream = fixture.seen.lock().unwrap().last().unwrap().clone();
                assert!(valid(upstream["messages"].as_array().unwrap()));
                assert_eq!(upstream["model"], "mock-model");
                let messages = upstream["messages"].as_array().unwrap();
                assert_eq!(messages[0]["role"], "user");
                assert_eq!(messages[0]["content"], PRIVATE);
                let parallel = messages
                    .iter()
                    .find(|m| {
                        m["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| calls.len() == 2)
                    })
                    .unwrap();
                assert_eq!(parallel["tool_calls"][0]["id"], "A");
                assert_eq!(parallel["tool_calls"][1]["id"], "B");
                let assistant_text = messages
                    .iter()
                    .filter(|m| m["role"] == "assistant")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<String>();
                assert_eq!(assistant_text, "parallel work");
                for (id, content) in [("A", "one"), ("B", "two")] {
                    assert_eq!(
                        messages
                            .iter()
                            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
                            .unwrap()["content"],
                        content
                    );
                }
                assert!(!upstream.to_string().contains("duplicate"));
                let c = messages
                    .iter()
                    .find(|m| m["role"] == "tool" && m["tool_call_id"] == "C")
                    .unwrap();
                assert!(c["content"].as_str().unwrap().contains("unavailable"));
                assert!(messages.iter().any(|m| m["role"] == "user"
                    && m["content"].as_str().is_some_and(
                        |s| s.contains("late real result") && s.contains("interrupted")
                    )));
                let snapshot = fixture.log.lock().unwrap().snapshot();
                assert_eq!(snapshot.total_requests, completed);
                let record = snapshot.recent_requests.last().unwrap();
                assert_eq!(record.status, "ok");
                assert_eq!(record.requested_model, model);
                assert_eq!(record.usage.input, Some(5));
                assert_eq!(record.usage.output, Some(2));
                assert_eq!(snapshot.providers["mock"].keys["0"].attempts, completed);
            }
        }
    }
    let history = std::fs::read_to_string(&fixture.history).unwrap();
    for secret in [PRIVATE, TOKEN, KEY, "late real result", "parallel work"] {
        assert!(!history.contains(secret), "history leaked {secret}");
    }
    assert_eq!(history.lines().count(), completed as usize);
}

#[tokio::test]
async fn ambiguous_parallel_ids_fail_locally_without_inference_or_private_error_details() {
    let fixture = fixture().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    for protocol in [
        Protocol::Chat,
        Protocol::Anthropic,
        Protocol::Responses,
        Protocol::Gemini,
    ] {
        for streaming in [false, true] {
            let model = "mock/mock-model";
            let response = client
                .post(format!(
                    "{}{}",
                    fixture.gateway.base,
                    path(protocol, model, streaming)
                ))
                .bearer_auth(TOKEN)
                .json(&request(protocol, model, streaming, true))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let wire = response.text().await.unwrap();
            assert!(!status.is_success(), "{protocol:?}: {wire}");
            assert!(wire.contains("ambiguous"), "{protocol:?}: {wire}");
            assert!(!wire.contains("PRIVATE_"), "{protocol:?}: {wire}");
            assert!(fixture.seen.lock().unwrap().is_empty());
        }
    }
    let history = std::fs::read_to_string(&fixture.history).unwrap();
    assert!(!history.contains("PRIVATE_"));
}
