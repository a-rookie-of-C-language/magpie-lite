//! Real HTTP fixtures for stream termination and cancellation, not live-vendor tests.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{
    Router,
    body::{Body, Bytes},
    response::Response,
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig,
    build_router, usage::RequestLog,
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
    /// Once armed, every ask's stream resets mid-reply — a continuation's
    /// too.
    Reset(Arc<AtomicBool>),
    Heartbeats,
    Paced(Vec<(Duration, String)>),
    PacedEof(Vec<(Duration, String)>),
    PerSlot(Vec<Ending>),
    /// Select a response script by request number, allowing one key to be
    /// exercised across the initial attempt and a reasoning-only reask.
    PerRequest(Vec<String>, Box<Ending>),
}

struct Transport {
    chunk_size: usize,
    // Delay one selected slot; non-2xx status still applies only to slot zero.
    header_delay: Duration,
    header_delay_slot: usize,
    // Optional request ordinal overrides the slot filter for reask fixtures.
    header_delay_request: Option<usize>,
    status_request: Option<usize>,
    status: axum::http::StatusCode,
}
impl Default for Transport {
    fn default() -> Self {
        Self {
            chunk_size: 19,
            header_delay: Duration::ZERO,
            header_delay_slot: 0,
            header_delay_request: None,
            status_request: None,
            status: axum::http::StatusCode::OK,
        }
    }
}

struct Fixture {
    _config_directory: tempfile::TempDir,
    upstream: Server,
    gateway: Server,
    dropped: Arc<AtomicUsize>,
    log: Arc<Mutex<RequestLog>>,
    attempts: Arc<Mutex<Vec<usize>>>,
    state: Arc<AppState>,
    sent_models: Arc<Mutex<Vec<String>>>,
    sent_bodies: Arc<Mutex<Vec<Value>>>,
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
            header_delay_slot,
            header_delay_request,
            status_request,
            status,
        } = transport;
        let sent_models = Arc::new(Mutex::new(Vec::new()));
        let captured_models = sent_models.clone();
        let sent_bodies = Arc::new(Mutex::new(Vec::new()));
        let captured_bodies = sent_bodies.clone();
        let extra_keys = (0..scripts.len())
            .map(|i| format!("secret-upstream-key-{i}"))
            .collect();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let dropped = Arc::new(AtomicUsize::new(0));
        let flag = dropped.clone();
        let request_counter = Arc::new(AtomicUsize::new(0));
        let requests = request_counter.clone();
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
                    captured_bodies.lock().unwrap().push(body);
                    let request_number = requests.fetch_add(1, Ordering::SeqCst);
                    let script = match &ending {
                        Ending::PerRequest(queue, _) => queue
                            .get(request_number)
                            .or_else(|| queue.last())
                            .cloned()
                            .expect("non-empty per-request fixture queue"),
                        _ => scripts[slot].clone(),
                    };
                    let ending = match &ending {
                        Ending::PerSlot(slots) => slots[slot].clone(),
                        Ending::PerRequest(_, ending) => ending.as_ref().clone(),
                        ending => ending.clone(),
                    };
                    let guard = Dropped(flag.clone());
                    async move {
                        if header_delay_request.map_or(slot == header_delay_slot, |request| request == request_number) { tokio::time::sleep(header_delay).await; }
                        Response::builder().status(if status_request.map_or(slot == 0, |request| request == request_number) { status } else { axum::http::StatusCode::OK }).header("content-type", "text/event-stream").header("retry-after", "2")
                    .body(Body::from_stream(async_stream::stream! {
                        let _guard = guard;
                        // Deliberately cut inside JSON/SSE frame boundaries.
                        for bytes in script.as_bytes().chunks(chunk_size) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(bytes));
                        }
                        match ending {
                            Ending::PerSlot(_) | Ending::PerRequest(_, _) => unreachable!("selected before response"),
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
                            Ending::PacedEof(frames) => {
                                for (delay, frame) in frames {
                                    tokio::time::sleep(delay).await;
                                    yield Ok(Bytes::from(frame));
                                }
                            },
                            Ending::Reset(signal) => {
                                // Each ask resets once armed: a mid-reply
                                // continuation gets the same cut again.
                                while !signal.load(Ordering::SeqCst) {
                                    tokio::time::sleep(Duration::from_millis(5)).await;
                                }
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
            contexts: Default::default(),
            fallback: vec![],
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: magpie_gateway::RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
        };
        let mut config = Config {
            token: "secret-local-token".into(),
            default_provider: "mock".into(),
            provider: vec![provider],
            ..Config::default()
        };
        configure(&mut config);
        let config_directory = tempfile::tempdir().unwrap();
        let config_path = config_directory.path().join("config.toml");
        config.save(&config_path).unwrap();
        let log = Arc::new(Mutex::new(RequestLog::default()));
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(config, Some(config_path)),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: log.clone(),
            key_rotation: KeyRotator::default(),
            affinity: AffinityStore::default(),
            lanes: magpie_gateway::lanes::Lanes::default(),
            rpms: magpie_gateway::lanes::Rpms::default(),
        });
        let gateway = serve(build_router(state.clone())).await;
        Self {
            _config_directory: config_directory,
            upstream,
            gateway,
            dropped,
            log,
            attempts,
            state,
            sent_models,
            sent_bodies,
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
        let (path, body) = client_request(protocol, model, true);
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
    assert_completed_usage(body, protocol, 7, 3);
}

fn assert_completed_usage(
    body: &str,
    protocol: Protocol,
    expected_input: u64,
    expected_output: u64,
) {
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
    assert_eq!(input, expected_input, "{body}");
    assert_eq!(output, expected_output, "{body}");
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

/// Whether a mid-reply cut asks the same candidate to go on from what the
/// client has (the reference's `continuation.go`): only where the
/// upstream goes on from a reply's part sent back — an Anthropic upstream
/// natively. A Chat upstream re-answers instead, a Gemini or Responses
/// one isn't asked, so there a cut reply ends as it used to.
fn mid_reply_continues(protocol: Protocol) -> bool {
    protocol == Protocol::Anthropic
}

/// The attempts a mid-reply cut costs: one plus a continuation's tries
/// where the reply can go on, one where it can't.
fn cut_attempts(protocol: Protocol) -> Vec<usize> {
    vec![0; if mid_reply_continues(protocol) { 3 } else { 1 }]
}

/// assert_partial_usage with the tries' usage summed, as a continuation
/// bills them.
fn assert_partial_usage_summed(record: &Value, protocol: Protocol) {
    if mid_reply_continues(protocol) {
        assert_eq!(record["usage"]["input"], 7 * 3);
        assert_eq!(record["usage"]["output"], 0);
    } else {
        assert_partial_usage(record, protocol);
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
async fn clean_eof_without_completion_only_succeeds_for_answered_gemini() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::new(upstream, prefix(upstream), Ending::Eof).await;
            let body = f.request(client).await.text().await.unwrap();
            let record = f.record().await;
            if upstream == Protocol::Gemini {
                assert!(body.contains("partial"), "{body}");
                assert_completed_usage(&body, client, 7, 0);
                assert_eq!(record["status"], "ok");
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
            } else {
                assert!(
                    body.contains("upstream stream ended before a completion marker"),
                    "{upstream:?} -> {client:?}: {body}"
                );
                assert_no_success(&body);
                assert_eq!(record["status"], "upstream_stream_error");
            }
            if upstream != Protocol::Gemini {
                assert_eq!(*f.attempts.lock().unwrap(), cut_attempts(upstream));
            }
            assert_partial_usage_summed(&record, upstream);
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
            let reset = Arc::new(AtomicBool::new(false));
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
            reset.store(true, Ordering::SeqCst);
            body.push_str(
                // A continuation's asks pause a second apiece between tries.
                &tokio::time::timeout(LIMIT * 2, response.text())
                    .await
                    .unwrap()
                    .unwrap(),
            );
            // A continued try's own transport failure reads differently;
            // the reply ends with an error either way.
            assert!(
                body.contains("upstream stream read failed")
                    || body.contains("reask could not reach upstream"),
                "{body}"
            );
            assert!(!body.contains("fixture upstream reset"));
            assert_no_success(&body);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            // A continued try's transport cut ends the reply rather than
            // ask a third time.
            assert_eq!(
                *f.attempts.lock().unwrap(),
                vec![0; if mid_reply_continues(upstream) { 2 } else { 1 }]
            );
            if mid_reply_continues(upstream) {
                // The continued try was cut before it said a thing:
                // nothing of it is billed.
                assert_eq!(record["usage"]["input"], 7);
                assert_eq!(record["usage"]["output"], 0);
            } else {
                assert_partial_usage(&record, upstream);
            }
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
        let message = if upstream == Protocol::Gemini {
            "bad Gemini upstream stream payload"
        } else {
            "upstream stream ended before a completion marker"
        };
        assert!(body.contains(message), "{body}");
        // An Anthropic upstream goes on from the reply's part sent back,
        // so the cut is re-asked of it until the tries are up.
        assert_eq!(*f.attempts.lock().unwrap(), cut_attempts(upstream));
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
                                rules: Vec::new(),
                                classifier: String::new(),
                                effort: String::new(),
                                pick: String::new(),
                                weights: Vec::new(),
                                routing: RoutingPolicy::Order,
                                first_token_timeout_ms: 0,
                                affinity: Default::default(),
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
                        rules: Vec::new(),
                        classifier: String::new(),
                        effort: String::new(),
                        pick: String::new(),
                        weights: Vec::new(),
                        routing: magpie_gateway::RoutingPolicy::Order,
                        first_token_timeout_ms: 0,
                        affinity: Default::default(),
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
                            rules: Vec::new(),
                            classifier: String::new(),
                            effort: String::new(),
                            pick: String::new(),
                            weights: Vec::new(),
                            routing: magpie_gateway::RoutingPolicy::Order,
                            first_token_timeout_ms: 0,
                            affinity: Default::default(),
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
        // Where the reply can go on from its part, the cut is asked again
        // of the same candidate before it ends with the error.
        assert_eq!(*f.attempts.lock().unwrap(), cut_attempts(upstream));
        let record = f.record().await;
        assert_eq!(record["status"], "upstream_stream_error");
        assert_eq!(record["provider"], "mock");
        assert_partial_usage_summed(&record, upstream);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn opening_metadata_byte_budget_commits_before_a_later_error() {
    for upstream in PROTOCOLS {
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
    // A usage-only Chat chunk has no assistant delta and cannot create Start.
    // Its ignored model must not spend retained metadata or prevent safe retry.
    let usage_only = json!({
        "model": "m".repeat(70 * 1024),
        "choices": [],
        "usage": {"prompt_tokens": 99, "completion_tokens": 0}
    });
    let f = Fixture::with_chunk_size(
        Protocol::Chat,
        vec![
            format!("data: {usage_only}\n\n{}", limited(Protocol::Chat)),
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
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            pick: String::new(),
            weights: Vec::new(),
            routing: magpie_gateway::RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
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
            // Where the reply can go on, its part is sent back to the same
            // stalled candidate before the error ends it.
            assert_eq!(*f.attempts.lock().unwrap(), cut_attempts(upstream));
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_partial_usage_summed(&record, upstream);
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
                // A Gemini reasoning-only STOP is now an empty reply. This
                // test isolates deadlines: the delayed completion answers.
                if upstream == Protocol::Gemini {
                    prefix(upstream) + completion(upstream)
                } else {
                    completion(upstream).into()
                },
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

fn client_request(protocol: Protocol, model: &str, stream: bool) -> (String, Value) {
    match protocol {
        Protocol::Chat => (
            "/v1/chat/completions".to_owned(),
            json!({"model": model, "stream": stream, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hello"}]}),
        ),
        Protocol::Anthropic => (
            "/v1/messages".to_owned(),
            json!({"model": model, "stream": stream, "max_tokens": 32, "messages": [{"role": "user", "content": "hello"}]}),
        ),
        Protocol::Responses => (
            "/v1/responses".to_owned(),
            json!({"model": model, "stream": stream, "input": "hello"}),
        ),
        Protocol::Gemini => (
            format!(
                "/v1beta/models/{model}:{}",
                if stream {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                }
            ),
            json!({"contents": [{"role": "user", "parts": [{"text": "hello"}]}]}),
        ),
    }
}

impl Fixture {
    fn persistent_history(&self) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        *self.log.lock().unwrap() =
            RequestLog::open(&directory.path().join("requests.jsonl")).unwrap();
        directory
    }

    async fn socket_request(
        &self,
        client: Protocol,
        model: &str,
        stream: bool,
    ) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let (path, mut body) = client_request(client, model, stream);
        // This must not appear in persisted metadata, even on cancellation.
        if client == Protocol::Responses {
            body["input"] = json!("private cancellation prompt");
        } else if client == Protocol::Gemini {
            body["contents"][0]["parts"][0]["text"] = json!("private cancellation prompt");
        } else {
            body["messages"][0]["content"] = json!("private cancellation prompt");
        }
        let body = body.to_string();
        let mut socket =
            tokio::net::TcpStream::connect(self.gateway.url.trim_start_matches("http://"))
                .await
                .unwrap();
        socket.write_all(format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer secret-local-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        socket
    }

    async fn wait_for_attempts(&self, expected: &[usize]) {
        tokio::time::timeout(LIMIT, async {
            loop {
                let attempts = self.attempts.lock().unwrap().clone();
                assert!(
                    expected.starts_with(&attempts),
                    "unexpected attempt: {attempts:?}"
                );
                if attempts == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("upstream never received expected requests");
    }

    /// Drop the actual HTTP/1.1 socket while the gateway has sent no headers.
    /// Neither server nor handler is aborted; all timeout policies are disabled.
    async fn disconnect_before_headers(
        &self,
        mut socket: tokio::net::TcpStream,
        history: &std::path::Path,
    ) -> Value {
        use tokio::io::AsyncReadExt;
        assert!(
            tokio::time::timeout(Duration::from_millis(40), socket.read_u8())
                .await
                .is_err(),
            "gateway sent a response or closed before downstream cancellation"
        );
        assert!(
            self.log
                .lock()
                .unwrap()
                .snapshot()
                .recent_requests
                .is_empty()
        );
        drop(socket);
        let record = self.record().await;
        self.assert_released().await;
        assert_eq!(record["status"], "client_disconnected");
        let records = self.log.lock().unwrap().snapshot().recent_requests;
        assert_eq!(records.len(), 1);
        let persisted = std::fs::read_to_string(history.join("requests.jsonl")).unwrap();
        assert_eq!(persisted.lines().count(), 1);
        assert_eq!(serde_json::from_str::<Value>(&persisted).unwrap(), record);
        assert!(
            !persisted.contains("secret-")
                && !persisted.contains("private cancellation prompt")
                && !persisted.contains("abandoned-model")
        );
        let restored = RequestLog::open(&history.join("requests.jsonl"))
            .unwrap()
            .snapshot()
            .recent_requests;
        assert_eq!(restored.len(), 1);
        assert_eq!(serde_json::to_value(&restored[0]).unwrap(), record);
        record
    }
}

fn disable_stream_deadlines(config: &mut Config) {
    config.stream_first_content_timeout_ms = 0;
    config.stream_idle_timeout_ms = 0;
}

fn assert_pending_record(record: &Value, client: Protocol, stream: bool) {
    assert_eq!(
        record["client_protocol"],
        serde_json::to_value(client).unwrap()
    );
    assert_eq!(record["stream"], stream);
    assert_eq!(record["requested_model"], "mock/up-model");
    assert_eq!(record["provider"], "mock");
    assert_eq!(record["upstream_model"], "up-model");
    assert_eq!(record["key_slot"], 0);
}

#[tokio::test]
async fn socket_disconnect_before_headers_cancels_pending_upstream() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            for stream in [false, true] {
                let f = Fixture::with_transport(
                    upstream,
                    vec![prefix(upstream)],
                    Ending::Hold,
                    Transport {
                        header_delay: Duration::from_secs(60),
                        ..Transport::default()
                    },
                    disable_stream_deadlines,
                )
                .await;
                let history = f.persistent_history();
                let socket = f.socket_request(client, "mock/up-model", stream).await;
                f.wait_for_attempts(&[0]).await;
                let record = f.disconnect_before_headers(socket, history.path()).await;
                assert_pending_record(&record, client, stream);
                assert!(record["usage"]["input"].is_null() && record["usage"]["output"].is_null());
                assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
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

#[tokio::test]
async fn socket_disconnect_during_opening_prefetch_keeps_partial_usage() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![opening(upstream).into()],
                Ending::Hold,
                disable_stream_deadlines,
            )
            .await;
            let history = f.persistent_history();
            let socket = f.socket_request(client, "mock/up-model", true).await;
            f.wait_for_attempts(&[0]).await;
            let record = f.disconnect_before_headers(socket, history.path()).await;
            assert_pending_record(&record, client, true);
            if upstream == Protocol::Responses {
                assert!(record["usage"]["input"].is_null() && record["usage"]["output"].is_null());
            } else {
                assert_eq!(record["usage"]["input"], 99);
                assert_eq!(record["usage"]["output"], 0);
            }
            assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        }
    }
}

#[tokio::test]
async fn socket_disconnect_during_nonstream_body_cancels_upstream() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            // HTTP 200, but an incomplete JSON body that cannot be decoded yet.
            let f = Fixture::with_config(
                upstream,
                vec!["{\"private\":".into()],
                Ending::Hold,
                disable_stream_deadlines,
            )
            .await;
            let history = f.persistent_history();
            let socket = f.socket_request(client, "mock/up-model", false).await;
            f.wait_for_attempts(&[0]).await;
            let record = f.disconnect_before_headers(socket, history.path()).await;
            assert_pending_record(&record, client, false);
            assert!(record["usage"]["input"].is_null() && record["usage"]["output"].is_null());
            assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        }
    }
}

#[tokio::test]
async fn socket_disconnect_during_error_body_does_not_start_fallback() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            for stream in [false, true] {
                let f = Fixture::with_transport(
                    upstream,
                    vec![
                        "{\"error\":".into(),
                        prefix(upstream) + completion(upstream),
                    ],
                    Ending::Hold,
                    Transport {
                        status: axum::http::StatusCode::BAD_GATEWAY,
                        ..Transport::default()
                    },
                    |config| {
                        disable_stream_deadlines(config);
                        let mut backup = config.provider[0].clone();
                        backup.id = "spare".into();
                        backup.api_keys = vec!["secret-upstream-key-1".into()];
                        config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                        config.provider[0].fallback = vec!["spare/up-model".into()];
                        config.provider.push(backup);
                    },
                )
                .await;
                let history = f.persistent_history();
                let socket = f.socket_request(client, "mock/up-model", stream).await;
                f.wait_for_attempts(&[0]).await;
                let record = f.disconnect_before_headers(socket, history.path()).await;
                assert_pending_record(&record, client, stream);
                assert!(record["usage"]["input"].is_null() && record["usage"]["output"].is_null());
                assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
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

#[tokio::test]
async fn socket_disconnect_after_fallback_keeps_only_last_attempt_and_persists() {
    use magpie_gateway::{GroupConfig, RoutingPolicy};
    for grouped in [false, true] {
        for spare in PROTOCOLS {
            for client in PROTOCOLS {
                for stream in [false, true] {
                    let f = Fixture::with_transport(
                        Protocol::Chat,
                        vec![
                            opening(Protocol::Chat).to_owned() + limited(Protocol::Chat),
                            prefix(spare),
                            prefix(spare) + completion(spare),
                        ],
                        if stream { Ending::Hold } else { Ending::Eof },
                        Transport {
                            header_delay: Duration::from_secs(60),
                            header_delay_slot: 1,
                            status: if stream {
                                axum::http::StatusCode::OK
                            } else {
                                axum::http::StatusCode::TOO_MANY_REQUESTS
                            },
                            ..Transport::default()
                        },
                        |config| {
                            disable_stream_deadlines(config);
                            let mut backup = config.provider[0].clone();
                            backup.id = "spare".into();
                            backup.protocol = spare;
                            backup.api_keys = vec![
                                "secret-upstream-key-1".into(),
                                "secret-upstream-key-2".into(),
                            ];
                            backup.models = vec!["backup-model".into()];
                            config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                            if grouped {
                                config.group.push(GroupConfig {
                                    id: "daily".into(),
                                    name: "Daily".into(),
                                    routes: vec![
                                        "mock/up-model".into(),
                                        "spare/backup-model".into(),
                                    ],
                                    rules: Vec::new(),
                                    classifier: String::new(),
                                    effort: String::new(),
                                    pick: String::new(),
                                    weights: Vec::new(),
                                    routing: RoutingPolicy::Order,
                                    first_token_timeout_ms: 0,
                                    affinity: Default::default(),
                                });
                            } else {
                                config.provider[0].fallback = vec!["spare/backup-model".into()];
                            }
                            config.provider.push(backup);
                        },
                    )
                    .await;
                    let history = f.persistent_history();
                    let requested = if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    };
                    let socket = f.socket_request(client, requested, stream).await;
                    // For nonstreaming, let the first non-2xx body reach EOF so
                    // routing can select the spare. Streaming needs no EOF.
                    f.wait_for_attempts(&[0, 1]).await;
                    let record = f.disconnect_before_headers(socket, history.path()).await;
                    assert_eq!(record["requested_model"], requested);
                    assert_eq!(record["provider"], "spare");
                    assert_eq!(record["upstream_model"], "backup-model");
                    assert_eq!(record["key_slot"], 0);
                    assert_eq!(record["stream"], stream);
                    assert!(
                        record["usage"]["input"].is_null() && record["usage"]["output"].is_null(),
                        "abandoned usage leaked: {record}"
                    );
                    assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
                    assert_eq!(
                        *f.sent_models.lock().unwrap(),
                        vec!["up-model", "backup-model"]
                    );
                }
            }
        }
    }
}

fn enable_fast_keepalive(config: &mut Config) {
    disable_stream_deadlines(config);
    config.stream_keepalive_interval_ms = 30;
    config.stream_keepalive_max_silence_ms = 2_000;
}

const COMMENT_CLIENTS: [Protocol; 3] = [Protocol::Chat, Protocol::Anthropic, Protocol::Responses];
const KEEPALIVE: &str = ": keepalive\n";

async fn first_comment(response: &mut reqwest::Response) {
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let first = tokio::time::timeout(LIMIT, response.chunk())
        .await
        .expect("no comment while upstream is pending")
        .unwrap()
        .unwrap();
    assert_eq!(first, KEEPALIVE);
}

async fn read_through(response: &mut reqwest::Response, needle: &str) -> String {
    tokio::time::timeout(LIMIT, async {
        let mut received = String::new();
        while !received.contains(needle) {
            let chunk = response.chunk().await.unwrap().expect("stream ended early");
            received.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        received
    })
    .await
    .expect("stream did not produce expected output")
}

fn assert_one_stream_error(body: &str, client: Protocol, message: &str) {
    assert_no_success(body);
    let mut parser = magpie_core::stream::SseParser::new();
    let mut remaining = body.as_bytes();
    let mut errors = 0;
    while let Some(data) = parser.feed(&mut remaining).unwrap() {
        let value: Value = serde_json::from_str(&data).unwrap();
        let error = match client {
            Protocol::Chat | Protocol::Gemini => value.get("error"),
            Protocol::Anthropic => (value["type"] == "error").then(|| &value["error"]),
            Protocol::Responses => {
                (value["type"] == "response.failed").then(|| &value["response"]["error"])
            }
        };
        if let Some(error) = error {
            assert!(
                error["message"].as_str().unwrap().contains(message),
                "{body}"
            );
            errors += 1;
        }
    }
    assert_eq!(errors, 1, "exactly one terminal stream error: {body}");
}

#[tokio::test]
async fn keepalive_precedes_delayed_upstream_headers_without_opening_metadata() {
    for upstream in PROTOCOLS {
        for client in COMMENT_CLIENTS {
            let f = Fixture::with_transport(
                upstream,
                vec![prefix(upstream) + completion(upstream)],
                Ending::Hold,
                Transport {
                    header_delay: Duration::from_millis(180),
                    ..Transport::default()
                },
                enable_fast_keepalive,
            )
            .await;
            let mut response = f.request(client).await;
            first_comment(&mut response).await;
            assert!(f.log.lock().unwrap().snapshot().recent_requests.is_empty());
            let body = response.text().await.unwrap();
            assert!(body.contains("partial"));
            assert_completed(&body, client);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn keepalive_holds_opening_metadata_until_content_without_changing_usage() {
    for upstream in PROTOCOLS {
        for client in COMMENT_CLIENTS {
            let f = Fixture::with_config(
                upstream,
                vec![opening(upstream).into()],
                Ending::Paced(vec![(
                    Duration::from_millis(180),
                    prefix(upstream) + completion(upstream),
                )]),
                enable_fast_keepalive,
            )
            .await;
            let mut response = f.request(client).await;
            first_comment(&mut response).await;
            assert!(f.log.lock().unwrap().snapshot().recent_requests.is_empty());
            let body = response.text().await.unwrap();
            assert!(body.contains("partial"));
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
async fn keepalive_does_not_commit_candidates_before_key_provider_or_group_retry() {
    // Same-key-pool retry and every protocol pair through provider/group retry.
    for mode in 0..3 {
        for upstream in PROTOCOLS {
            for spare in PROTOCOLS {
                if mode == 0 && upstream != spare {
                    continue;
                }
                for client in COMMENT_CLIENTS {
                    let f = Fixture::with_transport(
                        upstream,
                        vec![
                            opening(upstream).to_owned() + limited(upstream),
                            prefix(spare) + completion(spare),
                        ],
                        Ending::Hold,
                        Transport {
                            header_delay: Duration::from_millis(180),
                            ..Transport::default()
                        },
                        |config| {
                            enable_fast_keepalive(config);
                            if mode != 0 {
                                configure_timeout_fallback(config, mode == 2);
                                config.provider[0].api_keys.truncate(1);
                                config.provider[1].protocol = spare;
                            }
                        },
                    )
                    .await;
                    let model = if mode == 2 {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    };
                    let mut response = f.request_model(client, model).await;
                    first_comment(&mut response).await;
                    let body = response.text().await.unwrap();
                    assert!(
                        !body.contains("abandoned-model") && !body.contains("fixture rate limit"),
                        "{body}"
                    );
                    assert_completed(&body, client);
                    assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
                    let record = f.record().await;
                    assert_eq!(record["status"], "ok");
                    assert_eq!(record["provider"], if mode == 0 { "mock" } else { "spare" });
                    assert_eq!(
                        record["upstream_model"],
                        if mode == 0 {
                            "up-model"
                        } else {
                            "backup-model"
                        }
                    );
                    assert_eq!(record["key_slot"], if mode == 0 { 1 } else { 0 });
                    assert_eq!(record["usage"]["input"], 7);
                    assert_eq!(record["usage"]["output"], 3);
                    f.assert_released().await;
                }
            }
        }
    }
}

#[tokio::test]
async fn keepalive_turns_late_http_failures_into_one_protocol_error() {
    for client in COMMENT_CLIENTS {
        for status in [400, 429, 503] {
            let f = Fixture::with_transport(
                Protocol::Chat,
                vec![json!({"error":{"message":"late fixture failure"}}).to_string()],
                Ending::Eof,
                Transport {
                    header_delay: Duration::from_millis(180),
                    status: axum::http::StatusCode::from_u16(status).unwrap(),
                    ..Transport::default()
                },
                enable_fast_keepalive,
            )
            .await;
            let mut response = f.request(client).await;
            first_comment(&mut response).await;
            let body = response.text().await.unwrap();
            assert_one_stream_error(&body, client, "late fixture failure");
            assert_eq!(f.record().await["status"], "upstream_error");
            assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn keepalive_preserves_fast_http_errors_and_known_error_body_waits() {
    for client in PROTOCOLS {
        let f = Fixture::with_transport(
            Protocol::Chat,
            vec![json!({"error":{"message":"fast fixture failure"}}).to_string()],
            Ending::Eof,
            Transport {
                status: axum::http::StatusCode::BAD_REQUEST,
                ..Transport::default()
            },
            |config| {
                enable_fast_keepalive(config);
                config.stream_keepalive_interval_ms = 250;
            },
        )
        .await;
        let response = f.request_model_raw(client, "mock/up-model").await;
        assert_eq!(response.status(), 400);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let body = response.text().await.unwrap();
        assert!(body.contains("fast fixture failure") && !body.contains(KEEPALIVE));
        assert_eq!(f.record().await["status"], "upstream_error");
        f.assert_released().await;

        // Known non-2xx bodies must not become 200 merely because their EOF is late.
        let f = Fixture::with_transport(
            Protocol::Chat,
            vec![json!({"error":{"message":"pending failure"}}).to_string()],
            Ending::Hold,
            Transport {
                status: axum::http::StatusCode::BAD_REQUEST,
                ..Transport::default()
            },
            enable_fast_keepalive,
        )
        .await;
        let history = f.persistent_history();
        let socket = f.socket_request(client, "mock/up-model", true).await;
        f.wait_for_attempts(&[0]).await;
        // This helper asserts no headers/comments arrive before dropping TCP.
        let record = f.disconnect_before_headers(socket, history.path()).await;
        assert_eq!(record["status"], "client_disconnected");
        assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
    }
}

#[tokio::test]
async fn keepalive_never_extends_first_content_or_read_idle_deadlines() {
    for upstream in PROTOCOLS {
        for client in COMMENT_CLIENTS {
            for mode in 0..4 {
                let f = Fixture::with_transport(
                    upstream,
                    if mode == 1 {
                        vec![prefix(upstream), prefix(upstream) + completion(upstream)]
                    } else {
                        vec![opening(upstream).into()]
                    },
                    if mode == 2 {
                        Ending::Heartbeats
                    } else {
                        Ending::Hold
                    },
                    Transport {
                        header_delay: if mode == 3 {
                            Duration::from_secs(60)
                        } else {
                            Duration::ZERO
                        },
                        ..Transport::default()
                    },
                    |config| {
                        enable_fast_keepalive(config);
                        config.stream_first_content_timeout_ms = if mode == 1 { 0 } else { 180 };
                        config.stream_idle_timeout_ms = if mode == 1 { 180 } else { 0 };
                        if mode == 1 {
                            configure_timeout_fallback(config, false);
                        }
                    },
                )
                .await;
                let start = std::time::Instant::now();
                let response = f.request(client).await;
                assert_eq!(response.status(), 200);
                let body = tokio::time::timeout(LIMIT, response.text())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(body.contains(KEEPALIVE), "no downstream liveness: {body}");
                assert_one_stream_error(
                    &body,
                    client,
                    if mode == 1 {
                        "upstream stream idle timeout"
                    } else {
                        "upstream first-content timeout"
                    },
                );
                // A mode-1 cut mid-reply is asked to go on where the
                // upstream takes a prefill: a pause a try, not the
                // deadline restarted.
                let continued = mode == 1 && mid_reply_continues(upstream);
                assert!(
                    start.elapsed()
                        < Duration::from_secs(2) + Duration::from_secs(3 * continued as u64),
                    "comments restarted deadline"
                );
                let record = f.record().await;
                assert_eq!(
                    record["status"],
                    if mode == 3 {
                        "upstream_error"
                    } else {
                        "upstream_stream_error"
                    }
                );
                assert_eq!(
                    *f.attempts.lock().unwrap(),
                    if continued {
                        cut_attempts(upstream)
                    } else {
                        vec![0]
                    }
                );
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn keepalive_after_content_stops_at_upstream_silence_cap_without_finishing_request() {
    for upstream in PROTOCOLS {
        for client in COMMENT_CLIENTS {
            let f =
                Fixture::with_config(upstream, vec![prefix(upstream)], Ending::Hold, |config| {
                    enable_fast_keepalive(config);
                    config.stream_keepalive_max_silence_ms = 160;
                })
                .await;
            let mut response = f.request(client).await;
            let initial = read_through(&mut response, "partial").await;
            assert!(!initial.contains(KEEPALIVE));
            let mut comments = 0;
            loop {
                match tokio::time::timeout(Duration::from_millis(250), response.chunk()).await {
                    Err(_) => break,
                    Ok(Ok(Some(chunk))) => {
                        assert_eq!(chunk, KEEPALIVE);
                        comments += 1;
                    }
                    other => panic!("silence cap incorrectly ended stream: {other:?}"),
                }
                assert!(comments < 10, "comments outlived upstream silence cap");
            }
            assert!(comments > 0, "no post-content keepalive");
            assert!(f.log.lock().unwrap().snapshot().recent_requests.is_empty());
            drop(response);
            let record = f.record().await;
            assert_eq!(record["status"], "client_disconnected");
            assert_partial_usage(&record, upstream);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn keepalive_observes_ignored_upstream_heartbeats_not_only_decoded_events() {
    for upstream in PROTOCOLS {
        for client in COMMENT_CLIENTS {
            let f = Fixture::with_config(
                upstream,
                vec![prefix(upstream)],
                Ending::Heartbeats,
                |config| {
                    enable_fast_keepalive(config);
                    config.stream_keepalive_max_silence_ms = 90;
                },
            )
            .await;
            let mut response = f.request(client).await;
            read_through(&mut response, "partial").await;
            let start = std::time::Instant::now();
            for _ in 0..7 {
                let chunk = tokio::time::timeout(LIMIT, response.chunk())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(chunk, KEEPALIVE);
            }
            assert!(start.elapsed() >= Duration::from_millis(150));
            drop(response);
            assert_eq!(f.record().await["status"], "client_disconnected");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn keepalive_is_disabled_for_gemini_nonstream_and_either_zero_setting() {
    for client in PROTOCOLS {
        for mode in 0..4 {
            if mode == 0 && client != Protocol::Gemini {
                continue;
            }
            let f = Fixture::with_transport(
                Protocol::Chat,
                vec![prefix(Protocol::Chat)],
                Ending::Hold,
                Transport {
                    header_delay: Duration::from_secs(60),
                    ..Transport::default()
                },
                |config| {
                    enable_fast_keepalive(config);
                    if mode == 2 {
                        config.stream_keepalive_interval_ms = 0;
                    }
                    if mode == 3 {
                        config.stream_keepalive_max_silence_ms = 0;
                    }
                },
            )
            .await;
            let history = f.persistent_history();
            let socket = f.socket_request(client, "mock/up-model", mode != 1).await;
            f.wait_for_attempts(&[0]).await;
            let record = f.disconnect_before_headers(socket, history.path()).await;
            assert_pending_record(&record, client, mode != 1);
        }
    }
    // Gemini remains comment-free after content too, even with ignored upstream traffic.
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![prefix(upstream)],
            Ending::Heartbeats,
            enable_fast_keepalive,
        )
        .await;
        let mut response = f.request(Protocol::Gemini).await;
        read_through(&mut response, "partial").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), response.chunk())
                .await
                .is_err()
        );
        drop(response);
        assert_eq!(f.record().await["status"], "client_disconnected");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn keepalive_socket_disconnect_cancels_owned_work_and_persists_last_attempt() {
    use tokio::io::AsyncReadExt;
    for upstream in PROTOCOLS {
        for client in COMMENT_CLIENTS {
            // Pending headers, opening metadata, ordinary/group fallback headers.
            for mode in 0..4 {
                let fallback = mode >= 2;
                let f = Fixture::with_transport(
                    upstream,
                    if fallback {
                        vec![
                            opening(upstream).to_owned() + error(upstream),
                            prefix(upstream),
                        ]
                    } else {
                        vec![opening(upstream).into()]
                    },
                    Ending::Hold,
                    Transport {
                        header_delay: if mode == 1 {
                            Duration::ZERO
                        } else {
                            Duration::from_secs(60)
                        },
                        header_delay_slot: usize::from(fallback),
                        ..Transport::default()
                    },
                    |config| {
                        enable_fast_keepalive(config);
                        if fallback {
                            configure_timeout_fallback(config, mode == 3);
                            config.provider[0].api_keys.truncate(1);
                        }
                    },
                )
                .await;
                let history = f.persistent_history();
                let model = if mode == 3 {
                    "group/daily"
                } else {
                    "mock/up-model"
                };
                let mut socket = f.socket_request(client, model, true).await;
                f.wait_for_attempts(if fallback { &[0, 1] } else { &[0] })
                    .await;
                let received = tokio::time::timeout(LIMIT, async {
                    let mut received = Vec::new();
                    while !String::from_utf8_lossy(&received).contains(KEEPALIVE) {
                        received.push(socket.read_u8().await.unwrap());
                    }
                    String::from_utf8(received).unwrap()
                })
                .await
                .unwrap();
                assert!(received.starts_with("HTTP/1.1 200 OK\r\n"), "{received}");
                assert!(!received.contains("abandoned-model") && !received.contains("data:"));
                assert!(f.log.lock().unwrap().snapshot().recent_requests.is_empty());
                drop(socket);
                let record = f.record().await;
                assert_eq!(record["status"], "client_disconnected");
                assert_eq!(record["requested_model"], model);
                assert_eq!(record["provider"], if fallback { "spare" } else { "mock" });
                assert_eq!(
                    record["upstream_model"],
                    if fallback { "backup-model" } else { "up-model" }
                );
                assert_eq!(record["key_slot"], 0);
                if mode == 1 && upstream != Protocol::Responses {
                    assert_eq!(record["usage"]["input"], 99);
                    assert_eq!(record["usage"]["output"], 0);
                } else {
                    assert!(
                        record["usage"]["input"].is_null() && record["usage"]["output"].is_null()
                    );
                }
                f.assert_released().await;
                let persisted =
                    std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
                assert_eq!(persisted.lines().count(), 1);
                assert!(
                    !persisted.contains("secret-")
                        && !persisted.contains("private cancellation prompt")
                        && !persisted.contains("abandoned-model")
                );
                assert_eq!(serde_json::from_str::<Value>(&persisted).unwrap(), record);
                let restored = RequestLog::open(&history.path().join("requests.jsonl"))
                    .unwrap()
                    .snapshot()
                    .recent_requests;
                assert_eq!(restored.len(), 1);
                assert_eq!(serde_json::to_value(&restored[0]).unwrap(), record);
                assert_eq!(
                    *f.attempts.lock().unwrap(),
                    if fallback { vec![0, 1] } else { vec![0] }
                );
            }
        }
    }
}

#[tokio::test]
async fn keepalive_preserves_http_error_and_header_timeout_fallback_after_comments() {
    for timeout in [false, true] {
        for grouped in [false, true] {
            for spare in PROTOCOLS {
                for client in COMMENT_CLIENTS {
                    let f = Fixture::with_transport(
                        Protocol::Chat,
                        vec![
                            json!({"error":{"message":"abandoned HTTP failure"}}).to_string(),
                            prefix(spare) + completion(spare),
                        ],
                        Ending::Eof,
                        Transport {
                            header_delay: if timeout {
                                Duration::from_secs(60)
                            } else {
                                Duration::from_millis(180)
                            },
                            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            ..Transport::default()
                        },
                        |config| {
                            enable_fast_keepalive(config);
                            configure_timeout_fallback(config, grouped);
                            config.provider[1].protocol = spare;
                            if timeout {
                                config.stream_first_content_timeout_ms = 180;
                            }
                        },
                    )
                    .await;
                    let model = if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    };
                    let mut response = f.request_model(client, model).await;
                    first_comment(&mut response).await;
                    let body = response.text().await.unwrap();
                    assert!(!body.contains("abandoned HTTP failure"), "{body}");
                    assert_completed(&body, client);
                    assert_eq!(*f.attempts.lock().unwrap(), vec![0, 1]);
                    let record = f.record().await;
                    assert_eq!(record["status"], "ok");
                    assert_eq!(record["provider"], "spare");
                    assert_eq!(record["upstream_model"], "backup-model");
                    assert_eq!(record["key_slot"], 0);
                    assert_eq!(record["usage"]["input"], 7);
                    assert_eq!(record["usage"]["output"], 3);
                    f.assert_released().await;
                }
            }
        }
    }
}

#[tokio::test]
async fn keepalive_uses_fast_gap_for_translated_activity_and_resumes_after_silence_cap() {
    for client in COMMENT_CLIENTS {
        let f = Fixture::with_config(
            Protocol::Responses,
            vec![prefix(Protocol::Responses)],
            Ending::Heartbeats,
            |config| {
                enable_fast_keepalive(config);
                config.stream_keepalive_interval_ms = 1_500;
                config.stream_keepalive_max_silence_ms = 2_000;
            },
        )
        .await;
        let mut response = f.request(client).await;
        read_through(&mut response, "partial").await;
        let start = std::time::Instant::now();
        let chunk = tokio::time::timeout(Duration::from_millis(1_400), response.chunk())
            .await
            .expect("ignored transport did not trigger the one-second gap")
            .unwrap()
            .unwrap();
        assert_eq!(chunk, KEEPALIVE);
        assert!(start.elapsed() >= Duration::from_millis(800));
        drop(response);
        assert_eq!(f.record().await["status"], "client_disconnected");
        f.assert_released().await;

        let f = Fixture::with_config(
            Protocol::Responses,
            vec![prefix(Protocol::Responses)],
            Ending::Paced(vec![
                (
                    Duration::from_millis(400),
                    ": upstream transport resumed\n\n".into(),
                ),
                (
                    Duration::from_millis(100),
                    completion(Protocol::Responses).into(),
                ),
            ]),
            |config| {
                enable_fast_keepalive(config);
                config.stream_keepalive_max_silence_ms = 120;
            },
        )
        .await;
        let mut response = f.request(client).await;
        let mut body = read_through(&mut response, "partial").await;
        let mut comments = 0;
        // Transport falls silent, the comments stop, but the request remains live.
        loop {
            match tokio::time::timeout(Duration::from_millis(150), response.chunk()).await {
                Err(_) => break,
                Ok(Ok(Some(chunk))) => {
                    assert_eq!(chunk, KEEPALIVE);
                    body.push_str(KEEPALIVE);
                    comments += 1;
                }
                other => panic!("stream ended during recoverable silence: {other:?}"),
            }
            assert!(comments < 8);
        }
        assert!(comments > 0);
        // A transport comment, not a decoded event, reopens the liveness window.
        let chunk = tokio::time::timeout(LIMIT, response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(chunk, KEEPALIVE);
        body.push_str(KEEPALIVE);
        body.push_str(&response.text().await.unwrap());
        assert_completed(&body, client);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn keepalive_stops_during_late_known_http_error_body_without_background_work() {
    for client in COMMENT_CLIENTS {
        let f = Fixture::with_transport(
            Protocol::Chat,
            vec![json!({"error":{"message":"held late HTTP failure"}}).to_string()],
            Ending::Hold,
            Transport {
                header_delay: Duration::from_millis(180),
                status: axum::http::StatusCode::BAD_REQUEST,
                ..Transport::default()
            },
            enable_fast_keepalive,
        )
        .await;
        let mut response = f.request(client).await;
        first_comment(&mut response).await;
        let mut comments = 1;
        loop {
            match tokio::time::timeout(Duration::from_millis(120), response.chunk()).await {
                Err(_) => break,
                Ok(Ok(Some(chunk))) => {
                    assert_eq!(chunk, KEEPALIVE);
                    comments += 1;
                }
                other => panic!("incomplete error body ended unexpectedly: {other:?}"),
            }
            assert!(comments < 10, "keepalive ignored known HTTP failure");
        }
        assert!(f.log.lock().unwrap().snapshot().recent_requests.is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), response.chunk())
                .await
                .is_err()
        );
        drop(response);
        assert_eq!(f.record().await["status"], "client_disconnected");
        assert_eq!(*f.attempts.lock().unwrap(), vec![0]);
        f.assert_released().await;
    }
}

// Comments keep proxies alive, but Responses agents may require actual events.
fn enable_fast_responses_hold(config: &mut Config) {
    enable_fast_keepalive(config);
    config.stream_responses_hold_timeout_ms = 120;
}

async fn responses_opening(response: &mut reqwest::Response) -> String {
    tokio::time::timeout(Duration::from_millis(450), async {
        let mut received = String::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .unwrap()
                .expect("opening event missing");
            received.push_str(std::str::from_utf8(&chunk).unwrap());
            if received.contains("event: response.created") {
                assert!(!received.contains("partial"));
                assert!(!received.contains("response.completed"));
                return received;
            }
        }
    })
    .await
    .expect("Responses opening metadata stayed held beyond request-wide budget")
}

#[tokio::test]
async fn responses_hold_releases_real_opening_event_before_quiet_upstream_content() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![opening(upstream).into()],
            Ending::Paced(vec![(
                Duration::from_millis(650),
                prefix(upstream) + completion(upstream),
            )]),
            enable_fast_responses_hold,
        )
        .await;
        let mut response = f.request(Protocol::Responses).await;
        let opening = responses_opening(&mut response).await;
        assert!(opening.contains("abandoned-model"));
        assert!(f.log.lock().unwrap().snapshot().recent_requests.is_empty());
        let body = opening + &response.text().await.unwrap();
        assert_eq!(body.matches("event: response.created").count(), 1);
        assert!(body.contains("partial"));
        assert_completed(&body, Protocol::Responses);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn responses_hold_budget_spans_keys_providers_and_groups_without_leaking_abandoned_opening() {
    for mode in 0..3 {
        for upstream in PROTOCOLS {
            for spare in PROTOCOLS {
                if mode == 0 && upstream != spare {
                    continue;
                }
                let f = Fixture::with_config(
                    upstream,
                    vec![
                        opening(upstream).into(),
                        opening(spare)
                            .replace("abandoned-model", "selected-model")
                            .replace("99", "7"),
                    ],
                    Ending::PerSlot(vec![
                        Ending::Paced(vec![(Duration::from_millis(240), limited(upstream).into())]),
                        Ending::Paced(vec![(Duration::from_millis(240), limited(spare).into())]),
                    ]),
                    |config| {
                        enable_fast_responses_hold(config);
                        config.stream_responses_hold_timeout_ms = 360;
                        if mode != 0 {
                            configure_timeout_fallback(config, mode == 2);
                            config.provider[0].api_keys.truncate(1);
                            config.provider[1].protocol = spare;
                        }
                    },
                )
                .await;
                let model = if mode == 2 {
                    "group/daily"
                } else {
                    "mock/up-model"
                };
                let mut response = f.request_model(Protocol::Responses, model).await;
                let opening = responses_opening(&mut response).await;
                assert!(opening.contains("selected-model") && !opening.contains("abandoned-model"));
                assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
                // After the shared budget commits the second candidate, its
                // later error cannot retry or fabricate successful completion.
                let body = opening + &response.text().await.unwrap();
                assert_one_stream_error(&body, Protocol::Responses, "fixture rate limit");
                assert_eq!(body.matches("event: response.created").count(), 1);
                let record = f.record().await;
                assert_eq!(record["status"], "upstream_stream_error");
                assert_eq!(record["provider"], if mode == 0 { "mock" } else { "spare" });
                assert_eq!(record["key_slot"], if mode == 0 { 1 } else { 0 });
                if spare != Protocol::Responses {
                    assert_eq!(record["usage"]["input"], 7);
                }
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn responses_hold_commit_prevents_late_error_retry_but_preserves_partial_usage() {
    for upstream in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_config(
                upstream,
                vec![
                    opening(upstream).into(),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Paced(vec![(Duration::from_millis(350), limited(upstream).into())]),
                |config| {
                    enable_fast_responses_hold(config);
                    configure_timeout_fallback(config, grouped);
                },
            )
            .await;
            let model = if grouped {
                "group/daily"
            } else {
                "mock/up-model"
            };
            let mut response = f.request_model(Protocol::Responses, model).await;
            let opening = responses_opening(&mut response).await;
            let body = opening + &response.text().await.unwrap();
            assert_one_stream_error(&body, Protocol::Responses, "fixture rate limit");
            assert_eq!(body.matches("event: response.created").count(), 1);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            if upstream != Protocol::Responses {
                assert_eq!(record["usage"]["input"], 99);
            }
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn responses_hold_release_does_not_satisfy_first_content_or_idle_deadline() {
    for upstream in PROTOCOLS {
        for idle in [false, true] {
            let f = Fixture::with_config(
                upstream,
                vec![
                    opening(upstream).into(),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
                |config| {
                    enable_fast_responses_hold(config);
                    config.stream_first_content_timeout_ms = if idle { 0 } else { 300 };
                    config.stream_idle_timeout_ms = if idle { 300 } else { 0 };
                    configure_timeout_fallback(config, false);
                },
            )
            .await;
            let mut response = f.request(Protocol::Responses).await;
            let opening = responses_opening(&mut response).await;
            let body = opening + &response.text().await.unwrap();
            assert_one_stream_error(
                &body,
                Protocol::Responses,
                if idle {
                    "upstream stream idle timeout"
                } else {
                    "upstream first-content timeout"
                },
            );
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn responses_hold_requires_real_opening_metadata_not_headers_comments_or_usage() {
    for upstream in PROTOCOLS {
        for headers in [false, true] {
            let f = Fixture::with_transport(
                upstream,
                vec![if headers {
                    opening(upstream).into()
                } else {
                    String::new()
                }],
                Ending::Paced(vec![(
                    Duration::from_millis(550),
                    prefix(upstream) + completion(upstream),
                )]),
                Transport {
                    header_delay: if headers {
                        Duration::from_millis(550)
                    } else {
                        Duration::ZERO
                    },
                    ..Transport::default()
                },
                enable_fast_responses_hold,
            )
            .await;
            let mut response = f.request(Protocol::Responses).await;
            first_comment(&mut response).await;
            // After the hold budget, comments alone must not invent a start.
            let no_event = tokio::time::timeout(Duration::from_millis(250), async {
                while let Some(chunk) = response.chunk().await.unwrap() {
                    assert!(
                        !chunk.windows(5).any(|w| w == b"data:"),
                        "fake event: {chunk:?}"
                    );
                }
            })
            .await;
            assert!(no_event.is_err());
            drop(response);
            assert_eq!(f.record().await["status"], "client_disconnected");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn responses_hold_ignores_upstream_heartbeats_and_runs_without_comment_keepalive() {
    for upstream in PROTOCOLS {
        for comments in [false, true] {
            let f = Fixture::with_config(
                upstream,
                vec![opening(upstream).into()],
                Ending::Heartbeats,
                |config| {
                    enable_fast_responses_hold(config);
                    if !comments {
                        config.stream_keepalive_interval_ms = 0;
                    }
                },
            )
            .await;
            let mut response = f.request(Protocol::Responses).await;
            let body = responses_opening(&mut response).await;
            assert_eq!(body.contains(KEEPALIVE), comments);
            drop(response);
            assert_eq!(f.record().await["status"], "client_disconnected");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn responses_hold_zero_and_nonresponses_clients_keep_existing_hold_policy() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![opening(upstream).into()],
                Ending::Hold,
                |config| {
                    enable_fast_responses_hold(config);
                    if client == Protocol::Responses {
                        config.stream_responses_hold_timeout_ms = 0;
                    }
                },
            )
            .await;
            if client == Protocol::Gemini {
                assert!(
                    tokio::time::timeout(Duration::from_millis(300), f.request(client))
                        .await
                        .is_err()
                );
            } else {
                let mut response = f.request(client).await;
                first_comment(&mut response).await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(250), async {
                        while let Some(chunk) = response.chunk().await.unwrap() {
                            assert!(
                                !chunk.windows(5).any(|w| w == b"data:"),
                                "unexpected opening: {chunk:?}"
                            );
                        }
                    })
                    .await
                    .is_err()
                );
                drop(response);
            }
            assert_eq!(f.record().await["status"], "client_disconnected");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn responses_hold_expired_budget_still_prefers_ready_stream_error_for_safe_retry() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_transport(
            upstream,
            vec![
                opening(upstream).to_owned() + limited(upstream),
                prefix(upstream) + completion(upstream),
            ],
            Ending::Hold,
            Transport {
                chunk_size: usize::MAX,
                header_delay: Duration::from_millis(180),
                ..Transport::default()
            },
            |config| {
                enable_fast_responses_hold(config);
                config.stream_responses_hold_timeout_ms = 1;
            },
        )
        .await;
        let body = f.request(Protocol::Responses).await.text().await.unwrap();
        assert!(
            !body.contains("abandoned-model") && !body.contains("fixture rate limit"),
            "{body}"
        );
        assert_completed(&body, Protocol::Responses);
        assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn responses_hold_committed_socket_disconnect_persists_one_safe_record() {
    use tokio::io::AsyncReadExt;
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![opening(upstream).into()],
            Ending::Hold,
            enable_fast_responses_hold,
        )
        .await;
        let history = f.persistent_history();
        let mut socket = f
            .socket_request(Protocol::Responses, "mock/up-model", true)
            .await;
        let mut body = String::new();
        tokio::time::timeout(LIMIT, async {
            let mut buffer = [0u8; 2048];
            while !body.contains("event: response.created") {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                body.push_str(std::str::from_utf8(&buffer[..n]).unwrap());
            }
        })
        .await
        .unwrap();
        drop(socket);
        let record = f.record().await;
        assert_eq!(record["status"], "client_disconnected");
        f.assert_released().await;
        let text = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
        for private in [
            "secret-upstream-key",
            "secret-local-token",
            "private cancellation prompt",
        ] {
            assert!(!text.contains(private));
        }
        let reloaded = RequestLog::open(&history.path().join("requests.jsonl"))
            .unwrap()
            .snapshot();
        assert_eq!(
            serde_json::to_value(&reloaded.recent_requests[0]).unwrap(),
            record
        );
    }
}

#[tokio::test]
async fn group_first_token_slow_opening_fails_over_without_leaks_or_key_cooldown() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![
                    opening(upstream).to_owned(),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::PerSlot(vec![Ending::Heartbeats, Ending::Eof]),
                |config| {
                    config.stream_first_content_timeout_ms = 0;
                    config.stream_idle_timeout_ms = 0;
                    config.stream_keepalive_interval_ms = 20;
                    configure_timeout_fallback(config, true);
                    config.group[0].first_token_timeout_ms = 120;
                },
            )
            .await;
            let body =
                tokio::time::timeout(LIMIT, f.request_model(client, "group/daily").await.text())
                    .await
                    .unwrap()
                    .unwrap();
            assert_completed(&body, client);
            assert!(!body.contains("abandoned"), "{body}");
            assert!(!body.contains("timeout"), "{body}");
            assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
            assert!(
                f.state
                    .key_rotation
                    .cooldown_snapshot(&f.state.config.current())
                    .is_empty()
            );
            let record = f.record().await;
            assert_eq!(record["provider"], "spare");
            assert_eq!(record["status"], "ok");
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            assert!(!record.to_string().contains("secret"));
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn group_first_token_headers_and_opening_share_one_absolute_budget() {
    for delayed_headers in [false, true] {
        let f = Fixture::with_transport(
            Protocol::Chat,
            vec![
                opening(Protocol::Chat).into(),
                prefix(Protocol::Chat) + completion(Protocol::Chat),
            ],
            Ending::PerSlot(vec![
                Ending::Paced(vec![(
                    Duration::from_millis(350),
                    prefix(Protocol::Chat) + completion(Protocol::Chat),
                )]),
                Ending::Eof,
            ]),
            Transport {
                header_delay: Duration::from_millis(if delayed_headers { 350 } else { 0 }),
                ..Transport::default()
            },
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = 550;
            },
        )
        .await;
        let body = tokio::time::timeout(
            LIMIT,
            f.request_model(Protocol::Chat, "group/daily").await.text(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_completed(&body, Protocol::Chat);
        assert_eq!(
            *f.attempts.lock().unwrap(),
            if delayed_headers { vec![0, 1] } else { vec![0] }
        );
        f.assert_released().await;
    }
}

#[tokio::test]
async fn group_first_token_cancels_a_route_still_waiting_for_headers() {
    let f = Fixture::with_transport(
        Protocol::Chat,
        vec![prefix(Protocol::Chat) + completion(Protocol::Chat); 2],
        Ending::Eof,
        Transport {
            header_delay: Duration::from_millis(400),
            ..Transport::default()
        },
        |config| {
            disable_stream_deadlines(config);
            configure_timeout_fallback(config, true);
            config.group[0].first_token_timeout_ms = 120;
        },
    )
    .await;
    let body = f
        .request_model(Protocol::Chat, "group/daily")
        .await
        .text()
        .await
        .unwrap();
    assert_completed(&body, Protocol::Chat);
    assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
    assert_eq!(f.record().await["provider"], "spare");
    f.assert_released().await;
}

#[tokio::test]
async fn group_first_token_final_and_single_routes_keep_waiting_for_content() {
    for single in [false, true] {
        let f = Fixture::with_config(
            Protocol::Chat,
            vec![opening(Protocol::Chat).into(); 2],
            Ending::Paced(vec![(
                Duration::from_millis(300),
                prefix(Protocol::Chat) + completion(Protocol::Chat),
            )]),
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = 100;
                if single {
                    config.group[0].routes.truncate(1);
                }
            },
        )
        .await;
        let body = tokio::time::timeout(
            LIMIT,
            f.request_model(Protocol::Chat, "group/daily").await.text(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_completed(&body, Protocol::Chat);
        assert_eq!(
            *f.attempts.lock().unwrap(),
            if single { vec![0] } else { vec![0, 1] }
        );
        f.assert_released().await;
    }
}

#[tokio::test]
async fn group_first_token_final_route_still_obeys_gateway_first_content_limit() {
    let f = Fixture::with_config(
        Protocol::Chat,
        vec![opening(Protocol::Chat).into(); 2],
        Ending::Hold,
        |config| {
            config.stream_first_content_timeout_ms = 250;
            config.stream_idle_timeout_ms = 0;
            configure_timeout_fallback(config, true);
            config.group[0].first_token_timeout_ms = 100;
        },
    )
    .await;
    let body = tokio::time::timeout(
        LIMIT,
        f.request_model(Protocol::Chat, "group/daily").await.text(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_one_stream_error(&body, Protocol::Chat, "upstream first-content timeout");
    assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
    let record = f.record().await;
    assert_eq!(record["provider"], "spare");
    assert_eq!(record["status"], "upstream_stream_error");
    f.assert_released().await;
}

#[tokio::test]
async fn group_first_token_text_reasoning_and_tools_commit_before_slow_completion() {
    let mut scripts: Vec<_> = PROTOCOLS.into_iter().map(|p| (p, prefix(p))).collect();
    scripts.extend(reasoning_and_tool_frames().into_iter().map(|(p, frame)| {
        (
            p,
            format!("{}data: {frame}\n\n", opening(p).replace("99", "7")),
        )
    }));
    for (upstream, script) in scripts {
        let f = Fixture::with_config(
            upstream,
            vec![script, prefix(upstream) + completion(upstream)],
            Ending::Paced(vec![(
                Duration::from_millis(220),
                // A Gemini reasoning-only STOP is now an empty reply. This
                // test isolates deadlines: the delayed completion answers.
                if upstream == Protocol::Gemini {
                    prefix(upstream) + completion(upstream)
                } else {
                    completion(upstream).into()
                },
            )]),
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = 100;
            },
        )
        .await;
        let body =
            tokio::time::timeout(LIMIT, f.request_model(upstream, "group/daily").await.text())
                .await
                .unwrap()
                .unwrap();
        assert_completed(&body, upstream);
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        assert_eq!(f.record().await["provider"], "mock");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn group_first_token_metadata_bounds_and_responses_release_commit_the_candidate() {
    for upstream in PROTOCOLS {
        for responses_release in [false, true] {
            let script = if responses_release {
                opening(upstream).to_owned()
            } else {
                opening(upstream).replace("abandoned-model", &"m".repeat(70 * 1024))
            };
            let f = Fixture::with_chunk_size(
                upstream,
                vec![script, prefix(upstream) + completion(upstream)],
                Ending::Paced(vec![(Duration::from_millis(350), limited(upstream).into())]),
                4096,
                |config| {
                    disable_stream_deadlines(config);
                    configure_timeout_fallback(config, true);
                    config.group[0].first_token_timeout_ms = 240;
                    config.stream_responses_hold_timeout_ms =
                        if responses_release { 100 } else { 0 };
                },
            )
            .await;
            let body = tokio::time::timeout(
                LIMIT,
                f.request_model(Protocol::Responses, "group/daily")
                    .await
                    .text(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(body.contains("fixture rate limit"));
            assert!(!body.contains("group first-token timeout"));
            assert_no_success(&body);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["provider"], "mock");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn group_first_token_zero_and_ordinary_provider_routes_are_excluded() {
    for ordinary in [false, true] {
        let f = Fixture::with_config(
            Protocol::Chat,
            vec![
                opening(Protocol::Chat).into(),
                prefix(Protocol::Chat) + completion(Protocol::Chat),
            ],
            Ending::Paced(vec![(
                Duration::from_millis(250),
                prefix(Protocol::Chat) + completion(Protocol::Chat),
            )]),
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = if ordinary { 100 } else { 0 };
                config.provider[0].fallback = vec!["spare/backup-model".into()];
            },
        )
        .await;
        let body = tokio::time::timeout(
            LIMIT,
            f.request_model(
                Protocol::Chat,
                if ordinary {
                    "mock/up-model"
                } else {
                    "group/daily"
                },
            )
            .await
            .text(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_completed(&body, Protocol::Chat);
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn group_first_token_nonstream_and_known_http_errors_do_not_use_opening_budget() {
    for nonstream in [false, true] {
        let script = if nonstream {
            json!({"model":"up-model", "choices":[{"message":{"role":"assistant", "content":"primary"},"finish_reason":"stop"}]}).to_string()
        } else {
            json!({"error":{"message":"known bad input"}}).to_string()
        };
        let f = Fixture::with_transport(
            Protocol::Chat,
            vec![
                String::new(),
                prefix(Protocol::Chat) + completion(Protocol::Chat),
            ],
            Ending::PacedEof(vec![(Duration::from_millis(300), script)]),
            Transport {
                status: if nonstream {
                    axum::http::StatusCode::OK
                } else {
                    axum::http::StatusCode::BAD_REQUEST
                },
                ..Transport::default()
            },
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = 100;
            },
        )
        .await;
        let (path, body) = client_request(Protocol::Chat, "group/daily", !nonstream);
        let response = tokio::time::timeout(
            LIMIT,
            reqwest::Client::new()
                .post(format!("{}{path}", f.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            response.status(),
            if nonstream {
                reqwest::StatusCode::OK
            } else {
                reqwest::StatusCode::BAD_REQUEST
            }
        );
        let body = tokio::time::timeout(LIMIT, response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains(if nonstream {
            "primary"
        } else {
            "known bad input"
        }));
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn group_first_token_applies_to_distinct_models_on_one_provider() {
    let f = Fixture::with_config(
        Protocol::Chat,
        vec![
            opening(Protocol::Chat).into(),
            prefix(Protocol::Chat) + completion(Protocol::Chat),
        ],
        Ending::PerSlot(vec![Ending::Hold, Ending::Eof]),
        |config| {
            disable_stream_deadlines(config);
            config.group.push(magpie_gateway::GroupConfig {
                id: "daily".into(),
                name: String::new(),
                routes: vec!["mock/up-model".into(), "mock/alternate-model".into()],
                rules: Vec::new(),
                classifier: String::new(),
                effort: String::new(),
                pick: String::new(),
                weights: Vec::new(),
                routing: magpie_gateway::RoutingPolicy::Order,
                first_token_timeout_ms: 120,
                affinity: Default::default(),
            });
        },
    )
    .await;
    let body = f
        .request_model(Protocol::Chat, "group/daily")
        .await
        .text()
        .await
        .unwrap();
    assert_completed(&body, Protocol::Chat);
    assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
    assert_eq!(
        *f.sent_models.lock().unwrap(),
        ["up-model", "alternate-model"]
    );
    assert_eq!(f.record().await["upstream_model"], "alternate-model");
    f.assert_released().await;
}

#[tokio::test]
async fn group_first_token_final_means_last_scheduled_candidate_not_last_configured_index() {
    use magpie_gateway::RoutingPolicy;
    for policy in [RoutingPolicy::Rotate, RoutingPolicy::Usage] {
        let f = Fixture::with_config(
            Protocol::Chat,
            vec![opening(Protocol::Chat).into(); 2],
            Ending::PerSlot(vec![
                Ending::Paced(vec![(
                    Duration::from_millis(300),
                    prefix(Protocol::Chat) + completion(Protocol::Chat),
                )]),
                Ending::Hold,
            ]),
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = 100;
                config.group[0].routing = policy;
            },
        )
        .await;
        let routes = f.state.config.current().group[0].routes.clone();
        let weights = f.state.config.current().group[0].route_weights();
        if policy == RoutingPolicy::Rotate {
            f.state
                .key_rotation
                .group_order("daily", policy, &routes, &weights, "");
        } else {
            f.state
                .key_rotation
                .record_group_attempt("daily", policy, &routes, &weights, 0);
        }
        let body = tokio::time::timeout(
            LIMIT,
            f.request_model(Protocol::Chat, "group/daily").await.text(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_completed(&body, Protocol::Chat);
        assert_eq!(*f.attempts.lock().unwrap(), [1, 0]);
        assert_eq!(f.record().await["provider"], "mock");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn group_first_token_inflight_config_is_pinned_and_future_requests_use_edits() {
    let f = Fixture::with_config(
        Protocol::Chat,
        vec![opening(Protocol::Chat).into(); 2],
        Ending::Paced(vec![(
            Duration::from_millis(450),
            prefix(Protocol::Chat) + completion(Protocol::Chat),
        )]),
        |config| {
            disable_stream_deadlines(config);
            configure_timeout_fallback(config, true);
            config.group[0].first_token_timeout_ms = 200;
        },
    )
    .await;
    let request = f.request_model(Protocol::Chat, "group/daily");
    let update = async {
        f.wait_for_attempts(&[0]).await;
        f.state
            .config
            .update(|config| {
                config.group[0].first_token_timeout_ms = 0;
                Ok(())
            })
            .await
            .unwrap();
    };
    let (response, ()) = tokio::join!(request, update);
    let body = tokio::time::timeout(LIMIT, response.text())
        .await
        .unwrap()
        .unwrap();
    assert_completed(&body, Protocol::Chat);
    assert_eq!(
        *f.attempts.lock().unwrap(),
        [0, 1],
        "inflight request must keep its original group budget"
    );
    assert_eq!(f.record().await["provider"], "spare");
    let body = tokio::time::timeout(
        LIMIT,
        f.request_model(Protocol::Chat, "group/daily").await.text(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_completed(&body, Protocol::Chat);
    // Key rotation selects the second primary key; the edited zero budget lets
    // it finish on primary even though content takes longer than the old cap.
    assert_eq!(*f.attempts.lock().unwrap(), [0, 1, 1]);
    f.assert_released().await;
}

#[tokio::test]
async fn group_first_token_socket_disconnect_after_failover_persists_only_last_attempt() {
    for upstream in PROTOCOLS {
        let f = Fixture::with_config(
            upstream,
            vec![
                opening(upstream).into(),
                opening(upstream).replace("99", "7"),
            ],
            Ending::Hold,
            |config| {
                disable_stream_deadlines(config);
                configure_timeout_fallback(config, true);
                config.group[0].first_token_timeout_ms = 120;
            },
        )
        .await;
        let history = f.persistent_history();
        let socket = f.socket_request(upstream, "group/daily", true).await;
        f.wait_for_attempts(&[0, 1]).await;
        // The final candidate is held without a group timer; disconnect cancels
        // it and records its usage, never the abandoned candidate's usage.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let record = f.disconnect_before_headers(socket, history.path()).await;
        assert_eq!(record["provider"], "spare");
        assert_eq!(record["requested_model"], "group/daily");
        assert_eq!(record["upstream_model"], "backup-model");
        assert_eq!(record["key_slot"], 0);
        if upstream == Protocol::Responses {
            assert!(record["usage"]["input"].is_null());
        } else {
            assert_eq!(record["usage"]["input"], 7);
        }
    }
}

fn loop_delta(protocol: Protocol, reasoning: bool, text: &str) -> String {
    let value = match (protocol, reasoning) {
        (Protocol::Chat, false) => json!({"choices":[{"delta":{"content":text}}]}),
        (Protocol::Chat, true) => json!({"choices":[{"delta":{"reasoning_content":text}}]}),
        (Protocol::Anthropic, false) => {
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})
        }
        (Protocol::Anthropic, true) => {
            json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":text}})
        }
        (Protocol::Responses, false) => json!({"type":"response.output_text.delta","delta":text}),
        (Protocol::Responses, true) => {
            json!({"type":"response.reasoning_summary_text.delta","delta":text})
        }
        (Protocol::Gemini, _) => {
            json!({"candidates":[{"content":{"parts":[{"text":text,"thought":reasoning}]}}]})
        }
    };
    format!("data: {value}\n\n")
}

#[tokio::test]
async fn loop_guard_ends_text_and_reasoning_without_retry_for_every_protocol_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            for reasoning in [false, true] {
                for grouped in [false, true] {
                    let mut script = prefix(upstream);
                    for _ in 0..300 {
                        script += &loop_delta(
                            upstream,
                            reasoning,
                            "Now. Writing. Go. Let me write. OK. Here. Produce.\n",
                        );
                    }
                    script += &loop_delta(upstream, false, "SHOULD-NOT-LEAK");
                    script += completion(upstream);
                    let f = Fixture::with_chunk_size(
                        upstream,
                        vec![script, prefix(upstream) + completion(upstream)],
                        Ending::Hold,
                        4096,
                        |config| {
                            disable_stream_deadlines(config);
                            configure_timeout_fallback(config, grouped);
                        },
                    )
                    .await;
                    let model = if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    };
                    let body =
                        tokio::time::timeout(LIMIT, f.request_model(client, model).await.text())
                            .await
                            .expect(
                                "loop guard must terminate even with upstream timeouts disabled",
                            )
                            .unwrap();
                    let channel = if reasoning { "reasoning" } else { "text" };
                    assert_one_stream_error(
                        &body,
                        client,
                        &format!("model's {channel} was stuck in a loop"),
                    );
                    assert!(body.contains("partial"), "{body}");
                    assert!(body.contains("Now."), "{body}");
                    assert!(!body.contains("SHOULD-NOT-LEAK"), "{body}");
                    assert_no_success(&body);
                    // A looped reasoning ends as it did; a looped text is
                    // a cut mid-reply, asked to go on where the upstream
                    // takes a prefill and nobody else answers for it.
                    let replays = !reasoning && !grouped && mid_reply_continues(upstream);
                    assert_eq!(
                        *f.attempts.lock().unwrap(),
                        if replays {
                            cut_attempts(upstream)
                        } else {
                            vec![0]
                        },
                        "unexpected key/provider/group replay"
                    );
                    let record = f.record().await;
                    assert_eq!(record["status"], "upstream_stream_error");
                    assert_eq!(record["provider"], "mock");
                    if replays {
                        assert_partial_usage_summed(&record, upstream);
                    } else {
                        assert_partial_usage(&record, upstream);
                    }
                    assert!(!record.to_string().contains("Now."));
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
}

#[tokio::test]
async fn reasoning_only_loop_reasks_same_candidate_once_and_preserves_one_client_response() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let mut first = opening(upstream).replace(":99", ":7");
            first += &loop_delta(upstream, true, &"mcp ".repeat(4096));
            let second = prefix(upstream) + completion(upstream);
            let f = Fixture::with_config(
                upstream,
                vec![String::new()],
                Ending::PerRequest(vec![first, second], Box::new(Ending::Hold)),
                |config| {
                    disable_stream_deadlines(config);
                    config.stream_loop_guard = true;
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .expect("reasoning-only loop reask timed out")
                .unwrap();

            let expected_input = if upstream == Protocol::Responses {
                7
            } else {
                14
            };
            assert_completed_usage(&body, client, expected_input, 3);
            {
                let requests = f.sent_bodies.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(
                    requests[0], requests[1],
                    "reask must resend the original request"
                );
            }
            assert!(body.contains("mcp"), "first reasoning was lost: {body}");
            assert!(body.contains("partial"), "reask text was lost: {body}");
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
            assert_eq!(
                f.sent_models.lock().unwrap().as_slice(),
                ["up-model", "up-model"]
            );
            if client == Protocol::Anthropic {
                assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
            }
            if client == Protocol::Responses {
                assert_eq!(body.matches("event: response.created").count(), 1, "{body}");
            }

            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["provider"], "mock");
            assert_eq!(record["key_slot"], 0);
            assert_eq!(record["usage"]["output"], 3);
            if upstream == Protocol::Responses {
                assert_eq!(record["usage"]["input"], 7);
            } else {
                assert_eq!(record["usage"]["input"], 14);
            }
            let stats = f.log.lock().unwrap().snapshot();
            let key = &stats.providers["mock"].keys["0"];
            assert_eq!((key.attempts, key.successes, key.errors), (2, 2, 0));
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn loop_guard_pure_reasoning_aliases_and_run_on_loops_obey_disable_setting() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            for enabled in [false, true] {
                let script = opening(upstream).replace(":99", ":7")
                    + &loop_delta(upstream, true, &"mcp ".repeat(4096))
                    + &loop_delta(upstream, false, "after-loop")
                    + completion(upstream);
                // Exercise Chat's alternate reasoning field and raw Responses
                // reasoning, with neither an earlier nor a simultaneous text delta.
                let script = script.replace("reasoning_content", "reasoning").replace(
                    "response.reasoning_summary_text.delta",
                    "response.reasoning_text.delta",
                );
                let f = Fixture::with_chunk_size(
                    upstream,
                    vec![script],
                    Ending::Hold,
                    4096,
                    |config| {
                        disable_stream_deadlines(config);
                        config.stream_loop_guard = enabled;
                    },
                )
                .await;
                let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                    .await
                    .unwrap()
                    .unwrap();
                if enabled {
                    assert_one_stream_error(&body, client, "model's reasoning was stuck in a loop");
                    assert!(!body.contains("after-loop"), "{body}");
                    assert_no_success(&body);
                    assert_eq!(f.record().await["status"], "upstream_stream_error");
                } else {
                    assert_completed(&body, client);
                    assert!(body.contains("after-loop"), "{body}");
                    assert!(!body.contains("stuck in a loop"), "{body}");
                    assert_eq!(f.record().await["status"], "ok");
                }
                assert!(body.contains("mcp"));
                f.assert_released().await;
            }
        }
    }
}

fn repetitive_tool_call(protocol: Protocol) -> String {
    let args = json!({"contents":"Now.\n".repeat(400)}).to_string();
    let events = match protocol {
        Protocol::Chat => vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_loop","function":{"name":"write","arguments":args}}]}}]}),
        ],
        Protocol::Anthropic => vec![
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_loop","name":"write","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":args}}),
            json!({"type":"content_block_stop","index":1}),
        ],
        Protocol::Responses => vec![
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_loop","name":"write"}}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":args}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call"}}),
        ],
        Protocol::Gemini => vec![
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_loop","name":"write","args":serde_json::from_str::<Value>(&args).unwrap()}}]}}]}),
        ],
    };
    events
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

#[tokio::test]
async fn loop_guard_does_not_treat_tool_arguments_or_normal_code_as_prose_loops() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let code = (0..300)
                .map(|i| format!("fn item_{i}() {{ return {i}; }}\n| row {i} | cell {i} |\n"))
                .collect::<String>();
            let script = prefix(upstream)
                + &repetitive_tool_call(upstream)
                + &loop_delta(upstream, false, &code)
                + completion(upstream);
            let f = Fixture::with_chunk_size(
                upstream,
                vec![script],
                Ending::Hold,
                4096,
                disable_stream_deadlines,
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_completed(&body, client);
            assert!(body.contains("item_299"));
            assert!(body.contains("call_loop"));
            assert!(body.contains("Now."));
            assert!(!body.contains("stuck in a loop"));
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn loop_guard_checks_unterminated_final_frame_without_success_or_replay() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script =
                prefix(upstream) + loop_delta(upstream, false, &"Now.\n".repeat(300)).trim_end();
            let f = Fixture::with_config(
                upstream,
                vec![script, prefix(upstream) + completion(upstream)],
                Ending::Eof,
                |config| {
                    disable_stream_deadlines(config);
                    configure_timeout_fallback(config, false);
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_one_stream_error(&body, client, "model's text was stuck in a loop");
            assert_no_success(&body);
            // The reply had begun when its text looped: where it can go
            // on, the same candidate is asked again before the error ends
            // it.
            assert_eq!(*f.attempts.lock().unwrap(), cut_attempts(upstream));
            assert_eq!(f.record().await["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn loop_guard_is_pinned_per_request_and_fresh_for_following_requests() {
    let f = Fixture::with_config(
        Protocol::Chat,
        vec![prefix(Protocol::Chat)],
        Ending::Paced(vec![(
            Duration::from_millis(200),
            loop_delta(Protocol::Chat, true, &"Now.\n".repeat(300)) + completion(Protocol::Chat),
        )]),
        disable_stream_deadlines,
    )
    .await;
    let mut response = f.request(Protocol::Chat).await;
    let mut body = read_through(&mut response, "partial").await;
    f.state
        .config
        .update(|config| {
            config.stream_loop_guard = false;
            Ok(())
        })
        .await
        .unwrap();
    body += &tokio::time::timeout(LIMIT, response.text())
        .await
        .unwrap()
        .unwrap();
    assert_one_stream_error(
        &body,
        Protocol::Chat,
        "model's reasoning was stuck in a loop",
    );
    assert_no_success(&body);
    let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
        .await
        .unwrap()
        .unwrap();
    assert_completed(&body, Protocol::Chat);
    assert!(!body.contains("stuck in a loop"));
    assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
    f.assert_released().await;
    assert_eq!(f.log.lock().unwrap().snapshot().recent_requests.len(), 2);

    // Neither channel's window is carried into the next request.
    let script = prefix(Protocol::Chat)
        + &loop_delta(Protocol::Chat, true, &"Now.\n".repeat(200))
        + completion(Protocol::Chat);
    let f = Fixture::with_config(
        Protocol::Chat,
        vec![script],
        Ending::Hold,
        disable_stream_deadlines,
    )
    .await;
    for _ in 0..2 {
        let body = tokio::time::timeout(LIMIT, f.request(Protocol::Chat).await.text())
            .await
            .unwrap()
            .unwrap();
        assert_completed(&body, Protocol::Chat);
        assert!(!body.contains("stuck in a loop"));
    }
    f.assert_released().await;
}

fn assert_loop_history(history: &std::path::Path, record: &Value) {
    let path = history.join("requests.jsonl");
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert_eq!(persisted.lines().count(), 1);
    assert_eq!(serde_json::from_str::<Value>(&persisted).unwrap(), *record);
    for private in [
        "secret-",
        "private cancellation prompt",
        "private repeated reply",
        "abandoned-model",
        "SHOULD-NOT-LEAK",
    ] {
        assert!(
            !persisted.contains(private),
            "private content in history: {private}"
        );
    }
    let restored = RequestLog::open(&path).unwrap().snapshot().recent_requests;
    assert_eq!(restored.len(), 1);
    assert_eq!(serde_json::to_value(&restored[0]).unwrap(), *record);
}

#[tokio::test]
async fn loop_guard_after_fallback_and_keepalive_persists_only_selected_partial_usage() {
    use magpie_gateway::{GroupConfig, RoutingPolicy};
    for grouped in [false, true] {
        for upstream in PROTOCOLS {
            for client in PROTOCOLS {
                let f = Fixture::with_chunk_size(
                    Protocol::Chat,
                    vec![
                        opening(Protocol::Chat).to_owned() + limited(Protocol::Chat),
                        prefix(upstream),
                        prefix(upstream) + completion(upstream),
                    ],
                    Ending::PerSlot(vec![
                        Ending::Hold,
                        Ending::Paced(vec![(
                            Duration::from_millis(160),
                            loop_delta(upstream, true, &"private repeated reply.\n".repeat(300))
                                + &loop_delta(upstream, false, "SHOULD-NOT-LEAK")
                                + completion(upstream),
                        )]),
                        Ending::Hold,
                    ]),
                    4096,
                    |config| {
                        enable_fast_keepalive(config);
                        config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                        let mut spare = config.provider[0].clone();
                        spare.id = "spare".into();
                        spare.protocol = upstream;
                        spare.models = vec!["backup-model".into()];
                        spare.api_keys = vec![
                            "secret-upstream-key-1".into(),
                            "secret-upstream-key-2".into(),
                        ];
                        if grouped {
                            config.group.push(GroupConfig {
                                id: "daily".into(),
                                name: "Daily".into(),
                                routes: vec!["mock/up-model".into(), "spare/backup-model".into()],
                                rules: Vec::new(),
                                classifier: String::new(),
                                effort: String::new(),
                                pick: String::new(),
                                weights: Vec::new(),
                                routing: RoutingPolicy::Order,
                                first_token_timeout_ms: 0,
                                affinity: Default::default(),
                            });
                        } else {
                            config.provider[0].fallback = vec!["spare/backup-model".into()];
                        }
                        config.provider.push(spare);
                    },
                )
                .await;
                let history = f.persistent_history();
                let requested = if grouped {
                    "group/daily"
                } else {
                    "mock/up-model"
                };
                let (path, mut request) = client_request(client, requested, true);
                match client {
                    Protocol::Responses => request["input"] = json!("private cancellation prompt"),
                    Protocol::Gemini => {
                        request["contents"][0]["parts"][0]["text"] =
                            json!("private cancellation prompt")
                    }
                    _ => request["messages"][0]["content"] = json!("private cancellation prompt"),
                }
                let mut response = tokio::time::timeout(
                    LIMIT,
                    reqwest::Client::new()
                        .post(format!("{}{path}", f.gateway.url))
                        .bearer_auth("secret-local-token")
                        .json(&request)
                        .send(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(response.status(), 200);
                let mut body = read_through(&mut response, "partial").await;
                if client != Protocol::Gemini {
                    body += &read_through(&mut response, KEEPALIVE).await;
                }
                body += &tokio::time::timeout(LIMIT, response.text())
                    .await
                    .unwrap()
                    .unwrap();
                assert_one_stream_error(&body, client, "model's reasoning was stuck in a loop");
                assert!(!body.contains("SHOULD-NOT-LEAK"));
                assert!(!body.contains("abandoned-model"));
                assert_eq!(
                    *f.attempts.lock().unwrap(),
                    [0, 1],
                    "loop cannot spend the spare key or replay the group"
                );
                assert_eq!(*f.sent_models.lock().unwrap(), ["up-model", "backup-model"]);
                let record = f.record().await;
                assert_eq!(record["requested_model"], requested);
                assert_eq!(record["provider"], "spare");
                assert_eq!(record["upstream_model"], "backup-model");
                assert_eq!(record["key_slot"], 0);
                assert_eq!(record["status"], "upstream_stream_error");
                assert_partial_usage(&record, upstream);
                assert_loop_history(history.path(), &record);
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn loop_guard_does_not_change_repetitive_nonstreaming_replies() {
    let repeated = "private repeated reply.\n".repeat(300);
    for upstream in PROTOCOLS {
        let reply = match upstream {
            Protocol::Chat => {
                json!({"model":"up-model", "choices":[{"message":{"role":"assistant", "content":repeated, "reasoning_content":repeated},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}})
            }
            Protocol::Anthropic => {
                json!({"type":"message","role":"assistant","model":"up-model","content":[{"type":"text","text":repeated},{"type":"thinking","thinking":repeated}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}})
            }
            Protocol::Responses => {
                json!({"model":"up-model","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":repeated}]}],"usage":{"input_tokens":7,"output_tokens":3}})
            }
            Protocol::Gemini => {
                json!({"modelVersion":"up-model","candidates":[{"content":{"role":"model","parts":[{"text":repeated},{"thought":true,"text":repeated}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}})
            }
        };
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![reply.to_string()],
                Ending::Eof,
                enable_fast_keepalive,
            )
            .await;
            let history = f.persistent_history();
            let (path, request) = client_request(client, "mock/up-model", false);
            let response = tokio::time::timeout(
                LIMIT,
                reqwest::Client::new()
                    .post(format!("{}{path}", f.gateway.url))
                    .bearer_auth("secret-local-token")
                    .json(&request)
                    .send(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], "application/json");
            let reply = tokio::time::timeout(LIMIT, response.json::<Value>())
                .await
                .unwrap()
                .unwrap();
            let text = match client {
                Protocol::Chat => reply["choices"][0]["message"]["content"].as_str().unwrap(),
                Protocol::Anthropic => reply["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["type"] == "text")
                    .unwrap()["text"]
                    .as_str()
                    .unwrap(),
                Protocol::Responses => reply["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["type"] == "message")
                    .unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap(),
                Protocol::Gemini => reply["candidates"][0]["content"]["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["thought"] != true)
                    .unwrap()["text"]
                    .as_str()
                    .unwrap(),
            };
            assert_eq!(text, repeated);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["stream"], false);
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            assert_loop_history(history.path(), &record);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn loop_guard_pending_window_is_dropped_on_client_disconnect_without_late_record() {
    use tokio::io::AsyncReadExt;
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script = prefix(upstream)
                + &loop_delta(upstream, true, &"private repeated reply.\n".repeat(200))
                + &loop_delta(upstream, false, "window-ready");
            let f = Fixture::with_chunk_size(
                upstream,
                vec![script],
                Ending::Hold,
                4096,
                enable_fast_keepalive,
            )
            .await;
            let history = f.persistent_history();
            let mut socket = f.socket_request(client, "mock/up-model", true).await;
            tokio::time::timeout(LIMIT, async {
                let mut body = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0, "guard ended below its window threshold");
                    body.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&body).contains("window-ready") {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            drop(socket);
            let record = f.record().await;
            assert_eq!(record["status"], "client_disconnected");
            assert_partial_usage(&record, upstream);
            f.assert_released().await;
            // No detached loop detector or keepalive can produce another record.
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(f.log.lock().unwrap().snapshot().recent_requests.len(), 1);
            assert_loop_history(history.path(), &record);
        }
    }
}

#[tokio::test]
async fn reasoning_reask_header_wait_obeys_first_content_deadline_without_fallback() {
    for client in PROTOCOLS {
        let upstream = client;
        let mut first = opening(upstream).replace(":99", ":7");
        first += &loop_delta(upstream, true, &"mcp ".repeat(4096));
        let f = Fixture::with_transport(
            upstream,
            vec![String::new(), prefix(upstream) + completion(upstream)],
            Ending::PerRequest(
                vec![first, prefix(upstream) + completion(upstream)],
                Box::new(Ending::Hold),
            ),
            Transport {
                header_delay: Duration::from_secs(3),
                header_delay_request: Some(1),
                ..Transport::default()
            },
            |config| {
                config.stream_first_content_timeout_ms = 200;
                config.stream_idle_timeout_ms = 0;
                config.stream_keepalive_interval_ms = 25;
                config.stream_keepalive_max_silence_ms = 10000;
            },
        )
        .await;
        let body = tokio::time::timeout(Duration::from_secs(2), f.request(client).await.text())
            .await
            .expect("reask ignored first-content header deadline")
            .unwrap();
        assert_one_stream_error(&body, client, "upstream first-content timeout");
        assert!(!body.contains("partial"));
        assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
        assert_eq!(f.record().await["status"], "upstream_stream_error");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn reasoning_reask_second_loop_or_prefetch_error_never_retries_again() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            for second_loop in [true, false] {
                let first = opening(upstream).replace(":99", ":7")
                    + &loop_delta(upstream, true, &"mcp ".repeat(4096));
                let second = opening(upstream).replace(":99", ":7")
                    + &if second_loop {
                        loop_delta(upstream, true, &"mcp ".repeat(4096))
                    } else {
                        error(upstream).to_owned()
                    };
                let f = Fixture::with_config(
                    upstream,
                    vec![String::new(), prefix(upstream) + completion(upstream)],
                    Ending::PerRequest(vec![first, second], Box::new(Ending::Hold)),
                    disable_stream_deadlines,
                )
                .await;
                let history = f.persistent_history();
                let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                    .await
                    .unwrap()
                    .unwrap();
                assert_one_stream_error(
                    &body,
                    client,
                    if second_loop {
                        "model's reasoning was stuck in a loop"
                    } else {
                        "fixture rate limit"
                    },
                );
                assert!(!body.contains("partial"));
                assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
                let record = f.record().await;
                assert_eq!(record["status"], "upstream_stream_error");
                assert_eq!(record["key_slot"], 0);
                assert_eq!(
                    record["usage"]["input"],
                    if upstream == Protocol::Responses {
                        Value::Null
                    } else {
                        json!(14)
                    }
                );
                assert_loop_history(history.path(), &record);
                let stats = f.log.lock().unwrap().snapshot();
                let key = &stats.providers["mock"].keys["0"];
                assert_eq!(
                    (key.attempts, key.successes, key.errors),
                    if second_loop { (2, 2, 0) } else { (2, 1, 1) }
                );
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

#[tokio::test]
async fn reasoning_reask_http_error_never_spends_another_key_or_route() {
    for client in PROTOCOLS {
        for status in [401, 429, 500] {
            let first = opening(client).replace(":99", ":7")
                + &loop_delta(client, true, &"mcp ".repeat(4096));
            let f = Fixture::with_transport(
                client,
                vec![String::new(), prefix(client) + completion(client)],
                Ending::PerRequest(
                    vec![
                        first,
                        r#"{"error":{"message":"reask rejected"}}"#.to_owned(),
                    ],
                    Box::new(Ending::Eof),
                ),
                Transport {
                    status: axum::http::StatusCode::from_u16(status).unwrap(),
                    status_request: Some(1),
                    ..Transport::default()
                },
                |config| {
                    disable_stream_deadlines(config);
                    configure_timeout_fallback(config, true);
                },
            )
            .await;
            let body =
                tokio::time::timeout(LIMIT, f.request_model(client, "group/daily").await.text())
                    .await
                    .unwrap()
                    .unwrap();
            assert_one_stream_error(&body, client, "reask rejected");
            assert!(!body.contains("partial"));
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_eq!(record["provider"], "mock");
            assert_partial_usage(&record, client);
            let stats = f.log.lock().unwrap().snapshot();
            let key = &stats.providers["mock"].keys["0"];
            assert_eq!((key.attempts, key.successes, key.errors), (2, 1, 1));
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
async fn reasoning_reask_socket_disconnect_cancels_pause_and_header_wait() {
    for client in PROTOCOLS {
        for awaiting_headers in [false, true] {
            let first = opening(client).replace(":99", ":7")
                + &loop_delta(client, true, &"mcp ".repeat(4096));
            let f = Fixture::with_transport(
                client,
                vec![String::new()],
                Ending::PerRequest(vec![first, opening(client).into()], Box::new(Ending::Hold)),
                Transport {
                    header_delay: Duration::from_secs(3),
                    header_delay_request: Some(1),
                    ..Transport::default()
                },
                disable_stream_deadlines,
            )
            .await;
            let history = f.persistent_history();
            let socket = f.socket_request(client, "mock/up-model", true).await;
            // A released first response proves the guard entered reask, not
            // merely that the client saw some earlier reasoning frames.
            tokio::time::timeout(LIMIT, async {
                while f.dropped.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            if awaiting_headers {
                f.wait_for_attempts(&[0, 0]).await;
            } else {
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
            }
            drop(socket);
            let record = f.record().await;
            assert_eq!(record["status"], "client_disconnected");
            assert_eq!(record["provider"], "mock");
            assert_partial_usage(&record, client);
            f.assert_released().await;
            // Wait past the retry pause to reject a detached retry or a late
            // terminal record after the downstream socket has closed.
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert_eq!(
                *f.attempts.lock().unwrap(),
                if awaiting_headers {
                    vec![0, 0]
                } else {
                    vec![0]
                }
            );
            assert_eq!(f.log.lock().unwrap().snapshot().recent_requests.len(), 1);
            assert_loop_history(history.path(), &record);
        }
    }
}

#[tokio::test]
async fn reasoning_reask_opening_and_http_error_body_share_first_content_deadline() {
    for client in PROTOCOLS {
        for http_error in [false, true] {
            let first = opening(client).replace(":99", ":7")
                + &loop_delta(client, true, &"mcp ".repeat(4096));
            let second = if http_error {
                String::new()
            } else {
                opening(client).replace(":99", ":7")
            };
            let f = Fixture::with_transport(
                client,
                vec![String::new(), prefix(client) + completion(client)],
                Ending::PerRequest(vec![first, second], Box::new(Ending::Hold)),
                Transport {
                    header_delay: Duration::from_millis(100),
                    header_delay_request: Some(1),
                    status: if http_error {
                        axum::http::StatusCode::TOO_MANY_REQUESTS
                    } else {
                        axum::http::StatusCode::OK
                    },
                    status_request: Some(1),
                    ..Transport::default()
                },
                |config| {
                    config.stream_first_content_timeout_ms = 250;
                    config.stream_idle_timeout_ms = 0;
                    config.stream_keepalive_interval_ms = 25;
                    config.stream_keepalive_max_silence_ms = 10000;
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_one_stream_error(&body, client, "upstream first-content timeout");
            // A timed-out second try is a cut after reasoning the client
            // has: where it goes on from a prefill it is asked again.
            let continued = !http_error && mid_reply_continues(client);
            assert_eq!(
                *f.attempts.lock().unwrap(),
                vec![0; if continued { 4 } else { 2 }]
            );
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_eq!(
                record["usage"]["input"],
                if client == Protocol::Responses {
                    Value::Null
                } else {
                    json!(if http_error {
                        7
                    } else if continued {
                        28
                    } else {
                        14
                    })
                }
            );
            let stats = f.log.lock().unwrap().snapshot();
            let key = &stats.providers["mock"].keys["0"];
            assert_eq!(
                (key.attempts, key.successes, key.errors),
                if continued { (4, 1, 3) } else { (2, 1, 1) }
            );
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

// Reference gemini_empty_reply_test.go: an empty STOP must not end the agent's turn.
#[tokio::test]
async fn gemini_empty_reply_reasks_before_ending_the_client_turn() {
    for client in PROTOCOLS {
        let empty = completion(Protocol::Gemini).to_owned();
        let success = format!(
            "{}{}",
            prefix(Protocol::Gemini),
            completion(Protocol::Gemini)
        );
        let f = Fixture::new(
            Protocol::Gemini,
            empty.clone(),
            Ending::PerRequest(vec![empty, success], Box::new(Ending::Hold)),
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
            .await
            .unwrap()
            .unwrap();
        assert!(
            body.contains("partial"),
            "empty reply ended the turn: {body}"
        );
        assert_completed_usage(&body, client, 14, 6);
        assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
        assert_eq!(f.record().await["usage"]["input"], 14);
        f.assert_released().await;
    }
}

fn gemini_reasoning_empty() -> String {
    "data: {\"modelVersion\":\"up-model\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Let me see.\",\"thought\":true}]}}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":0}}\n\n".to_owned() + completion(Protocol::Gemini)
}

#[tokio::test]
async fn gemini_empty_reply_literal_stop_reasoning_and_no_candidate_eof_recover() {
    let variants = [
        ("data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":0}}\n\n".to_owned(), Ending::Hold, 7, 3),
        (gemini_reasoning_empty(), Ending::Hold, 14, 6),
        ("data: {\"usageMetadata\":{\"promptTokenCount\":7}}\n\n".to_owned(), Ending::Eof, 14, 3),
    ];
    for (empty, ending, input, output) in variants {
        for client in PROTOCOLS {
            let f = Fixture::new(
                Protocol::Gemini,
                empty.clone(),
                Ending::PerRequest(
                    vec![
                        empty.clone(),
                        prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                    ],
                    Box::new(ending.clone()),
                ),
            )
            .await;
            let history = f.persistent_history();
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_completed_usage(&body, client, input, output);
            if empty.contains("Let me see.") {
                assert!(body.contains("Let me see."), "reasoning was discarded");
            }
            assert!(body.contains("partial"));
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
            let bodies = f.sent_bodies.lock().unwrap().clone();
            assert_eq!(bodies[0], bodies[1], "same conversation/model/key reask");
            let record = f.record().await;
            assert_eq!(record["usage"]["input"], input);
            assert_eq!(record["usage"]["output"], output);
            assert_loop_history(history.path(), &record);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_committed_stream_allows_two_reasks_and_one_completion() {
    for client in PROTOCOLS {
        let empty = gemini_reasoning_empty();
        let f = Fixture::new(
            Protocol::Gemini,
            empty.clone(),
            Ending::PerRequest(
                vec![
                    empty.clone(),
                    empty,
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Box::new(Ending::Hold),
            ),
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
            .await
            .unwrap()
            .unwrap();
        assert_completed_usage(&body, client, 21, 9);
        assert!(body.contains("Let me see."));
        if client != Protocol::Responses {
            assert_eq!(body.matches("Let me see.").count(), 2);
        }
        assert_eq!(*f.attempts.lock().unwrap(), [0, 0, 0]);
        if client == Protocol::Anthropic {
            assert_eq!(body.matches("event: message_start").count(), 1);
        }
        if client == Protocol::Responses {
            assert_eq!(body.matches("event: response.created").count(), 1);
        }
        let bodies = f.sent_bodies.lock().unwrap().clone();
        assert!(bodies.windows(2).all(|b| b[0] == b[1]));
        let record = f.record().await;
        assert_eq!(record["usage"]["input"], 21);
        assert_eq!(record["usage"]["output"], 9);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_empty_reply_exhaustion_is_bounded_without_a_success_marker() {
    for client in PROTOCOLS {
        for committed in [false, true] {
            let empty = if committed {
                gemini_reasoning_empty()
            } else {
                completion(Protocol::Gemini).into()
            };
            let f = Fixture::new(Protocol::Gemini, empty, Ending::Hold).await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_one_stream_error(&body, client, "an empty reply");
            let count = if committed { 3 } else { 2 };
            assert_eq!(*f.attempts.lock().unwrap(), vec![0; count]);
            let record = f.record().await;
            assert_eq!(record["usage"]["input"], count * 7);
            assert_eq!(record["usage"]["output"], count * 3);
            assert_eq!(record["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_can_change_keys_or_routes_only_while_held() {
    for client in PROTOCOLS {
        for mode in 0..3 {
            let f = Fixture::with_config(
                Protocol::Gemini,
                vec![
                    completion(Protocol::Gemini).into(),
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Ending::Hold,
                |config| {
                    if mode != 0 {
                        configure_timeout_fallback(config, mode == 2);
                        config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                    }
                },
            )
            .await;
            let history = f.persistent_history();
            let requested = if mode == 2 {
                "group/daily"
            } else {
                "mock/up-model"
            };
            let body = tokio::time::timeout(LIMIT, f.request_model(client, requested).await.text())
                .await
                .unwrap()
                .unwrap();
            // Usage on the abandoned key/route is never charged to the selected reply.
            assert_completed_usage(&body, client, 7, 3);
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0, 1]);
            let record = f.record().await;
            assert_eq!(record["provider"], if mode == 0 { "mock" } else { "spare" });
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            assert_loop_history(history.path(), &record);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_group_commit_limits_reask_to_one_without_fallback() {
    for client in PROTOCOLS {
        let f = Fixture::with_config(
            Protocol::Gemini,
            vec![
                gemini_reasoning_empty(),
                prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            ],
            Ending::Hold,
            |config| {
                configure_timeout_fallback(config, true);
                config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
            },
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request_model(client, "group/daily").await.text())
            .await
            .unwrap()
            .unwrap();
        assert_one_stream_error(&body, client, "an empty reply");
        assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
        assert_eq!(f.record().await["usage"]["input"], 14);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_empty_reply_length_filter_tool_image_search_and_other_protocols_do_not_reask() {
    let scripts = [
        completion(Protocol::Gemini).replace("STOP", "MAX_TOKENS"),
        completion(Protocol::Gemini).replace("STOP", "SAFETY"),
        completion(Protocol::Gemini).replace("STOP", "IMAGE_SAFETY"),
        completion(Protocol::Gemini).replace("STOP", "NEW_FILTER"),
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{}}}]},\"finishReason\":\"STOP\"}]}\n\n".into(),
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"inlineData\":{\"mimeType\":\"image/png\",\"data\":\"aGk=\"}}]},\"finishReason\":\"STOP\"}]}\n\n".into(),
        "data: {\"candidates\":[{\"groundingMetadata\":{\"webSearchQueries\":[\"rust\"]},\"finishReason\":\"STOP\"}]}\n\n".into(),
    ];
    for client in PROTOCOLS {
        for script in &scripts {
            let script =
                "data: {\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3}}\n\n"
                    .to_owned()
                    + script;
            let refused = script.contains("SAFETY");
            let filtered = script.contains("NEW_FILTER");
            let length = script.contains("MAX_TOKENS");
            let f = Fixture::new(Protocol::Gemini, script, Ending::Hold).await;
            let response = f.request_model_raw(client, "mock/up-model").await;
            assert_eq!(response.status().as_u16(), if refused { 400 } else { 200 });
            let body = response.text().await.unwrap();
            if refused {
                assert!(body.contains("safety filter"), "{body}");
            } else if filtered {
                assert_filtered(&body, client);
            } else if length && client == Protocol::Responses {
                assert!(body.contains("response.incomplete"), "{body}");
                assert!(body.contains("max_output_tokens"), "{body}");
                assert!(!body.contains("response.completed"), "{body}");
            } else {
                assert_completed(&body, client);
            }
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
        for upstream in [Protocol::Chat, Protocol::Anthropic, Protocol::Responses] {
            let f = Fixture::new(
                upstream,
                opening(upstream).replace("99", "7") + completion(upstream),
                Ending::Hold,
            )
            .await;
            let body = f.request(client).await.text().await.unwrap();
            assert_completed(&body, client);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
        // Prompt filtering is not an empty-success retry, even without a candidate.
        let f = Fixture::new(
            Protocol::Gemini,
            "data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}\n\n".into(),
            Ending::Eof,
        )
        .await;
        let response = f.request_model_raw(client, "mock/up-model").await;
        assert_eq!(response.status().as_u16(), 400);
        let body = response.text().await.unwrap();
        assert!(body.contains("safety filter"), "{body}");
        f.assert_released().await;
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
    }
}

#[tokio::test]
async fn gemini_empty_reply_does_not_reclassify_vendor_or_parser_errors() {
    for client in PROTOCOLS {
        for (script, message) in [
            ("data: {broken\n\n", "bad Gemini upstream stream payload"),
            (
                "data: {\"error\":{\"code\":400,\"message\":\"known bad input\"}}\n\n",
                "known bad input",
            ),
        ] {
            let f = Fixture::new(Protocol::Gemini, script.into(), Ending::Eof).await;
            let body = f.request(client).await.text().await.unwrap();
            assert_one_stream_error(&body, client, message);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_nonstream_recovery_and_exhaustion_share_routing_and_usage() {
    for client in PROTOCOLS {
        for recover in [false, true] {
            let empty = r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#.to_owned();
            let success = r#"{"modelVersion":"up-model","candidates":[{"content":{"parts":[{"text":"whole answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#.to_owned();
            let f = Fixture::new(
                Protocol::Gemini,
                empty.clone(),
                Ending::PerRequest(
                    vec![empty.clone(), if recover { success } else { empty }],
                    Box::new(Ending::Eof),
                ),
            )
            .await;
            let (path, body) = client_request(client, "mock/up-model", false);
            let response = reqwest::Client::new()
                .post(format!("{}{path}", f.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), if recover { 200 } else { 502 });
            let body: Value = response.json().await.unwrap();
            let record = f.record().await;
            assert_eq!(record["usage"]["input"], 14);
            assert_eq!(record["usage"]["output"], 6);
            if recover {
                let ir = match client {
                    Protocol::Chat => magpie_core::chat::decode_response(&body),
                    Protocol::Anthropic => magpie_core::anthropic::decode_response(&body),
                    Protocol::Responses => magpie_core::responses::decode_response(&body),
                    Protocol::Gemini => magpie_core::gemini::decode_response(&body),
                }
                .unwrap();
                assert_eq!(ir.usage.input, Some(14));
                assert_eq!(ir.usage.output, Some(6));
                assert!(body.to_string().contains("whole answer"));
            } else {
                assert!(body.to_string().contains("an empty reply"));
            }
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
            let bodies = f.sent_bodies.lock().unwrap().clone();
            assert_eq!(bodies[0], bodies[1]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_disconnect_cancels_pause_headers_and_nonstream_retry() {
    for client in PROTOCOLS {
        for stream in [false, true] {
            for awaiting_headers in [false, true] {
                let empty = if stream {
                    completion(Protocol::Gemini).to_owned()
                } else {
                    r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#.into()
                };
                let f = Fixture::with_transport(
                    Protocol::Gemini,
                    vec![empty],
                    Ending::Eof,
                    Transport {
                        header_delay: Duration::from_secs(60),
                        header_delay_request: Some(1),
                        ..Transport::default()
                    },
                    disable_stream_deadlines,
                )
                .await;
                let history = f.persistent_history();
                let socket = f.socket_request(client, "mock/up-model", stream).await;
                tokio::time::timeout(LIMIT, async {
                    while f.dropped.load(Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                if awaiting_headers {
                    f.wait_for_attempts(&[0, 0]).await;
                }
                drop(socket);
                let record = f.record().await;
                assert_eq!(record["status"], "client_disconnected");
                assert_eq!(record["usage"]["input"], 7);
                assert_eq!(record["usage"]["output"], 3);
                f.assert_released().await;
                tokio::time::sleep(Duration::from_millis(1100)).await;
                assert_eq!(
                    *f.attempts.lock().unwrap(),
                    if awaiting_headers {
                        vec![0, 0]
                    } else {
                        vec![0]
                    }
                );
                assert_loop_history(history.path(), &record);
            }
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_reask_headers_are_bounded_and_commit_still_prevents_fallback() {
    for client in PROTOCOLS {
        for committed in [false, true] {
            let f = Fixture::with_transport(
                Protocol::Gemini,
                vec![
                    String::new(),
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Ending::PerRequest(
                    vec![
                        if committed {
                            gemini_reasoning_empty()
                        } else {
                            completion(Protocol::Gemini).into()
                        },
                        prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                    ],
                    Box::new(Ending::Hold),
                ),
                Transport {
                    header_delay: Duration::from_secs(60),
                    header_delay_request: Some(1),
                    ..Transport::default()
                },
                |config| {
                    configure_timeout_fallback(config, false);
                    config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                    enable_fast_keepalive(config);
                    config.stream_first_content_timeout_ms = 200;
                    config.stream_idle_timeout_ms = 0;
                },
            )
            .await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            let record = f.record().await;
            if committed {
                assert_one_stream_error(&body, client, "upstream first-content timeout");
                assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
                assert_eq!(record["usage"]["input"], 7);
                assert_eq!(record["provider"], "mock");
            } else {
                assert_completed_usage(&body, client, 7, 3);
                assert_eq!(*f.attempts.lock().unwrap(), [0, 0, 1]);
                assert_eq!(record["provider"], "spare");
            }
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_reply_config_edit_cannot_redirect_an_inflight_reask() {
    for client in PROTOCOLS {
        let empty = gemini_reasoning_empty();
        let f = Fixture::with_config(
            Protocol::Gemini,
            vec![
                empty.clone(),
                prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            ],
            Ending::PerRequest(
                vec![
                    empty,
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Box::new(Ending::Hold),
            ),
            |config| configure_timeout_fallback(config, false),
        )
        .await;
        let mut response = f.request(client).await;
        read_through(&mut response, "Let me see.").await;
        f.state
            .config
            .update(|config| {
                config.provider[0].api_keys = vec!["secret-upstream-key-1".into()];
                config.provider[0].base_url = "http://127.0.0.1:1".into();
                config.provider[0].models = vec!["changed-model".into()];
                Ok(())
            })
            .await
            .unwrap();
        let body = tokio::time::timeout(LIMIT, response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("partial"));
        assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
        assert_eq!(*f.sent_models.lock().unwrap(), ["up-model", "up-model"]);
        let requests = f.sent_bodies.lock().unwrap().clone();
        assert_eq!(requests[0], requests[1]);
        assert_eq!(f.record().await["usage"]["input"], 14);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_empty_reply_retry_with_no_new_usage_keeps_prior_client_and_history_usage() {
    for client in PROTOCOLS {
        let success = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"finishReason\":\"STOP\"}]}\n\n".to_owned();
        let f = Fixture::new(
            Protocol::Gemini,
            completion(Protocol::Gemini).into(),
            Ending::PerRequest(
                vec![completion(Protocol::Gemini).into(), success],
                Box::new(Ending::Hold),
            ),
        )
        .await;
        let body = f.request(client).await.text().await.unwrap();
        assert_completed_usage(&body, client, 7, 3);
        assert_eq!(f.record().await["usage"]["input"], 7);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_empty_reply_nonstream_can_fall_back_between_keys_providers_and_groups() {
    for client in PROTOCOLS {
        for mode in 0..3 {
            let empty = r#"{"modelVersion":"up-model","candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#;
            let success = r#"{"modelVersion":"backup-model","candidates":[{"content":{"parts":[{"text":"whole answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#;
            let f = Fixture::with_config(
                Protocol::Gemini,
                vec![empty.into(), success.into()],
                Ending::Eof,
                |config| {
                    if mode != 0 {
                        configure_timeout_fallback(config, mode == 2);
                        config.provider[0].api_keys = vec!["secret-upstream-key-0".into()];
                    }
                },
            )
            .await;
            let history = f.persistent_history();
            let model = if mode == 2 {
                "group/daily"
            } else {
                "mock/up-model"
            };
            let (path, body) = client_request(client, model, false);
            let response = tokio::time::timeout(
                LIMIT,
                reqwest::Client::new()
                    .post(format!("{}{path}", f.gateway.url))
                    .bearer_auth("secret-local-token")
                    .json(&body)
                    .send(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status(), 200);
            let body: Value = response.json().await.unwrap();
            let ir = match client {
                Protocol::Chat => magpie_core::chat::decode_response(&body),
                Protocol::Anthropic => magpie_core::anthropic::decode_response(&body),
                Protocol::Responses => magpie_core::responses::decode_response(&body),
                Protocol::Gemini => magpie_core::gemini::decode_response(&body),
            }
            .unwrap();
            assert_eq!(ir.usage.input, Some(7));
            assert_eq!(ir.usage.output, Some(3));
            assert!(body.to_string().contains("whole answer"));
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0, 1]);
            let record = f.record().await;
            assert_eq!(record["provider"], if mode == 0 { "mock" } else { "spare" });
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            assert_loop_history(history.path(), &record);
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
async fn gemini_empty_reply_committed_http_errors_never_change_key_or_route() {
    for client in PROTOCOLS {
        for status in [401, 429, 500] {
            let f = Fixture::with_transport(
                Protocol::Gemini,
                vec![
                    String::new(),
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Ending::PerRequest(
                    vec![
                        gemini_reasoning_empty(),
                        r#"{"error":{"message":"empty reask rejected"}}"#.into(),
                    ],
                    Box::new(Ending::Eof),
                ),
                Transport {
                    status: axum::http::StatusCode::from_u16(status).unwrap(),
                    status_request: Some(1),
                    ..Transport::default()
                },
                |config| {
                    disable_stream_deadlines(config);
                    configure_timeout_fallback(config, true);
                },
            )
            .await;
            let body =
                tokio::time::timeout(LIMIT, f.request_model(client, "group/daily").await.text())
                    .await
                    .unwrap()
                    .unwrap();
            assert_one_stream_error(&body, client, "empty reask rejected");
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
            let record = f.record().await;
            assert_eq!(record["status"], "upstream_stream_error");
            assert_eq!(record["provider"], "mock");
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
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
async fn gemini_empty_reply_and_reasoning_loop_have_independent_bounded_reasks() {
    for client in PROTOCOLS {
        for empty_first in [false, true] {
            let empty = gemini_reasoning_empty();
            let looping = "data: {\"modelVersion\":\"up-model\",\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3}}\n\n".to_owned()
                + &loop_delta(Protocol::Gemini, true, &"mcp ".repeat(4096));
            let (first, second) = if empty_first {
                (empty.clone(), looping)
            } else {
                (looping, empty.clone())
            };
            let f = Fixture::with_config(
                Protocol::Gemini,
                vec![String::new()],
                Ending::PerRequest(
                    vec![
                        first,
                        second,
                        empty,
                        prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                    ],
                    Box::new(Ending::Hold),
                ),
                disable_stream_deadlines,
            )
            .await;
            let history = f.persistent_history();
            // One reasoning pause and the empty-reply 1s/2s pauses: allow headroom.
            let body = tokio::time::timeout(Duration::from_secs(8), f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_completed_usage(&body, client, 28, 12);
            assert_eq!(*f.attempts.lock().unwrap(), [0, 0, 0, 0]);
            let bodies = f.sent_bodies.lock().unwrap().clone();
            assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
            let record = f.record().await;
            assert_eq!(record["usage"]["input"], 28);
            assert_eq!(record["usage"]["output"], 12);
            assert_loop_history(history.path(), &record);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_image_or_search_answer_prevents_reasoning_loop_replay() {
    for client in PROTOCOLS {
        for answer in [
            json!({"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"aGk="}}]}}),
            json!({"groundingMetadata":{"webSearchQueries":["rust"]}}),
        ] {
            let script = format!(
                "data: {}\n\n",
                json!({"modelVersion":"up-model", "candidates":[answer], "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}})
            ) + &loop_delta(Protocol::Gemini, true, &"mcp ".repeat(4096));
            let f = Fixture::new(Protocol::Gemini, script, Ending::Hold).await;
            let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
                .await
                .unwrap()
                .unwrap();
            assert_one_stream_error(&body, client, "model's reasoning was stuck in a loop");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            let record = f.record().await;
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn gemini_empty_finish_reason_waits_for_answer_and_real_completion() {
    for client in PROTOCOLS {
        let pending =
            "data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"\"}]}\n\n";
        let f = Fixture::new(
            Protocol::Gemini,
            pending.to_owned() + &prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            Ending::Hold,
        )
        .await;
        let body = tokio::time::timeout(LIMIT, f.request(client).await.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("partial"));
        assert_completed(&body, client);
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        f.assert_released().await;
    }
}

fn filtered_completion(protocol: Protocol) -> String {
    match protocol {
        Protocol::Chat => completion(protocol).replace("stop", "content_filter"),
        Protocol::Anthropic => completion(protocol).replace("end_turn", "refusal"),
        Protocol::Responses => completion(protocol)
            .replace("response.completed", "response.incomplete")
            .replace(
                "\"status\":\"completed\"",
                "\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"content_filter\"}",
            ),
        Protocol::Gemini => completion(protocol).replace("STOP", "SAFETY"),
    }
}

fn filtered_json(protocol: Protocol, answer: bool) -> String {
    let ir = magpie_core::ir::CompletionResponse {
        model: "up-model".into(),
        message: magpie_core::ir::Message {
            role: magpie_core::ir::Role::Assistant,
            parts: if answer {
                vec![magpie_core::ir::Part::text("partial answer")]
            } else {
                vec![]
            },
        },
        stop: magpie_core::ir::StopReason::ContentFilter,
        usage: magpie_core::ir::Usage {
            input: Some(7),
            output: Some(3),
            ..Default::default()
        },
    };
    match protocol {
        Protocol::Chat => magpie_core::chat::encode_response(&ir),
        Protocol::Anthropic => magpie_core::anthropic::encode_response(&ir),
        Protocol::Responses => magpie_core::responses::encode_response(&ir),
        Protocol::Gemini => magpie_core::gemini::encode_response(&ir),
    }
    .to_string()
}

fn assert_filtered(body: &str, protocol: Protocol) {
    assert!(!body.contains("upstream refused"), "{body}");
    assert!(
        body.contains(match protocol {
            Protocol::Chat => "content_filter",
            Protocol::Anthropic => "refusal",
            Protocol::Responses => "content_filter",
            Protocol::Gemini => "SAFETY",
        }),
        "{body}"
    );
    if protocol == Protocol::Responses {
        assert!(body.contains("content_filter"), "{body}");
        assert!(!body.contains("response.completed"), "{body}");
    }
}

#[tokio::test]
async fn safety_held_refusals_return_400_without_key_retry_or_cooldown() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script = opening(upstream).replace("99", "7") + &filtered_completion(upstream);
            let f = Fixture::with_keys(upstream, vec![script.clone(), script], Ending::Hold).await;
            let response = f.request_model_raw(client, "mock/up-model").await;
            assert_eq!(
                response.status().as_u16(),
                400,
                "{upstream:?} -> {client:?}"
            );
            let body: Value = response.json().await.unwrap();
            assert!(body.to_string().contains("safety filter"), "{body}");
            let record = f.record().await;
            assert_eq!(record["usage"]["input"], 7, "{record}");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
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
async fn safety_held_refusals_skip_keys_and_use_only_configured_routes() {
    for grouped in [false, true] {
        for upstream in PROTOCOLS {
            for client in PROTOCOLS {
                let f = Fixture::with_config(
                    upstream,
                    vec![
                        opening(upstream).replace("99", "7") + &filtered_completion(upstream),
                        prefix(upstream) + completion(upstream),
                    ],
                    Ending::Hold,
                    |config| configure_timeout_fallback(config, grouped),
                )
                .await;
                let body = f
                    .request_model(
                        client,
                        if grouped {
                            "group/daily"
                        } else {
                            "mock/up-model"
                        },
                    )
                    .await
                    .text()
                    .await
                    .unwrap();
                assert_completed(&body, client);
                assert!(!body.contains("safety filter"), "{body}");
                assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
                assert_eq!(*f.sent_models.lock().unwrap(), ["up-model", "backup-model"]);
                let record = f.record().await;
                assert_eq!(record["provider"], "spare", "{record}");
                assert_eq!(record["usage"]["input"], 7, "{record}");
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn safety_after_answer_preserves_filter_and_never_replays() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::with_config(
                upstream,
                vec![
                    prefix(upstream) + &filtered_completion(upstream),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Hold,
                |config| configure_timeout_fallback(config, false),
            )
            .await;
            let body = f.request(client).await.text().await.unwrap();
            assert_filtered(&body, client);
            assert!(body.contains("partial"), "{body}");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            let record = f.record().await;
            assert_eq!(record["status"], "ok", "{record}");
            assert_eq!(record["usage"]["input"], 7, "{record}");
            assert_eq!(record["usage"]["output"], 3, "{record}");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn safety_nonstream_empty_refusal_and_partial_answer_remain_distinct() {
    for answer in [false, true] {
        for upstream in PROTOCOLS {
            for client in PROTOCOLS {
                let f = Fixture::with_keys(
                    upstream,
                    vec![filtered_json(upstream, answer); 2],
                    Ending::Eof,
                )
                .await;
                let (path, body) = client_request(client, "mock/up-model", false);
                let response = reqwest::Client::new()
                    .post(format!("{}{path}", f.gateway.url))
                    .bearer_auth("secret-local-token")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status().as_u16(),
                    if answer { 200 } else { 400 },
                    "{upstream:?} -> {client:?}"
                );
                let body = response.text().await.unwrap();
                if answer {
                    assert_filtered(&body, client);
                } else {
                    assert!(body.contains("safety filter"), "{body}");
                }
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
                let record = f.record().await;
                assert_eq!(record["usage"]["input"], 7, "{record}");
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn safety_reasoning_or_released_metadata_never_switches_routes() {
    for metadata in [false, true] {
        for upstream in PROTOCOLS {
            for client in PROTOCOLS {
                let committed = if metadata {
                    opening(upstream).repeat(70)
                } else {
                    opening(upstream).to_owned()
                        + &loop_delta(upstream, true, "reasoning already sent")
                };
                let f = Fixture::with_config(
                    upstream,
                    vec![
                        committed + &filtered_completion(upstream),
                        prefix(upstream) + completion(upstream),
                    ],
                    Ending::Hold,
                    |config| configure_timeout_fallback(config, false),
                )
                .await;
                let body = f.request(client).await.text().await.unwrap();
                assert_one_stream_error(&body, client, "safety filter");
                if !metadata {
                    assert!(body.contains("reasoning already sent"), "{body}");
                }
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
                let record = f.record().await;
                assert_eq!(record["status"], "refusal", "{record}");
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

#[tokio::test]
async fn safety_same_frame_answer_and_filter_are_not_empty_refusals() {
    for upstream in [Protocol::Chat, Protocol::Gemini] {
        for client in PROTOCOLS {
            let frame = match upstream {
                Protocol::Chat => {
                    json!({"model":"up-model","choices":[{"delta":{"content":"same frame answer"},"finish_reason":"content_filter"}], "usage":{"prompt_tokens":7,"completion_tokens":3}})
                }
                _ => {
                    json!({"modelVersion":"up-model", "promptFeedback":{"blockReason":"SAFETY"}, "candidates":[{"content":{"parts":[{"text":"same frame answer"}]},"finishReason":"SAFETY"}], "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}})
                }
            };
            let script = format!(
                "data: {frame}\n\n{}",
                if upstream == Protocol::Chat {
                    "data: [DONE]\n\n"
                } else {
                    ""
                }
            );
            let f = Fixture::new(upstream, script, Ending::Hold).await;
            let body = f.request(client).await.text().await.unwrap();
            assert!(body.contains("same frame answer"), "{body}");
            assert_filtered(&body, client);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn safety_policy_error_envelopes_fall_back_without_leaking_vendor_messages() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            for http in [false, true] {
                let policy = json!({"code":"content_policy_violation", "message":"PRIVATE PROMPT secret-upstream-key-0"});
                let value = if upstream == Protocol::Responses && !http {
                    json!({"type":"response.failed", "response":{"error":policy}})
                } else {
                    json!({"type":"error", "error":policy})
                };
                let script = if http {
                    value.to_string()
                } else {
                    format!("data: {value}\n\n")
                };
                let f = Fixture::with_transport(
                    upstream,
                    vec![script, prefix(upstream) + completion(upstream)],
                    Ending::PerSlot(vec![Ending::Eof, Ending::Hold]),
                    Transport {
                        status: if http {
                            axum::http::StatusCode::TOO_MANY_REQUESTS
                        } else {
                            axum::http::StatusCode::OK
                        },
                        ..Default::default()
                    },
                    |config| configure_timeout_fallback(config, false),
                )
                .await;
                let body = f.request(client).await.text().await.unwrap();
                assert_completed(&body, client);
                assert!(!body.contains("PRIVATE PROMPT"), "{body}");
                assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
                assert!(
                    f.state
                        .key_rotation
                        .cooldown_snapshot(&f.state.config.current())
                        .is_empty()
                );
                let record = f.record().await;
                assert!(!record.to_string().contains("PRIVATE PROMPT"));
                let stats = f.log.lock().unwrap().snapshot();
                assert_eq!(stats.providers["mock"].keys["0"].errors, 1);
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn safety_policy_error_exhaustion_is_sanitized_400_even_with_quota_status() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let script = json!({"error":{"code":"bio_policy","message":"PRIVATE PROMPT secret-upstream-key-0"}}).to_string();
            let f = Fixture::with_transport(
                upstream,
                vec![script.clone(), script],
                Ending::Eof,
                Transport {
                    status: axum::http::StatusCode::TOO_MANY_REQUESTS,
                    ..Default::default()
                },
                |_| {},
            )
            .await;
            let response = f.request_model_raw(client, "mock/up-model").await;
            assert_eq!(response.status().as_u16(), 400);
            let body: Value = response.json().await.unwrap();
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("safety filter")
            );
            assert!(!body.to_string().contains("PRIVATE PROMPT"));
            if client != Protocol::Gemini {
                assert_eq!(body["error"]["type"], "invalid_request_error");
            }
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
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
async fn safety_nonstream_refusals_use_configured_fallback_and_reset_usage() {
    for grouped in [false, true] {
        for upstream in PROTOCOLS {
            for client in PROTOCOLS {
                let mut success: Value =
                    serde_json::from_str(&filtered_json(upstream, true)).unwrap();
                match upstream {
                    Protocol::Chat => success["choices"][0]["finish_reason"] = json!("stop"),
                    Protocol::Anthropic => success["stop_reason"] = json!("end_turn"),
                    Protocol::Responses => {
                        success["status"] = json!("completed");
                        success
                            .as_object_mut()
                            .unwrap()
                            .remove("incomplete_details");
                    }
                    Protocol::Gemini => success["candidates"][0]["finishReason"] = json!("STOP"),
                }
                let success = success.to_string();
                let f = Fixture::with_config(
                    upstream,
                    vec![filtered_json(upstream, false), success],
                    Ending::Eof,
                    |config| configure_timeout_fallback(config, grouped),
                )
                .await;
                let (path, body) = client_request(
                    client,
                    if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    },
                    false,
                );
                let response = reqwest::Client::new()
                    .post(format!("{}{path}", f.gateway.url))
                    .bearer_auth("secret-local-token")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 200);
                let body: Value = response.json().await.unwrap();
                assert!(body.to_string().contains("partial answer"));
                assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
                let record = f.record().await;
                assert_eq!(record["provider"], "spare", "{record}");
                assert_eq!(record["usage"]["input"], 7, "{record}");
                assert_eq!(record["usage"]["output"], 3, "{record}");
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn safety_gemini_prompt_feedback_is_terminal_in_both_modes() {
    for stream in [false, true] {
        for client in PROTOCOLS {
            let value = json!({"prompt_feedback":{"block_reason":"PROHIBITED_CONTENT"},"usage_metadata":{"prompt_token_count":7}});
            let script = if stream {
                format!("data: {value}\n\n")
            } else {
                value.to_string()
            };
            let f = Fixture::new(
                Protocol::Gemini,
                script,
                if stream { Ending::Hold } else { Ending::Eof },
            )
            .await;
            let (path, body) = client_request(client, "mock/up-model", stream);
            let response = reqwest::Client::new()
                .post(format!("{}{path}", f.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 400);
            assert!(response.text().await.unwrap().contains("safety filter"));
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["usage"]["input"], 7);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn safety_nonstream_missing_model_refusals_preserve_reported_usage() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let mut value: Value = serde_json::from_str(&filtered_json(upstream, false)).unwrap();
            value.as_object_mut().unwrap().remove("model");
            value.as_object_mut().unwrap().remove("modelVersion");
            let f = Fixture::new(upstream, value.to_string(), Ending::Eof).await;
            let (path, body) = client_request(client, "mock/up-model", false);
            let response = reqwest::Client::new()
                .post(format!("{}{path}", f.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 400);
            let record = f.record().await;
            assert_eq!(record["status"], "refusal", "{record}");
            assert_eq!(record["usage"]["input"], 7, "{record}");
            assert_eq!(record["usage"]["output"], 3, "{record}");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn safety_malformed_nonstream_success_is_a_502_not_a_fabricated_answer() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::new(
                upstream,
                json!({"candidates":[{"finishReason":"MAX_TOKENS"}]}).to_string(),
                Ending::Eof,
            )
            .await;
            let (path, body) = client_request(client, "mock/up-model", false);
            let response = reqwest::Client::new()
                .post(format!("{}{path}", f.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send()
                .await
                .unwrap();
            // Missing required model fields are malformed, not fabricated empty answers.
            // The length stop excludes Gemini empty-reply recovery.
            assert_eq!(response.status().as_u16(), 502);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn safety_unspecified_gemini_feedback_does_not_mask_refusal_or_empty_recovery() {
    for filtered in [false, true] {
        for client in PROTOCOLS {
            let value = json!({"modelVersion":"up-model", "promptFeedback":{"blockReason":"BLOCK_REASON_UNSPECIFIED"}, "candidates":[{"content":{"parts":[]},"finishReason":if filtered { "SAFETY" } else { "STOP" }}], "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}});
            let first = format!("data: {value}\n\n");
            let f = Fixture::new(
                Protocol::Gemini,
                first.clone(),
                Ending::PerRequest(
                    vec![
                        first,
                        prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                    ],
                    Box::new(Ending::Hold),
                ),
            )
            .await;
            let response = f.request_model_raw(client, "mock/up-model").await;
            assert_eq!(response.status().as_u16(), if filtered { 400 } else { 200 });
            let body = response.text().await.unwrap();
            if filtered {
                assert!(body.contains("safety filter"), "{body}");
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
            } else {
                assert_completed_usage(&body, client, 14, 6);
                assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
            }
            f.assert_released().await;
        }
    }
}

// The native Gemini image path must participate in the same routing, accounting
// and cancellation contracts as text; these are local fragmented HTTP fixtures.
fn gemini_image_frame(draft: bool, finish: Option<&str>) -> String {
    let mut candidate = json!({"content":{"parts":[{"thought":draft,
        "inlineData":{"mimeType":"image/png","data":if draft {"ZHJhZnQ="} else {"aGVsbG8="}},
        "thoughtSignature":"image-sig"}]}});
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    format!(
        "data: {}\n\n",
        json!({"modelVersion":"up-model", "candidates":[candidate],
        "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}})
    )
}

#[tokio::test]
async fn gemini_images_native_stream_and_json_preserve_final_images_not_drafts() {
    for stream in [false, true] {
        let draft = gemini_image_frame(true, None);
        let image = gemini_image_frame(false, Some("STOP"));
        let script =
            if stream {
                draft + &image
            } else {
                let mut value: Value =
                    serde_json::from_str(image.trim().strip_prefix("data: ").unwrap()).unwrap();
                value["candidates"][0]["content"]["parts"].as_array_mut().unwrap().insert(0,
                json!({"thought":true,"inlineData":{"mimeType":"image/png","data":"ZHJhZnQ="}}));
                value.to_string()
            };
        let f = Fixture::new(Protocol::Gemini, script, Ending::Eof).await;
        let (path, body) = client_request(Protocol::Gemini, "mock/up-model", stream);
        let response = reqwest::Client::new()
            .post(format!("{}{path}", f.gateway.url))
            .bearer_auth("secret-local-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = tokio::time::timeout(LIMIT, response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("aGVsbG8="), "{body}");
        assert!(body.contains("image-sig"), "{body}");
        assert!(!body.contains("ZHJhZnQ="), "{body}");
        if stream {
            assert_completed(&body, Protocol::Gemini);
        }
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        let record = f.record().await;
        assert_eq!(record["status"], "ok");
        assert_eq!(record["usage"]["input"], 7);
        assert_eq!(record["usage"]["output"], 3);
        assert!(!record.to_string().contains("aGVsbG8="));
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_image_satisfies_first_content_and_group_deadlines() {
    for grouped in [false, true] {
        let f = Fixture::with_config(
            Protocol::Gemini,
            vec![
                gemini_image_frame(false, None),
                prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            ],
            Ending::Paced(vec![(
                Duration::from_millis(650),
                completion(Protocol::Gemini).into(),
            )]),
            |config| {
                configure_timeout_fallback(config, grouped);
                config.stream_first_content_timeout_ms = 300;
                config.stream_idle_timeout_ms = 2_000;
                if grouped {
                    config.group[0].first_token_timeout_ms = 300;
                }
            },
        )
        .await;
        let response = f
            .request_model(
                Protocol::Gemini,
                if grouped {
                    "group/daily"
                } else {
                    "mock/up-model"
                },
            )
            .await;
        let body = tokio::time::timeout(LIMIT, response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("aGVsbG8="), "{body}");
        assert_completed(&body, Protocol::Gemini);
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_image_then_filter_never_switches_configured_routes() {
    for grouped in [false, true] {
        let f = Fixture::with_config(
            Protocol::Gemini,
            vec![
                gemini_image_frame(false, Some("SAFETY")),
                prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            ],
            Ending::Hold,
            |config| configure_timeout_fallback(config, grouped),
        )
        .await;
        let response = f
            .request_model(
                Protocol::Gemini,
                if grouped {
                    "group/daily"
                } else {
                    "mock/up-model"
                },
            )
            .await;
        let body = tokio::time::timeout(LIMIT, response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("aGVsbG8="), "{body}");
        assert_filtered(&body, Protocol::Gemini);
        assert!(!body.contains("upstream refused"));
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_image_before_reasoning_loop_is_never_reasked_or_replayed() {
    for client in PROTOCOLS {
        let mut script = gemini_image_frame(false, None);
        for _ in 0..300 {
            script += &loop_delta(
                Protocol::Gemini,
                true,
                "Now. Writing. Go. Let me write. OK. Here. Produce.\n",
            );
        }
        let f = Fixture::with_config(
            Protocol::Gemini,
            vec![
                script,
                prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            ],
            Ending::Hold,
            |config| configure_timeout_fallback(config, false),
        )
        .await;
        let response = f.request(client).await;
        let body = tokio::time::timeout(LIMIT, response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("stuck in a loop"), "{body}");
        if client == Protocol::Gemini {
            assert_eq!(body.matches("aGVsbG8=").count(), 1);
        }
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        let record = f.record().await;
        assert_eq!(record["status"], "upstream_stream_error");
        assert_eq!(record["usage"]["input"], 7);
        f.assert_released().await;
    }
}

#[tokio::test]
async fn gemini_image_client_disconnect_retains_usage_without_payload_in_history() {
    let f = Fixture::new(
        Protocol::Gemini,
        gemini_image_frame(false, None),
        Ending::Hold,
    )
    .await;
    let history = f.persistent_history();
    let mut socket = f
        .socket_request(Protocol::Gemini, "mock/up-model", true)
        .await;
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(LIMIT, async {
        let mut received = Vec::new();
        while !String::from_utf8_lossy(&received).contains("aGVsbG8=") {
            let mut buffer = [0; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0, "socket ended before its image");
            received.extend_from_slice(&buffer[..n]);
            assert!(received.len() < 256 * 1024);
        }
    })
    .await
    .unwrap();
    drop(socket);
    let record = f.record().await;
    assert_eq!(record["status"], "client_disconnected");
    assert_eq!(record["usage"]["input"], 7);
    assert_eq!(record["usage"]["output"], 3);
    let persisted = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
    assert!(!persisted.contains("aGVsbG8="));
    assert!(!persisted.contains("image-sig"));
    assert_eq!(*f.attempts.lock().unwrap(), [0]);
    f.assert_released().await;
}

// Public query/source metadata must survive the IR, not merely block retries.
const SEARCH_QUERY: &str = "private search fixture query";
const SEARCH_URL: &str = "https://example.invalid/private-search-source";
fn search_reply(upstream: Protocol) -> Value {
    match upstream {
        Protocol::Anthropic => json!({"model":"up-model","content":[
            {"type":"server_tool_use","id":"srv_fixture","name":"web_search","input":{"query":SEARCH_QUERY}},
            {"type":"web_search_tool_result","tool_use_id":"srv_fixture","content":[
                {"type":"web_search_result","url":SEARCH_URL,"title":"Fixture page","encrypted_content":"native-secret"}]}],
            "stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}),
        Protocol::Gemini => json!({"modelVersion":"up-model","candidates":[{"content":{"parts":[]},
            "groundingMetadata":{"webSearchQueries":[SEARCH_QUERY],"groundingChunks":[{"web":{"uri":SEARCH_URL,"title":"Fixture page"}}]},
            "finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}),
        Protocol::Responses => {
            json!({"model":"up-model","status":"completed","output":[{"type":"web_search_call","id":"ws_search_fixture","status":"completed",
            "action":{"type":"search","query":SEARCH_QUERY,"sources":[{"type":"url","url":SEARCH_URL}]}}],
            "usage":{"input_tokens":7,"output_tokens":3}})
        }
        _ => unreachable!(),
    }
}
fn search_frame(upstream: Protocol) -> String {
    let mut reply = search_reply(upstream);
    match upstream {
        Protocol::Anthropic => {
            let mut wire = format!(
                "data: {}\n\n",
                json!({"type":"message_start","message":{"model":"up-model","usage":{"input_tokens":7,"output_tokens":3}}})
            );
            for (index, block) in reply["content"].as_array().unwrap().iter().enumerate() {
                wire += &format!(
                    "data: {}\n\ndata: {}\n\n",
                    json!({"type":"content_block_start","index":index,"content_block":block}),
                    json!({"type":"content_block_stop","index":index})
                );
            }
            wire
        }
        Protocol::Gemini => {
            reply["candidates"][0]
                .as_object_mut()
                .unwrap()
                .remove("finishReason");
            format!("data: {reply}\n\n")
        }
        Protocol::Responses => format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.created","response":{"model":"up-model","usage":{"input_tokens":7,"output_tokens":3}}}),
            json!({"type":"response.output_item.done","output_index":0,"item":reply["output"][0]})
        ),
        _ => unreachable!(),
    }
}
fn assert_search_output(body: &str, client: Protocol) {
    assert!(body.contains(SEARCH_URL), "{client:?}: {body}");
    // Anthropic query deltas are JSON-escaped inside their SSE JSON envelope.
    assert!(body.contains(SEARCH_QUERY), "{body}");
    let marker = match client {
        Protocol::Anthropic => "web_search_tool_result",
        Protocol::Responses => "web_search_call",
        Protocol::Gemini => "groundingMetadata",
        _ => unreachable!(),
    };
    assert!(body.contains(marker), "{body}");
}
#[tokio::test]
async fn web_search_stream_and_json_reach_supported_clients_and_keep_usage() {
    for upstream in [Protocol::Gemini, Protocol::Responses] {
        for client in [Protocol::Anthropic, Protocol::Responses, Protocol::Gemini] {
            for stream in [false, true] {
                let script = if stream {
                    search_frame(upstream) + completion(upstream)
                } else {
                    search_reply(upstream).to_string()
                };
                let f = Fixture::new(upstream, script, Ending::Eof).await;
                let (path, body) = client_request(client, "mock/up-model", stream);
                let response = reqwest::Client::new()
                    .post(format!("{}{path}", f.gateway.url))
                    .bearer_auth("secret-local-token")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                let body = tokio::time::timeout(LIMIT, response.text())
                    .await
                    .unwrap()
                    .unwrap();
                assert_search_output(&body, client);
                if stream {
                    assert_completed_usage(&body, client, 7, 3);
                }
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
                let record = f.record().await;
                assert_eq!(record["status"], "ok");
                assert_eq!(record["usage"]["input"], 7);
                assert_eq!(record["usage"]["output"], 3);
                assert!(!record.to_string().contains(SEARCH_QUERY));
                assert!(!record.to_string().contains(SEARCH_URL));
                f.assert_released().await;
            }
        }
    }
}
#[tokio::test]
async fn web_search_satisfies_first_content_and_group_deadlines() {
    for upstream in [Protocol::Gemini, Protocol::Responses] {
        for grouped in [false, true] {
            let f = Fixture::with_config(
                upstream,
                vec![
                    search_frame(upstream),
                    prefix(upstream) + completion(upstream),
                ],
                Ending::Paced(vec![(
                    Duration::from_millis(650),
                    completion(upstream).into(),
                )]),
                |config| {
                    configure_timeout_fallback(config, grouped);
                    config.stream_first_content_timeout_ms = 300;
                    config.stream_idle_timeout_ms = 2_000;
                    if grouped {
                        config.group[0].first_token_timeout_ms = 300;
                    }
                },
            )
            .await;
            let response = f
                .request_model(
                    Protocol::Anthropic,
                    if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    },
                )
                .await;
            let body = tokio::time::timeout(LIMIT, response.text())
                .await
                .unwrap()
                .unwrap();
            assert_search_output(&body, Protocol::Anthropic);
            assert_completed_usage(&body, Protocol::Anthropic, 7, 3);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}
#[tokio::test]
async fn web_search_before_reasoning_loop_never_reasks_or_replays() {
    for upstream in [Protocol::Gemini, Protocol::Responses] {
        for client in PROTOCOLS {
            let mut script = search_frame(upstream);
            for _ in 0..300 {
                script += &loop_delta(
                    upstream,
                    true,
                    "Now. Writing. Go. Let me write. OK. Here. Produce.\n",
                );
            }
            let f = Fixture::with_config(
                upstream,
                vec![script, prefix(upstream) + completion(upstream)],
                Ending::Hold,
                |config| configure_timeout_fallback(config, false),
            )
            .await;
            let response = f.request(client).await;
            let body = tokio::time::timeout(LIMIT, response.text())
                .await
                .unwrap()
                .unwrap();
            if client != Protocol::Chat {
                assert_search_output(&body, client);
            }
            assert!(body.contains("stuck in a loop"), "{body}");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "upstream_stream_error");
            f.assert_released().await;
        }
    }
}
#[tokio::test]
async fn web_search_then_refusal_does_not_switch_configured_routes() {
    for upstream in [Protocol::Gemini, Protocol::Responses] {
        for grouped in [false, true] {
            let mut finish = search_reply(upstream);
            let filter = match upstream {
                Protocol::Gemini => {
                    finish["candidates"][0]["finishReason"] = json!("SAFETY");
                    format!("data: {finish}\n\n")
                }
                Protocol::Responses => format!(
                    "data: {}\n\n",
                    json!({"type":"response.incomplete","response":{"status":"incomplete",
                    "incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":7,"output_tokens":3}}})
                ),
                _ => unreachable!(),
            };
            let script = if upstream == Protocol::Gemini {
                filter
            } else {
                search_frame(upstream) + &filter
            };
            let f = Fixture::with_config(
                upstream,
                vec![script, prefix(upstream) + completion(upstream)],
                Ending::Hold,
                |config| configure_timeout_fallback(config, grouped),
            )
            .await;
            let response = f
                .request_model(
                    Protocol::Anthropic,
                    if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    },
                )
                .await;
            let body = tokio::time::timeout(LIMIT, response.text())
                .await
                .unwrap()
                .unwrap();
            assert_search_output(&body, Protocol::Anthropic);
            assert_filtered(&body, Protocol::Anthropic);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}
#[tokio::test]
async fn web_search_socket_disconnect_retains_usage_without_query_or_sources_in_history() {
    for upstream in [Protocol::Gemini, Protocol::Responses] {
        let f = Fixture::new(upstream, search_frame(upstream), Ending::Hold).await;
        let history = f.persistent_history();
        let mut socket = f
            .socket_request(Protocol::Anthropic, "mock/up-model", true)
            .await;
        use tokio::io::AsyncReadExt;
        tokio::time::timeout(LIMIT, async {
            let mut received = Vec::new();
            while !String::from_utf8_lossy(&received).contains(SEARCH_URL) {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0, "socket ended before its search results");
                received.extend_from_slice(&buffer[..n]);
                assert!(received.len() < 256 * 1024);
            }
        })
        .await
        .unwrap();
        drop(socket);
        let record = f.record().await;
        assert_eq!(record["status"], "client_disconnected");
        assert_eq!(record["usage"]["input"], 7);
        assert_eq!(record["usage"]["output"], 3);
        let persisted = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
        assert!(!persisted.contains(SEARCH_QUERY));
        assert!(!persisted.contains(SEARCH_URL));
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        f.assert_released().await;
    }
}

#[path = "support/search_requests.rs"]
mod search_requests;

#[path = "support/anthropic_search.rs"]
mod anthropic_search;

#[path = "support/gemini_grounding.rs"]
mod gemini_grounding;
