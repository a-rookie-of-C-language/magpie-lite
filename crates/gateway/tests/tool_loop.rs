//! Cross-turn tool-loop protection over real HTTP requests in all four
//! client dialects, with a counting local Chat vendor.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, build_router,
    usage::RequestLog,
};
use serde_json::{Value, json};

const TOKEN: &str = "magpie-lite";
const KEY: &str = "test-provider-key";
const MODEL: &str = "mock/mock-model";

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
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { base, task }
}

#[derive(Clone)]
struct Vendor {
    asked: Arc<AtomicUsize>,
}

async fn counting_vendor(State(vendor): State<Vendor>, body: Bytes) -> Response {
    vendor.asked.fetch_add(1, Ordering::SeqCst);
    let request: Value = serde_json::from_slice(&body).expect("vendor body is JSON");
    if request["stream"].as_bool().unwrap_or(false) {
        let wire = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"upstream answer\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n"
        );
        return ([("content-type", "text/event-stream")], wire).into_response();
    }
    Json(json!({
        "id": "c1",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "upstream answer"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
    }))
    .into_response()
}

struct Fixture {
    gateway: Server,
    _vendor: Server,
    asked: Arc<AtomicUsize>,
    requests: Arc<Mutex<RequestLog>>,
    client: reqwest::Client,
}

async fn fixture(stream_loop_guard: bool) -> Fixture {
    let asked = Arc::new(AtomicUsize::new(0));
    let vendor = serve(
        Router::new()
            .route("/v1/chat/completions", post(counting_vendor))
            .with_state(Vendor {
                asked: asked.clone(),
            }),
    )
    .await;
    let config: Config = serde_json::from_value(json!({
        "token": TOKEN,
        "default_provider": "mock",
        "stream_loop_guard": stream_loop_guard,
        "provider": [{
            "id": "mock",
            "name": "Mock",
            "protocol": "chat",
            "base_url": format!("{}/v1", vendor.base),
            "api_key": KEY,
            "models": ["mock-model"]
        }]
    }))
    .unwrap();
    let requests = Arc::new(Mutex::new(RequestLog::default()));
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: requests.clone(),
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let gateway = serve(build_router(state)).await;
    Fixture {
        gateway,
        _vendor: vendor,
        asked,
        requests,
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap(),
    }
}

#[derive(Clone, Copy)]
struct Ask {
    name: &'static str,
    protocol: Protocol,
    path: &'static str,
    stream: bool,
    done: &'static str,
}

fn asks() -> [Ask; 8] {
    [
        Ask {
            name: "responses stream",
            protocol: Protocol::Responses,
            path: "/v1/responses",
            stream: true,
            done: r#""type":"response.completed""#,
        },
        Ask {
            name: "responses whole",
            protocol: Protocol::Responses,
            path: "/v1/responses",
            stream: false,
            done: r#""status":"completed""#,
        },
        Ask {
            name: "chat stream",
            protocol: Protocol::Chat,
            path: "/v1/chat/completions",
            stream: true,
            done: "data: [DONE]",
        },
        Ask {
            name: "chat whole",
            protocol: Protocol::Chat,
            path: "/v1/chat/completions",
            stream: false,
            done: r#""finish_reason":"stop""#,
        },
        Ask {
            name: "anthropic stream",
            protocol: Protocol::Anthropic,
            path: "/v1/messages",
            stream: true,
            done: r#""type":"message_stop""#,
        },
        Ask {
            name: "anthropic whole",
            protocol: Protocol::Anthropic,
            path: "/v1/messages",
            stream: false,
            done: r#""stop_reason":"end_turn""#,
        },
        Ask {
            name: "gemini stream",
            protocol: Protocol::Gemini,
            path: "/v1beta/models/mock/mock-model:streamGenerateContent?alt=sse",
            stream: true,
            done: r#""finishReason":"STOP""#,
        },
        Ask {
            name: "gemini whole",
            protocol: Protocol::Gemini,
            path: "/v1beta/models/mock/mock-model:generateContent",
            stream: false,
            done: r#""finishReason":"STOP""#,
        },
    ]
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Chat => "chat",
        Protocol::Anthropic => "anthropic",
        Protocol::Responses => "responses",
        Protocol::Gemini => "gemini",
    }
}

fn codex_read(i: usize) -> (Value, String) {
    (
        json!({
            "cmd": "sed -n 1,200p src/parser.rs",
            "workdir": "/Users/audie/code/app"
        }),
        format!(
            "Chunk ID: 0x{:06x}\nWall time: 0.0{}1 seconds\nOutput:\npub fn parse(s: &str) -> Ast {{ todo!() }}",
            0xa1b2c3 + i,
            i % 9
        ),
    )
}

fn poll_job(i: usize) -> (Value, String) {
    (
        json!({"cmd": "gh run view 123 --json status"}),
        format!(r#"{{"status":"in_progress","steps_done":{i}}}"#),
    )
}

fn tail(protocol: Protocol) -> Vec<Value> {
    match protocol {
        Protocol::Responses => vec![
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "magpie stopped this turn"}]
            }),
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "read it once more, then fix it"}]
            }),
        ],
        Protocol::Chat => vec![
            json!({"role": "assistant", "content": "magpie stopped this turn"}),
            json!({"role": "user", "content": "read it once more, then fix it"}),
        ],
        Protocol::Anthropic => vec![
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "magpie stopped this turn"}]
            }),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "read it once more, then fix it"}]
            }),
        ],
        Protocol::Gemini => vec![
            json!({
                "role": "model",
                "parts": [{"text": "magpie stopped this turn"}]
            }),
            json!({
                "role": "user",
                "parts": [{"text": "read it once more, then fix it"}]
            }),
        ],
    }
}

fn tool_turns<F>(protocol: Protocol, n: usize, turn: F, stream: bool, tail: &[Value]) -> Value
where
    F: Fn(usize) -> (Value, String),
{
    match protocol {
        Protocol::Responses => {
            let mut input = vec![json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "why does parse panic?"}]
            })];
            for i in 0..n {
                let (args, result) = turn(i);
                input.push(json!({
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": format!("Reading parser {i}")}]
                }));
                input.push(json!({
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": args.to_string(),
                    "call_id": format!("call_{i}")
                }));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": format!("call_{i}"),
                    "output": result
                }));
            }
            input.extend(tail.iter().cloned());
            json!({
                "model": MODEL,
                "stream": stream,
                "store": false,
                "instructions": "You are Codex.",
                "tools": [{
                    "type": "function",
                    "name": "exec_command",
                    "parameters": {
                        "type": "object",
                        "properties": {"cmd": {"type": "string"}}
                    }
                }],
                "input": input
            })
        }
        Protocol::Chat => {
            let mut messages = vec![json!({
                "role": "user",
                "content": "why does parse panic?"
            })];
            for i in 0..n {
                let (args, result) = turn(i);
                messages.push(json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [{
                        "id": format!("call_{i}"),
                        "type": "function",
                        "function": {"name": "exec_command", "arguments": args.to_string()}
                    }]
                }));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": format!("call_{i}"),
                    "content": result
                }));
            }
            messages.extend(tail.iter().cloned());
            json!({
                "model": MODEL,
                "stream": stream,
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "exec_command",
                        "parameters": {"type": "object"}
                    }
                }],
                "messages": messages
            })
        }
        Protocol::Anthropic => {
            let mut messages = vec![json!({
                "role": "user",
                "content": "why does parse panic?"
            })];
            for i in 0..n {
                let (args, result) = turn(i);
                messages.push(json!({
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me look again."},
                        {
                            "type": "tool_use",
                            "id": format!("toolu_{i}"),
                            "name": "Bash",
                            "input": args
                        }
                    ]
                }));
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": format!("toolu_{i}"),
                        "content": result
                    }]
                }));
            }
            messages.extend(tail.iter().cloned());
            json!({
                "model": MODEL,
                "max_tokens": 8000,
                "stream": stream,
                "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
                "messages": messages
            })
        }
        Protocol::Gemini => {
            let mut contents = vec![json!({
                "role": "user",
                "parts": [{"text": "why does parse panic?"}]
            })];
            for i in 0..n {
                let (args, result) = turn(i);
                contents.push(json!({
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "id": format!("call_{i}"),
                            "name": "run_shell_command",
                            "args": args
                        }
                    }]
                }));
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "id": format!("call_{i}"),
                            "name": "run_shell_command",
                            "response": {"output": result}
                        }
                    }]
                }));
            }
            contents.extend(tail.iter().cloned());
            json!({
                "tools": [{
                    "functionDeclarations": [{
                        "name": "run_shell_command",
                        "parameters": {"type": "object"}
                    }]
                }],
                "contents": contents
            })
        }
    }
}

async fn send(fixture: &Fixture, ask: Ask, body: &Value) -> (StatusCode, String) {
    let response = fixture
        .client
        .post(format!("{}{}", fixture.gateway.base, ask.path))
        .bearer_auth(TOKEN)
        .json(body)
        .send()
        .await
        .unwrap_or_else(|error| panic!("{} request failed: {error}", ask.name));
    let status = response.status();
    let wire = response.text().await.unwrap();
    (status, wire)
}

fn assert_rejected_usage(fixture: &Fixture, ask: Ask) {
    let snapshot = fixture.requests.lock().unwrap().snapshot();
    let record = snapshot
        .recent_requests
        .last()
        .unwrap_or_else(|| panic!("{}: no usage record", ask.name));
    assert_eq!(record.provider, "magpie", "{}", ask.name);
    assert!(record.rejected, "{}: {record:?}", ask.name);
    assert_ne!(record.status, "ok", "{}", ask.name);
    assert!(
        record.error.contains("tool loop"),
        "{}: {record:?}",
        ask.name
    );
    assert_eq!(
        record.client_protocol,
        protocol_name(ask.protocol),
        "{}",
        ask.name
    );
    assert_eq!(record.requested_model, MODEL, "{}", ask.name);
    assert_eq!(record.stream, ask.stream, "{}", ask.name);
}

#[tokio::test]
async fn loop_is_answered_locally_in_every_protocol_and_mode() {
    let fixture = fixture(true).await;
    for ask in asks() {
        let before = fixture.asked.load(Ordering::SeqCst);
        let body = tool_turns(
            ask.protocol,
            60 + 8,
            |i| {
                if i < 60 { poll_job(i) } else { codex_read(i) }
            },
            ask.stream,
            &[],
        );
        let (status, wire) = send(&fixture, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{}: {wire}", ask.name);
        assert!(
            wire.contains("magpie stopped this turn"),
            "{}: {wire}",
            ask.name
        );
        assert!(wire.contains(ask.done), "{}: {wire}", ask.name);
        assert!(
            wire.contains("sed -n 1,200p src/parser.rs"),
            "{}: {wire}",
            ask.name
        );
        assert_eq!(
            fixture.asked.load(Ordering::SeqCst),
            before,
            "{} reached the vendor",
            ask.name
        );
        assert_rejected_usage(&fixture, ask);
    }
}

#[tokio::test]
async fn only_a_long_loop_with_the_guard_on_is_stopped_locally() {
    let guarded = fixture(true).await;
    for ask in asks() {
        let before = guarded.asked.load(Ordering::SeqCst);
        let body = tool_turns(ask.protocol, 7, codex_read, ask.stream, &[]);
        let (status, wire) = send(&guarded, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{} short: {wire}", ask.name);
        assert!(
            !wire.contains("magpie stopped this turn"),
            "{} short: {wire}",
            ask.name
        );
        assert_eq!(
            guarded.asked.load(Ordering::SeqCst),
            before + 1,
            "{} short did not reach the vendor",
            ask.name
        );

        let before = guarded.asked.load(Ordering::SeqCst);
        let body = tool_turns(ask.protocol, 40, codex_read, ask.stream, &[]);
        let (status, wire) = send(&guarded, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{} long: {wire}", ask.name);
        assert!(
            wire.contains("magpie stopped this turn"),
            "{} long: {wire}",
            ask.name
        );
        assert_eq!(
            guarded.asked.load(Ordering::SeqCst),
            before,
            "{} long reached the vendor",
            ask.name
        );
    }

    let unguarded = fixture(false).await;
    for ask in asks() {
        let before = unguarded.asked.load(Ordering::SeqCst);
        let body = tool_turns(ask.protocol, 40, codex_read, ask.stream, &[]);
        let (status, wire) = send(&unguarded, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{} off: {wire}", ask.name);
        assert!(
            !wire.contains("magpie stopped this turn"),
            "{} off: {wire}",
            ask.name
        );
        assert_eq!(
            unguarded.asked.load(Ordering::SeqCst),
            before + 1,
            "{} off did not reach the vendor",
            ask.name
        );
    }
}

#[tokio::test]
async fn progress_and_a_user_message_after_the_notice_go_on() {
    let fixture = fixture(true).await;
    for ask in asks() {
        let before = fixture.asked.load(Ordering::SeqCst);
        let body = tool_turns(ask.protocol, 200, poll_job, ask.stream, &[]);
        let (status, wire) = send(&fixture, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{} poll: {wire}", ask.name);
        assert!(
            !wire.contains("magpie stopped this turn"),
            "{} poll: {wire}",
            ask.name
        );
        assert_eq!(
            fixture.asked.load(Ordering::SeqCst),
            before + 1,
            "{} poll did not reach the vendor",
            ask.name
        );

        let before = fixture.asked.load(Ordering::SeqCst);
        let body = tool_turns(ask.protocol, 8, codex_read, ask.stream, &tail(ask.protocol));
        let (status, wire) = send(&fixture, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{} tail: {wire}", ask.name);
        assert!(
            !wire.contains("magpie stopped this turn before"),
            "{} tail: {wire}",
            ask.name
        );
        assert_eq!(
            fixture.asked.load(Ordering::SeqCst),
            before + 1,
            "{} tail did not reach the vendor",
            ask.name
        );
    }
}

#[tokio::test]
async fn repeated_loop_requests_are_both_rejected_without_a_vendor_send() {
    let fixture = fixture(true).await;
    let ask = Ask {
        name: "chat stream",
        protocol: Protocol::Chat,
        path: "/v1/chat/completions",
        stream: true,
        done: "data: [DONE]",
    };
    let body = tool_turns(Protocol::Chat, 8, codex_read, true, &[]);
    for _ in 0..2 {
        let (status, wire) = send(&fixture, ask, &body).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert!(wire.contains("magpie stopped this turn"), "{wire}");
        assert!(wire.contains(ask.done), "{wire}");
    }
    assert_eq!(fixture.asked.load(Ordering::SeqCst), 0);
    let snapshot = fixture.requests.lock().unwrap().snapshot();
    assert_eq!(snapshot.recent_requests.len(), 2);
    assert!(
        snapshot
            .recent_requests
            .iter()
            .all(|record| record.rejected && record.status == "rejected")
    );
}
