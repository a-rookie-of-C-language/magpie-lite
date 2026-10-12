//! Real HTTP fixtures for stream termination and cancellation, not live-vendor tests.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{
    Router,
    body::{Body, Bytes},
    response::Response,
    routing::post,
};
use magpie_gateway::{
    AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig, build_router,
    usage::RequestLog,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const PROTOCOLS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Anthropic,
    Protocol::Responses,
    Protocol::Gemini,
];
const LIMIT: Duration = Duration::from_secs(4);

struct Server {
    url: String,
    task: JoinHandle<()>,
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
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { url, task }
}

struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn prefix(protocol: Protocol) -> String {
    match protocol {
        Protocol::Chat => concat!(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n"
        ).into(),
        Protocol::Anthropic => concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"up-model\",\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n"
        ).into(),
        Protocol::Responses => concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"up-model\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
        ).into(),
        Protocol::Gemini => "data: {\"modelVersion\":\"up-model\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"partial\"}]},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":0}}\n\n".into(),
    }
}
fn completion(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Chat => concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        ),
        Protocol::Anthropic => concat!(
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ),
        Protocol::Responses => {
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}}\n\n"
        }
        Protocol::Gemini => {
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3}}\n\n"
        }
    }
}
fn error(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Chat => "data: {\"error\":{\"code\":429,\"message\":\"fixture rate limit\"}}\n\n",
        Protocol::Anthropic => {
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"fixture rate limit\"}}\n\n"
        }
        Protocol::Responses => {
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"fixture rate limit\"}}}\n\n"
        }
        Protocol::Gemini => {
            "data: {\"error\":{\"code\":429,\"message\":\"fixture rate limit\"}}\n\n"
        }
    }
}

#[derive(Clone)]
enum Ending {
    Eof,
    Hold,
    Reset(Arc<tokio::sync::Notify>),
    Heartbeats,
    Paced(Vec<(Duration, String)>),
}

struct Transport {
    chunk_size: usize,
    // Apply these only to slot zero; a fallback slot stays healthy.
    header_delay: Duration,
    status: axum::http::StatusCode,
}
impl Default for Transport {
    fn default() -> Self {
        Self {
            chunk_size: 19,
            header_delay: Duration::ZERO,
            status: axum::http::StatusCode::OK,
        }
    }
}

struct Fixture {
    upstream: Server,
    gateway: Server,
    dropped: Arc<AtomicUsize>,
    log: Arc<Mutex<RequestLog>>,
    attempts: Arc<Mutex<Vec<usize>>>,
    state: Arc<AppState>,
    sent_models: Arc<Mutex<Vec<String>>>,
}
impl Fixture {
    async fn new(protocol: Protocol, script: String, ending: Ending) -> Self {
        Self::with_keys(protocol, vec![script], ending).await
    }
    async fn with_keys(protocol: Protocol, scripts: Vec<String>, ending: Ending) -> Self {
        Self::with_config(protocol, scripts, ending, |_| {}).await
    }
    async fn with_config(
        protocol: Protocol,
        scripts: Vec<String>,
        ending: Ending,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        Self::with_chunk_size(protocol, scripts, ending, 19, configure).await
    }
    async fn with_chunk_size(
        protocol: Protocol,
        scripts: Vec<String>,
        ending: Ending,
        chunk_size: usize,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        Self::with_transport(
            protocol,
            scripts,
            ending,
            Transport {
                chunk_size,
                ..Transport::default()
            },
            configure,
        )
        .await
    }
    async fn with_transport(
        protocol: Protocol,
        scripts: Vec<String>,
        ending: Ending,
        transport: Transport,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        let Transport {
            chunk_size,
            header_delay,
            status,
        } = transport;
        let sent_models = Arc::new(Mutex::new(Vec::new()));
        let captured_models = sent_models.clone();
        let extra_keys = (0..scripts.len())
            .map(|i| format!("secret-upstream-key-{i}"))
            .collect();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let dropped = Arc::new(AtomicUsize::new(0));
        let flag = dropped.clone();
        let upstream =
            serve(Router::new().route(
                "/{*path}",
                post(move |headers: axum::http::HeaderMap, uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
                    let key = headers
                        .get("authorization")
                        .or_else(|| headers.get("x-api-key"))
                        .or_else(|| headers.get("x-goog-api-key"))
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .trim_start_matches("Bearer ");
                    let slot = key
                        .strip_prefix("secret-upstream-key-")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    observed.lock().unwrap().push(slot);
                    let model = body.get("model").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| uri.path().split("/models/").nth(1).unwrap().split(':').next().unwrap().to_owned());
                    captured_models.lock().unwrap().push(model);
                    let script = scripts[slot].clone();
                    let ending = ending.clone();
                    let guard = Dropped(flag.clone());
                    async move {
                        if slot == 0 { tokio::time::sleep(header_delay).await; }
                        Response::builder().status(if slot == 0 { status } else { axum::http::StatusCode::OK }).header("content-type", "text/event-stream").header("retry-after", "2")
                    .body(Body::from_stream(async_stream::stream! {
                        let _guard = guard;
                        // Deliberately cut inside JSON/SSE frame boundaries.
                        for bytes in script.as_bytes().chunks(chunk_size) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(bytes));
                        }
                        match ending {
                            Ending::Eof => {},
                            Ending::Hold => std::future::pending::<()>().await,
                            Ending::Heartbeats => loop {
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                yield Ok(Bytes::from_static(b": heartbeat\n\n"));
                            },
                            Ending::Paced(frames) => {
                                for (delay, frame) in frames {
                                    tokio::time::sleep(delay).await;
                                    yield Ok(Bytes::from(frame));
                                }
                                std::future::pending::<()>().await;
                            },
                            Ending::Reset(signal) => {
                                signal.notified().await;
                                yield Err(std::io::Error::other("fixture upstream reset"));
                            }
                        }
                    })).unwrap()
                    }
                }),
            ))
            .await;
        let provider = ProviderConfig {
            id: "mock".into(),
            name: "fixture".into(),
            protocol,
            base_url: format!(
                "{}/{}",
                upstream.url,
                if protocol == Protocol::Gemini {
                    "v1beta"
                } else {
                    "v1"
                }
            ),
            api_key: "secret-upstream-key".into(),
            api_keys: extra_keys,
            models: vec!["up-model".into()],
            fallback: vec![],
            key_cooldown_ms: None,
            capabilities: Default::default(),
            key_generation: 0,
        };
        let mut config = Config {
            token: "secret-local-token".into(),
            default_provider: "mock".into(),
            provider: vec![provider],
            ..Config::default()
        };
        configure(&mut config);
        let log = Arc::new(Mutex::new(RequestLog::default()));
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(config, None),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: log.clone(),
            key_rotation: KeyRotator::default(),
        });
        let gateway = serve(build_router(state.clone())).await;
        Self {
            upstream,
            gateway,
            dropped,
            log,
            attempts,
            state,
            sent_models,
        }
    }
    async fn request(&self, protocol: Protocol) -> reqwest::Response {
        self.request_model(protocol, "mock/up-model").await
    }
    async fn request_model(&self, protocol: Protocol, model: &str) -> reqwest::Response {
        let response = self.request_model_raw(protocol, model).await;
        assert!(response.status().is_success());
        response
    }
    async fn request_model_raw(&self, protocol: Protocol, model: &str) -> reqwest::Response {
        let (path, body) = match protocol {
            Protocol::Chat => (
                "/v1/chat/completions".to_owned(),
                json!({"model": model, "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hello"}]}),
            ),
            Protocol::Anthropic => (
                "/v1/messages".to_owned(),
                json!({"model": model, "stream": true, "max_tokens": 32, "messages": [{"role": "user", "content": "hello"}]}),
            ),
            Protocol::Responses => (
                "/v1/responses".to_owned(),
                json!({"model": model, "stream": true, "input": "hello"}),
            ),
            Protocol::Gemini => (
                format!("/v1beta/models/{model}:streamGenerateContent"),
                json!({"contents": [{"role": "user", "parts": [{"text": "hello"}]}]}),
            ),
        };
        tokio::time::timeout(
            LIMIT,
            reqwest::Client::new()
                .post(format!("{}{path}", self.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send(),
        )
        .await
        .expect("gateway headers timed out")
        .unwrap()
    }
    async fn record(&self) -> Value {
        tokio::time::timeout(LIMIT, async {
            loop {
                let records = self.log.lock().unwrap().snapshot().recent_requests;
                if !records.is_empty() {
                    assert_eq!(records.len(), 1, "one terminal record per request");
                    return serde_json::to_value(&records[0]).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no final request record")
    }
    async fn assert_released(&self) {
        tokio::time::timeout(LIMIT, async {
            while self.dropped.load(Ordering::SeqCst) < self.attempts.lock().unwrap().len() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("upstream response still live at {}", self.upstream.url));
    }
}

fn assert_completed(body: &str, protocol: Protocol) {
    let mut parser = magpie_core::stream::SseParser::new();
    let mut remaining = body.as_bytes();
    let mut frames = Vec::<Value>::new();
    let mut done = 0;
    while let Some(data) = parser.feed(&mut remaining).unwrap() {
        if data == "[DONE]" {
            done += 1;
        } else {
            frames.push(serde_json::from_str(&data).unwrap());
        }
    }
    let (input, output) = match protocol {
        Protocol::Chat => {
            assert_eq!(done, 1, "duplicate Chat terminal: {body}");
            let reports: Vec<_> = frames.iter().filter_map(|v| v.get("usage")).collect();
            assert_eq!(reports.len(), 1, "usage must be emitted only once");
            (
                reports[0]["prompt_tokens"].clone(),
                reports[0]["completion_tokens"].clone(),
            )
        }
        Protocol::Anthropic => {
            assert_eq!(
                frames
                    .iter()
                    .filter(|v| v["type"] == "message_stop")
                    .count(),
                1
            );
            let final_delta = frames
                .iter()
                .find(|v| v["type"] == "message_delta")
                .unwrap();
            (
                final_delta["usage"]["input_tokens"].clone(),
                final_delta["usage"]["output_tokens"].clone(),
            )
        }
        Protocol::Responses => {
            let completed: Vec<_> = frames
                .iter()
                .filter(|v| v["type"] == "response.completed")
                .collect();
            assert_eq!(completed.len(), 1);
            (
                completed[0]["response"]["usage"]["input_tokens"].clone(),
                completed[0]["response"]["usage"]["output_tokens"].clone(),
            )
        }
        Protocol::Gemini => {
            let completed: Vec<_> = frames
                .iter()
                .filter(|v| v.pointer("/candidates/0/finishReason").is_some())
                .collect();
            assert_eq!(completed.len(), 1);
            (
                completed[0]["usageMetadata"]["promptTokenCount"].clone(),
                completed[0]["usageMetadata"]["candidatesTokenCount"].clone(),
            )
        }
    };
    assert_eq!(input, 7, "{body}");
    assert_eq!(output, 3, "{body}");
}

fn assert_no_success(body: &str) {
    for marker in [
        "data: [DONE]",
        "event: message_stop",
        "event: response.completed",
        "\"finishReason\"",
    ] {
        assert!(
            !body.contains(marker),
            "fabricated success after error: {body}"
        );
    }
}
fn assert_partial_usage(record: &Value, protocol: Protocol) {
    if protocol == Protocol::Responses {
        assert!(record["usage"]["input"].is_null());
        assert!(record["usage"]["output"].is_null());
    } else {
        assert_eq!(record["usage"]["input"], 7);
        assert_eq!(record["usage"]["output"], 0);
    }
}

#[tokio::test]
async fn terminal_markers_release_held_upstream_for_every_protocol_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            // Data following the terminal marker must never reach the agent.
            let script = prefix(upstream) + completion(upstream) + error(upstream);
            let f = Fixture::new(upstream, script, Ending::Hold).await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap_or_else(|_| panic!("held terminal: {upstream:?} -> {client:?}"))
                .unwrap();
            assert!(body.contains("partial"));
            assert!(!body.contains("fixture rate limit"));
            assert_completed(&body, client);
            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn explicit_errors_release_held_upstream_without_success_for_every_protocol_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script = prefix(upstream) + error(upstream) + completion(upstream);
            let f = Fixture::new(upstream, script, Ending::Hold).await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap_or_else(|_| panic!("held error: {upstream:?} -> {client:?}"))
                .unwrap();
            assert!(body.contains("fixture rate limit"), "{body}");
            assert_no_success(&body);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_partial_usage(&record, upstream);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn eof_without_completion_is_an_error_for_every_protocol_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::new(upstream, prefix(upstream), Ending::Eof).await;
            let body = f.request(client).await.text().await.unwrap();
            assert!(
                body.contains("upstream stream ended before a completion marker"),
                "{upstream:?} -> {client:?}: {body}"
            );
            assert_no_success(&body);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_partial_usage(&record, upstream);
        }
    }
}

#[tokio::test]
async fn unterminated_final_sse_frame_completes_once_for_every_protocol_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script = (prefix(upstream) + completion(upstream))
                .trim_end()
                .to_owned();
            let f = Fixture::new(upstream, script, Ending::Eof).await;
            let body = f.request(client).await.text().await.unwrap();
            assert!(body.contains("partial"));
            assert!(!body.contains("upstream stream ended"));
            assert_completed(&body, client);
            assert_eq!(f.record().await["status"], "ok");
        }
    }
}

#[tokio::test]
async fn downstream_disconnect_releases_upstream_and_keeps_observed_usage() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::new(upstream, prefix(upstream), Ending::Hold).await;
            let mut response = f.request(client).await;
            let mut received = String::new();
            tokio::time::timeout(LIMIT, async {
                while !received.contains("partial") {
                    let chunk = response
                        .chunk()
                        .await
                        .unwrap()
                        .expect("stream ended before text");
                    received.push_str(&String::from_utf8_lossy(&chunk));
                }
            })
            .await
            .expect("no partial text");
            drop(response);
            f.assert_released().await;
            let record = f.record().await;
            assert_eq!(record["status"], "client_disconnected");
            assert_eq!(record["provider"], "mock");
            assert_eq!(record["upstream_model"], "up-model");
            assert_partial_usage(&record, upstream);
            let serialized = record.to_string();
            assert!(!serialized.contains("secret-"));
            assert!(!serialized.contains("partial"));
        }
    }
}

#[tokio::test]
async fn upstream_transport_reset_is_not_a_success_and_keeps_usage() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let reset = Arc::new(tokio::sync::Notify::new());
            let f = Fixture::new(upstream, prefix(upstream), Ending::Reset(reset.clone())).await;
            let mut response = f.request(client).await;
            let mut body = String::new();
            tokio::time::timeout(LIMIT, async {
                while !body.contains("partial") {
                    let chunk = response
                        .chunk()
                        .await
                        .unwrap()
                        .expect("missing partial text");
                    body.push_str(&String::from_utf8_lossy(&chunk));
                }
            })
            .await
            .expect("no text before reset");
            // Wait for downstream text so this is a mid-body transport failure,
            // not a connection/header failure or an accidental prefetch race.
            reset.notify_one();
            body.push_str(
                &tokio::time::timeout(LIMIT, response.text())
                    .await
                    .unwrap()
                    .unwrap(),
            );
            assert!(body.contains("upstream stream read failed"), "{body}");
            assert!(!body.contains("fixture upstream reset"));
            assert_no_success(&body);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_partial_usage(&record, upstream);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn chat_finish_reason_accepts_clean_eof_but_keeps_later_usage() {
    for client in PROTOCOLS {
        let script =
            (prefix(Protocol::Chat) + completion(Protocol::Chat)).replace("data: [DONE]\n\n", "");
        let f = Fixture::new(Protocol::Chat, script, Ending::Eof).await;
        let body = f.request(client).await.text().await.unwrap();
        assert_completed(&body, client);
        assert_eq!(f.record().await["status"], "ok");
    }
}

#[tokio::test]
async fn chat_sentinel_is_not_a_completion_marker_in_other_protocols() {
    for upstream in [Protocol::Anthropic, Protocol::Responses, Protocol::Gemini] {
        let f = Fixture::new(upstream, prefix(upstream) + "data: [DONE]\n\n", Ending::Eof).await;
        let body = f.request(Protocol::Chat).await.text().await.unwrap();
        assert!(body.contains("upstream stream ended before a completion marker"));
        assert_no_success(&body);
        assert_eq!(f.record().await["status"], "upstream_stream_error");
    }
}

#[tokio::test]
async fn responses_error_envelopes_close_held_connections() {
    for frame in [
        "data: {\"error\":{\"message\":\"fixture rate limit\"}}\n\n",
        "event: error\ndata: {\"type\":\"error\",\"message\":\"fixture rate limit\"}\n\n",
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"fixture rate limit\"}}\n\n",
    ] {
        for client in PROTOCOLS {
            let f = Fixture::new(
                Protocol::Responses,
                prefix(Protocol::Responses) + frame,
                Ending::Hold,
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert!(body.contains("fixture rate limit"));
            assert_no_success(&body);
            assert_eq!(f.record().await["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}

// Metadata is not an answer: abandoning it must not leak its model/usage.
fn opening(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Chat => {
            "data: {\"model\":\"abandoned-model\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}],\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":0}}\n\n"
        }
        Protocol::Anthropic => {
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"abandoned-model\",\"usage\":{\"input_tokens\":99,\"output_tokens\":0}}}\n\n"
        }
        Protocol::Responses => {
            "data: {\"type\":\"response.created\",\"response\":{\"model\":\"abandoned-model\"}}\n\n"
        }
        Protocol::Gemini => {
            "data: {\"modelVersion\":\"abandoned-model\",\"usageMetadata\":{\"promptTokenCount\":99,\"candidatesTokenCount\":0}}\n\n"
        }
    }
}
fn limited(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Responses => {
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"fixture rate limit\"}}}\n\n"
        }
        _ => error(protocol),
    }
}

#[tokio::test]
async fn early_stream_rate_limit_tries_next_key_without_leaking_opening() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_keys(
                upstream,
                vec![
                    opening(upstream).to_owned() + limited(upstream),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_completed(&body, client);
            assert!(
                !body.contains("abandoned-model") && !body.contains("fixture rate limit"),
                "{body}"
            );
            assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["key_slot"], 1);
            assert_eq!(record["usage"]["input"], 7);
            let stats = serde_json::to_value(f.log.lock().unwrap().snapshot()).unwrap();
            assert_eq!(stats["providers"]["mock"]["keys"]["0"]["attempts"], 1);
            assert_eq!(stats["providers"]["mock"]["keys"]["0"]["errors"], 1);
            assert_eq!(stats["providers"]["mock"]["keys"]["1"]["successes"], 1);
            assert!(
                f.state
                    .key_rotation
                    .cooldown_snapshot(&f.state.config.current())["mock"]
                    .contains_key("0")
            );
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn exhausted_stream_keys_return_one_error_and_last_attempt_usage() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script = opening(upstream).to_owned() + limited(upstream);
            let f = Fixture::with_keys(upstream, vec![script.clone(), script], Ending::Hold).await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(body.matches("fixture rate limit").count(), 1, "{body}");
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_eq!(record["key_slot"], 1);
            if upstream != Protocol::Responses {
                assert_eq!(record["usage"]["input"], 99);
            }
            let cooldowns = f
                .state
                .key_rotation
                .cooldown_snapshot(&f.state.config.current());
            assert_eq!(cooldowns["mock"].len(), 2);
            for slot in cooldowns["mock"].values() {
                assert!(slot.remaining_ms > 0 && slot.remaining_ms <= 2000);
            }
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn invalid_stream_requests_do_not_consume_other_keys_or_cool_down() {
    for upstream in PROTOCOLS {
        let invalid = if upstream == Protocol::Responses {
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"fixture request limit\"}}}\n\n"
        } else {
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"code\":400,\"message\":\"fixture request limit\"}}\n\n"
        };
        for client in PROTOCOLS {
            let f = Fixture::with_keys(
                upstream,
                vec![
                    opening(upstream).to_owned() + invalid,
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert!(body.contains("fixture request limit"), "{body}");
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
            assert_eq!(f.record().await["key_slot"], 0);
            assert!(
                f.state
                    .key_rotation
                    .cooldown_snapshot(&f.state.config.current())
                    .is_empty()
            );
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn stream_rate_limit_after_text_never_retries_or_splices_answers() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_keys(
                upstream,
                vec![
                    prefix(upstream) + limited(upstream),
                    prefix(upstream).replace("partial", "must not appear") + completion(upstream),
                ],
                Ending::Hold,
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert!(
                body.contains("partial") && body.contains("fixture rate limit"),
                "{body}"
            );
            assert!(!body.contains("must not appear"));
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_partial_usage(&record, upstream);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn early_stream_errors_fall_back_across_protocols_and_groups() {
    use magpie_gateway::{GroupConfig, RoutingPolicy};
    for grouped in [false, true] {
        for upstream in PROTOCOLS {
            for spare in PROTOCOLS {
                let f = Fixture::with_config(
                    upstream,
                    vec![
                        opening(upstream).to_owned() + limited(upstream),
                        prefix(spare) + completion(spare),
                    ],
                    Ending::Hold,
                    |config| {
                        let mut backup = config.provider[0].clone();
                        backup.id = "spare".into();
                        backup.protocol = spare;
                        backup.api_keys = vec!["secret-upstream-key-1".into()];
                        backup.models = vec!["backup-model".into()];
                        config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                        if grouped {
                            config.group.push(GroupConfig {
                                id: "daily".into(),
                                name: "Daily".into(),
                                routes: vec!["mock/up-model".into(), "spare/backup-model".into()],
                                routing: RoutingPolicy::Order,
                            });
                        } else {
                            config.provider[0].fallback = vec!["spare/backup-model".into()];
                        }
                        config.provider.push(backup);
                    },
                )
                .await;
                let requested = if grouped {
                    "group/daily"
                } else {
                    "mock/up-model"
                };
                let body = tokio::time::timeout(
                    LIMIT,
                    f.request_model(Protocol::Chat, requested).await.text(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_completed(&body, Protocol::Chat);
                assert!(
                    !body.contains("fixture rate limit") && !body.contains("abandoned-model"),
                    "{body}"
                );
                assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
                assert_eq!(
                    *f.sent_models.lock().unwrap(),
                    vec!["up-model", "backup-model"]
                );
                let record = f.record().await;
                assert_eq!(record["provider"], "spare");
                assert_eq!(record["requested_model"], requested);
                assert_eq!(record["upstream_model"], "backup-model");
                assert_eq!(record["key_slot"], 0);
                assert_eq!(record["usage"]["input"], 7);
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn reasoning_and_tool_calls_commit_the_route_before_a_later_error() {
    let frames = reasoning_and_tool_frames();
    for (upstream, frame) in frames {
        for client in PROTOCOLS {
            let f = Fixture::with_keys(
                upstream,
                vec![
                    format!(
                        "{}data: {frame}\n\n{}",
                        opening(upstream),
                        limited(upstream)
                    ),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert!(body.contains("fixture rate limit"), "{body}");
            assert_no_success(&body);
            assert_eq!(
                *f.attempts.lock().unwrap(),
                vec![0],
                "replayed reasoning/tool call: {body}"
            );
            assert_eq!(f.record().await["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn bounded_opening_commits_without_unbounded_retry_buffering() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_keys(
            upstream,
            vec![
                opening(upstream).repeat(80) + limited(upstream),
                prefix(upstream) + completion(upstream),
            ],
            Ending::Hold,
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("fixture rate limit"));
        assert_no_success(&body);
        assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        assert_eq!(f.record().await["status"], "upstream_stream_error");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn early_auth_errors_exhaust_keys_then_fall_back_but_do_not_cool_down() {
    let auth = "data: {\"error\":{\"code\":401,\"message\":\"fixture authentication failed\"}}\n\n";
    let f = Fixture::with_config(
        Protocol::Chat,
        vec![
            auth.into(),
            auth.into(),
            prefix(Protocol::Chat) + completion(Protocol::Chat),
        ],
        Ending::Hold,
        |config| {
            let mut backup = config.provider[0].clone();
            backup.id = "spare".into();
            backup.api_keys = vec!["secret-upstream-key-2".into()];
            config.provider[0].api_keys.truncate(2);
            config.provider[0].fallback = vec!["spare/up-model".into()];
            config.provider.push(backup);
        },
    )
    .await;
    let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
        .await
        .unwrap()
        .unwrap();
    assert_completed(&body, Protocol::Chat);
    assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1, 2]);
    assert_eq!(f.record().await["provider"], "spare");
    assert!(
        f.state
            .key_rotation
            .cooldown_snapshot(&f.state.config.current())
            .is_empty()
    );
    f.assert_released().await;
}

#[tokio::test]
async fn cancelled_fallback_persists_only_selected_provider_and_partial_usage() {
    for grouped in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let history = directory.path().join("history.jsonl");
        let f = Fixture::with_config(
            Protocol::Chat,
            vec![
                opening(Protocol::Chat).to_owned() + limited(Protocol::Chat),
                prefix(Protocol::Anthropic),
            ],
            Ending::Hold,
            |config| {
                let mut backup = config.provider[0].clone();
                backup.id = "spare".into();
                backup.protocol = Protocol::Anthropic;
                backup.api_keys = vec!["secret-upstream-key-1".into()];
                backup.models = vec!["backup-model".into()];
                config.provider[0].api_keys.truncate(1);
                if grouped {
                    config.group.push(magpie_gateway::GroupConfig {
                        id: "daily".into(),
                        name: "".into(),
                        routes: vec!["mock/up-model".into(), "spare/backup-model".into()],
                        routing: magpie_gateway::RoutingPolicy::Order,
                    });
                } else {
                    config.provider[0].fallback = vec!["spare/backup-model".into()];
                }
                config.provider.push(backup);
            },
        )
        .await;
        *f.log.lock().unwrap() = RequestLog::open(&history).unwrap();
        let requested = if grouped {
            "group/daily"
        } else {
            "mock/up-model"
        };
        let mut response = f.request_model(Protocol::Chat, requested).await;
        let mut body = String::new();
        tokio::time::timeout(LIMIT, async {
            while !body.contains("partial") {
                body.push_str(&String::from_utf8_lossy(
                    &response.chunk().await.unwrap().unwrap(),
                ));
            }
        })
        .await
        .unwrap();
        drop(response);
        f.assert_released().await;
        let record = f.record().await;
        assert_eq!(record["status"], "client_disconnected");
        assert_eq!(record["provider"], "spare");
        assert_eq!(record["upstream_model"], "backup-model");
        assert_eq!(record["requested_model"], requested);
        assert_eq!(record["usage"]["input"], 7);
        assert_eq!(record["usage"]["output"], 0);
        let persisted = std::fs::read_to_string(&history).unwrap();
        assert_eq!(persisted.lines().count(), 1);
        assert_eq!(serde_json::from_str::<Value>(&persisted).unwrap(), record);
        let loaded = RequestLog::open(&history)
            .unwrap()
            .snapshot()
            .recent_requests;
        assert_eq!(serde_json::to_value(&loaded[0]).unwrap(), record);
        for private in [
            "secret-upstream",
            "secret-local",
            "abandoned-model",
            "partial",
        ] {
            assert!(!persisted.contains(private));
        }
    }
}

#[tokio::test]
async fn bom_and_cr_only_streams_complete_for_every_protocol_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script =
                format!("\u{feff}{}{}", prefix(upstream), completion(upstream)).replace('\n', "\r");
            let f = Fixture::new(upstream, script, Ending::Hold).await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_completed(&body, client);
            f.assert_released().await;
            assert_eq!(f.record().await["status"], "ok");
        }
    }
}

// No separator and no EOF: the size guard must terminate while the peer holds
// the connection open, not wait for a completed frame before checking it.
fn oversized_frame() -> String {
    format!(
        "data: {}",
        "x".repeat(magpie_core::stream::MAX_SSE_EVENT_BYTES)
    )
}

#[tokio::test]
async fn oversized_opening_frames_fail_once_without_spending_other_keys() {
    for (upstream, client) in PROTOCOLS.into_iter().zip(PROTOCOLS.into_iter().rev()) {
        let f = Fixture::with_chunk_size(
            upstream,
            vec![
                opening(upstream).to_owned() + &oversized_frame(),
                prefix(upstream) + completion(upstream),
            ],
            Ending::Hold,
            16 * 1024,
            |_| {},
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            body.matches("upstream SSE event exceeds").count(),
            1,
            "{body}"
        );
        assert!(!body.contains("xxx"));
        assert_no_success(&body);
        assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        assert_eq!(f.record().await["status"], "upstream_stream_error");
        assert!(
            f.state
                .key_rotation
                .cooldown_snapshot(&f.state.config.current())
                .is_empty()
        );
        f.assert_released().await;
    }
}

#[tokio::test]
async fn oversized_opening_frames_fall_back_before_content_for_routes_and_groups() {
    for upstream in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_chunk_size(
                upstream,
                vec![
                    opening(upstream).to_owned() + &oversized_frame(),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
                16 * 1024,
                |config| {
                    let mut spare = config.provider[0].clone();
                    spare.id = "spare".into();
                    spare.api_keys = vec!["secret-upstream-key-1".into()];
                    spare.models = vec!["backup-model".into()];
                    config.provider[0].api_keys.truncate(1);
                    if grouped {
                        config.group.push(magpie_gateway::GroupConfig {
                            id: "daily".into(),
                            name: "".into(),
                            routes: vec!["mock/up-model".into(), "spare/backup-model".into()],
                            routing: magpie_gateway::RoutingPolicy::Order,
                        });
                    } else {
                        config.provider[0].fallback = vec!["spare/backup-model".into()];
                    }
                    config.provider.push(spare);
                },
            )
            .await;
            let model = if grouped {
                "group/daily"
            } else {
                "mock/up-model"
            };
            let body =
                tokio::time::timeout(LIMIT, f.request_model(Protocol::Chat, model).await.text())
                    .await
                    .unwrap()
                    .unwrap();
            assert_completed(&body, Protocol::Chat);
            assert!(!body.contains("exceeds"));
            assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["provider"], "spare");
            assert_eq!(record["upstream_model"], "backup-model");
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn oversized_frames_after_content_never_retry_and_keep_partial_usage() {
    for (upstream, client) in PROTOCOLS.into_iter().zip(PROTOCOLS) {
        let f = Fixture::with_chunk_size(
            upstream,
            vec![
                prefix(upstream) + &oversized_frame(),
                prefix(upstream) + completion(upstream),
            ],
            Ending::Hold,
            16 * 1024,
            |config| {
                let mut spare = config.provider[0].clone();
                spare.id = "spare".into();
                spare.api_keys = vec!["secret-upstream-key-1".into()];
                config.provider[0].api_keys.truncate(1);
                config.provider[0].fallback = vec!["spare/up-model".into()];
                config.provider.push(spare);
            },
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("partial"));
        assert_eq!(body.matches("upstream SSE event exceeds").count(), 1);
        assert_no_success(&body);
        assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        let record = f.record().await;
        assert_eq!(record["status"], "upstream_stream_error");
        assert_eq!(record["provider"], "mock");
        assert_partial_usage(&record, upstream);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn opening_metadata_byte_budget_commits_before_a_later_error() {
    for upstream in [Protocol::Anthropic, Protocol::Responses, Protocol::Gemini] {
        let opening = opening(upstream).replace("abandoned-model", &"m".repeat(70 * 1024));
        let f = Fixture::with_chunk_size(
            upstream,
            vec![
                opening + limited(upstream),
                prefix(upstream) + completion(upstream),
            ],
            Ending::Hold,
            4096,
            |_| {},
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("fixture rate limit"));
        assert_no_success(&body);
        assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        assert_eq!(f.record().await["status"], "upstream_stream_error");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn discarded_chat_model_metadata_does_not_spend_the_opening_memory_budget() {
    // The Chat decoder does not retain model-only metadata in a Start event.
    // A wire-size budget here would incorrectly prevent a safe early retry.
    let f = Fixture::with_chunk_size(
        Protocol::Chat,
        vec![
            opening(Protocol::Chat).replace("abandoned-model", &"m".repeat(70 * 1024))
                + limited(Protocol::Chat),
            prefix(Protocol::Chat) + completion(Protocol::Chat),
        ],
        Ending::Hold,
        4096,
        |_| {},
    )
    .await;
    let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
        .await
        .unwrap()
        .unwrap();
    assert_completed(&body, Protocol::Chat);
    assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
    let record = f.record().await;
    assert_eq!(record["status"], "ok");
    assert_eq!(record["usage"]["input"], 7);
    f.assert_released().await;
}

#[tokio::test]
async fn stalled_opening_streams_timeout_once_without_spending_other_keys() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![opening(upstream).to_owned(), "must not try this key".into()],
                Ending::Hold,
                |config| {
                    config.stream_first_content_timeout_ms = 150;
                    config.stream_idle_timeout_ms = 0;
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                body.matches("upstream first-content timeout").count(),
                1,
                "{body}"
            );
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert!(
                f.state
                    .key_rotation
                    .cooldown_snapshot(&f.state.config.current())
                    .is_empty()
            );
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            if upstream == Protocol::Responses {
                assert!(record["usage"]["input"].is_null());
            } else {
                assert_eq!(record["usage"]["input"], 99);
                assert_eq!(record["usage"]["output"], 0);
            }
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn stalled_opening_streams_fall_back_before_content_for_routes_and_groups() {
    for upstream in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_config(
                upstream,
                vec![
                    opening(upstream).into(),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
                |config| {
                    config.stream_first_content_timeout_ms = 150;
                    config.stream_idle_timeout_ms = 0;
                    configure_timeout_fallback(config, grouped);
                },
            )
            .await;
            let body = tokio::time::timeout(
                LIMIT,
                f.request_model(
                    upstream,
                    if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    },
                )
                .await
                .text(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_completed(&body, upstream);
            assert!(!body.contains("timeout"));
            assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["provider"], "spare");
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            f.assert_released().await;
        }
    }
}

fn configure_timeout_fallback(config: &mut Config, grouped: bool) {
    let mut spare = config.provider[0].clone();
    spare.id = "spare".into();
    spare.api_keys = vec!["secret-upstream-key-1".into()];
    spare.models = vec!["backup-model".into()];
    // Leave a second primary credential present: a timeout must skip it.
    if grouped {
        config.group.push(magpie_gateway::GroupConfig {
            id: "daily".into(),
            name: String::new(),
            routes: vec!["mock/up-model".into(), "spare/backup-model".into()],
            routing: magpie_gateway::RoutingPolicy::Order,
        });
    } else {
        config.provider[0].fallback = vec!["spare/backup-model".into()];
    }
    config.provider.push(spare);
}

#[tokio::test]
async fn stalled_streams_after_content_timeout_without_retry_and_retain_usage() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![prefix(upstream), prefix(upstream) + completion(upstream)],
                Ending::Hold,
                |config| {
                    config.stream_first_content_timeout_ms = 150;
                    config.stream_idle_timeout_ms = 150;
                    configure_timeout_fallback(config, false);
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert!(body.contains("partial"), "{body}");
            assert_eq!(
                body.matches("upstream stream idle timeout").count(),
                1,
                "{body}"
            );
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_partial_usage(&record, upstream);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn heartbeats_do_not_extend_first_content_deadline_even_after_metadata_commit() {
    for upstream in PROTOCOLS {
        for commit_metadata in [false, true] {
            let f = Fixture::with_config(
                upstream,
                vec![
                    opening(upstream).repeat(if commit_metadata { 100 } else { 1 }),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Heartbeats,
                |config| {
                    config.stream_first_content_timeout_ms = 150;
                    config.stream_idle_timeout_ms = 100;
                    if commit_metadata {
                        configure_timeout_fallback(config, false);
                    }
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                body.matches("upstream first-content timeout").count(),
                1,
                "{body}"
            );
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn paced_streams_reset_idle_and_content_disarms_first_deadline() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![prefix(upstream)],
            Ending::Paced(vec![
                (Duration::from_millis(100), ": heartbeat\n\n".into()),
                (Duration::from_millis(100), ": heartbeat\n\n".into()),
                (Duration::from_millis(100), completion(upstream).into()),
            ]),
            |config| {
                config.stream_first_content_timeout_ms = 150;
                config.stream_idle_timeout_ms = 250;
            },
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(upstream).await.text())
            .await
            .unwrap()
            .unwrap();
        assert_completed(&body, upstream);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn header_and_error_body_stalls_use_504_and_skip_endpoint_keys() {
    for client in PROTOCOLS {
        for error_body in [false, true] {
            for fallback in [false, true] {
                let f = Fixture::with_transport(
                    Protocol::Chat,
                    vec![
                        if error_body {
                            "{\"error\":{\"message\":\"unfinished".into()
                        } else {
                            prefix(Protocol::Chat) + completion(Protocol::Chat)
                        },
                        prefix(Protocol::Chat) + completion(Protocol::Chat),
                    ],
                    Ending::Hold,
                    Transport {
                        header_delay: if error_body {
                            Duration::ZERO
                        } else {
                            Duration::from_millis(250)
                        },
                        status: if error_body {
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR
                        } else {
                            axum::http::StatusCode::OK
                        },
                        ..Transport::default()
                    },
                    |config| {
                        config.stream_first_content_timeout_ms = 150;
                        config.stream_idle_timeout_ms = 0;
                        if fallback {
                            configure_timeout_fallback(config, false);
                        }
                    },
                )
                .await;
                let response = f.request_model_raw(client, "mock/up-model").await;
                assert_eq!(response.status().as_u16(), if fallback { 200 } else { 504 });
                let body = tokio::time::timeout(LIMIT, response.text())
                    .await
                    .unwrap()
                    .unwrap();
                if fallback {
                    assert_completed(&body, client);
                    assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
                    assert_eq!(f.record().await["provider"], "spare");
                } else {
                    assert!(body.contains("upstream first-content timeout"), "{body}");
                    assert_eq!(*f.attempts.lock().unwrap(), [0]);
                    assert_eq!(f.record().await["status"], "upstream_error");
                }
                assert!(
                    f.state
                        .key_rotation
                        .cooldown_snapshot(&f.state.config.current())
                        .is_empty()
                );
                f.assert_released().await;
            }
        }
    }
}

fn reasoning_and_tool_frames() -> [(Protocol, Value); 8] {
    [
        (
            Protocol::Chat,
            json!({"choices":[{"delta":{"reasoning_content":"fixture reasoning"}}]}),
        ),
        (
            Protocol::Chat,
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fixture_tool"}}]}}]}),
        ),
        (
            Protocol::Anthropic,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"fixture reasoning"}}),
        ),
        (
            Protocol::Anthropic,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"fixture_tool","input":{}}}),
        ),
        (
            Protocol::Responses,
            json!({"type":"response.reasoning_summary_text.delta","delta":"fixture reasoning"}),
        ),
        (
            Protocol::Responses,
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"fixture_tool"}}),
        ),
        (
            Protocol::Gemini,
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"fixture reasoning"}]}}]}),
        ),
        (
            Protocol::Gemini,
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"fixture_tool","args":{}}}]}}]}),
        ),
    ]
}

#[tokio::test]
async fn reasoning_and_tools_disarm_first_content_deadline_in_real_streams() {
    for (upstream, frame) in reasoning_and_tool_frames() {
        let f = Fixture::with_config(
            upstream,
            vec![format!(
                "{}data: {frame}\n\n",
                opening(upstream).replace("99", "7")
            )],
            Ending::Paced(vec![(
                Duration::from_millis(250),
                completion(upstream).into(),
            )]),
            |config| {
                config.stream_first_content_timeout_ms = 150;
                config.stream_idle_timeout_ms = 500;
            },
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(upstream).await.text())
            .await
            .unwrap()
            .unwrap();
        assert_completed(&body, upstream);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn read_idle_can_fail_over_before_content_when_first_content_limit_is_disabled() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![
                opening(upstream).into(),
                prefix(upstream) + completion(upstream),
            ],
            Ending::Hold,
            |config| {
                config.stream_first_content_timeout_ms = 0;
                config.stream_idle_timeout_ms = 150;
                configure_timeout_fallback(config, true);
            },
        )
        .await;
        let body =
            tokio::time::timeout(LIMIT, f.request_model(upstream, "group/daily").await.text())
                .await
                .unwrap()
                .unwrap();
        assert_completed(&body, upstream);
        assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
        assert_eq!(f.record().await["provider"], "spare");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn zero_timeouts_allow_a_delayed_first_answer() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![opening(upstream).into()],
            Ending::Paced(vec![(
                Duration::from_millis(250),
                prefix(upstream) + completion(upstream),
            )]),
            |config| {
                config.stream_first_content_timeout_ms = 0;
                config.stream_idle_timeout_ms = 0;
            },
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(upstream).await.text())
            .await
            .unwrap()
            .unwrap();
        assert_completed(&body, upstream);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}
