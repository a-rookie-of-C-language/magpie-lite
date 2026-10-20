//! Native signature fixtures over loopback HTTP, with no paid vendor traffic.
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Uri},
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
const TOKEN: &str = "PRIVATE_SIGNATURE_GATEWAY_TOKEN";
const KEYS: [&str; 2] = ["PRIVATE_SIGNATURE_KEY_ONE", "PRIVATE_SIGNATURE_KEY_TWO"];
const SIGNATURE: &str = "PRIVATE_NATIVE_SIGNATURE";
const ANSWER: &str = "signed native answer";
const HISTORY: &str = "PRIVATE_SIGNATURE_HISTORY_TEXT";
const CLIENTS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Anthropic,
    Protocol::Responses,
    Protocol::Gemini,
];
#[derive(Clone, Copy)]
enum Mode {
    Success,
    AfterSignatureError,
    MetadataThenRetry,
}
fn native_parts(p: Protocol) -> Value {
    match p {
        Protocol::Anthropic => json!([
            {"type":"thinking","thinking":"","signature":SIGNATURE},
            {"type":"thinking","thinking":"public reasoning","signature":SIGNATURE},
            {"type":"text","text":ANSWER}]),
        Protocol::Gemini => json!([
            {"text":"","thought":true,"thoughtSignature":SIGNATURE},
            {"text":"public reasoning","thought":true,"thought_signature":SIGNATURE},
            {"text":ANSWER,"thoughtSignature":SIGNATURE},
            {"text":"","thought_signature":SIGNATURE},
            {"functionCall":{"id":"call_native","name":"inspect","args":{"path":"test"}},"thoughtSignature":SIGNATURE}]),
        _ => unreachable!(),
    }
}
fn reply(p: Protocol) -> Value {
    match p {
        Protocol::Anthropic => {
            json!({"type":"message","role":"assistant","model":"native-model","content":native_parts(p),"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}})
        }
        Protocol::Gemini => {
            json!({"modelVersion":"native-model","candidates":[{"content":{"role":"model","parts":native_parts(p)},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2}})
        }
        _ => unreachable!(),
    }
}
fn error(p: Protocol) -> Value {
    match p {
        Protocol::Anthropic => {
            json!({"type":"error","error":{"type":"rate_limit_error","message":"fixture rate limit"}})
        }
        Protocol::Gemini => {
            json!({"error":{"code":429,"message":"fixture rate limit","status":"RESOURCE_EXHAUSTED"}})
        }
        _ => unreachable!(),
    }
}
fn stream(p: Protocol, mode: Mode, retry: bool) -> String {
    let mut events = match p {
        Protocol::Anthropic => vec![
            json!({"type":"message_start","message":{"model":"native-model","usage":{"input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        ],
        Protocol::Gemini => {
            vec![json!({"modelVersion":"native-model","usageMetadata":{"promptTokenCount":5}})]
        }
        _ => unreachable!(),
    };
    if matches!(mode, Mode::MetadataThenRetry) && !retry {
        events.push(error(p));
    } else {
        match p {
            Protocol::Anthropic => events.extend([
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":SIGNATURE}}),
                json!({"type":"content_block_stop","index":0})]),
            Protocol::Gemini => events.push(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"","thought":true,"thoughtSignature":SIGNATURE}]}}]})),
            _ => unreachable!()
        }
        if matches!(mode, Mode::AfterSignatureError) {
            events.push(error(p));
        } else {
            match p {
                Protocol::Anthropic => events.extend([
                    json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"public reasoning","signature":SIGNATURE}}),
                    json!({"type":"content_block_stop","index":1}),
                    json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":ANSWER}}),
                    json!({"type":"content_block_stop","index":2}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),json!({"type":"message_stop"})]),
                Protocol::Gemini => {
                    let mut v = reply(p);
                    v["candidates"][0]["content"]["parts"].as_array_mut().unwrap().remove(0);
                    events.push(v);
                },
                _ => unreachable!()
            }
        }
    }
    events.iter().map(|v| format!("data: {v}\n\n")).collect()
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
    Server {
        base,
        task: tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    }
}
#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    mode: Mode,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}
async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    Json(body): Json<Value>,
) -> Response {
    let auth = headers[if mock.protocol == Protocol::Anthropic {
        "x-api-key"
    } else {
        "x-goog-api-key"
    }]
    .to_str()
    .unwrap()
    .to_owned();
    assert!(KEYS.contains(&auth.as_str()));
    let retry = {
        let mut seen = mock.seen.lock().unwrap();
        let retry = !seen.is_empty();
        seen.push((auth, body.clone()));
        retry
    };
    if body["stream"] != true && !uri.path().contains("streamGenerateContent") {
        return Json(reply(mock.protocol)).into_response();
    }
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(stream(mock.protocol, mock.mode, retry)))
        .unwrap()
}
struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    history: std::path::PathBuf,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    log: Arc<Mutex<RequestLog>>,
}
async fn fixture(protocol: Protocol, mode: Mode) -> Fixture {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = serve(Router::new().fallback(post(upstream)).with_state(Mock {
        protocol,
        mode,
        seen: seen.clone(),
    }))
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
            format!("{}/v1beta", upstream.base)
        },
        api_key: KEYS[0].into(),
        api_keys: KEYS.iter().map(|s| (*s).into()).collect(),
        models: vec!["native-model".into()],
        contexts: Default::default(),
        outputs: Default::default(),
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
        pin_upstream: false,
        key_limits: Vec::new(),
    };
    let backup = ProviderConfig {
        id: "backup".into(),
        fallback: vec![],
        ..provider.clone()
    };
    let log = Arc::new(Mutex::new(RequestLog::open(&history).unwrap()));
    let gateway = serve(build_router(Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(
            Config {
                token: TOKEN.into(),
                default_provider: "mock".into(),
                hidden_auto_groups: Vec::new(),
                no_auto_groups: false,
                vision: None,
                history_path: Some(history.clone()),
                provider: vec![provider, backup],
                group: vec![GroupConfig {
                    id: "daily".into(),
                    name: "Daily".into(),
                    routes: vec!["mock/native-model".into(), "backup/native-model".into()],
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
                    patience_s: None,
                }],
                ..Config::default()
            },
            None,
        ),
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
fn path(p: Protocol, model: &str, stream: bool) -> String {
    match p {
        Protocol::Chat => "/v1/chat/completions".into(),
        Protocol::Anthropic => "/v1/messages".into(),
        Protocol::Responses => "/v1/responses".into(),
        Protocol::Gemini => format!(
            "/v1beta/models/{model}:{}",
            if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            }
        ),
    }
}
fn request(p: Protocol, model: &str, stream: bool) -> Value {
    match p {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello"}]})
        }
        Protocol::Responses => json!({"model":model,"stream":stream,"input":"hello"}),
        Protocol::Anthropic => {
            json!({"model":model,"stream":stream,"max_tokens":100,"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":HISTORY,"signature":SIGNATURE}]},{"role":"user","content":"hello"}]})
        }
        Protocol::Gemini => {
            json!({"contents":[{"role":"model","parts":[{"text":HISTORY,"thought":true,"thoughtSignature":SIGNATURE}]},{"role":"user","parts":[{"text":"hello"}]}]})
        }
    }
}
async fn send(f: &Fixture, client: Protocol, model: &str, streaming: bool) -> String {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post(format!(
            "{}{}",
            f.gateway.base,
            path(client, model, streaming)
        ))
        .bearer_auth(TOKEN)
        .json(&request(client, model, streaming))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let wire = response.text().await.unwrap();
    assert_eq!(status, 200, "{wire}");
    wire
}
fn frames(wire: &str) -> Vec<Value> {
    wire.split("\n\n")
        .filter_map(|f| {
            f.lines().find_map(|l| {
                l.strip_prefix("data: ")
                    .filter(|s| *s != "[DONE]")
                    .map(|s| serde_json::from_str(s).unwrap())
            })
        })
        .collect()
}
fn text(client: Protocol, value: &Value, streaming: bool) -> String {
    match (client, streaming) {
        (Protocol::Chat, true) => value["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap_or_default()
            .into(),
        (Protocol::Chat, false) => value["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .into(),
        (Protocol::Anthropic, true) => value["delta"]["text"].as_str().unwrap_or_default().into(),
        (Protocol::Anthropic, false) => value["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect(),
        (Protocol::Responses, true) => {
            if value["type"] == "response.output_text.delta" {
                value["delta"].as_str().unwrap().into()
            } else {
                String::new()
            }
        }
        (Protocol::Responses, false) => value["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["content"].as_array())
            .flatten()
            .filter_map(|p| p["text"].as_str())
            .collect(),
        (Protocol::Gemini, _) => value["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["thought"] != true)
            .filter_map(|p| p["text"].as_str())
            .collect(),
    }
}
fn usage(client: Protocol, values: &[Value], streaming: bool) {
    let v = values.last().unwrap();
    match client {
        Protocol::Chat => {
            let v = values.iter().find(|v| v.get("usage").is_some()).unwrap();
            assert_eq!(v["usage"]["prompt_tokens"], 5);
            assert_eq!(v["usage"]["completion_tokens"], 2);
        }
        Protocol::Responses => {
            let v = if streaming { &v["response"] } else { v };
            assert_eq!(v["usage"]["input_tokens"], 5);
            assert_eq!(v["usage"]["output_tokens"], 2);
        }
        Protocol::Anthropic => {
            if streaming {
                assert_eq!(
                    values
                        .iter()
                        .find(|v| v["type"] == "message_delta")
                        .unwrap()["usage"]["input_tokens"],
                    5
                );
                assert_eq!(
                    values
                        .iter()
                        .find(|v| v["type"] == "message_delta")
                        .unwrap()["usage"]["output_tokens"],
                    2
                );
            } else {
                assert_eq!(v["usage"]["input_tokens"], 5);
                assert_eq!(v["usage"]["output_tokens"], 2);
            }
        }
        Protocol::Gemini => {
            assert_eq!(v["usageMetadata"]["promptTokenCount"], 5);
            assert_eq!(v["usageMetadata"]["candidatesTokenCount"], 2);
        }
    }
}
async fn records(f: &Fixture, count: u64) {
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
    let history = std::fs::read_to_string(&f.history).unwrap();
    let stats = serde_json::to_string(&f.log.lock().unwrap().snapshot()).unwrap();
    for secret in [
        SIGNATURE,
        KEYS[0],
        KEYS[1],
        TOKEN,
        ANSWER,
        "public reasoning",
        HISTORY,
    ] {
        assert!(!history.contains(secret), "history contains {secret}");
        assert!(!stats.contains(secret));
    }
    assert_eq!(history.lines().count(), count as usize);
}
#[tokio::test]
async fn native_signatures_survive_json_sse_direct_and_group_routes_without_foreign_leaks() {
    let mut total = 0;
    for provider in [Protocol::Anthropic, Protocol::Gemini] {
        let f = fixture(provider, Mode::Success).await;
        let mut completed = 0;
        for client in CLIENTS {
            for model in ["mock/native-model", "group/daily"] {
                for streaming in [false, true] {
                    let wire = send(&f, client, model, streaming).await;
                    let values = if streaming {
                        frames(&wire)
                    } else {
                        vec![serde_json::from_str(&wire).unwrap()]
                    };
                    assert_eq!(
                        values
                            .iter()
                            .map(|v| text(client, v, streaming))
                            .collect::<String>(),
                        ANSWER
                    );
                    usage(client, &values, streaming);
                    if client != provider {
                        assert!(!wire.contains(SIGNATURE));
                    } else {
                        let count = wire.matches(SIGNATURE).count();
                        assert_eq!(
                            count,
                            if provider == Protocol::Anthropic {
                                2
                            } else {
                                5
                            },
                            "{wire}"
                        );
                        if provider == Protocol::Gemini {
                            let parts: Vec<_> = values
                                .iter()
                                .filter_map(|v| {
                                    v.pointer("/candidates/0/content/parts")
                                        .and_then(Value::as_array)
                                })
                                .flatten()
                                .collect();
                            let call = parts
                                .iter()
                                .find(|p| p.get("functionCall").is_some())
                                .unwrap();
                            assert_eq!(call["thoughtSignature"], SIGNATURE);
                        }
                    }
                    completed += 1;
                    total += 1;
                    records(&f, completed).await;
                    let seen = f.seen.lock().unwrap();
                    assert_eq!(seen.len(), completed as usize);
                    let body = seen.last().unwrap().1.to_string();
                    assert_eq!(body.contains(SIGNATURE), client == provider);
                    let snapshot = f.log.lock().unwrap().snapshot();
                    assert_eq!(snapshot.recent_requests.last().unwrap().status, "ok");
                    assert_eq!(
                        snapshot.recent_requests.last().unwrap().usage.input,
                        Some(5)
                    );
                    assert_eq!(
                        snapshot.recent_requests.last().unwrap().usage.output,
                        Some(2)
                    );
                }
            }
        }
    }
    assert_eq!(total, 32);
}
#[tokio::test]
async fn signature_only_content_commits_before_rate_limit_errors_for_all_clients() {
    for provider in [Protocol::Anthropic, Protocol::Gemini] {
        for client in CLIENTS {
            for model in ["mock/native-model", "group/daily"] {
                let f = fixture(provider, Mode::AfterSignatureError).await;
                let wire = send(&f, client, model, true).await;
                assert!(wire.contains("error"));
                assert!(!wire.contains(ANSWER));
                if client != provider {
                    assert!(!wire.contains(SIGNATURE));
                } else {
                    assert!(wire.contains(SIGNATURE));
                }
                assert!(!wire.contains("response.completed"));
                assert!(!wire.contains("message_stop"));
                assert!(!wire.contains("[DONE]"));
                assert!(!wire.contains("finishReason"));
                records(&f, 1).await;
                assert_eq!(f.seen.lock().unwrap().len(), 1);
                let stats = f.log.lock().unwrap().snapshot();
                assert_eq!(stats.recent_requests[0].status, "upstream_stream_error");
                assert_eq!(stats.providers["mock"].keys["0"].attempts, 1);
                assert!(!stats.providers.contains_key("backup"));
            }
        }
    }
}
#[tokio::test]
async fn unsigned_opening_metadata_still_retries_with_a_different_key() {
    for provider in [Protocol::Anthropic, Protocol::Gemini] {
        for client in CLIENTS {
            for model in ["mock/native-model", "group/daily"] {
                let f = fixture(provider, Mode::MetadataThenRetry).await;
                let wire = send(&f, client, model, true).await;
                assert!(!wire.contains("fixture rate limit"));
                let values = frames(&wire);
                assert_eq!(
                    values
                        .iter()
                        .map(|v| text(client, v, true))
                        .collect::<String>(),
                    ANSWER
                );
                records(&f, 1).await;
                let seen = f.seen.lock().unwrap();
                assert_eq!(seen.len(), 2);
                assert_eq!(seen[0].0, KEYS[0]);
                assert_eq!(seen[1].0, KEYS[1]);
                let stats = f.log.lock().unwrap().snapshot();
                assert_eq!(stats.recent_requests[0].status, "ok");
                assert_eq!(stats.providers["mock"].keys["0"].attempts, 1);
                assert_eq!(stats.providers["mock"].keys["1"].attempts, 1);
                assert!(!stats.providers.contains_key("backup"));
            }
        }
    }
}
