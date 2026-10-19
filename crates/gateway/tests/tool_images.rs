//! Native client requests and strict loopback upstreams; no codec-generated fixtures.
//! No installed Agents, real configuration, image fetching or paid inference.
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

const PNG: &str = "c2NyZWVuc2hvdA==";
const URL: &str = "https://example.invalid/private-tool-shot.jpg";
const TEXT_A: &str = "PRIVATE_IMAGE_TOOL_A";
const TEXT_B: &str = "PRIVATE_IMAGE_TOOL_B";
const TOKEN: &str = "PRIVATE_IMAGE_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_IMAGE_PROVIDER_KEY";
const ANSWER: &str = "image acknowledged";
const PROTOCOLS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Anthropic,
    Protocol::Responses,
    Protocol::Gemini,
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

// B finishes before A. Both have images, then an ordinary user continuation.
// Manually author each dialect, without any production decoder/encoder helpers.
fn request(protocol: Protocol, model: &str, stream: bool) -> Value {
    match protocol {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[
                {"role":"assistant","tool_calls":[
                    {"id":"A","type":"function","function":{"name":"shot_a","arguments":"{}"}},
                    {"id":"B","type":"function","function":{"name":"shot_b","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"B","content":[
                    {"type":"text","text":TEXT_B}, {"type":"image_url","image_url":{"url":URL}}
                ]},
                {"role":"tool","tool_call_id":"A","content":[
                    {"type":"text","text":TEXT_A},
                    {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}}
                ]},
                {"role":"user","content":"continue"}
            ]})
        }
        Protocol::Anthropic => json!({"model":model,"stream":stream,"max_tokens":64,"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","id":"A","name":"shot_a","input":{}},
                {"type":"tool_use","id":"B","name":"shot_b","input":{}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"B","content":[
                    {"type":"text","text":TEXT_B}, {"type":"image","source":{"type":"url","url":URL}}
                ]},
                {"type":"tool_result","tool_use_id":"A","content":[
                    {"type":"text","text":TEXT_A},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}
                ]},
                {"type":"text","text":"continue"}
            ]}
        ]}),
        Protocol::Responses => json!({"model":model,"stream":stream,"input":[
            {"type":"function_call","call_id":"A","name":"shot_a","arguments":"{}"},
            {"type":"function_call","call_id":"B","name":"shot_b","arguments":"{}"},
            {"type":"function_call_output","call_id":"B","output":[
                {"type":"input_text","text":TEXT_B}, {"type":"input_image","image_url":URL}
            ]},
            {"type":"function_call_output","call_id":"A","output":[
                {"type":"input_text","text":TEXT_A},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")}
            ]},
            {"role":"user","content":"continue"}
        ]}),
        Protocol::Gemini => json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"id":"A","name":"shot_a","args":{}}},
                {"functionCall":{"id":"B","name":"shot_b","args":{}}}
            ]},
            {"role":"user","parts":[
                {"functionResponse":{"id":"B","name":"shot_b","response":{"output":TEXT_B},"parts":[
                    {"fileData":{"mimeType":"image/jpeg","fileUri":URL}}
                ]}},
                {"functionResponse":{"id":"A","name":"shot_a","response":{"output":TEXT_A},"parts":[
                    {"inlineData":{"mimeType":"image/png","data":PNG}}
                ]}},
                {"text":"continue"}
            ]}
        ]}),
    }
}

fn path(protocol: Protocol, model: &str, stream: bool) -> String {
    match protocol {
        Protocol::Chat => "/v1/chat/completions".into(),
        Protocol::Anthropic => "/v1/messages".into(),
        Protocol::Responses => "/v1/responses".into(),
        Protocol::Gemini => format!(
            "/v1beta/models/{model}:{}",
            if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            }
        ),
    }
}

// Read actual native wire image fields, never look for image data inside text.
fn images(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    match value {
        Value::Array(values) => {
            for value in values {
                found.extend(images(value));
            }
        }
        Value::Object(values) => {
            if let Some(url) = value["image_url"]
                .as_str()
                .or_else(|| value["image_url"]["url"].as_str())
            {
                found.push(url.into());
            }
            if value["type"] == "image" {
                if let Some(data) = value["source"]["data"].as_str() {
                    found.push(format!(
                        "data:{};base64,{data}",
                        value["source"]["media_type"].as_str().unwrap()
                    ));
                } else if let Some(url) = value["source"]["url"].as_str() {
                    found.push(url.into());
                }
            }
            if let Some(data) = value["inlineData"]["data"].as_str() {
                found.push(format!(
                    "data:{};base64,{data}",
                    value["inlineData"]["mimeType"].as_str().unwrap()
                ));
            }
            if let Some(url) = value["fileData"]["fileUri"].as_str() {
                found.push(url.into());
            }
            for value in values.values() {
                found.extend(images(value));
            }
        }
        _ => {}
    }
    found
}

fn assert_nested_result(value: &Value, text: &str, image: &str) {
    assert_eq!(images(value), [image]);
    assert!(value.to_string().contains(text));
}

fn assert_upstream(protocol: Protocol, body: &Value) {
    let mut all = images(body);
    all.sort();
    let inline = format!("data:image/png;base64,{PNG}");
    let mut expected = vec![inline.clone(), URL.to_owned()];
    expected.sort();
    assert_eq!(all, expected, "lost or duplicated images: {protocol:?}");
    assert!(body.to_string().contains("continue"));
    match protocol {
        Protocol::Chat => {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[0]["role"], "assistant");
            let calls = messages[0]["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["id"], "A");
            assert_eq!(calls[0]["function"]["name"], "shot_a");
            assert_eq!(calls[1]["id"], "B");
            assert_eq!(calls[1]["function"]["name"], "shot_b");
            for (index, id, text) in [(1, "A", TEXT_A), (2, "B", TEXT_B)] {
                assert_eq!(messages[index]["role"], "tool");
                assert_eq!(messages[index]["tool_call_id"], id);
                let content = messages[index]["content"].as_str().unwrap();
                assert_eq!(
                    content,
                    format!(
                        "{text}\n\n[The tool returned an image; it follows in the next message.]"
                    )
                );
                assert!(!content.contains(PNG));
                assert!(!content.contains(URL));
            }
            assert_eq!(messages[3]["role"], "user");
            let content = messages[3]["content"].as_array().unwrap();
            assert_eq!(content.len(), 5);
            assert_eq!(
                content[0]["text"],
                "[From the result of shot_a (tool call A):]"
            );
            assert_eq!(content[1]["image_url"]["url"], inline);
            assert_eq!(
                content[2]["text"],
                "[From the result of shot_b (tool call B):]"
            );
            assert_eq!(content[3]["image_url"]["url"], URL);
            assert_eq!(content[4]["text"], "continue");
        }
        Protocol::Anthropic => {
            let messages = body["messages"].as_array().unwrap();
            let calls: Vec<_> = messages
                .iter()
                .flat_map(|m| m["content"].as_array().unwrap())
                .filter(|b| b["type"] == "tool_use")
                .collect();
            assert_eq!(calls.len(), 2);
            let results: Vec<_> = messages
                .iter()
                .flat_map(|m| m["content"].as_array().unwrap())
                .filter(|b| b["type"] == "tool_result")
                .collect();
            assert_eq!(results.len(), 2);
            for (id, name, text, image) in [
                ("A", "shot_a", TEXT_A, inline.as_str()),
                ("B", "shot_b", TEXT_B, URL),
            ] {
                assert!(calls.iter().any(|c| c["id"] == id && c["name"] == name));
                let result = results.iter().find(|r| r["tool_use_id"] == id).unwrap();
                assert_nested_result(result, text, image);
                let content = result["content"].as_array().unwrap();
                assert_eq!(content.len(), 2);
                assert_eq!(content[0]["type"], "text");
                assert_eq!(content[0]["text"], text);
                assert_eq!(content[1]["type"], "image");
            }
        }
        Protocol::Responses => {
            let input = body["input"].as_array().unwrap();
            let calls: Vec<_> = input
                .iter()
                .filter(|b| b["type"] == "function_call")
                .collect();
            let results: Vec<_> = input
                .iter()
                .filter(|b| b["type"] == "function_call_output")
                .collect();
            assert_eq!(calls.len(), 2);
            assert_eq!(results.len(), 2);
            for (id, name, text, image) in [
                ("A", "shot_a", TEXT_A, inline.as_str()),
                ("B", "shot_b", TEXT_B, URL),
            ] {
                assert!(
                    calls
                        .iter()
                        .any(|c| c["call_id"] == id && c["name"] == name)
                );
                let result = results.iter().find(|r| r["call_id"] == id).unwrap();
                assert_nested_result(result, text, image);
                let output = result["output"].as_array().unwrap();
                assert_eq!(output.len(), 2);
                assert_eq!(output[0], json!({"type":"input_text", "text":text}));
                assert_eq!(output[1], json!({"type":"input_image", "image_url":image}));
            }
        }
        Protocol::Gemini => {
            let contents = body["contents"].as_array().unwrap();
            let parts: Vec<_> = contents
                .iter()
                .flat_map(|m| m["parts"].as_array().unwrap())
                .collect();
            let calls: Vec<_> = parts.iter().filter_map(|p| p.get("functionCall")).collect();
            let results: Vec<_> = parts
                .iter()
                .filter_map(|p| p.get("functionResponse"))
                .collect();
            assert_eq!(calls.len(), 2);
            assert_eq!(results.len(), 2);
            for (id, name, text, image) in [
                ("A", "shot_a", TEXT_A, inline.as_str()),
                ("B", "shot_b", TEXT_B, URL),
            ] {
                assert!(calls.iter().any(|c| c["id"] == id && c["name"] == name));
                let result = results.iter().find(|r| r["id"] == id).unwrap();
                assert_eq!(result["name"], name);
                assert_eq!(result["response"]["output"], text);
                assert_nested_result(result, text, image);
                assert_eq!(result["parts"].as_array().unwrap().len(), 1);
                assert!(!result["response"].to_string().contains(PNG));
                assert!(!result["response"].to_string().contains(URL));
            }
        }
    }
}

// Inspect the downstream dialect independently of the codecs being tested.
fn assert_reply(protocol: Protocol, stream: bool, wire: &str) {
    let values: Vec<Value> = if stream {
        wire.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    } else {
        vec![serde_json::from_str(wire).unwrap()]
    };
    let text: String = values
        .iter()
        .flat_map(|v| -> Vec<&str> {
            match protocol {
                Protocol::Chat => v
                    .pointer(if stream {
                        "/choices/0/delta/content"
                    } else {
                        "/choices/0/message/content"
                    })
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect(),
                Protocol::Anthropic if stream => v
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect(),
                Protocol::Anthropic => v["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect(),
                Protocol::Responses if stream => {
                    if v["type"] == "response.output_text.delta" {
                        vec![v["delta"].as_str().unwrap()]
                    } else {
                        vec![]
                    }
                }
                Protocol::Responses => v["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|m| m["content"].as_array().unwrap())
                    .filter_map(|b| b["text"].as_str())
                    .collect(),
                Protocol::Gemini => v
                    .pointer("/candidates/0/content/parts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect(),
            }
        })
        .collect();
    assert_eq!(text, ANSWER, "{protocol:?}: {wire}");
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
            let value = if protocol == Protocol::Responses && stream {
                &v["response"]
            } else {
                v
            };
            value.pointer(pointer).and_then(Value::as_u64)
        })
    };
    assert_eq!(usage(input), Some(5), "{protocol:?}: {wire}");
    assert_eq!(usage(output), Some(2), "{protocol:?}: {wire}");
    if stream {
        let terminal = match protocol {
            Protocol::Chat => "data: [DONE]",
            Protocol::Anthropic => "event: message_stop",
            Protocol::Responses => "event: response.completed",
            Protocol::Gemini => "\"finishReason\":\"STOP\"",
        };
        assert_eq!(wire.matches(terminal).count(), 1, "{protocol:?}: {wire}");
    }
    for secret in [PNG, URL, TEXT_A, TEXT_B, TOKEN, KEY] {
        assert!(!wire.contains(secret), "downstream echoed private input");
    }
}

#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn reply(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({"id":"img","model":"mock-model","choices":[{"index":0,
            "message":{"role":"assistant","content":ANSWER},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}}),
        Protocol::Anthropic => {
            json!({"id":"img","type":"message","role":"assistant","model":"mock-model",
            "content":[{"type":"text","text":ANSWER}],"stop_reason":"end_turn",
            "usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Responses => {
            json!({"id":"img","object":"response","model":"mock-model","status":"completed",
            "output":[{"id":"msg","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":ANSWER,"annotations":[]}]}],
            "usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Gemini => {
            json!({"responseId":"img","modelVersion":"mock-model","candidates":[{"index":0,
            "content":{"role":"model","parts":[{"text":ANSWER}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}})
        }
    }
}

fn stream_reply(protocol: Protocol) -> Vec<String> {
    let events = match protocol {
        Protocol::Chat => vec![
            json!({"id":"img","model":"mock-model","choices":[{"index":0,"delta":{"content":ANSWER},"finish_reason":null}]}),
            json!({"id":"img","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}),
        ],
        Protocol::Anthropic => vec![
            json!({"type":"message_start","message":{"id":"img","model":"mock-model","usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":ANSWER}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        Protocol::Responses => vec![
            json!({"type":"response.created","response":{"id":"img","model":"mock-model","status":"in_progress"}}),
            json!({"type":"response.output_text.delta","item_id":"msg","output_index":0,"content_index":0,"delta":ANSWER}),
            json!({"type":"response.completed","response":reply(protocol)}),
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

async fn upstream(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_upstream(mock.protocol, &body);
    let stream = if mock.protocol == Protocol::Gemini {
        assert_eq!(headers["x-goog-api-key"], KEY);
        assert!(uri.path().starts_with("/v1beta/models/mock-model:"));
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
        .push(json!({"body":body,"stream":stream}));
    if !stream {
        return Json(reply(mock.protocol)).into_response();
    }
    let frames = stream_reply(mock.protocol);
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(async_stream::stream! {
            for frame in frames { yield Ok::<Bytes, std::io::Error>(Bytes::from(frame)); }
        }))
        .unwrap()
}

struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    history: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    log: Arc<Mutex<RequestLog>>,
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
        history_path: Some(history.clone()),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol,
            capabilities: Default::default(),
            base_url: match protocol {
                Protocol::Anthropic => upstream.base.clone(),
                Protocol::Gemini => format!("{}/v1beta", upstream.base),
                Protocol::Chat | Protocol::Responses => format!("{}/v1", upstream.base),
            },
            api_key: KEY.into(),
            api_keys: vec![],
            models: vec!["mock-model".into()],
            contexts: Default::default(),
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
        }],
        group: vec![GroupConfig {
            id: "daily".into(),
            name: "Daily".into(),
            routes: vec!["mock/mock-model".into()],
            weights: Vec::new(),
            routing: RoutingPolicy::Rotate,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
        }],
        ..Config::default()
    };
    let log = Arc::new(Mutex::new(RequestLog::open(&history).unwrap()));
    let gateway = serve(build_router(Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: log.clone(),
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
        history,
        seen,
        log,
    }
}

#[tokio::test]
async fn tool_images_survive_all_client_upstream_direct_group_and_json_sse_paths() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut matrix = 0;
    for provider in PROTOCOLS {
        let fixture = fixture(provider).await;
        let mut completed = 0;
        for protocol in PROTOCOLS {
            for model in ["mock/mock-model", "group/daily"] {
                for stream in [false, true] {
                    let response = client
                        .post(format!(
                            "{}{}",
                            fixture.gateway.base,
                            path(protocol, model, stream)
                        ))
                        .bearer_auth(TOKEN)
                        .json(&request(protocol, model, stream))
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let content_type = response.headers()["content-type"]
                        .to_str()
                        .unwrap()
                        .to_owned();
                    let wire = response.text().await.unwrap();
                    assert_eq!(
                        status,
                        StatusCode::OK,
                        "{protocol:?}->{provider:?}/{model}/stream={stream}: {wire}"
                    );
                    assert!(!wire.contains("error"), "{wire}");
                    assert_reply(protocol, stream, &wire);
                    if stream {
                        assert!(content_type.starts_with("text/event-stream"));
                    } else {
                        assert!(content_type.starts_with("application/json"));
                    }
                    completed += 1;
                    matrix += 1;
                    {
                        let seen = fixture.seen.lock().unwrap();
                        assert_eq!(seen.len(), completed as usize);
                        assert_eq!(seen.last().unwrap()["stream"], stream);
                    }
                    // Wait for streaming accounting to finish rather than assuming EOF scheduling.
                    let snapshot = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let snapshot = fixture.log.lock().unwrap().snapshot();
                            if snapshot.total_requests == completed {
                                break snapshot;
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .unwrap();
                    let record = snapshot.recent_requests.last().unwrap();
                    assert_eq!(record.status, "ok");
                    assert_eq!(record.requested_model, model);
                    assert_eq!(record.upstream_model, "mock-model");
                    assert_eq!(record.stream, stream);
                    assert_eq!(record.usage.input, Some(5));
                    assert_eq!(record.usage.output, Some(2));
                    assert_eq!(snapshot.providers["mock"].keys["0"].attempts, completed);
                }
            }
        }
        assert_eq!(completed, 16);
        let history = std::fs::read_to_string(&fixture.history).unwrap();
        let stats = serde_json::to_string(&fixture.log.lock().unwrap().snapshot()).unwrap();
        for secret in [
            PNG, URL, TEXT_A, TEXT_B, TOKEN, KEY, ANSWER, "shot_a", "shot_b",
        ] {
            assert!(!history.contains(secret), "history leaked fixture content");
            assert!(!stats.contains(secret), "stats leaked fixture content");
        }
        assert_eq!(history.lines().count(), completed as usize);
    }
    assert_eq!(matrix, 64);
}
