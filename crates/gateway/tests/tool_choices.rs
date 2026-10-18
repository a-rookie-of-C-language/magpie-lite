// Real loopback policy checks: native requests and replies, not codec-built fixtures.
// No installed Agent configuration or paid provider calls are used.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, KeyRotator, Protocol, ProviderConfig,
    RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const TOKEN: &str = "PRIVATE_TOOL_CHOICE_GATEWAY_TOKEN";
const KEYS: [&str; 2] = ["PRIVATE_TOOL_CHOICE_KEY_ONE", "PRIVATE_TOOL_CHOICE_KEY_TWO"];
const INPUT: &str = "PRIVATE_TOOL_CHOICE_INPUT";
const DESCRIPTION: &str = "PRIVATE_TOOL_CHOICE_DESCRIPTION";
const UNKNOWN: &str = "PRIVATE_UNDECLARED_TOOL";
const ANSWER: &str = "choice accepted";
const PROTOCOLS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Responses,
    Protocol::Anthropic,
    Protocol::Gemini,
];
const NAMES: [&str; 3] = ["safeA", "forbidden", "safeB"];

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

fn choice(protocol: Protocol, mode: &str) -> Value {
    match (protocol, mode) {
        (Protocol::Chat | Protocol::Responses, "auto" | "none" | "required") => json!(mode),
        (Protocol::Chat, "named") => json!({"type":"function","function":{"name":"safeB"}}),
        (Protocol::Responses, "named") => json!({"type":"function","name":"safeB"}),
        (Protocol::Anthropic, "named") => json!({"type":"tool","name":"safeB"}),
        (Protocol::Anthropic, "required") => json!({"type":"any"}),
        (Protocol::Anthropic, "auto" | "none") => json!({"type":mode}),
        (Protocol::Gemini, "named") => {
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeB"]}})
        }
        (Protocol::Gemini, "auto") => json!({"functionCallingConfig":{"mode":"AUTO"}}),
        (Protocol::Gemini, "none") => json!({"functionCallingConfig":{"mode":"NONE"}}),
        (Protocol::Gemini, "required") => json!({"functionCallingConfig":{"mode":"ANY"}}),
        _ => unreachable!(),
    }
}
fn selection(body: &Value, protocol: Protocol) -> &Value {
    &body[if protocol == Protocol::Gemini {
        "toolConfig"
    } else {
        "tool_choice"
    }]
}
fn request(protocol: Protocol, model: &str, stream: bool, selector: &Value, search: bool) -> Value {
    let mut body = match protocol {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":INPUT}],"tools": NAMES.map(|name| json!({"type":"function","function":{"name":name,"description":DESCRIPTION,"parameters":{"type":"object"}}})),"tool_choice":selector})
        }
        Protocol::Responses => {
            json!({"model":model,"stream":stream,"input":INPUT,"tools":NAMES.map(|name| json!({"type":"function","name":name,"description":DESCRIPTION,"parameters":{"type":"object"}})),"tool_choice":selector})
        }
        Protocol::Anthropic => {
            json!({"model":model,"stream":stream,"max_tokens":64,"messages":[{"role":"user","content":INPUT}],"tools":NAMES.map(|name| json!({"name":name,"description":DESCRIPTION,"input_schema":{"type":"object"}})),"tool_choice":selector})
        }
        Protocol::Gemini => {
            json!({"contents":[{"role":"user","parts":[{"text":INPUT}]}],"tools":[{"functionDeclarations":NAMES.map(|name| json!({"name":name,"description":DESCRIPTION,"parameters":{"type":"object"}}))}],"toolConfig":selector})
        }
    };
    if search {
        match protocol {
            Protocol::Responses => body["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"web_search"})),
            Protocol::Gemini => body["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"googleSearch":{}})),
            _ => unreachable!(),
        }
    }
    body
}
fn names(body: &Value, protocol: Protocol) -> Vec<&str> {
    let tools = body["tools"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    match protocol {
        Protocol::Gemini => tools
            .iter()
            .filter_map(|t| t["functionDeclarations"].as_array())
            .flatten()
            .filter_map(|t| t["name"].as_str())
            .collect(),
        Protocol::Chat => tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect(),
        _ => tools.iter().filter_map(|t| t["name"].as_str()).collect(),
    }
}
fn reply(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => {
            json!({"id":"choice","model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":ANSWER},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
        }
        Protocol::Responses => {
            json!({"id":"choice","object":"response","model":"up-model","status":"completed","output":[{"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":ANSWER,"annotations":[]}]}],"usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Anthropic => {
            json!({"id":"choice","type":"message","role":"assistant","model":"up-model","content":[{"type":"text","text":ANSWER}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Gemini => {
            json!({"responseId":"choice","modelVersion":"up-model","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":ANSWER}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}})
        }
    }
}
fn stream_reply(protocol: Protocol) -> String {
    let events = match protocol {
        Protocol::Chat => vec![
            json!({"id":"choice","model":"up-model","choices":[{"index":0,"delta":{"content":ANSWER},"finish_reason":null}]}),
            json!({"id":"choice","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}),
        ],
        Protocol::Responses => vec![
            json!({"type":"response.created","response":{"id":"choice","model":"up-model","status":"in_progress"}}),
            json!({"type":"response.output_text.delta","item_id":"msg","output_index":0,"content_index":0,"delta":ANSWER}),
            json!({"type":"response.completed","response":reply(protocol)}),
        ],
        Protocol::Anthropic => vec![
            json!({"type":"message_start","message":{"id":"choice","model":"up-model","usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":ANSWER}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        Protocol::Gemini => vec![reply(protocol)],
    };
    let mut wire: String = events
        .into_iter()
        .map(|v| {
            if matches!(protocol, Protocol::Anthropic | Protocol::Responses) {
                format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())
            } else {
                format!("data: {v}\n\n")
            }
        })
        .collect();
    if protocol == Protocol::Chat {
        wire.push_str("data: [DONE]\n\n");
    }
    wire
}
#[derive(Clone)]
struct Capture {
    provider: usize,
    body: Value,
    stream: bool,
    key: String,
}
impl std::fmt::Display for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider={} body={}", self.provider, self.body)
    }
}
#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    provider: usize,
    fail: bool,
    seen: Arc<Mutex<Vec<Capture>>>,
}
async fn upstream(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let auth = match mock.protocol {
        Protocol::Chat | Protocol::Responses => headers["authorization"]
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap(),
        Protocol::Anthropic => headers["x-api-key"].to_str().unwrap(),
        Protocol::Gemini => headers["x-goog-api-key"].to_str().unwrap(),
    };
    assert!(KEYS.contains(&auth));
    let stream = match mock.protocol {
        Protocol::Gemini => {
            assert!(body.get("model").is_none());
            assert!(uri.path().starts_with("/v1beta/models/up-model:"));
            uri.path().ends_with(":streamGenerateContent")
        }
        protocol => {
            assert_eq!(body["model"], "up-model");
            assert_eq!(
                uri.path(),
                match protocol {
                    Protocol::Chat => "/v1/chat/completions",
                    Protocol::Responses => "/v1/responses",
                    Protocol::Anthropic => "/v1/messages",
                    _ => unreachable!(),
                }
            );
            body["stream"] == true
        }
    };
    mock.seen.lock().unwrap().push(Capture {
        provider: mock.provider,
        body,
        stream,
        key: auth.into(),
    });
    if mock.fail {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error":{"message":"fixture unavailable"}})),
        )
            .into_response();
    }
    if stream {
        return (
            [("content-type", "text/event-stream")],
            stream_reply(mock.protocol),
        )
            .into_response();
    }
    Json(reply(mock.protocol)).into_response()
}
struct Fixture {
    gateway: Server,
    _upstreams: Vec<Server>,
    _home: tempfile::TempDir,
    history: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Capture>>>,
    state: Arc<AppState>,
    http: reqwest::Client,
}
impl Fixture {
    async fn new(protocols: &[Protocol], fail_primary: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let history = home.path().join("history.jsonl");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut upstreams = Vec::new();
        let mut providers = Vec::new();
        for (index, &protocol) in protocols.iter().enumerate() {
            let server = serve(Router::new().fallback(post(upstream)).with_state(Mock {
                protocol,
                provider: index,
                fail: fail_primary && index == 0,
                seen: seen.clone(),
            }))
            .await;
            providers.push(ProviderConfig {
                id: format!("p{index}"),
                name: format!("Mock {index}"),
                protocol,
                capabilities: Default::default(),
                base_url: match protocol {
                    Protocol::Anthropic => server.url.clone(),
                    Protocol::Gemini => format!("{}/v1beta", server.url),
                    _ => format!("{}/v1", server.url),
                },
                api_key: KEYS[0].into(),
                api_keys: KEYS.iter().map(|s| (*s).into()).collect(),
                models: vec!["up-model".into()],
                fallback: if index == 0 && protocols.len() > 1 {
                    vec!["p1/up-model".into()]
                } else {
                    vec![]
                },
                key_cooldown_ms: None,
                routing: RoutingPolicy::Rotate,
                weights: Vec::new(),
                key_generation: 0,
            });
            upstreams.push(server);
        }
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(
                Config {
                    token: TOKEN.into(),
                    default_provider: "p0".into(),
                    history_path: Some(history.clone()),
                    provider: providers,
                    group: vec![GroupConfig {
                        id: "daily".into(),
                        name: "Daily".into(),
                        routes: (0..protocols.len())
                            .map(|i| format!("p{i}/up-model"))
                            .collect(),
                        routing: RoutingPolicy::Order,
                        first_token_timeout_ms: 0,
                    }],
                    ..Config::default()
                },
                None,
            ),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: Arc::new(Mutex::new(RequestLog::open(&history).unwrap())),
            key_rotation: KeyRotator::default(),
        });
        let gateway = serve(build_router(state.clone())).await;
        Self {
            gateway,
            _upstreams: upstreams,
            _home: home,
            history,
            seen,
            state,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }
    async fn send(
        &self,
        protocol: Protocol,
        model: &str,
        stream: bool,
        selector: &Value,
        search: bool,
    ) -> (StatusCode, String) {
        let path = match protocol {
            Protocol::Chat => "/v1/chat/completions".into(),
            Protocol::Responses => "/v1/responses".into(),
            Protocol::Anthropic => "/v1/messages".into(),
            Protocol::Gemini => format!(
                "/v1beta/models/{model}:{}",
                if stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                }
            ),
        };
        let response = self
            .http
            .post(format!("{}{path}", self.gateway.url))
            .bearer_auth(TOKEN)
            .json(&request(protocol, model, stream, selector, search))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.text().await.unwrap())
    }
    async fn record(&self, count: u64, ok: bool, provider: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .state
                    .requests
                    .lock()
                    .unwrap()
                    .snapshot()
                    .total_requests
                    == count
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = self.state.requests.lock().unwrap().snapshot();
        let record = snapshot.recent_requests.last().unwrap();
        assert_eq!(record.provider, provider);
        assert_eq!(record.status == "ok", ok);
        assert_eq!(record.usage.input, ok.then_some(5));
        assert_eq!(record.usage.output, ok.then_some(2));
        let history = std::fs::read_to_string(&self.history).unwrap();
        let stats = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(history.lines().count(), count as usize);
        for private in [
            TOKEN,
            KEYS[0],
            KEYS[1],
            INPUT,
            DESCRIPTION,
            UNKNOWN,
            ANSWER,
            "allowedFunctionNames",
            "disable_parallel_tool_use",
        ] {
            assert!(
                !history.contains(private),
                "persisted private input {private}"
            );
            assert!(
                !stats.contains(private),
                "stats echoed private input {private}"
            );
        }
    }
    fn assert_no_credentials_used(&self) {
        assert!(self.seen.lock().unwrap().is_empty());
        let snapshot = self.state.requests.lock().unwrap().snapshot();
        assert!(snapshot.providers.values().all(|p| p.keys.is_empty()));
        assert!(
            self.state
                .key_rotation
                .cooldown_snapshot(&self.state.config.current())
                .is_empty()
        );
    }
}
fn assert_reply(protocol: Protocol, stream: bool, status: StatusCode, wire: &str) {
    assert_eq!(status, StatusCode::OK, "{wire}");
    let values: Vec<Value> = if stream {
        wire.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|s| *s != "[DONE]")
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    } else {
        vec![serde_json::from_str(wire).unwrap()]
    };
    let text: String = values
        .iter()
        .flat_map(|v| -> Vec<&str> {
            match (protocol, stream) {
                (Protocol::Chat, _) => v
                    .pointer(if stream {
                        "/choices/0/delta/content"
                    } else {
                        "/choices/0/message/content"
                    })
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect(),
                (Protocol::Anthropic, true) => v
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect(),
                (Protocol::Anthropic, false) => v["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v["text"].as_str())
                    .collect(),
                (Protocol::Responses, true) => {
                    if v["type"] == "response.output_text.delta" {
                        vec![v["delta"].as_str().unwrap()]
                    } else {
                        vec![]
                    }
                }
                (Protocol::Responses, false) => v["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|v| v["content"].as_array().unwrap())
                    .filter_map(|v| v["text"].as_str())
                    .collect(),
                (Protocol::Gemini, _) => v["candidates"][0]["content"]["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["text"].as_str())
                    .collect(),
            }
        })
        .collect();
    assert_eq!(text, ANSWER, "{wire}");
    let (input, output) = match protocol {
        Protocol::Chat => ("/usage/prompt_tokens", "/usage/completion_tokens"),
        Protocol::Anthropic | Protocol::Responses => {
            ("/usage/input_tokens", "/usage/output_tokens")
        }
        Protocol::Gemini => (
            "/usageMetadata/promptTokenCount",
            "/usageMetadata/candidatesTokenCount",
        ),
    };
    let usage = |pointer| {
        values.iter().rev().find_map(|v| {
            let v = if protocol == Protocol::Responses && stream {
                &v["response"]
            } else {
                v
            };
            v.pointer(pointer).and_then(Value::as_u64)
        })
    };
    assert_eq!(usage(input), Some(5), "{wire}");
    assert_eq!(usage(output), Some(2), "{wire}");
    if stream {
        let terminal = match protocol {
            Protocol::Chat => "data: [DONE]",
            Protocol::Responses => "event: response.completed",
            Protocol::Anthropic => "event: message_stop",
            Protocol::Gemini => "\"finishReason\":\"STOP\"",
        };
        assert_eq!(wire.matches(terminal).count(), 1, "{wire}");
    }
    for private in [TOKEN, KEYS[0], KEYS[1], INPUT, DESCRIPTION, UNKNOWN] {
        assert!(!wire.contains(private));
    }
}
fn assert_sent(
    capture: &Capture,
    protocol: Protocol,
    selector: &Value,
    allowed: &[&str],
    stream: bool,
    search: bool,
) {
    assert_eq!(
        selection(&capture.body, protocol),
        selector,
        "{:?}: {}",
        protocol,
        capture.body
    );
    assert_eq!(names(&capture.body, protocol), allowed, "{}", capture.body);
    assert_eq!(capture.stream, stream);
    assert!(KEYS.contains(&capture.key.as_str()));
    let body = capture.body.to_string();
    assert_eq!(
        body.contains("web_search") || body.contains("googleSearch"),
        search,
        "{body}"
    );
    if selector.get("disable_parallel_tool_use") == Some(&json!(true)) {
        // The parallel constraint is translated, not dropped, on OpenAI shapes.
        let expected = match protocol {
            Protocol::Chat | Protocol::Responses => Some(json!(false)),
            Protocol::Anthropic | Protocol::Gemini => None,
        };
        assert_eq!(
            capture.body.get("parallel_tool_calls"),
            expected.as_ref(),
            "{capture}"
        );
    }
}

#[tokio::test]
async fn function_choice_modes_reach_all_four_providers_from_all_four_clients() {
    let mut total = 0;
    for provider in PROTOCOLS {
        let f = Fixture::new(&[provider], false).await;
        let mut completed = 0;
        for client in PROTOCOLS {
            for mode in ["auto", "none", "required", "named"] {
                for model in ["p0/up-model", "group/daily"] {
                    for stream in [false, true] {
                        let (status, wire) = f
                            .send(client, model, stream, &choice(client, mode), false)
                            .await;
                        assert_reply(client, stream, status, &wire);
                        completed += 1;
                        let sent = f.seen.lock().unwrap().last().unwrap().clone();
                        let allowed = if client == Protocol::Gemini && mode == "named" {
                            vec!["safeB"]
                        } else {
                            NAMES.to_vec()
                        };
                        assert_sent(
                            &sent,
                            provider,
                            &choice(provider, mode),
                            &allowed,
                            stream,
                            false,
                        );
                        assert_eq!(f.seen.lock().unwrap().len(), completed);
                        f.record(completed as u64, true, "p0").await;
                    }
                }
            }
        }
        total += completed;
    }
    assert_eq!(total, 256);
}

#[tokio::test]
async fn allowlists_filter_actual_provider_declarations_and_exclude_unselected_search() {
    let mut total = 0;
    for provider in PROTOCOLS {
        let f = Fixture::new(&[provider], false).await;
        let mut completed = 0;
        for (client, selector, mode, allowed) in [
            (
                Protocol::Responses,
                json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"safeA"},{"type":"function","name":"safeB"}]}),
                "auto",
                vec!["safeA", "safeB"],
            ),
            (
                Protocol::Responses,
                json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"safeA"},{"type":"function","name":"safeB"}]}),
                "required",
                vec!["safeA", "safeB"],
            ),
            (
                Protocol::Responses,
                json!({"type":"allowed_tools","mode":"auto","tools":[]}),
                "auto",
                vec![],
            ),
            (
                Protocol::Gemini,
                json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeA","safeB"]}}),
                "required",
                vec!["safeA", "safeB"],
            ),
        ] {
            for model in ["p0/up-model", "group/daily"] {
                for stream in [false, true] {
                    let (status, wire) = f
                        .send(
                            client,
                            model,
                            stream,
                            &selector,
                            client == Protocol::Responses,
                        )
                        .await;
                    assert_reply(client, stream, status, &wire);
                    completed += 1;
                    let expected = if allowed.is_empty() && provider != Protocol::Gemini {
                        Value::Null
                    } else if client == provider {
                        selector.clone()
                    } else {
                        choice(provider, mode)
                    };
                    assert_sent(
                        f.seen.lock().unwrap().last().unwrap(),
                        provider,
                        &expected,
                        &allowed,
                        stream,
                        false,
                    );
                    assert_eq!(f.seen.lock().unwrap().len(), completed);
                    f.record(completed as u64, true, "p0").await;
                }
            }
        }
        total += completed;
    }
    assert_eq!(total, 64);
}

#[tokio::test]
async fn impossible_or_malformed_choices_do_not_use_any_upstream_key() {
    for provider in PROTOCOLS {
        let f = Fixture::new(&[provider], false).await;
        let mut completed = 0;
        for (client, selector) in [
            (
                Protocol::Responses,
                json!({"type":"allowed_tools","mode":"required","tools":[]}),
            ),
            (
                Protocol::Responses,
                json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":UNKNOWN}]}),
            ),
            (
                Protocol::Chat,
                json!({"type":"function","function":{"name":UNKNOWN}}),
            ),
            (Protocol::Anthropic, json!({"type":"tool","name":UNKNOWN})),
            (
                Protocol::Gemini,
                json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[UNKNOWN]}}),
            ),
            (
                Protocol::Gemini,
                json!({"functionCallingConfig":{"mode":42}}),
            ),
            (
                Protocol::Gemini,
                json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeA"],"allowed_function_names":["forbidden"]}}),
            ),
        ] {
            for model in ["p0/up-model", "group/daily"] {
                for stream in [false, true] {
                    let (status, wire) = f.send(client, model, stream, &selector, false).await;
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{wire}");
                    let error: Value = serde_json::from_str(&wire).unwrap();
                    assert!(error.get("error").is_some());
                    for private in [INPUT, DESCRIPTION, UNKNOWN, TOKEN, KEYS[0], KEYS[1]] {
                        assert!(!wire.contains(private));
                    }
                    completed += 1;
                    f.record(completed, false, "p0").await;
                    f.assert_no_credentials_used();
                }
            }
        }
    }
}

#[tokio::test]
async fn native_only_choices_skip_foreign_key_pools_and_preserve_native_fallback() {
    for (client, selector, search) in [
        (
            Protocol::Gemini,
            json!({"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["safeA"]}}),
            false,
        ),
        (
            Protocol::Gemini,
            json!({"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["safeA"]}}),
            true,
        ),
    ] {
        for provider in PROTOCOLS.into_iter().filter(|&p| p != client) {
            let f = Fixture::new(&[provider, client], false).await;
            let mut completed = 0;
            for model in ["p0/up-model", "group/daily"] {
                for stream in [false, true] {
                    let (status, wire) = f.send(client, model, stream, &selector, search).await;
                    assert_reply(client, stream, status, &wire);
                    completed += 1;
                    let sent = f.seen.lock().unwrap().last().unwrap().clone();
                    assert_eq!(sent.provider, 1, "an incompatible route was contacted");
                    let allowed = if client == Protocol::Gemini && !search {
                        vec!["safeA"]
                    } else {
                        NAMES.to_vec()
                    };
                    assert_sent(&sent, client, &selector, &allowed, stream, search);
                    assert_eq!(f.seen.lock().unwrap().len(), completed);
                    f.record(completed as u64, true, "p1").await;
                    let snapshot = f.state.requests.lock().unwrap().snapshot();
                    assert!(!snapshot.providers.contains_key("p0"));
                    assert_eq!(
                        snapshot.providers["p1"]
                            .keys
                            .values()
                            .map(|k| k.attempts)
                            .sum::<u64>(),
                        completed as u64
                    );
                    assert!(
                        f.state
                            .key_rotation
                            .cooldown_snapshot(&f.state.config.current())
                            .is_empty()
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn failed_foreign_route_does_not_mutate_native_allowlist_on_fallback() {
    let selector = json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"safeA"},{"type":"function","name":"safeB"}]});
    for provider in [Protocol::Chat, Protocol::Anthropic, Protocol::Gemini] {
        let f = Fixture::new(&[provider, Protocol::Responses], true).await;
        let mut completed = 0;
        for model in ["p0/up-model", "group/daily"] {
            for stream in [false, true] {
                let before = f.seen.lock().unwrap().len();
                let (status, wire) = f
                    .send(Protocol::Responses, model, stream, &selector, true)
                    .await;
                assert_reply(Protocol::Responses, stream, status, &wire);
                completed += 1;
                let seen = f.seen.lock().unwrap()[before..].to_vec();
                assert_eq!(seen.last().unwrap().provider, 1);
                assert!(seen.iter().any(|s| s.provider == 0));
                for sent in seen {
                    let (protocol, expected) = if sent.provider == 0 {
                        (provider, choice(provider, "required"))
                    } else {
                        (Protocol::Responses, selector.clone())
                    };
                    assert_sent(
                        &sent,
                        protocol,
                        &expected,
                        &["safeA", "safeB"],
                        stream,
                        false,
                    );
                }
                f.record(completed, true, "p1").await;
            }
        }
    }
}

#[tokio::test]
async fn native_only_selectors_fail_locally_without_a_compatible_fallback() {
    let mut total = 0;
    for (client, selector, search) in [
        (
            Protocol::Gemini,
            json!({"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["safeA"]}}),
            false,
        ),
        (
            Protocol::Gemini,
            json!({"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["safeA"]}}),
            true,
        ),
    ] {
        for provider in PROTOCOLS {
            let f = Fixture::new(&[provider], false).await;
            let mut completed = 0;
            for model in ["p0/up-model", "group/daily"] {
                for stream in [false, true] {
                    let (status, wire) = f.send(client, model, stream, &selector, search).await;
                    completed += 1;
                    if client == provider {
                        assert_reply(client, stream, status, &wire);
                        let allowed = if client == Protocol::Gemini && !search {
                            vec!["safeA"]
                        } else {
                            NAMES.to_vec()
                        };
                        assert_sent(
                            f.seen.lock().unwrap().last().unwrap(),
                            provider,
                            &selector,
                            &allowed,
                            stream,
                            search,
                        );
                    } else {
                        assert_eq!(status, StatusCode::BAD_REQUEST, "{wire}");
                        let error: Value = serde_json::from_str(&wire).unwrap();
                        assert!(error.get("error").is_some());
                        assert!(wire.contains("cannot be translated"));
                        assert!(!wire.contains("allowedFunctionNames"));
                        assert!(!wire.contains("disable_parallel_tool_use"));
                        f.assert_no_credentials_used();
                    }
                    f.record(completed, client == provider, "p0").await;
                }
            }
            total += completed;
        }
    }
    assert_eq!(total, 32);
}

#[tokio::test]
async fn anthropic_parallel_disable_translates_to_openai_shapes() {
    let selector = json!({"type":"any","disable_parallel_tool_use":true});
    // The upstream selector is the translated wire form, not the raw client's.
    let upstream = |protocol| match protocol {
        Protocol::Chat | Protocol::Responses => json!("required"),
        Protocol::Anthropic => json!({"type":"any","disable_parallel_tool_use":true}),
        Protocol::Gemini => unreachable!(),
    };
    // A compatible single route serves the request with the constraint intact;
    // Gemini has no parallel flag and is rejected without touching any key.
    for provider in PROTOCOLS {
        let f = Fixture::new(&[provider], false).await;
        let mut completed = 0;
        for model in ["p0/up-model", "group/daily"] {
            for stream in [false, true] {
                let (status, wire) = f
                    .send(Protocol::Anthropic, model, stream, &selector, false)
                    .await;
                completed += 1;
                if provider == Protocol::Gemini {
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{wire}");
                    assert!(wire.contains("cannot be translated"));
                    assert!(!wire.contains("disable_parallel_tool_use"));
                    f.assert_no_credentials_used();
                } else {
                    assert_reply(Protocol::Anthropic, stream, status, &wire);
                    assert_sent(
                        f.seen.lock().unwrap().last().unwrap(),
                        provider,
                        &upstream(provider),
                        NAMES.as_ref(),
                        stream,
                        false,
                    );
                }
                f.record(completed, provider != Protocol::Gemini, "p0")
                    .await;
            }
        }
    }
}

#[tokio::test]
async fn search_does_not_hide_malformed_gemini_function_configuration() {
    let mut total = 0;
    for provider in PROTOCOLS {
        let f = Fixture::new(&[provider], false).await;
        let mut completed = 0;
        for selector in [
            json!({"functionCallingConfig":{"mode":42}}),
            json!({"functionCallingConfig":{"mode":"ANY"},"function_calling_config":{"mode":"NONE"}}),
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeA"],"allowed_function_names":["forbidden"]}}),
        ] {
            for model in ["p0/up-model", "group/daily"] {
                for stream in [false, true] {
                    let (status, wire) = f
                        .send(Protocol::Gemini, model, stream, &selector, true)
                        .await;
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{wire}");
                    let error: Value = serde_json::from_str(&wire).unwrap();
                    assert!(error.get("error").is_some());
                    assert!(!wire.contains("allowedFunctionNames"));
                    assert!(!wire.contains("forbidden"));
                    completed += 1;
                    f.record(completed, false, "p0").await;
                    f.assert_no_credentials_used();
                }
            }
        }
        total += completed;
    }
    assert_eq!(total, 48);
}
