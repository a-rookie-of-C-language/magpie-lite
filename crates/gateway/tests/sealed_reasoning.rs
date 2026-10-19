//! Literal native fixtures over real loopback HTTP; no live credentials/Agents.
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, GroupConfig, KeyRotator, Protocol,
    ProviderConfig, RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
const TOKEN: &str = "PRIVATE_SEALED_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_SEALED_PROVIDER_KEY";
const SECOND_KEY: &str = "PRIVATE_SEALED_SECOND_KEY";
const CIPHER_A: &str = "PRIVATE_ANTHROPIC_CIPHER";
const CIPHER_R: &str = "PRIVATE_RESPONSES_CIPHER";
const SUMMARY: &str = "public reasoning summary";
const ANSWER: &str = "sealed reasoning acknowledged";
const CLIENTS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Anthropic,
    Protocol::Responses,
    Protocol::Gemini,
];
fn redacted() -> Value {
    json!({"type":"redacted_thinking","data":CIPHER_A,"future":{"keep":[1,null]}})
}
fn encrypted(id: &str, summary: &str) -> Value {
    json!({"type":"reasoning","id":id,"status":"completed","summary":if summary.is_empty(){json!([])}else{json!([{"type":"summary_text","text":summary}])},"encrypted_content":CIPHER_R,"future":{"keep":[false,2]}})
}
fn path(p: Protocol, model: &str, stream: bool) -> String {
    match p {
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
fn request(p: Protocol, model: &str, stream: bool) -> Value {
    match p {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"continue"}]})
        }
        Protocol::Anthropic => {
            json!({"model":model,"stream":stream,"max_tokens":64,"messages":[{"role":"assistant","content":[redacted(),{"type":"text","text":"previous"}]},{"role":"user","content":"continue"}]})
        }
        Protocol::Responses => {
            json!({"model":model,"stream":stream,"input":[encrypted("rs_history",SUMMARY),{"role":"user","content":"continue"}]})
        }
        Protocol::Gemini => json!({"contents":[{"role":"user","parts":[{"text":"continue"}]}]}),
    }
}
fn reply(p: Protocol) -> Value {
    match p {
        Protocol::Anthropic => {
            json!({"type":"message","role":"assistant","model":"native-model","content":[redacted(),{"type":"text","text":ANSWER}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Responses => {
            json!({"id":"resp_fixture","model":"native-model","status":"completed","output":[encrypted("rs_empty",""),encrypted("rs_summary",SUMMARY),{"type":"message","role":"assistant","content":[{"type":"output_text","text":ANSWER}]}],"usage":{"input_tokens":5,"output_tokens":2}})
        }
        _ => unreachable!(),
    }
}
fn stream_frames(p: Protocol, fail: bool) -> Vec<String> {
    let mut events = match p {
        Protocol::Anthropic => vec![
            json!({"type":"message_start","message":{"model":"native-model","usage":{"input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":redacted()}),
            json!({"type":"content_block_stop","index":0}),
        ],
        Protocol::Responses => vec![
            json!({"type":"response.created","response":{"model":"native-model","usage":{"input_tokens":5}}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_empty","summary":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":encrypted("rs_empty","")}),
        ],
        _ => unreachable!(),
    };
    if fail {
        events.push(
            json!({"type":"error","error":{"message":"fixture failed after opaque reasoning"}}),
        );
    } else {
        match p {
        Protocol::Anthropic=>events.extend([
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":ANSWER}}),
            json!({"type":"content_block_stop","index":1}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),json!({"type":"message_stop"})]),
        Protocol::Responses=>events.extend([
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_summary","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":1,"item_id":"rs_summary","delta":SUMMARY}),
            json!({"type":"response.output_item.done","output_index":1,"item":encrypted("rs_summary",SUMMARY)}),
            json!({"type":"response.output_text.delta","output_index":2,"item_id":"msg_answer","delta":ANSWER}),
            json!({"type":"response.completed","response":reply(p)})]),_=>unreachable!()}
    }
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}
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
#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    fail: bool,
    seen: Arc<Mutex<Vec<Value>>>,
}
async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if mock.protocol == Protocol::Anthropic {
        assert!([KEY, SECOND_KEY].contains(&headers["x-api-key"].to_str().unwrap()));
    } else {
        assert!(
            [format!("Bearer {KEY}"), format!("Bearer {SECOND_KEY}")]
                .contains(&headers["authorization"].to_str().unwrap().to_owned())
        );
    }
    mock.seen.lock().unwrap().push(body.clone());
    if body["stream"] != true {
        return Json(reply(mock.protocol)).into_response();
    }
    let frames = stream_frames(mock.protocol, mock.fail);
    Response::builder().header("content-type","text/event-stream").body(Body::from_stream(async_stream::stream!{for frame in frames{yield Ok::<Bytes,std::io::Error>(Bytes::from(frame));}})).unwrap()
}
struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    history: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    log: Arc<Mutex<RequestLog>>,
}
async fn fixture(protocol: Protocol, fail: bool) -> Fixture {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let endpoint = if protocol == Protocol::Anthropic {
        "/v1/messages"
    } else {
        "/v1/responses"
    };
    let upstream = serve(
        Router::new()
            .route(endpoint, post(upstream))
            .with_state(Mock {
                protocol,
                fail,
                seen: seen.clone(),
            }),
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let history = home.path().join("history.jsonl");
    let provider = ProviderConfig {
        id: "mock".into(),
        name: "Mock".into(),
        protocol,
        capabilities: Default::default(),
        base_url: if protocol == Protocol::Anthropic {
            upstream.base.clone()
        } else {
            format!("{}/v1", upstream.base)
        },
        api_key: KEY.into(),
        api_keys: vec![KEY.into(), SECOND_KEY.into()],
        models: vec!["native-model".into()],
        contexts: Default::default(),
        fallback: vec!["backup/native-model".into()],
        key_cooldown_ms: None,
        routing: RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
    };
    let backup = ProviderConfig {
        id: "backup".into(),
        fallback: vec![],
        ..provider.clone()
    };
    let config = Config {
        token: TOKEN.into(),
        default_provider: "mock".into(),
        hidden_auto_groups: Vec::new(),
        history_path: Some(history.clone()),
        provider: vec![provider, backup],
        group: vec![GroupConfig {
            id: "daily".into(),
            name: "Daily".into(),
            routes: vec!["mock/native-model".into(), "backup/native-model".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
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
fn frames(wire: &str) -> Vec<Value> {
    wire.split("\n\n")
        .filter_map(|frame| {
            frame.lines().find_map(|line| {
                line.strip_prefix("data: ")
                    .filter(|s| *s != "[DONE]")
                    .map(|s| serde_json::from_str(s).unwrap())
            })
        })
        .collect()
}
fn answer_json(p: Protocol, v: &Value) -> String {
    match p {
        Protocol::Chat => v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .into(),
        Protocol::Anthropic => v["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        Protocol::Responses => v["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "output_text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        Protocol::Gemini => v["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["thought"] != true)
            .filter_map(|b| b["text"].as_str())
            .collect(),
    }
}
fn assert_reply(client: Protocol, provider: Protocol, stream: bool, wire: &str) {
    if client != provider {
        assert!(!wire.contains(CIPHER_A));
        assert!(!wire.contains(CIPHER_R));
        assert!(!wire.contains("encrypted_content"));
        assert!(!wire.contains("redacted_thinking"));
    }
    let events = if stream {
        frames(wire)
    } else {
        vec![serde_json::from_str(wire).unwrap()]
    };
    let text: String = if !stream {
        answer_json(client, &events[0])
    } else {
        events
            .iter()
            .filter_map(|e| match client {
                Protocol::Chat => e["choices"][0]["delta"]["content"].as_str(),
                Protocol::Anthropic => {
                    if e["delta"]["type"] == "text_delta" {
                        e["delta"]["text"].as_str()
                    } else {
                        None
                    }
                }
                Protocol::Responses => {
                    if e["type"] == "response.output_text.delta" {
                        e["delta"].as_str()
                    } else {
                        None
                    }
                }
                Protocol::Gemini => None,
            })
            .collect()
    };
    let text = if stream && client == Protocol::Gemini {
        events
            .iter()
            .map(|v| answer_json(client, v))
            .collect::<String>()
    } else {
        text
    };
    assert_eq!(text, ANSWER, "{wire}");
    if client == provider {
        match client {
            Protocol::Anthropic => {
                let blocks: Vec<_> = if stream {
                    events
                        .iter()
                        .filter(|e| {
                            e["type"] == "content_block_start"
                                && e["content_block"]["type"] == "redacted_thinking"
                        })
                        .map(|e| e["content_block"].clone())
                        .collect()
                } else {
                    events[0]["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|b| b["type"] == "redacted_thinking")
                        .cloned()
                        .collect()
                };
                assert_eq!(blocks, vec![redacted()]);
            }
            Protocol::Responses => {
                let v = if stream {
                    &events.last().unwrap()["response"]
                } else {
                    &events[0]
                };
                let items: Vec<_> = v["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|i| i["type"] == "reasoning")
                    .cloned()
                    .collect();
                assert_eq!(
                    items,
                    vec![encrypted("rs_empty", ""), encrypted("rs_summary", SUMMARY)]
                );
                if stream {
                    let done: Vec<_> = events
                        .iter()
                        .filter(|e| {
                            e["type"] == "response.output_item.done"
                                && e["item"]["type"] == "reasoning"
                        })
                        .map(|e| e["item"].clone())
                        .collect();
                    assert_eq!(done, items);
                }
            }
            _ => unreachable!(),
        }
    }
    if stream {
        let terminals = match client {
            Protocol::Chat => wire.matches("data: [DONE]").count(),
            Protocol::Anthropic => events
                .iter()
                .filter(|e| e["type"] == "message_stop")
                .count(),
            Protocol::Responses => events
                .iter()
                .filter(|e| e["type"] == "response.completed")
                .count(),
            Protocol::Gemini => events
                .iter()
                .filter(|e| e["candidates"][0]["finishReason"] == "STOP")
                .count(),
        };
        assert_eq!(terminals, 1, "{wire}");
    }
}
async fn wait_records(f: &Fixture, count: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.log.lock().unwrap().snapshot().total_requests == count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn assert_private(f: &Fixture, count: usize) {
    let history = std::fs::read_to_string(&f.history).unwrap();
    let stats = serde_json::to_string(&f.log.lock().unwrap().snapshot()).unwrap();
    for secret in [
        CIPHER_A,
        CIPHER_R,
        KEY,
        SECOND_KEY,
        TOKEN,
        SUMMARY,
        ANSWER,
        "rs_history",
        "rs_empty",
        "rs_summary",
    ] {
        assert!(!history.contains(secret));
        assert!(!stats.contains(secret));
    }
    assert_eq!(history.lines().count(), count);
}
#[tokio::test]
async fn opaque_reasoning_history_and_replies_survive_native_routes_without_foreign_leaks() {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let mut total = 0;
    for provider in [Protocol::Anthropic, Protocol::Responses] {
        let f = fixture(provider, false).await;
        let mut completed = 0;
        for client in CLIENTS {
            for model in ["mock/native-model", "group/daily"] {
                for stream in [false, true] {
                    let response = http
                        .post(format!("{}{}", f.gateway.base, path(client, model, stream)))
                        .bearer_auth(TOKEN)
                        .json(&request(client, model, stream))
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let wire = response.text().await.unwrap();
                    assert_eq!(status, 200, "{client:?}->{provider:?} {wire}");
                    assert_reply(client, provider, stream, &wire);
                    completed += 1;
                    total += 1;
                    wait_records(&f, completed).await;
                    {
                        let seen = f.seen.lock().unwrap();
                        // A streamed reply may take one extra same-key ask
                        // before it answers, so the upstream saw at least one
                        // call per completed request.
                        assert!(seen.len() >= completed as usize);
                        let body = seen.last().unwrap();
                        assert_eq!(body["model"], "native-model");
                        assert_eq!(body["stream"], stream);
                        if client == provider {
                            if client == Protocol::Anthropic {
                                assert_eq!(body["messages"][0]["content"][0], redacted());
                            } else {
                                assert_eq!(body["input"][0], encrypted("rs_history", SUMMARY));
                            }
                        } else {
                            let wire = body.to_string();
                            assert!(!wire.contains(CIPHER_A));
                            assert!(!wire.contains(CIPHER_R));
                        }
                    }
                    let stats = f.log.lock().unwrap().snapshot();
                    let record = stats.recent_requests.last().unwrap();
                    assert_eq!(record.status, "ok");
                    assert_eq!(record.requested_model, model);
                    assert_eq!(record.usage.input, Some(5));
                    assert_eq!(record.usage.output, Some(2));
                    let attempts = |log: &Arc<Mutex<RequestLog>>| {
                        let stats = log.lock().unwrap().snapshot();
                        stats
                            .providers
                            .values()
                            .map(|p| p.keys.values().map(|k| k.attempts).sum::<u64>())
                            .sum::<u64>()
                    };
                    // The attempt counter is written after the request log
                    // entry, so wait for each completed request to be counted.
                    // A fallback route counts here too: a sealed history the
                    // provider refuses goes to the backup.
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while attempts(&f.log) < completed && std::time::Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    assert_eq!(attempts(&f.log), completed);
                }
            }
        }
        assert_private(&f, completed as usize);
    }
    assert_eq!(total, 32);
}
#[tokio::test]
async fn post_seal_stream_error_never_retries_keys_fallbacks_or_group_routes() {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    for provider in [Protocol::Anthropic, Protocol::Responses] {
        for client in CLIENTS {
            for model in ["mock/native-model", "group/daily"] {
                let f = fixture(provider, true).await;
                let response = http
                    .post(format!("{}{}", f.gateway.base, path(client, model, true)))
                    .bearer_auth(TOKEN)
                    .json(&request(client, model, true))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                let wire = response.text().await.unwrap();
                assert!(wire.contains("error"), "{wire}");
                assert!(!wire.contains(ANSWER));
                if client != provider {
                    assert!(!wire.contains(CIPHER_A));
                    assert!(!wire.contains(CIPHER_R));
                }
                wait_records(&f, 1).await;
                assert_eq!(
                    f.seen.lock().unwrap().len(),
                    1,
                    "opaque reasoning must commit even when foreign clients cannot render it"
                );
                let stats = f.log.lock().unwrap().snapshot();
                assert_eq!(stats.recent_requests[0].status, "upstream_stream_error");
                assert_eq!(stats.providers["mock"].keys["0"].attempts, 1);
                assert!(!stats.providers.contains_key("backup"));
                assert_private(&f, 1);
            }
        }
    }
}
