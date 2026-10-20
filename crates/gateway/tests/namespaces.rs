// Request-scoped namespace restoration through real loopback JSON/SSE gateways.
// Payloads are authored in each wire dialect, not generated with core codecs.
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
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const TOKEN: &str = "namespace-fixture-token";
const KEY: &str = "namespace-fixture-key";
const NAMES: [&str; 4] = ["teamA__run", "teamB__run", "ordinary", "unknown__run"];
const PROTOCOLS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Responses,
    Protocol::Anthropic,
    Protocol::Gemini,
];

struct Server {
    url: String,
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
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, task }
}
fn args(i: usize) -> Value {
    json!({"message": format!("private-namespace-argument-{i}")})
}
fn call(i: usize, native: bool) -> Value {
    let mut v = json!({"type":"function_call","id":format!("fc_{i}"),"call_id":format!("call_{i}"),"name":NAMES[i],"arguments":args(i).to_string(),"status":"completed"});
    if native && i < 2 {
        v["name"] = json!("run");
        v["namespace"] = json!(if i == 0 { "teamA" } else { "teamB" });
    }
    v
}
fn reply(p: Protocol, native: bool) -> Value {
    match p {
        Protocol::Chat => {
            json!({"id":"r","model":"up-model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":(0..4).map(|i| json!({"id":format!("call_{i}"),"type":"function","function":{"name":NAMES[i],"arguments":args(i).to_string()}})).collect::<Vec<_>>()},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
        }
        Protocol::Responses => {
            json!({"id":"r","model":"up-model","status":"completed","output":(0..4).map(|i| call(i,native)).collect::<Vec<_>>(),"usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Anthropic => {
            json!({"id":"r","type":"message","role":"assistant","model":"up-model","content":(0..4).map(|i| json!({"type":"tool_use","id":format!("call_{i}"),"name":NAMES[i],"input":args(i)})).collect::<Vec<_>>(),"stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Gemini => {
            json!({"responseId":"r","modelVersion":"up-model","candidates":[{"index":0,"content":{"role":"model","parts":(0..4).map(|i| json!({"functionCall":{"id":format!("call_{i}"),"name":NAMES[i],"args":args(i)}})).collect::<Vec<_>>()},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}})
        }
    }
}
fn stream_reply(p: Protocol, native: bool) -> String {
    let mut events = Vec::new();
    match p {
        Protocol::Chat => {
            // Start parallel calls first, then interleave argument fragments.
            events.push(json!({"id":"r","model":"up-model","choices":[{"index":0,"delta":{"tool_calls":(0..4).map(|i| json!({"index":i,"id":format!("call_{i}"),"type":"function","function":{"name":NAMES[i],"arguments":""}})).collect::<Vec<_>>()}}]}));
            for half in 0..2 {
                for i in 0..4 {
                    let a = args(i).to_string();
                    let mid = a.len() / 2;
                    events.push(json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":i,"function":{"arguments":if half == 0 { &a[..mid] } else { &a[mid..] }}}]}}]}));
                }
            }
            events.push(json!({"id":"r","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}));
        }
        Protocol::Responses => {
            events.push(json!({"type":"response.created","response":{"id":"r","model":"up-model","status":"in_progress"}}));
            for i in 0..4 {
                let mut item = call(i, native);
                item["arguments"] = json!("");
                item["status"] = json!("in_progress");
                events.push(
                    json!({"type":"response.output_item.added","output_index":i,"item":item}),
                );
            }
            for half in 0..2 {
                for i in 0..4 {
                    let a = args(i).to_string();
                    let mid = a.len() / 2;
                    events.push(json!({"type":"response.function_call_arguments.delta","output_index":i,"item_id":format!("fc_{i}"),"delta":if half == 0 { &a[..mid] } else { &a[mid..] }}));
                }
            }
            // No item.done: the encoder must close outstanding calls at terminal.
            events.push(json!({"type":"response.completed","response":reply(p,native)}));
        }
        Protocol::Anthropic => {
            events.push(json!({"type":"message_start","message":{"id":"r","model":"up-model","usage":{"input_tokens":5,"output_tokens":0}}}));
            for (i, name) in NAMES.iter().enumerate() {
                events.push(json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":format!("call_{i}"),"name":name,"input":{}}}));
            }
            for half in 0..2 {
                for i in 0..4 {
                    let a = args(i).to_string();
                    let mid = a.len() / 2;
                    events.push(json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":if half == 0 { &a[..mid] } else { &a[mid..] }}}));
                }
            }
            for i in (0..4).rev() {
                events.push(json!({"type":"content_block_stop","index":i}));
            }
            events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}));
            events.push(json!({"type":"message_stop"}));
        }
        Protocol::Gemini => events.push(reply(p, native)),
    }
    let mut wire: String = events
        .iter()
        .map(|v| {
            if matches!(p, Protocol::Responses | Protocol::Anthropic) {
                format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())
            } else {
                format!("data: {v}\n\n")
            }
        })
        .collect();
    if p == Protocol::Chat {
        wire.push_str("data: [DONE]\n\n");
    }
    wire
}
#[derive(Clone)]
struct Mock {
    p: Protocol,
    native: bool,
    fail: bool,
    seen: Arc<Mutex<Vec<Value>>>,
}
async fn upstream(
    State(mock): State<Mock>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    let stream = body["stream"] == true || uri.path().ends_with(":streamGenerateContent");
    mock.seen.lock().unwrap().push(body);
    if mock.fail {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":{"message":"fixture unavailable"}})),
        )
            .into_response();
    }
    if stream {
        return (
            [("content-type", "text/event-stream")],
            stream_reply(mock.p, mock.native),
        )
            .into_response();
    }
    Json(reply(mock.p, mock.native)).into_response()
}
struct Fixture {
    gateway: Server,
    _upstreams: Vec<Server>,
    _home: tempfile::TempDir,
    state: Arc<AppState>,
    seen: Vec<Arc<Mutex<Vec<Value>>>>,
    history: std::path::PathBuf,
    http: reqwest::Client,
}
impl Fixture {
    async fn new(protocols: &[Protocol], native: bool, fail_primary: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let history = home.path().join("history.jsonl");
        let mut upstreams = Vec::new();
        let mut providers = Vec::new();
        let mut seen = Vec::new();
        for (i, &p) in protocols.iter().enumerate() {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let server = serve(Router::new().fallback(post(upstream)).with_state(Mock {
                p,
                native,
                fail: fail_primary && i == 0,
                seen: captured.clone(),
            }))
            .await;
            providers.push(ProviderConfig {
                id: format!("p{i}"),
                name: format!("Mock {i}"),
                protocol: p,
                capabilities: Default::default(),
                base_url: match p {
                    Protocol::Anthropic => server.url.clone(),
                    Protocol::Gemini => format!("{}/v1beta", server.url),
                    _ => format!("{}/v1", server.url),
                },
                api_key: KEY.into(),
                api_keys: vec![],
                models: vec!["up-model".into()],
                contexts: Default::default(),
                fallback: if i == 0 && protocols.len() > 1 {
                    vec!["p1/up-model".into()]
                } else {
                    vec![]
                },
                key_cooldown_ms: None,
                routing: RoutingPolicy::Rotate,
                weights: Vec::new(),
                affinity: Default::default(),
                max_concurrency: None,
                max_rpm: None,
                queue_limit: None,
                queue_wait_s: None,
                key_generation: 0,
            });
            seen.push(captured);
            upstreams.push(server);
        }
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(
                Config {
                    token: TOKEN.into(),
                    default_provider: "p0".into(),
                    hidden_auto_groups: Vec::new(),
                    no_auto_groups: false,
                    history_path: Some(history.clone()),
                    provider: providers,
                    group: vec![GroupConfig {
                        id: "daily".into(),
                        name: "Daily".into(),
                        routes: (0..protocols.len())
                            .map(|i| format!("p{i}/up-model"))
                            .collect(),
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
                    ..Config::default()
                },
                None,
            ),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: Arc::new(Mutex::new(RequestLog::open(&history).unwrap())),
            key_rotation: KeyRotator::default(),
            affinity: AffinityStore::default(),
            lanes: magpie_gateway::lanes::Lanes::default(),
            rpms: magpie_gateway::lanes::Rpms::default(),
        });
        let gateway = serve(build_router(state.clone())).await;
        Self {
            gateway,
            _upstreams: upstreams,
            _home: home,
            state,
            seen,
            history,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
        }
    }
    async fn send(&self, mut body: Value, model: &str, stream: bool) -> String {
        body["model"] = json!(model);
        body["stream"] = json!(stream);
        let response = self
            .http
            .post(format!("{}/v1/responses", self.gateway.url))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let wire = response.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{wire}");
        wire
    }
    fn assert_private(&self, requests: u64) {
        let snapshot = self.state.requests.lock().unwrap().snapshot();
        assert_eq!(snapshot.total_requests, requests);
        assert!(
            snapshot
                .recent_requests
                .iter()
                .all(|r| r.status == "ok" && r.usage.input == Some(5) && r.usage.output == Some(2))
        );
        let history = std::fs::read_to_string(&self.history).unwrap();
        let stats = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(history.lines().count(), requests as usize);
        for private in [
            TOKEN,
            KEY,
            "private-namespace-argument",
            "private-namespace-input",
            "private-namespace-description",
            "teamA__run",
            "teamB__run",
            "unknown__run",
        ] {
            assert!(!history.contains(private), "{private}");
            assert!(!stats.contains(private), "{private}");
        }
    }
}
fn request(namespaced: bool) -> Value {
    let tools = if namespaced {
        json!([{"type":"namespace","name":"teamA","tools":[{"type":"function","name":"run","description":"private-namespace-description","parameters":{"type":"object"}}]},{"type":"namespace","name":"teamB","tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]},{"type":"function","name":"ordinary","parameters":{"type":"object"}}])
    } else {
        json!([{"type":"function","name":"ordinary","parameters":{"type":"object"}}])
    };
    json!({"input":"private-namespace-input","tools":tools})
}
fn values(wire: &str, stream: bool) -> Vec<Value> {
    if stream {
        wire.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|s| *s != "[DONE]")
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    } else {
        vec![serde_json::from_str(wire).unwrap()]
    }
}
fn assert_call(item: &Value, namespaced: bool, complete: bool) {
    let i = item["call_id"]
        .as_str()
        .unwrap()
        .strip_prefix("call_")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(item["type"], "function_call");
    if namespaced && i < 2 {
        assert_eq!(item["name"], "run");
        assert_eq!(item["namespace"], if i == 0 { "teamA" } else { "teamB" });
        // Synthesized plaintext arguments must not look sealed to Codex.
        assert_eq!(item["encrypted_function_args"], json!([]));
    } else {
        assert_eq!(item["name"], NAMES[i]);
        assert!(item.get("namespace").is_none());
        assert!(item.get("encrypted_function_args").is_none());
    }
    if complete {
        assert_eq!(
            serde_json::from_str::<Value>(item["arguments"].as_str().unwrap()).unwrap(),
            args(i)
        );
    }
}
fn assert_response(wire: &str, stream: bool, namespaced: bool) -> Vec<Value> {
    let frames = values(wire, stream);
    let output = if stream {
        assert_eq!(
            frames
                .iter()
                .filter(|v| v["type"] == "response.completed")
                .count(),
            1
        );
        for typ in ["response.output_item.added", "response.output_item.done"] {
            let items: Vec<_> = frames
                .iter()
                .filter(|v| v["type"] == typ && v["item"]["type"] == "function_call")
                .collect();
            assert_eq!(items.len(), 4, "{wire}");
            for frame in items {
                assert_call(&frame["item"], namespaced, typ.ends_with("done"));
            }
        }
        frames
            .iter()
            .find(|v| v["type"] == "response.completed")
            .unwrap()["response"]["output"]
            .as_array()
            .unwrap()
    } else {
        frames[0]["output"].as_array().unwrap()
    };
    assert_eq!(output.len(), 4);
    for item in output {
        assert_call(item, namespaced, true);
    }
    output.clone()
}
fn offered_names(p: Protocol, body: &Value) -> Vec<String> {
    match p {
        Protocol::Chat => body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect(),
        Protocol::Anthropic => body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect(),
        Protocol::Gemini => body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect(),
        Protocol::Responses => body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| {
                if t["type"] == "namespace" {
                    t["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|f| {
                            format!(
                                "{}__{}",
                                t["name"].as_str().unwrap(),
                                f["name"].as_str().unwrap()
                            )
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![t["name"].as_str().unwrap().to_owned()]
                }
            })
            .collect(),
    }
}
#[tokio::test]
async fn namespaces_survive_four_upstreams_json_sse_direct_group_and_history() {
    for p in PROTOCOLS {
        let fixture = Fixture::new(&[p], true, false).await;
        let mut count = 0;
        for model in ["p0/up-model", "group/daily"] {
            for stream in [false, true] {
                let wire = fixture.send(request(true), model, stream).await;
                let mut history = assert_response(&wire, stream, true);
                for i in 0..4 {
                    history.push(json!({"type":"function_call_output","call_id":format!("call_{i}"),"output":"completed"}));
                }
                let mut followup = request(true);
                followup["input"] = json!(history);
                let wire = fixture.send(followup, model, stream).await;
                assert_response(&wire, stream, true);
                count += 2;
                let seen = fixture.seen[0].lock().unwrap();
                for body in &*seen {
                    assert_eq!(offered_names(p, body), NAMES[..3]);
                }
                let body = seen.last().unwrap();
                let encoded = body.to_string();
                if p == Protocol::Responses {
                    let calls: Vec<&Value> = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| item["type"] == "function_call")
                        .collect();
                    assert_eq!(calls.len(), 4);
                    for (i, expected) in [
                        ("run", Some("teamA")),
                        ("run", Some("teamB")),
                        ("ordinary", None),
                        ("unknown__run", None),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let call = calls
                            .iter()
                            .find(|c| c["call_id"] == format!("call_{i}"))
                            .unwrap_or_else(|| panic!("missing call_{i}"));
                        assert_eq!(call["name"], expected.0);
                        assert_eq!(call.get("namespace").and_then(Value::as_str), expected.1);
                    }
                } else {
                    assert!(encoded.contains("teamA__run"));
                    assert!(!encoded.contains("\"namespace\""));
                }
            }
        }
        fixture.assert_private(count);
    }
}
#[tokio::test]
async fn namespace_bindings_are_request_local_without_guessing_flat_names() {
    // Native Responses upstreams sometimes return flat names too. A request
    // without namespace declarations must leave those identical flat names alone.
    let fixture = Fixture::new(&[Protocol::Responses], false, false).await;
    for stream in [false, true] {
        let (named, plain) = tokio::join!(
            fixture.send(request(true), "p0/up-model", stream),
            fixture.send(request(false), "p0/up-model", stream)
        );
        assert_response(&named, stream, true);
        assert_response(&plain, stream, false);
    }
    fixture.assert_private(4);
}
#[tokio::test]
async fn namespaces_are_restored_after_cross_protocol_fallback() {
    for p in [Protocol::Chat, Protocol::Anthropic, Protocol::Gemini] {
        for stream in [false, true] {
            let fixture = Fixture::new(&[p, Protocol::Responses], true, true).await;
            for model in ["p0/up-model", "group/daily"] {
                let wire = fixture.send(request(true), model, stream).await;
                assert_response(&wire, stream, true);
                let seen = fixture.seen[1].lock().unwrap();
                assert!(
                    seen.last().unwrap()["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["type"] == "namespace")
                );
            }
            assert!(!fixture.seen[0].lock().unwrap().is_empty());
            fixture.assert_private(2);
        }
    }
}
