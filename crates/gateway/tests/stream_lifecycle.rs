//! Real HTTP fixtures for stream termination and cancellation, not live-vendor tests.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
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

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
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
}

struct Fixture {
    upstream: Server,
    gateway: Server,
    dropped: Arc<AtomicBool>,
    log: Arc<Mutex<RequestLog>>,
}
impl Fixture {
    async fn new(protocol: Protocol, script: String, ending: Ending) -> Self {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let upstream =
            serve(Router::new().route(
                "/{*path}",
                post(move || {
                    let script = script.clone();
                    let ending = ending.clone();
                    let guard = Dropped(flag.clone());
                    async move {
                        Response::builder().header("content-type", "text/event-stream")
                    .body(Body::from_stream(async_stream::stream! {
                        let _guard = guard;
                        // Deliberately cut inside JSON/SSE frame boundaries.
                        for bytes in script.as_bytes().chunks(19) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(bytes));
                        }
                        match ending {
                            Ending::Eof => {},
                            Ending::Hold => std::future::pending::<()>().await,
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
            api_keys: vec![],
            models: vec!["up-model".into()],
            fallback: vec![],
            key_cooldown_ms: None,
            capabilities: Default::default(),
            key_generation: 0,
        };
        let config = Config {
            token: "secret-local-token".into(),
            default_provider: "mock".into(),
            provider: vec![provider],
            ..Config::default()
        };
        let log = Arc::new(Mutex::new(RequestLog::default()));
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(config, None),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: log.clone(),
            key_rotation: KeyRotator::default(),
        });
        let gateway = serve(build_router(state)).await;
        Self {
            upstream,
            gateway,
            dropped,
            log,
        }
    }
    async fn request(&self, protocol: Protocol) -> reqwest::Response {
        let model = "mock/up-model";
        let (path, body) = match protocol {
            Protocol::Chat => (
                "/v1/chat/completions",
                json!({"model": model, "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hello"}]}),
            ),
            Protocol::Anthropic => (
                "/v1/messages",
                json!({"model": model, "stream": true, "max_tokens": 32, "messages": [{"role": "user", "content": "hello"}]}),
            ),
            Protocol::Responses => (
                "/v1/responses",
                json!({"model": model, "stream": true, "input": "hello"}),
            ),
            Protocol::Gemini => (
                "/v1beta/models/mock/up-model:streamGenerateContent",
                json!({"contents": [{"role": "user", "parts": [{"text": "hello"}]}]}),
            ),
        };
        let response = tokio::time::timeout(
            LIMIT,
            reqwest::Client::new()
                .post(format!("{}{path}", self.gateway.url))
                .bearer_auth("secret-local-token")
                .json(&body)
                .send(),
        )
        .await
        .expect("gateway headers timed out")
        .unwrap();
        assert!(response.status().is_success());
        response
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
            while !self.dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("upstream response still live at {}", self.upstream.url));
    }
}

fn assert_completed(body: &str, protocol: Protocol) {
    let mut parser = magpie_core::stream::SseParser::new();
    parser.extend(body.as_bytes());
    let mut frames = Vec::<Value>::new();
    let mut done = 0;
    while let Some(data) = parser.next_data() {
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
