//! Responses client tool-search relay across native upstreams.
//! No installed Agents, real configuration, or paid inference.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, GroupConfig, KeyRotator, Protocol,
    ProviderConfig, RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const TOKEN: &str = "PRIVATE_SEARCH_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_SEARCH_PROVIDER_KEY";
const UPSTREAMS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Anthropic,
    Protocol::Gemini,
    Protocol::Responses,
];

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
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { base, task }
}

fn client_request(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "stream": stream,
        "store": false,
        "tools": [
            {"type": "function", "name": "exec_command",
             "description": "run", "parameters": {"type": "object"}},
            {"type": "tool_search", "execution": "client",
             "description": "Searches deferred tools",
             "parameters": {"type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]}}
        ],
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "make a calendar event"}]},
            {"type": "tool_search_call", "call_id": "s1",
             "execution": "client", "arguments": {"query": "calendar create"}},
            {"type": "tool_search_output", "call_id": "s1",
             "status": "completed", "execution": "client", "tools": [
                {"type": "namespace", "name": "mcp__cal",
                 "description": "Calendar", "tools": [
                    {"type": "function", "name": "create_event",
                     "description": "Create a calendar event",
                     "defer_loading": true,
                     "parameters": {"type": "object"}}]}
             ]}
        ]
    })
}

fn assert_upstream(protocol: Protocol, body: &Value) {
    let wire = body.to_string();
    assert!(wire.contains("tool_search"), "{protocol:?}: {wire}");
    assert!(wire.contains("calendar create"), "{protocol:?}: {wire}");
    assert!(
        wire.contains("These tools are now available to call"),
        "{protocol:?}: {wire}"
    );
    for forbidden in [
        "tool_search_call",
        "tool_search_output",
        "\"execution\"",
        "defer_loading",
        "\"type\":\"tool_search\"",
    ] {
        assert!(
            !wire.contains(forbidden),
            "{protocol:?} leaked {forbidden}: {wire}"
        );
    }
    match protocol {
        Protocol::Chat => {
            let tools = body["tools"].as_array().unwrap();
            let names: Vec<_> = tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
                .collect();
            assert!(names.contains(&"tool_search".to_owned()), "{wire}");
            assert!(
                names.contains(&"mcp__cal__create_event".to_owned()),
                "{wire}"
            );
        }
        Protocol::Anthropic => {
            let tools = body["tools"].as_array().unwrap();
            let names: Vec<_> = tools
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect();
            assert!(names.contains(&"tool_search".to_owned()), "{wire}");
            assert!(
                names.contains(&"mcp__cal__create_event".to_owned()),
                "{wire}"
            );
        }
        Protocol::Gemini => {
            assert!(wire.contains("mcp__cal__create_event"), "{wire}");
        }
        Protocol::Responses => {
            let tools = body["tools"].as_array().unwrap();
            assert!(
                tools
                    .iter()
                    .any(|t| t["type"] == "function" && t["name"] == "tool_search"),
                "{wire}"
            );
            assert!(wire.contains("mcp__cal"), "{wire}");
            assert!(wire.contains("create_event"), "{wire}");
        }
    }
}

fn reply(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({"id":"search","model":"mock-model",
            "choices":[{"index":0,"message":{"role":"assistant","tool_calls":[
                {"id":"s2","type":"function",
                 "function":{"name":"tool_search",
                    "arguments":"{\"query\":\"mail\"}"}}]},
                "finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}}),
        Protocol::Anthropic => json!({"id":"search","type":"message",
            "role":"assistant","model":"mock-model",
            "content":[{"type":"tool_use","id":"s2",
                "name":"tool_search","input":{"query":"mail"}}],
            "stop_reason":"tool_use",
            "usage":{"input_tokens":5,"output_tokens":2}}),
        Protocol::Responses => json!({"id":"search","object":"response",
            "model":"mock-model","status":"completed",
            "output":[{"type":"function_call","id":"fc_s2","call_id":"s2",
                "name":"tool_search","arguments":"{\"query\":\"mail\"}",
                "status":"completed"}],
            "usage":{"input_tokens":5,"output_tokens":2}}),
        Protocol::Gemini => json!({"responseId":"search",
            "modelVersion":"mock-model",
            "candidates":[{"index":0,
                "content":{"role":"model","parts":[
                    {"functionCall":{"id":"s2","name":"tool_search",
                        "args":{"query":"mail"}}}]},
                "finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,
                "totalTokenCount":7}}),
    }
}

fn stream_reply(protocol: Protocol) -> Vec<String> {
    let events = match protocol {
        Protocol::Chat => vec![
            json!({"id":"search","model":"mock-model","choices":[{"index":0,
                "delta":{"tool_calls":[{"index":0,"id":"s2","type":"function",
                    "function":{"name":"tool_search","arguments":""}}]},
                "finish_reason":null}]}),
            json!({"id":"search","choices":[{"index":0,
                "delta":{"tool_calls":[{"index":0,
                    "function":{"arguments":"{\"query\":\"mail\"}"}}]}}]}),
            json!({"id":"search","choices":[{"index":0,"delta":{},
                "finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":5,"completion_tokens":2}}),
        ],
        Protocol::Anthropic => vec![
            json!({"type":"message_start","message":{"id":"search",
                "model":"mock-model",
                "usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":"s2",
                    "name":"tool_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta",
                    "partial_json":"{\"query\":"}}),
            json!({"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta",
                    "partial_json":"\"mail\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta",
                "delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        Protocol::Responses => vec![
            json!({"type":"response.created",
                "response":{"id":"search","model":"mock-model",
                    "status":"in_progress"}}),
            json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","id":"fc_s2","call_id":"s2",
                    "name":"tool_search","arguments":"",
                    "status":"in_progress"}}),
            json!({"type":"response.function_call_arguments.delta",
                "item_id":"fc_s2","output_index":0,
                "delta":"{\"query\":\"mail\"}"}),
            json!({"type":"response.output_item.done","output_index":0,
                "item":{"type":"function_call","id":"fc_s2","call_id":"s2",
                    "name":"tool_search",
                    "arguments":"{\"query\":\"mail\"}",
                    "status":"completed"}}),
            json!({"type":"response.completed",
                "response":{"id":"search","model":"mock-model",
                    "status":"completed",
                    "output":[{"type":"function_call","id":"fc_s2",
                        "call_id":"s2","name":"tool_search",
                        "arguments":"{\"query\":\"mail\"}"}],
                    "usage":{"input_tokens":5,"output_tokens":2}}}),
        ],
        Protocol::Gemini => vec![reply(protocol)],
    };
    let mut frames: Vec<String> = events
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
        frames.push("data: [DONE]\n\n".into());
    }
    frames
}

#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    seen: Arc<Mutex<Vec<Value>>>,
}

async fn upstream(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_upstream(mock.protocol, &body);
    let stream = if mock.protocol == Protocol::Gemini {
        assert_eq!(headers["x-goog-api-key"], KEY);
        uri.path().ends_with(":streamGenerateContent")
    } else {
        assert_eq!(body["model"], "mock-model");
        if mock.protocol == Protocol::Anthropic {
            assert_eq!(headers["x-api-key"], KEY);
        } else {
            assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
        }
        body["stream"] == true
    };
    mock.seen
        .lock()
        .unwrap()
        .push(json!({"body": body, "stream": stream}));
    if !stream {
        return Json(reply(mock.protocol)).into_response();
    }
    let frames = stream_reply(mock.protocol);
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(async_stream::stream! {
            for frame in frames {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
            }
        }))
        .unwrap()
}

fn assert_downstream_json(wire: &str) {
    let body: Value = serde_json::from_str(wire).unwrap();
    let output = body["output"].as_array().unwrap();
    assert_eq!(output.len(), 1, "{wire}");
    let item = &output[0];
    assert_eq!(item["type"], "tool_search_call", "{wire}");
    assert_eq!(item["call_id"], "s2", "{wire}");
    assert_eq!(item["execution"], "client", "{wire}");
    assert_eq!(item["arguments"], json!({"query": "mail"}), "{wire}");
    assert!(item["id"].as_str().unwrap().starts_with("tsc_"), "{wire}");
    assert!(item.get("name").is_none(), "{wire}");
}

fn assert_downstream_stream(wire: &str) {
    assert!(wire.contains("tool_search_call"), "{wire}");
    assert!(wire.contains("tsc_"), "{wire}");
    assert!(!wire.contains("function_call_arguments.done"), "{wire}");
    let mut saw_done = false;
    for line in wire.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(data).unwrap();
        if event["type"] == "response.output_item.done" {
            let item = &event["item"];
            if item["type"] == "tool_search_call" {
                assert_eq!(item["call_id"], "s2", "{wire}");
                assert_eq!(item["execution"], "client", "{wire}");
                assert_eq!(item["arguments"], json!({"query": "mail"}), "{wire}");
                assert!(item["id"].as_str().unwrap().starts_with("tsc_"), "{wire}");
                assert!(item.get("name").is_none(), "{wire}");
                saw_done = true;
            }
        }
    }
    assert!(saw_done, "{wire}");
}

struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    seen: Arc<Mutex<Vec<Value>>>,
}
async fn fixture(protocol: Protocol) -> Fixture {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let endpoint = match protocol {
        Protocol::Chat => "/v1/chat/completions",
        Protocol::Anthropic => "/v1/messages",
        Protocol::Responses => "/v1/responses",
        Protocol::Gemini => "/v1beta/models/{*model}",
    };
    let upstream = serve(
        Router::new()
            .route(endpoint, post(upstream))
            .with_state(Mock {
                protocol,
                seen: seen.clone(),
            }),
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
        history_path: Some(history),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol,
            capabilities: Default::default(),
            base_url: match protocol {
                Protocol::Anthropic => upstream.base.clone(),
                Protocol::Gemini => format!("{}/v1beta", upstream.base),
                Protocol::Chat | Protocol::Responses => {
                    format!("{}/v1", upstream.base)
                }
            },
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
    let log = Arc::new(Mutex::new(
        RequestLog::open(&home.path().join("history.jsonl")).unwrap(),
    ));
    let gateway = serve(build_router(Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: log,
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    })))
    .await;
    Fixture {
        gateway,
        _upstream: upstream,
        _home: home,
        seen,
    }
}

#[tokio::test]
async fn responses_tool_search_survives_native_upstreams() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    for provider in UPSTREAMS {
        for stream in [false, true] {
            let fixture = fixture(provider).await;
            let response = client
                .post(format!("{}/v1/responses", fixture.gateway.base))
                .bearer_auth(TOKEN)
                .json(&client_request("mock/mock-model", stream))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let wire = response.text().await.unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{provider:?}/stream={stream}: {wire}"
            );
            if stream {
                assert_downstream_stream(&wire);
            } else {
                assert_downstream_json(&wire);
            }
            let seen = fixture.seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "{provider:?}/stream={stream}");
            assert_eq!(seen[0]["stream"], stream);
        }
    }
}
