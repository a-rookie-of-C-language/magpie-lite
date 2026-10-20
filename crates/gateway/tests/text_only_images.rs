//! A provider that declares no vision is never sent image bytes.
//! Native client requests and strict loopback upstreams; no codec-generated
//! fixtures. A current-turn user image is refused before any upstream call;
//! older and tool-returned images go on as an omission note; a group whose
//! first member is text-only serves the images from the member that sees.
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
    ProviderCapabilities, ProviderConfig, RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const PNG: &str = "c2NyZWVuc2hvdA==";
const URL: &str = "https://example.invalid/private-text-only-shot.jpg";
const READ: &str = "PRIVATE_TEXT_ONLY_READ";
const CONTINUE: &str = "PRIVATE_TEXT_ONLY_CONTINUE";
const TOKEN: &str = "PRIVATE_TEXT_ONLY_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_TEXT_ONLY_PROVIDER_KEY";
const ANSWER: &str = "text only acknowledged";
const OMITTED: &str = "[Image omitted: this model accepts text only.]";
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

// A current-turn user image: text plus one inline PNG and one URL image.
fn current(protocol: Protocol, model: &str, stream: bool) -> Value {
    match protocol {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[
                {"role":"user","content":[
                    {"type":"text","text":READ},
                    {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}},
                    {"type":"image_url","image_url":{"url":URL}}
                ]}
            ]})
        }
        Protocol::Anthropic => json!({"model":model,"stream":stream,"max_tokens":64,"messages":[
            {"role":"user","content":[
                {"type":"text","text":READ},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
                {"type":"image","source":{"type":"url","url":URL}}
            ]}
        ]}),
        Protocol::Responses => json!({"model":model,"stream":stream,"input":[
            {"role":"user","content":[
                {"type":"input_text","text":READ},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")},
                {"type":"input_image","image_url":URL}
            ]}
        ]}),
        Protocol::Gemini => json!({"contents":[
            {"role":"user","parts":[
                {"text":READ},
                {"inlineData":{"mimeType":"image/png","data":PNG}},
                {"fileData":{"mimeType":"image/jpeg","fileUri":URL}}
            ]}
        ]}),
    }
}

// The images ride in an older turn; the latest turn is text.
fn historical(protocol: Protocol, model: &str, stream: bool) -> Value {
    match protocol {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[
                {"role":"user","content":[
                    {"type":"text","text":READ},
                    {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}},
                    {"type":"image_url","image_url":{"url":URL}}
                ]},
                {"role":"user","content":CONTINUE}
            ]})
        }
        Protocol::Anthropic => json!({"model":model,"stream":stream,"max_tokens":64,"messages":[
            {"role":"user","content":[
                {"type":"text","text":READ},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
                {"type":"image","source":{"type":"url","url":URL}}
            ]},
            {"role":"user","content":CONTINUE}
        ]}),
        Protocol::Responses => json!({"model":model,"stream":stream,"input":[
            {"role":"user","content":[
                {"type":"input_text","text":READ},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")},
                {"type":"input_image","image_url":URL}
            ]},
            {"role":"user","content":CONTINUE}
        ]}),
        Protocol::Gemini => json!({"contents":[
            {"role":"user","parts":[
                {"text":READ},
                {"inlineData":{"mimeType":"image/png","data":PNG}},
                {"fileData":{"mimeType":"image/jpeg","fileUri":URL}}
            ]},
            {"role":"user","parts":[{"text":CONTINUE}]}
        ]}),
    }
}

// A tool returned one inline image; the turn goes on in words.
fn tool_image(protocol: Protocol, model: &str, stream: bool) -> Value {
    match protocol {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[
                {"role":"assistant","tool_calls":[
                    {"id":"A","type":"function","function":{"name":"shot","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"A","content":[
                    {"type":"text","text":READ},
                    {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}}
                ]},
                {"role":"user","content":CONTINUE}
            ]})
        }
        Protocol::Anthropic => json!({"model":model,"stream":stream,"max_tokens":64,"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","id":"A","name":"shot","input":{}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"A","content":[
                    {"type":"text","text":READ},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}
                ]},
                {"type":"text","text":CONTINUE}
            ]}
        ]}),
        Protocol::Responses => json!({"model":model,"stream":stream,"input":[
            {"type":"function_call","call_id":"A","name":"shot","arguments":"{}"},
            {"type":"function_call_output","call_id":"A","output":[
                {"type":"input_text","text":READ},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")}
            ]},
            {"role":"user","content":CONTINUE}
        ]}),
        Protocol::Gemini => json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"id":"A","name":"shot","args":{}}}
            ]},
            {"role":"user","parts":[
                {"functionResponse":{"id":"A","name":"shot","response":{"output":READ},"parts":[
                    {"inlineData":{"mimeType":"image/png","data":PNG}}
                ]}},
                {"text":CONTINUE}
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

fn assert_no_images(protocol: Protocol, body: &Value) {
    assert_eq!(images(body), Vec::<String>::new(), "{protocol:?}: {body}");
    assert!(
        body.to_string().contains(OMITTED),
        "{protocol:?}: omission note lost: {body}"
    );
    for secret in [PNG, URL] {
        assert!(
            !body.to_string().contains(secret),
            "{protocol:?}: image bytes reached a text-only model"
        );
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
}

#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn reply(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({"id":"txt","model":"mock-model","choices":[{"index":0,
            "message":{"role":"assistant","content":ANSWER},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}}),
        Protocol::Anthropic => {
            json!({"id":"txt","type":"message","role":"assistant","model":"mock-model",
            "content":[{"type":"text","text":ANSWER}],"stop_reason":"end_turn",
            "usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Responses => {
            json!({"id":"txt","object":"response","model":"mock-model","status":"completed",
            "output":[{"id":"msg","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":ANSWER,"annotations":[]}]}],
            "usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Gemini => {
            json!({"responseId":"txt","modelVersion":"mock-model","candidates":[{"index":0,
            "content":{"role":"model","parts":[{"text":ANSWER}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}})
        }
    }
}

fn stream_reply(protocol: Protocol) -> Vec<String> {
    let events = match protocol {
        Protocol::Chat => vec![
            json!({"id":"txt","model":"mock-model","choices":[{"index":0,"delta":{"content":ANSWER},"finish_reason":null}]}),
            json!({"id":"txt","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}),
        ],
        Protocol::Anthropic => vec![
            json!({"type":"message_start","message":{"id":"txt","model":"mock-model","usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":ANSWER}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        Protocol::Responses => vec![
            json!({"type":"response.created","response":{"id":"txt","model":"mock-model","status":"in_progress"}}),
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
    _blind_upstream: Server,
    _seeing_upstream: Server,
    _home: tempfile::TempDir,
    blind: Arc<Mutex<Vec<Value>>>,
    seeing: Arc<Mutex<Vec<Value>>>,
}
async fn fixture(protocol: Protocol) -> Fixture {
    let blind = Arc::new(Mutex::new(Vec::new()));
    let seeing = Arc::new(Mutex::new(Vec::new()));
    let endpoint = match protocol {
        Protocol::Chat => "/v1/chat/completions",
        Protocol::Anthropic => "/v1/messages",
        Protocol::Responses => "/v1/responses",
        Protocol::Gemini => "/v1beta/models/{*model}",
    };
    let blind_upstream = serve(Router::new().route(
        endpoint,
        post(upstream).with_state(Mock {
            protocol,
            seen: blind.clone(),
        }),
    ))
    .await;
    let seeing_upstream = serve(Router::new().route(
        endpoint,
        post(upstream).with_state(Mock {
            protocol,
            seen: seeing.clone(),
        }),
    ))
    .await;
    let home = tempfile::tempdir().unwrap();
    let history = home.path().join("history.jsonl");
    let base = |upstream: &Server| match protocol {
        Protocol::Anthropic => upstream.base.clone(),
        Protocol::Gemini => format!("{}/v1beta", upstream.base),
        Protocol::Chat | Protocol::Responses => format!("{}/v1", upstream.base),
    };
    let no_vision = ProviderCapabilities {
        vision: Some(false),
        ..Default::default()
    };
    let config = Config {
        token: TOKEN.into(),
        default_provider: "blind".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        history_path: Some(history.clone()),
        provider: vec![
            ProviderConfig {
                id: "blind".into(),
                name: "Blind".into(),
                protocol,
                capabilities: no_vision,
                base_url: base(&blind_upstream),
                api_key: KEY.into(),
                api_keys: vec![],
                models: vec!["mock-model".into()],
                contexts: Default::default(),
                outputs: Default::default(),
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
                key_limits: Vec::new(),
            },
            ProviderConfig {
                id: "seeing".into(),
                name: "Seeing".into(),
                protocol,
                capabilities: Default::default(),
                base_url: base(&seeing_upstream),
                api_key: KEY.into(),
                api_keys: vec![],
                models: vec!["mock-model".into()],
                contexts: Default::default(),
                outputs: Default::default(),
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
                key_limits: Vec::new(),
            },
        ],
        group: vec![GroupConfig {
            id: "mixed".into(),
            name: "Mixed".into(),
            routes: vec!["blind/mock-model".into(), "seeing/mock-model".into()],
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
        _blind_upstream: blind_upstream,
        _seeing_upstream: seeing_upstream,
        _home: home,
        blind,
        seeing,
    }
}

#[tokio::test]
async fn text_only_members_refuse_current_images_and_omit_the_rest() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    for provider in PROTOCOLS {
        let fixture = fixture(provider).await;
        // A current-turn user image is refused before any upstream call.
        for protocol in PROTOCOLS {
            let response = client
                .post(format!(
                    "{}{}",
                    fixture.gateway.base,
                    path(protocol, "blind/mock-model", false)
                ))
                .bearer_auth(TOKEN)
                .json(&current(protocol, "blind/mock-model", false))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let wire = response.text().await.unwrap();
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{protocol:?}->{provider:?}: {wire}"
            );
            assert!(wire.contains("does not support image input"), "{wire}");
        }
        assert!(
            fixture.blind.lock().unwrap().is_empty(),
            "refused images reached the text-only upstream"
        );
        assert!(
            fixture.seeing.lock().unwrap().is_empty(),
            "a direct route leaked to the second provider"
        );
        // Older and tool-returned images go on as an omission note.
        for protocol in PROTOCOLS {
            for (name, body) in [
                (
                    "historical",
                    historical(protocol, "blind/mock-model", false),
                ),
                ("tool", tool_image(protocol, "blind/mock-model", false)),
            ] {
                let response = client
                    .post(format!(
                        "{}{}",
                        fixture.gateway.base,
                        path(protocol, "blind/mock-model", false)
                    ))
                    .bearer_auth(TOKEN)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let wire = response.text().await.unwrap();
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{protocol:?}->{provider:?}/{name}: {wire}"
                );
                assert_reply(protocol, false, &wire);
                let seen = fixture.blind.lock().unwrap();
                assert_no_images(protocol, &seen.last().unwrap()["body"]);
            }
        }
        // A group whose first member is text-only serves the images from
        // the member that sees, over JSON and SSE.
        for protocol in PROTOCOLS {
            for stream in [false, true] {
                let response = client
                    .post(format!(
                        "{}{}",
                        fixture.gateway.base,
                        path(protocol, "group/mixed", stream)
                    ))
                    .bearer_auth(TOKEN)
                    .json(&current(protocol, "group/mixed", stream))
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let wire = response.text().await.unwrap();
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{protocol:?}->{provider:?}/group stream={stream}: {wire}"
                );
                assert_reply(protocol, stream, &wire);
                let seen = fixture.seeing.lock().unwrap();
                let last = seen.last().unwrap();
                assert_eq!(last["stream"], stream);
                let mut all = images(&last["body"]);
                all.sort();
                let inline = format!("data:image/png;base64,{PNG}");
                let mut expected = vec![inline, URL.to_owned()];
                expected.sort();
                assert_eq!(all, expected, "{protocol:?}: images lost: {last}");
            }
        }
        assert!(
            fixture.blind.lock().unwrap().len() == 8,
            "historical and tool images should reach the text-only member as notes"
        );
        assert_eq!(
            fixture.seeing.lock().unwrap().len(),
            8,
            "every grouped image request should reach the member that sees"
        );
    }
}
