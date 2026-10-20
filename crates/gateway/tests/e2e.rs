//! End-to-end: an agent talks Anthropic (or Chat) to the gateway, the
//! gateway talks Chat to a mock upstream, and the translation survives
//! the round trip — non-streaming and streaming, tool calls included.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use magpie_gateway::{
    AppState, Config, GroupConfig, Protocol, ProviderConfig, RoutingPolicy, build_router,
    init_retired_store, model_is_retired,
};
use serde_json::{Value, json};

#[derive(Clone)]
struct MockUpstream {
    last_request: Arc<Mutex<Option<Value>>>,
}

#[derive(Clone)]
struct MockGeminiUpstream {
    last_request: Arc<Mutex<Option<Value>>>,
}

async fn mock_chat_completions(
    State(mock): State<MockUpstream>,
    body: axum::body::Bytes,
) -> Response {
    let v: Value = serde_json::from_slice(&body).expect("upstream body is JSON");
    *mock.last_request.lock().unwrap() = Some(v.clone());
    let stream = v.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if !stream {
        return Json(json!({
            "id": "chatcmpl-mock", "object": "chat.completion", "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Beijing is sunny.",
                    "tool_calls": [{
                        "id": "call_mock", "type": "function",
                        "function": {"name": "print", "arguments": "{\"text\": \"sunny\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 31, "completion_tokens": 12, "total_tokens": 43}
        }))
        .into_response();
    }
    // A streamed chat reply: text, a tool call, finish, usage, [DONE].
    let body = std::sync::Arc::new(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n\
         data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Rain\"},\"finish_reason\":null}]}\n\n\
         data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"soon\"},\"finish_reason\":null}]}\n\n\
         data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
         data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
         data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4,\"total_tokens\":12}}\n\n\
         data: [DONE]\n\n"
            .to_owned(),
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(async_stream::stream! {
            let body = body.clone();
            for line in body.split_inclusive("\n\n") {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(line.to_owned()));
            }
        }))
        .unwrap()
}

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

async fn mock_gemini_generate(
    State(mock): State<MockGeminiUpstream>,
    Path(model): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let v: Value = serde_json::from_slice(&body).expect("Gemini body is JSON");
    *mock.last_request.lock().unwrap() = Some(json!({
        "path": model,
        "api_key": headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        "body": v,
    }));

    if model.ends_with(":streamGenerateContent") {
        let body = std::sync::Arc::new(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello \"}]},\"index\":0}]}\n\n\
             data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"id\":\"call_g\",\"name\":\"bash\",\"args\":{\"cmd\":\"pwd\"}}}]},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2,\"totalTokenCount\":6}}\n\n\
             data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2,\"totalTokenCount\":6}}\n\n"
                .to_owned(),
        );
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                let body = body.clone();
                for line in body.split_inclusive("\n\n") {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(line.to_owned()));
                }
            }))
            .unwrap();
    }

    Json(json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {"text": "Gemini says hello."},
                    {"functionCall": {"id": "call_g", "name": "bash", "args": {"cmd": "pwd"}}}
                ]
            },
            "finishReason": "STOP",
            "index": 0
        }],
        "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 5, "totalTokenCount": 14},
        "modelVersion": "mock-model",
        "responseId": "gemini-mock"
    }))
    .into_response()
}

async fn spawn_gemini_mock() -> (String, Arc<Mutex<Option<Value>>>) {
    let last_request = Arc::new(Mutex::new(None));
    let mock = MockGeminiUpstream {
        last_request: last_request.clone(),
    };
    let app = Router::new()
        .route("/v1beta/models/{*model}", post(mock_gemini_generate))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), last_request)
}

async fn spawn_mock() -> (String, Arc<Mutex<Option<Value>>>) {
    let last_request = Arc::new(Mutex::new(None));
    let mock = MockUpstream {
        last_request: last_request.clone(),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat_completions))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), last_request)
}

/// A vendor that ignores `stream: true` and always answers with one whole
/// JSON body.
async fn spawn_whole_reply_mock() -> (String, Arc<Mutex<Option<Value>>>) {
    let last_request = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/v1/chat/completions", post(whole_chat_completions))
        .with_state(MockUpstream {
            last_request: last_request.clone(),
        });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), last_request)
}

async fn whole_chat_completions(
    State(mock): State<MockUpstream>,
    body: axum::body::Bytes,
) -> Response {
    let v: Value = serde_json::from_slice(&body).expect("upstream body is JSON");
    *mock.last_request.lock().unwrap() = Some(v);
    Json(json!({
        "id": "chatcmpl-whole", "object": "chat.completion", "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Whole reply.",
                "tool_calls": [{
                    "id": "call_whole", "type": "function",
                    "function": {"name": "print", "arguments": "{\"text\": \"whole\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 21, "completion_tokens": 9, "total_tokens": 30}
    }))
    .into_response()
}

async fn spawn_gateway(upstream_base: String) -> String {
    spawn_gateway_with_protocol(upstream_base, Protocol::Chat).await
}

fn init_retired_for_test(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("magpie-lite-retired-e2e-{name}"));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("retired.json");
    let _ = std::fs::remove_file(&path);
    init_retired_store(path.clone());
    path
}

fn request_log(config: &Config) -> Arc<Mutex<magpie_gateway::usage::RequestLog>> {
    match config.history_path.as_ref() {
        Some(path) => Arc::new(Mutex::new(
            magpie_gateway::usage::RequestLog::open(path).unwrap(),
        )),
        None => Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
    }
}

async fn spawn_gateway_with_protocol(upstream_base: String, protocol: Protocol) -> String {
    spawn_gateway_with_protocol_and_history(upstream_base, protocol, None).await
}

async fn spawn_gateway_with_history(
    upstream_base: String,
    history_path: std::path::PathBuf,
) -> String {
    spawn_gateway_with_protocol_and_history(upstream_base, Protocol::Chat, Some(history_path)).await
}

async fn spawn_gateway_with_protocol_and_history(
    upstream_base: String,
    protocol: Protocol,
    history_path: Option<std::path::PathBuf>,
) -> String {
    spawn_gateway_with_providers_and_groups(
        vec![(upstream_base, protocol)],
        Vec::new(),
        history_path,
    )
    .await
}

/// A gateway over the given upstreams, all named `a`, `b`, ... in order,
/// with `mock-model` on each, and the given routes in a group `g`.
async fn spawn_gateway_with_providers_and_groups(
    upstreams: Vec<(String, Protocol)>,
    group_routes: Vec<String>,
    history_path: Option<std::path::PathBuf>,
) -> String {
    let mut provider = Vec::new();
    for (i, (upstream_base, protocol)) in upstreams.into_iter().enumerate() {
        // The single-upstream tests name their provider `mock`; a
        // multi-upstream one is `a`, `b`, ... as its group routes say.
        let id = if i == 0 {
            "mock".to_string()
        } else {
            ((b'a' + i as u8) as char).to_string()
        };
        provider.push(ProviderConfig {
            id: id.clone(),
            name: id.to_uppercase(),
            protocol,
            base_url: if protocol == Protocol::Anthropic {
                upstream_base.clone()
            } else {
                format!(
                    "{upstream_base}/{}",
                    if protocol == Protocol::Gemini {
                        "v1beta"
                    } else {
                        "v1"
                    }
                )
            },
            api_key: "test-key".into(),
            api_keys: Vec::new(),
            models: vec!["mock-model".into()],
            contexts: Default::default(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
            key_limits: Vec::new(),
        });
    }
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path,
        default_provider: provider.first().map(|p| p.id.clone()).unwrap_or_default(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider,
        group: if group_routes.is_empty() {
            Vec::new()
        } else {
            vec![GroupConfig {
                id: "g".into(),
                name: "G".into(),
                routes: group_routes,
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
            }]
        },
    };
    let requests = request_log(&config);
    let state = Arc::new(AppState {
        agents: None,
        config: magpie_gateway::ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests,
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn stream_request_over_whole_chat_upstream_reply_replays_as_stream() {
    // A vendor that ignores `stream: true` and answers with one whole JSON
    // body must not hand its JSON to an agent already reading SSE: the
    // gateway replays the reply as the client dialect's stream.
    let (upstream, last) = spawn_whole_reply_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    // The upstream really did see stream:true and answer whole anyway.
    let upstream_req = last.lock().unwrap().clone().expect("upstream was called");
    assert_eq!(upstream_req["stream"], true);

    let text = resp.text().await.unwrap();
    let mut saw_message_start = false;
    let mut visible = String::new();
    let mut args = String::new();
    let mut stop_reason = String::new();
    let mut output_tokens: Option<u64> = None;
    let mut saw_message_stop = false;
    for frame in text.split("\n\n") {
        let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => saw_message_start = true,
            "content_block_delta" => match v
                .pointer("/delta/type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text_delta" => visible.push_str(
                    v.pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                "input_json_delta" => args.push_str(
                    v.pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                _ => {}
            },
            "message_delta" => {
                stop_reason = v
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                output_tokens = v.pointer("/usage/output_tokens").and_then(Value::as_u64);
            }
            "message_stop" => saw_message_stop = true,
            _ => {}
        }
    }
    assert!(saw_message_start);
    assert_eq!(visible, "Whole reply.");
    assert_eq!(args, r#"{"text":"whole"}"#);
    assert_eq!(stop_reason, "tool_use");
    assert_eq!(output_tokens, Some(9));
    assert!(saw_message_stop);
}

#[tokio::test]
async fn anthropic_client_nonstream_over_chat_upstream() {
    let (upstream, last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "max_tokens": 100,
            "system": "Be terse.",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_prev", "name": "forecast", "input": {"city": "Beijing"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_prev", "content": "5-day forecast"}
                ]},
                {"role": "user", "content": "Summary?"}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();

    // The reply came back in the Anthropic shape.
    assert_eq!(v["type"], "message");
    assert_eq!(v["stop_reason"], "tool_use");
    let content = v["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "Beijing is sunny.");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["name"], "print");
    assert_eq!(content[1]["input"]["text"], "sunny");
    assert_eq!(v["usage"]["input_tokens"], 31);
    assert_eq!(v["usage"]["output_tokens"], 12);

    // The upstream saw the Chat shape, tool result and all.
    let upstream_req = last.lock().unwrap().clone().expect("upstream was called");
    let msgs = upstream_req["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "forecast");
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "call_prev");
    assert_eq!(msgs[2]["content"], "5-day forecast");
    assert_eq!(msgs[3]["role"], "user");
    assert_eq!(upstream_req["model"], "mock-model");
}

#[tokio::test]
async fn anthropic_client_stream_over_chat_upstream() {
    let (upstream, _) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let text = resp.text().await.unwrap();
    let mut saw_message_start = false;
    let mut visible = String::new();
    let mut args = String::new();
    let mut stop_reason = String::new();
    let mut output_tokens: Option<u64> = None;
    let mut saw_message_stop = false;
    for frame in text.split("\n\n") {
        let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) else {
            continue;
        };
        let v: Value = serde_json::from_str(data).expect("frame data is JSON");
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => saw_message_start = true,
            "content_block_delta" => match v
                .pointer("/delta/type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text_delta" => visible.push_str(
                    v.pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                "input_json_delta" => args.push_str(
                    v.pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                _ => {}
            },
            "message_delta" => {
                stop_reason = v
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                output_tokens = v.pointer("/usage/output_tokens").and_then(Value::as_u64);
            }
            "message_stop" => saw_message_stop = true,
            _ => {}
        }
    }
    assert!(saw_message_start);
    assert_eq!(visible, "Rainsoon");
    assert_eq!(args, r#"{"cmd":"ls"}"#);
    assert_eq!(stop_reason, "tool_use");
    assert_eq!(output_tokens, Some(4));
    assert!(saw_message_stop);
}

#[tokio::test]
async fn chat_client_nonstream_passthrough_shape() {
    let (upstream, _) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "Beijing is sunny.");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn a_chat_clients_max_completion_tokens_reaches_an_openai_chat_upstream() {
    // The client asks OpenAI's newer way; the gateway's Chat encoder
    // writes the reply limit as max_tokens, which every Chat vendor reads.
    let (upstream, last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}],
            "max_completion_tokens": 900
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = last.lock().unwrap().clone().expect("upstream request");
    assert_eq!(sent["max_tokens"], 900);
    assert!(sent.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn a_bare_trace_id_400_hands_a_group_request_to_the_next_member() {
    // Command Code's bare 400 names nothing about the request: the key is
    // fine, and the next member may take the same turn (#2313).
    let (first, first_calls) = spawn_scripted(vec![(
        400,
        json!({"message":"invalid request error trace_id: 251c3c29761c17fdc459480ed9c68932",
            "type":"invalid_request_error"}),
    )])
    .await;
    let (second, second_calls) = spawn_scripted(Vec::new()).await;
    let gw = spawn_gateway_with_providers_and_groups(
        vec![(first, Protocol::Chat), (second, Protocol::Chat)],
        vec!["mock/mock-model".into(), "b/mock-model".into()],
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model":"group/g",
            "messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "hello"
    );
    assert_eq!(*first_calls.lock().unwrap(), 1);
    assert_eq!(*second_calls.lock().unwrap(), 1);
}

/// Command Code's Zero Data Retention refusal — the error code
/// `cmd_zdr_no_providers` on a 422 — is the request's setting, not the
/// model or the key failing: the next group member is asked, and the
/// refusing provider's other keys are not (the reference's `zdrRefused`,
/// c8639043 / plugins#246).
#[tokio::test]
async fn a_zdr_refusal_goes_to_the_next_member_and_rests_nothing() {
    let (first, first_calls) = spawn_scripted(vec![(
        422,
        json!({"error":{"message":"This model has no zero-data-retention upstream. Remove the x-cmd-zdr header or choose a different model.","type":"invalid_request_error","code":"cmd_zdr_no_providers"}}),
    )])
    .await;
    let (second, second_calls) = spawn_scripted(Vec::new()).await;
    let gw = spawn_gateway_with_providers_and_groups(
        vec![(first, Protocol::Chat), (second, Protocol::Chat)],
        vec!["mock/mock-model".into(), "b/mock-model".into()],
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model":"group/g",
            "messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "hello"
    );
    assert_eq!(*first_calls.lock().unwrap(), 1);
    assert_eq!(*second_calls.lock().unwrap(), 1);
}

/// A plain 422 that names nothing known is not route-retryable: it stays
/// with its route and reaches the agent as its own error.
#[tokio::test]
async fn a_422_that_says_nothing_known_stays_with_its_route() {
    let (first, first_calls) = spawn_scripted(vec![(
        422,
        json!({"error":{"message":"The request could not be processed.","type":"invalid_request_error"}}),
    )])
    .await;
    let (second, second_calls) = spawn_scripted(Vec::new()).await;
    let gw = spawn_gateway_with_providers_and_groups(
        vec![(first, Protocol::Chat), (second, Protocol::Chat)],
        vec!["mock/mock-model".into(), "b/mock-model".into()],
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model":"group/g",
            "messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(*first_calls.lock().unwrap(), 1);
    assert_eq!(*second_calls.lock().unwrap(), 0);
}

/// The Ollama-shaped surface: a client that takes a custom endpoint only
/// as an Ollama server (VS/VS Code Copilot BYOK) asks /api/version, lists
/// /api/tags and shows each model (the reference's `ollama.go`, 94df2392).
#[tokio::test]
async fn the_gateway_answers_as_an_ollama_server() {
    let (upstream, _last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let version: Value = client
        .get(format!("{gw}/api/version"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(version["version"], "0.12.6");

    let tags: Value = client
        .get(format!("{gw}/api/tags"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models = tags["models"].as_array().unwrap();
    assert!(
        models.iter().any(|m| m["model"] == "mock/mock-model"),
        "{tags}"
    );

    let show = client
        .post(format!("{gw}/api/show"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model": "mock/mock-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(show.status(), StatusCode::OK);
    let shown: Value = show.json().await.unwrap();
    assert_eq!(shown["model_info"]["general.architecture"], "magpie");
    assert_eq!(shown["model_info"]["magpie.context_length"], 32768);
    assert!(
        shown["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("completion"))
    );

    let missing = client
        .post(format!("{gw}/api/show"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model": "mock/no-such-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_chat_tool_in_another_apis_shape_reaches_the_upstream_named() {
    // A Chat client may send an MCP or Responses tool by mistake. It must
    // reach a strict Chat upstream as a function tool with its name, not
    // as a tool with none (#2622).
    let (upstream, last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({"model":"mock/mock-model",
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"read_file","description":"Read",
                "inputSchema":{"type":"object"}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = last.lock().unwrap().clone().expect("upstream request");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
    assert_eq!(sent["tools"][0]["function"]["parameters"]["type"], "object");
}

async fn rate_limited_chat() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": {"message": "primary is rate limited", "type": "rate_limit_error"}})),
    )
        .into_response()
}

async fn spawn_rate_limited() -> String {
    let app = Router::new().route("/v1/chat/completions", post(rate_limited_chat));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn retired_model_chat() -> Response {
    (
        StatusCode::GONE,
        Json(json!({
            "error": {
                "type": "ModelDeprecated",
                "message": "Model exo-free has been deprecated."
            }
        })),
    )
        .into_response()
}

async fn spawn_retired_model() -> String {
    let app = Router::new().route("/v1/chat/completions", post(retired_model_chat));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A scripted upstream: the Nth request gets the Nth scripted reply, or
/// a plain OK chat answer once the script runs out. It counts requests.
#[derive(Clone)]
struct ScriptedUpstream {
    replies: Arc<Mutex<Vec<(u16, Value)>>>,
    requests: Arc<Mutex<usize>>,
}

async fn scripted_chat_completions(
    State(mock): State<ScriptedUpstream>,
    body: axum::body::Bytes,
) -> Response {
    let _ = serde_json::from_slice::<Value>(&body).expect("upstream body is JSON");
    let count = {
        let mut requests = mock.requests.lock().unwrap();
        let count = *requests;
        *requests += 1;
        count
    };
    // The last scripted reply repeats once the script runs out; no
    // script at all, an OK chat answer.
    let replies = mock.replies.lock().unwrap();
    let index = count.min(replies.len().saturating_sub(1));
    if let Some((status, reply)) = replies.get(index).cloned() {
        drop(replies);
        let status = StatusCode::from_u16(status).unwrap();
        return (status, Json(reply)).into_response();
    }
    drop(replies);
    Json(json!({
        "id": "chatcmpl-scripted", "object": "chat.completion", "model": "m",
        "choices": [{"index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

async fn spawn_scripted(replies: Vec<(u16, Value)>) -> (String, Arc<Mutex<usize>>) {
    let mock = ScriptedUpstream {
        replies: Arc::new(Mutex::new(replies)),
        requests: Arc::new(Mutex::new(0)),
    };
    let requests = mock.requests.clone();
    let app = Router::new()
        .route("/v1/chat/completions", post(scripted_chat_completions))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), requests)
}

/// The scripted upstream over Anthropic's path: the Nth request gets the
/// Nth scripted reply, or a plain OK Anthropic message once the script
/// runs out. It counts requests.
#[derive(Clone)]
struct ScriptedAnthropicUpstream {
    replies: Arc<Mutex<Vec<(u16, Value)>>>,
    requests: Arc<Mutex<usize>>,
}

async fn scripted_anthropic_messages(
    State(mock): State<ScriptedAnthropicUpstream>,
    body: axum::body::Bytes,
) -> Response {
    let _ = serde_json::from_slice::<Value>(&body).expect("upstream body is JSON");
    let count = {
        let mut requests = mock.requests.lock().unwrap();
        let count = *requests;
        *requests += 1;
        count
    };
    let replies = mock.replies.lock().unwrap();
    let index = count.min(replies.len().saturating_sub(1));
    if let Some((status, reply)) = replies.get(index).cloned() {
        drop(replies);
        let status = StatusCode::from_u16(status).unwrap();
        return (status, Json(reply)).into_response();
    }
    drop(replies);
    Json(json!({
        "id": "msg-scripted", "type": "message", "role": "assistant",
        "model": "m",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .into_response()
}

async fn spawn_scripted_anthropic(replies: Vec<(u16, Value)>) -> (String, Arc<Mutex<usize>>) {
    let mock = ScriptedAnthropicUpstream {
        replies: Arc::new(Mutex::new(replies)),
        requests: Arc::new(Mutex::new(0)),
    };
    let requests = mock.requests.clone();
    let app = Router::new()
        .route("/v1/messages", post(scripted_anthropic_messages))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), requests)
}

/// A scripted upstream whose Nth reply may be an SSE error event sent
/// after 200 headers — a mid-reply failure. It counts requests.
#[derive(Clone)]
struct StreamScriptedUpstream {
    replies: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<usize>>,
}

async fn stream_scripted_chat_completions(
    State(mock): State<StreamScriptedUpstream>,
    body: axum::body::Bytes,
) -> Response {
    let _ = serde_json::from_slice::<Value>(&body).expect("upstream body is JSON");
    let count = {
        let mut requests = mock.requests.lock().unwrap();
        let count = *requests;
        *requests += 1;
        count
    };
    let error = {
        let replies = mock.replies.lock().unwrap();
        replies.get(count).cloned()
    };
    let Some(error) = error else {
        return Json(json!({
            "id": "chatcmpl-stream-scripted", "object": "chat.completion", "model": "m",
            "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
        .into_response();
    };
    let frame = format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({"error": error})).unwrap()
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(frame))
        .unwrap()
}

async fn spawn_stream_scripted(replies: Vec<Value>) -> (String, Arc<Mutex<usize>>) {
    let mock = StreamScriptedUpstream {
        replies: Arc::new(Mutex::new(replies)),
        requests: Arc::new(Mutex::new(0)),
    };
    let requests = mock.requests.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(stream_scripted_chat_completions),
        )
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), requests)
}

/// A member whose API can't read the request's shape hands it to the
/// next one, asked once, and doesn't rest for it: the next request asks
/// it first again (#350). Nobody left, the agent gets its error.
#[tokio::test]
async fn group_fails_over_a_request_shape() {
    let grok_shape_422 = json!({
        "error": "Failed to deserialize the JSON body into the target type: input[0]: unknown item type \"additional_tools\"; expected one of: message, reasoning, function_call"
    });
    let (a, a_requests) = spawn_scripted(vec![(422, grok_shape_422)]).await;
    let (b, b_requests) = spawn_scripted(vec![]).await;
    let gw = spawn_gateway_with_group(a.clone(), b.clone(), RoutingPolicy::Order).await;
    let client = reqwest::Client::new();

    for expected_a in 1..=2 {
        let resp = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "group/daily-coding",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        assert_eq!(*a_requests.lock().unwrap(), expected_a);
        assert_eq!(*b_requests.lock().unwrap(), expected_a);
    }

    // Alone, the shape error is the agent's, and a retry asks it again:
    // the shape failure never rests the member.
    let solo = spawn_gateway_with_group(a, b, RoutingPolicy::Order).await;
    // Replace b's base with a's by building a one-member config: reuse
    // the fallback helper's shape through a provider fallback chain.
    let resp = client
        .post(format!("{solo}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.text().await.unwrap().contains("unknown item type"));
    assert_eq!(*a_requests.lock().unwrap(), 3);

    // The next request to the group asks a first again.
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(*a_requests.lock().unwrap(), 4);
}

/// A vendor that refuses the conversation outright ("unapproved channel")
/// hands it to the next route at any status — including a 400, which
/// otherwise stays with the route — and doesn't rest for it: the next
/// request asks it first again. Nobody left, the agent gets its error.
#[tokio::test]
async fn prompt_refusal_falls_over_and_rests_nobody() {
    let refusal = json!({
        "code": 11128,
        "msg": "Illegal API invocation from an unapproved channel"
    });
    let (a, a_requests) = spawn_scripted(vec![(400, refusal)]).await;
    let (b, b_requests) = spawn_scripted(vec![]).await;
    let gw = spawn_gateway_with_group(a.clone(), b.clone(), RoutingPolicy::Order).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    assert_eq!(*a_requests.lock().unwrap(), 1);
    assert_eq!(*b_requests.lock().unwrap(), 1);

    // The refusal never rests the member: the next request asks it first.
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(*a_requests.lock().unwrap(), 2);
    assert_eq!(*b_requests.lock().unwrap(), 2);

    // Alone (no fallback), the refusal is the agent's, at its original
    // status.
    let solo = spawn_gateway_with_group(a, b, RoutingPolicy::Order).await;
    let resp = client
        .post(format!("{solo}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Illegal API invocation from an unapproved channel")
    );
    assert_eq!(*a_requests.lock().unwrap(), 3);
}

/// A vendor whose response-protection layer is unavailable answers 4xx/5xx
/// — including a 502 — no matter what is asked. That is not a busy outage:
/// the next route is tried, the refusal doesn't cool the key down, and
/// the asking member doesn't rest.
#[tokio::test]
async fn protection_refusal_falls_over_at_any_status() {
    let refusal = json!({
        "error": {
            "message": "response protection is unavailable",
            "type": "server_error"
        }
    });
    let (a, a_requests) = spawn_scripted(vec![(502, refusal)]).await;
    let (b, b_requests) = spawn_scripted(vec![]).await;
    let gw = spawn_gateway_with_group(a.clone(), b.clone(), RoutingPolicy::Order).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    assert_eq!(*a_requests.lock().unwrap(), 1);
    assert_eq!(*b_requests.lock().unwrap(), 1);

    // One provider's refusal doesn't cool its key: the next request asks
    // the same route first again and falls over once more.
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(*a_requests.lock().unwrap(), 2);
    assert_eq!(*b_requests.lock().unwrap(), 2);

    // Solo (no fallback), the 502 reaches the agent with the vendor's
    // wording.
    let solo = spawn_gateway_with_group(a, b, RoutingPolicy::Order).await;
    let resp = client
        .post(format!("{solo}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("response protection is unavailable")
    );
    assert_eq!(*a_requests.lock().unwrap(), 3);
}

#[tokio::test]
async fn mid_reply_refusal_falls_over_after_headers_commit() {
    // The route answers 200 and starts its stream, then its error event
    // says the conversation is refused — a mid-reply failure carries the
    // vendor's raw error body, so the refusal wording is still seen and
    // the next route is asked, without resting the refusing one.
    let refusal = json!({
        "message": "Illegal API invocation from an unapproved channel",
        "type": "invalid_request_error"
    });
    let (a, a_requests) = spawn_stream_scripted(vec![refusal]).await;
    let (b, b_requests) = spawn_scripted(vec![]).await;
    let gw = spawn_gateway_with_group(a.clone(), b.clone(), RoutingPolicy::Order).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert!(text.contains("hello"), "{text}");
    assert_eq!(*a_requests.lock().unwrap(), 1);
    assert_eq!(*b_requests.lock().unwrap(), 1);

    // The refusal never rests the member: the next request asks it first.
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert!(text.contains("hello"), "{text}");
    assert_eq!(*a_requests.lock().unwrap(), 2);
    // The script is spent, so a answers for itself this time.
    assert_eq!(*b_requests.lock().unwrap(), 1);
}

#[tokio::test]
async fn context_overflow_stays_with_its_route() {
    // A conversation too long for the model is too long for every
    // member: the overflow refusal goes back to the agent with the
    // vendor's wording and status, the backup is not asked, and the
    // primary is not retired or cooled down.
    let overflow = json!({
        "error": {
            "message": "This model's maximum context length is 131072 tokens. However, you requested 207000 tokens.",
            "type": "invalid_request_error",
            "code": "context_length_exceeded"
        }
    });
    let (a, a_requests) = spawn_scripted(vec![(400, overflow)]).await;
    let (b, b_requests) = spawn_scripted(vec![]).await;
    let gw = spawn_gateway_with_group(a.clone(), b.clone(), RoutingPolicy::Order).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("maximum context length")
    );
    assert_eq!(*a_requests.lock().unwrap(), 1);
    assert_eq!(*b_requests.lock().unwrap(), 0);

    // The overflow never rests the member: the next request asks it first.
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(*a_requests.lock().unwrap(), 2);
    assert_eq!(*b_requests.lock().unwrap(), 0);

    // A worded model refusal at the same status is different: worth the
    // next member.
    let refused = json!({
        "error": {"message": "model mini is not supported in the current token plan"}
    });
    let (c, c_requests) = spawn_scripted(vec![(400, refused)]).await;
    let (d, d_requests) = spawn_scripted(vec![]).await;
    let gw = spawn_gateway_with_group(c, d, RoutingPolicy::Order).await;
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(*c_requests.lock().unwrap(), 1);
    assert_eq!(*d_requests.lock().unwrap(), 1);
}

#[tokio::test]
async fn retired_model_falls_back_to_another_provider() {
    let primary = spawn_retired_model().await;
    let (backup, last) = spawn_mock().await;
    let gw = spawn_gateway_with_fallback_named(primary, backup, "gone", "spare").await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "gone/mock-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"Rain\"") && text.contains("[DONE]"));
    assert!(last.lock().unwrap().is_some(), "backup was called");
}

#[tokio::test]
async fn retired_model_leaves_the_catalog_until_it_answers() {
    let path = init_retired_for_test("catalog");
    let primary = spawn_retired_model().await;
    let (backup, _) = spawn_mock().await;
    let gw =
        spawn_gateway_with_fallback_named(primary, backup, "catalog-gone", "catalog-spare").await;
    let client = reqwest::Client::new();

    // Before anything: mock-model is listed, and nothing is retired.
    let models: Value = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "catalog-gone/mock-model")
    );
    assert!(!model_is_retired(
        "catalog-gone",
        "mock-model",
        std::time::SystemTime::now()
    ));

    // The vendor answers 410 ModelDeprecated; the fallback still answers.
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "catalog-gone/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    drop(resp);
    assert!(model_is_retired(
        "catalog-gone",
        "mock-model",
        std::time::SystemTime::now()
    ));

    // The retired model leaves the catalog; the group entry stays.
    let models: Value = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "catalog-gone/mock-model")
    );

    // A vendor answer takes the retirement back, and the model is listed.
    let _ = std::fs::remove_file(&path);
    init_retired_store(path.clone());
    let models: Value = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "catalog-gone/mock-model")
    );
    let _ = std::fs::remove_file(&path);
}

async fn edge_blocked_chat() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        "<html><body><p>Sorry, your request has been blocked due to unusual activity.</p></body></html>",
    )
        .into_response()
}

async fn spawn_edge_blocked() -> String {
    let app = Router::new().route("/v1/chat/completions", post(edge_blocked_chat));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn edge_firewall_block_falls_back_and_says_what_happened() {
    let primary = spawn_edge_blocked().await;
    let (backup, last) = spawn_mock().await;
    let gw = spawn_gateway_with_fallback(primary.clone(), backup).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Beijing is sunny."
    );
    assert!(last.lock().unwrap().is_some(), "backup was called");

    // The same block with no fallback route reports the vendor's error in
    // plain words, not the block page.
    let gw = spawn_gateway(primary).await;
    let failed = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::METHOD_NOT_ALLOWED);
    let body: Value = failed.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("request has been blocked"), "{message}");
    assert!(message.contains("firewall blocked requests from this IP"));
}

#[derive(Clone)]
struct SameProviderModelMock {
    seen_models: Arc<Mutex<Vec<String>>>,
}

async fn same_provider_model_chat(
    State(mock): State<SameProviderModelMock>,
    body: axum::body::Bytes,
) -> Response {
    let request: Value = serde_json::from_slice(&body).expect("upstream body is JSON");
    let model = request["model"]
        .as_str()
        .expect("upstream model is a string")
        .to_owned();
    mock.seen_models.lock().unwrap().push(model.clone());

    if model == "model-a" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {"message": "model-a is temporarily unavailable"}
            })),
        )
            .into_response();
    }

    Json(json!({
        "id": "chatcmpl-same-provider",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "model-b worked."},
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

async fn spawn_same_provider_model_mock() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen_models = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(same_provider_model_chat))
        .with_state(SameProviderModelMock {
            seen_models: seen_models.clone(),
        });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen_models)
}

#[derive(Clone)]
struct KeyPoolMock {
    seen_keys: Arc<Mutex<Vec<String>>>,
    retry_after: Option<&'static str>,
    retry_after_body: Option<&'static str>,
}

async fn key_pool_chat_completions(
    State(mock): State<KeyPoolMock>,
    headers: HeaderMap,
) -> Response {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let key = authorization
        .strip_prefix("Bearer ")
        .unwrap_or_default()
        .to_owned();
    mock.seen_keys.lock().unwrap().push(key.clone());

    if key == "first-key" {
        let mut error = json!({"error": {"message": "first key is rate limited"}});
        if let Some(retry_after) = mock.retry_after_body {
            error["error"]["retryAfter"] = json!(retry_after);
        }
        let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response();
        if let Some(retry_after) = mock.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(retry_after).expect("static retry-after header"),
            );
        }
        return response;
    }

    Json(json!({
        "id": "chatcmpl-key-pool", "object": "chat.completion", "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "second key worked."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    }))
    .into_response()
}

async fn spawn_key_pool_mock_with_hints(
    retry_after: Option<&'static str>,
    retry_after_body: Option<&'static str>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen_keys = Arc::new(Mutex::new(Vec::new()));
    let mock = KeyPoolMock {
        seen_keys: seen_keys.clone(),
        retry_after,
        retry_after_body,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(key_pool_chat_completions))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen_keys)
}

async fn spawn_key_pool_mock_with_retry_after(
    retry_after: Option<&'static str>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    spawn_key_pool_mock_with_hints(retry_after, None).await
}

async fn spawn_key_pool_mock_with_retry_body(
    retry_after_body: Option<&'static str>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    spawn_key_pool_mock_with_hints(None, retry_after_body).await
}

async fn spawn_key_pool_mock() -> (String, Arc<Mutex<Vec<String>>>) {
    spawn_key_pool_mock_with_retry_after(None).await
}

#[derive(Clone)]
struct InternalErrorMock {
    seen_keys: Arc<Mutex<Vec<String>>>,
}

async fn internal_error_chat_completions(
    State(mock): State<InternalErrorMock>,
    headers: HeaderMap,
) -> Response {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let key = authorization
        .strip_prefix("Bearer ")
        .unwrap_or_default()
        .to_owned();
    mock.seen_keys.lock().unwrap().push(key);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": {"message": "provider is overloaded"}})),
    )
        .into_response()
}

async fn spawn_internal_error_mock() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen_keys = Arc::new(Mutex::new(Vec::new()));
    let mock = InternalErrorMock {
        seen_keys: seen_keys.clone(),
    };
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(internal_error_chat_completions),
        )
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen_keys)
}

/// A vendor that answers every request with the same 403 — `message` sets
/// the wording, so a quota-worded one is told apart from a plain one.
#[derive(Clone)]
struct ForbiddenMock {
    seen_keys: Arc<Mutex<Vec<String>>>,
    message: &'static str,
}

async fn forbidden_chat_completions(
    State(mock): State<ForbiddenMock>,
    headers: HeaderMap,
) -> Response {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let key = authorization
        .strip_prefix("Bearer ")
        .unwrap_or_default()
        .to_owned();
    mock.seen_keys.lock().unwrap().push(key);
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error": {"message": mock.message}})),
    )
        .into_response()
}

async fn spawn_forbidden_mock(message: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen_keys = Arc::new(Mutex::new(Vec::new()));
    let mock = ForbiddenMock {
        seen_keys: seen_keys.clone(),
        message,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(forbidden_chat_completions))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen_keys)
}

async fn spawn_gateway_with_key_pool(primary: String, backup: Option<String>) -> String {
    let mut fallback = Vec::new();
    let mut providers = vec![ProviderConfig {
        id: "primary".into(),
        name: "Primary".into(),
        protocol: Protocol::Chat,
        base_url: format!("{primary}/v1"),
        api_key: String::new(),
        api_keys: vec!["first-key".into(), "second-key".into()],
        models: vec!["mock-model".into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    }];
    if let Some(backup) = backup {
        fallback.push("backup/mock-model".into());
        providers.push(ProviderConfig {
            id: "backup".into(),
            name: "Backup".into(),
            protocol: Protocol::Chat,
            base_url: format!("{backup}/v1"),
            api_key: "backup-key".into(),
            api_keys: Vec::new(),
            models: vec!["mock-model".into()],
            contexts: Default::default(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
            key_limits: Vec::new(),
        });
    }
    providers[0].fallback = fallback;

    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path: None,
        default_provider: "primary".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: providers,
        group: Vec::new(),
    };
    let requests = request_log(&config);
    let state = Arc::new(AppState {
        agents: None,
        config: magpie_gateway::ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests,
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}
async fn spawn_gateway_with_fallback(primary: String, backup: String) -> String {
    spawn_gateway_with_fallback_named(primary, backup, "primary", "backup").await
}

async fn spawn_gateway_with_fallback_named(
    primary: String,
    backup: String,
    primary_id: &str,
    backup_id: &str,
) -> String {
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path: None,
        default_provider: "primary".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: vec![
            ProviderConfig {
                id: primary_id.into(),
                name: "Primary".into(),
                protocol: Protocol::Chat,
                base_url: format!("{primary}/v1"),
                api_key: "primary-key".into(),
                api_keys: Vec::new(),
                models: vec!["mock-model".into()],
                contexts: Default::default(),
                fallback: vec![format!("{backup_id}/mock-model")],
                key_cooldown_ms: None,
                capabilities: Default::default(),
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
                id: backup_id.into(),
                name: "Backup".into(),
                protocol: Protocol::Chat,
                base_url: format!("{backup}/v1"),
                api_key: "backup-key".into(),
                api_keys: Vec::new(),
                models: vec!["mock-model".into()],
                contexts: Default::default(),
                fallback: Vec::new(),
                key_cooldown_ms: None,
                capabilities: Default::default(),
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
        group: Vec::new(),
    };
    let requests = request_log(&config);
    let state = Arc::new(AppState {
        agents: None,
        config: magpie_gateway::ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests,
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn spawn_gateway_with_same_provider_group(upstream: String) -> String {
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path: None,
        default_provider: "mock".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Chat,
            base_url: format!("{upstream}/v1"),
            api_key: "mock-key".into(),
            api_keys: Vec::new(),
            models: vec!["model-a".into(), "model-b".into()],
            contexts: Default::default(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
            key_limits: Vec::new(),
        }],
        group: vec![GroupConfig {
            id: "same-provider".into(),
            name: "Same provider models".into(),
            routes: vec!["mock/model-a".into(), "mock/model-b".into()],
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
        }],
    };
    let requests = request_log(&config);
    let state = Arc::new(AppState {
        agents: None,
        config: magpie_gateway::ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests,
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}
async fn spawn_gateway_with_group(
    primary: String,
    backup: String,
    routing: RoutingPolicy,
) -> String {
    spawn_gateway_with_group_weights(primary, backup, routing, Vec::new()).await
}

async fn spawn_gateway_with_group_weights(
    primary: String,
    backup: String,
    routing: RoutingPolicy,
    weights: Vec<u32>,
) -> String {
    spawn_gateway_with_group_routes(
        primary,
        backup,
        routing,
        weights,
        vec!["primary/mock-model".into(), "backup/mock-model".into()],
    )
    .await
}

async fn spawn_gateway_with_group_routes(
    primary: String,
    backup: String,
    routing: RoutingPolicy,
    weights: Vec<u32>,
    routes: Vec<String>,
) -> String {
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        key_cooldown_ms: 60_000,
        stream_first_content_timeout_ms: Config::default().stream_first_content_timeout_ms,
        stream_idle_timeout_ms: Config::default().stream_idle_timeout_ms,
        stream_keepalive_interval_ms: Config::default().stream_keepalive_interval_ms,
        stream_keepalive_max_silence_ms: Config::default().stream_keepalive_max_silence_ms,
        stream_responses_hold_timeout_ms: Config::default().stream_responses_hold_timeout_ms,
        stream_loop_guard: Config::default().stream_loop_guard,
        history_path: None,
        default_provider: "primary".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        provider: vec![
            ProviderConfig {
                id: "primary".into(),
                name: "Primary".into(),
                protocol: Protocol::Chat,
                base_url: format!("{primary}/v1"),
                api_key: "primary-key".into(),
                api_keys: Vec::new(),
                models: vec!["mock-model".into()],
                contexts: Default::default(),
                fallback: Vec::new(),
                key_cooldown_ms: None,
                capabilities: Default::default(),
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
                id: "backup".into(),
                name: "Backup".into(),
                protocol: Protocol::Chat,
                base_url: format!("{backup}/v1"),
                api_key: "backup-key".into(),
                api_keys: Vec::new(),
                models: vec!["mock-model".into()],
                contexts: Default::default(),
                fallback: Vec::new(),
                key_cooldown_ms: None,
                capabilities: Default::default(),
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
            id: "daily-coding".into(),
            name: "Daily coding".into(),
            routes,
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights,
            routing,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
        }],
    };
    let requests = request_log(&config);
    let state = Arc::new(AppState {
        agents: None,
        config: magpie_gateway::ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests,
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn responses_client_nonstream_over_chat_upstream() {
    let (upstream, last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/responses"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "instructions": "Be terse.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Weather?"}
                ]},
                {"type": "function_call", "call_id": "call_prev", "name": "forecast", "arguments": "{\"city\":\"Beijing\"}"},
                {"type": "function_call_output", "call_id": "call_prev", "output": "5-day forecast"}
            ],
            "tools": [{
                "type": "function", "name": "forecast", "description": "forecast",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }],
            "max_output_tokens": 100
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();

    assert_eq!(v["object"], "response");
    assert_eq!(v["status"], "completed");
    let output = v["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "Beijing is sunny.");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "call_mock");
    assert_eq!(output[1]["arguments"], r#"{"text":"sunny"}"#);
    assert_eq!(v["usage"]["input_tokens"], 31);
    assert_eq!(v["usage"]["output_tokens"], 12);

    let upstream_req = last.lock().unwrap().clone().expect("upstream was called");
    assert_eq!(upstream_req["model"], "mock-model");
    let msgs = upstream_req["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "Be terse.");
    assert_eq!(msgs[1]["content"], "Weather?");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "forecast");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_prev");
}

#[tokio::test]
async fn responses_client_stream_over_chat_upstream() {
    let (upstream, _) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/responses"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "stream": true,
            "input": "Weather?"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();

    let mut events = Vec::new();
    for frame in text.split("\n\n") {
        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        events.push(serde_json::from_str::<Value>(data).expect("event JSON"));
    }

    assert_eq!(events[0]["type"], "response.created");
    // A real Chat opening now preserves the upstream model, as other dialects do.
    assert_eq!(events[0]["response"]["model"], "mock-model");
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "response.created")
            .count(),
        1
    );
    assert!(
        events.iter().any(|e| {
            e["type"] == "response.output_item.added" && e["item"]["type"] == "message"
        })
    );
    let visible: String = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect();
    assert_eq!(visible, "Rainsoon");
    let args: String = events
        .iter()
        .filter(|e| e["type"] == "response.function_call_arguments.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect();
    assert_eq!(args, r#"{"cmd":"ls"}"#);
    let completed = events
        .iter()
        .rev()
        .find(|e| e["type"] == "response.completed")
        .expect("terminal event");
    assert_eq!(completed["response"]["model"], "mock-model");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 8);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 4);
    assert!(
        completed["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call")
    );
}

#[tokio::test]
async fn retryable_primary_falls_back_before_streaming() {
    let primary = spawn_rate_limited().await;
    let (backup, last) = spawn_mock().await;
    let gw = spawn_gateway_with_fallback(primary, backup).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"Rain\"") && text.contains("\"soon\""));
    assert!(text.contains("[DONE]"));

    let upstream_req = last.lock().unwrap().clone().expect("backup was called");
    assert_eq!(upstream_req["model"], "mock-model");
    assert!(upstream_req["stream"].as_bool().unwrap());

    let stats = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    let stats: Value = stats.json().await.unwrap();
    assert_eq!(stats["total_requests"], 1);
    assert_eq!(stats["providers"]["primary"]["requests"], 0);
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["attempts"], 1);
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["errors"], 1);
    assert_eq!(stats["providers"]["backup"]["requests"], 1);
    assert_eq!(stats["recent_requests"][0]["provider"], "backup");
}

#[tokio::test]
async fn routing_group_retries_same_provider_with_a_different_model() {
    let (upstream, seen_models) = spawn_same_provider_model_mock().await;
    let gw = spawn_gateway_with_same_provider_group(upstream).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/same-provider",
            "messages": [{"role": "user", "content": "Try both models."}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "model-b worked.");
    assert_eq!(
        *seen_models.lock().unwrap(),
        vec!["model-a".to_owned(), "model-b".to_owned()]
    );
}
#[tokio::test]
async fn routing_groups_fall_back_in_configured_order() {
    let primary = spawn_rate_limited().await;
    let (backup, last) = spawn_mock().await;
    let gw = spawn_gateway_with_group(primary, backup, RoutingPolicy::Order).await;
    let client = reqwest::Client::new();

    let models = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    let models: Value = models.json().await.unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "group/daily-coding")
    );

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Beijing is sunny."
    );

    let upstream_req = last.lock().unwrap().clone().expect("backup was called");
    assert_eq!(upstream_req["model"], "mock-model");

    let stats = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    let stats: Value = stats.json().await.unwrap();
    assert_eq!(stats["providers"]["backup"]["requests"], 1);
    assert_eq!(
        stats["recent_requests"][0]["requested_model"],
        "group/daily-coding"
    );
}

#[tokio::test]
async fn routing_groups_rotate_the_starting_route() {
    let (primary, _) = spawn_mock().await;
    let (backup, _) = spawn_mock().await;
    let gw = spawn_gateway_with_group(primary, backup, RoutingPolicy::Rotate).await;
    let client = reqwest::Client::new();

    for _ in 0..3 {
        let response = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "group/daily-coding",
                "messages": [{"role": "user", "content": "rotate"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let stats: Value = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["providers"]["primary"]["requests"], 2);
    assert_eq!(stats["providers"]["backup"]["requests"], 1);
}

#[tokio::test]
async fn routing_groups_weight_the_starting_route_smoothly() {
    let (primary, _) = spawn_mock().await;
    let (backup, _) = spawn_mock().await;
    let gw = spawn_gateway_with_group_weights(primary, backup, RoutingPolicy::Weighted, vec![3, 1])
        .await;
    let client = reqwest::Client::new();

    for _ in 0..8 {
        let response = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "group/daily-coding",
                "messages": [{"role": "user", "content": "weight"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let stats: Value = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["providers"]["primary"]["requests"], 6);
    assert_eq!(stats["providers"]["backup"]["requests"], 2);
}

#[tokio::test]
async fn api_key_pools_retry_rate_limits_and_rotate() {
    let (upstream, seen_keys) = spawn_key_pool_mock().await;
    let gw = spawn_gateway_with_key_pool(upstream, None).await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value = first.json().await.unwrap();
    assert_eq!(
        first_body["choices"][0]["message"]["content"],
        "second key worked."
    );

    // The rate-limited key is retried only within the same request. Its 429
    // also starts the configured cooldown, so following requests avoid it.
    for expected_content in ["Again?", "Once more?"] {
        let later = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "primary/mock-model",
                "messages": [{"role": "user", "content": expected_content}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(later.status(), StatusCode::OK);
        drop(later.json::<Value>().await.unwrap());
    }

    assert_eq!(
        *seen_keys.lock().unwrap(),
        vec![
            "first-key".to_owned(),
            "second-key".to_owned(),
            "second-key".to_owned(),
            "second-key".to_owned()
        ]
    );

    let stats = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    let stats: Value = stats.json().await.unwrap();
    let recent = stats["recent_requests"].as_array().unwrap();
    assert_eq!(recent[0]["key_slot"], 1);
    assert_eq!(recent[1]["key_slot"], 1);
    assert_eq!(recent[2]["key_slot"], 1);
    assert_eq!(stats["providers"]["primary"]["requests"], 3);
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["attempts"], 1);
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["successes"], 0);
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["errors"], 1);
    assert_eq!(stats["providers"]["primary"]["keys"]["1"]["attempts"], 3);
    assert_eq!(stats["providers"]["primary"]["keys"]["1"]["successes"], 3);
    assert_eq!(stats["providers"]["primary"]["keys"]["1"]["errors"], 0);
    let cooldown = &stats["key_cooldowns"]["primary"]["0"];
    assert!(cooldown["remaining_ms"].as_u64().unwrap() > 0);
    assert!(cooldown["remaining_ms"].as_u64().unwrap() <= 60_000);
    assert!(cooldown["until_unix_ms"].as_u64().unwrap() > 0);
    let serialized = stats.to_string();
    assert!(!serialized.contains("first-key"));
    assert!(!serialized.contains("second-key"));
}

#[tokio::test]
async fn retry_after_headers_shorten_key_cooldowns() {
    let (upstream, seen_keys) = spawn_key_pool_mock_with_retry_after(Some("25ms")).await;
    let gw = spawn_gateway_with_key_pool(upstream, None).await;
    let client = reqwest::Client::new();

    for expected_content in ["Weather?", "Again?"] {
        let resp = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "primary/mock-model",
                "messages": [{"role": "user", "content": expected_content}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        drop(resp.json::<Value>().await.unwrap());
    }

    // The gateway default is 60s, but this upstream's Retry-After says 25ms.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let recovered = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Once more?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
    drop(recovered.json::<Value>().await.unwrap());

    assert_eq!(
        *seen_keys.lock().unwrap(),
        vec![
            "first-key".to_owned(),
            "second-key".to_owned(),
            "second-key".to_owned(),
            "first-key".to_owned(),
            "second-key".to_owned()
        ]
    );
}
#[tokio::test]
async fn body_retry_delays_shorten_key_cooldowns() {
    let (upstream, seen_keys) = spawn_key_pool_mock_with_retry_body(Some("25ms")).await;
    let gw = spawn_gateway_with_key_pool(upstream, None).await;
    let client = reqwest::Client::new();

    for expected_content in ["Weather?", "Again?"] {
        let resp = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "primary/mock-model",
                "messages": [{"role": "user", "content": expected_content}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        drop(resp.json::<Value>().await.unwrap());
    }

    // Gemini-style providers often put RetryInfo in the JSON error body.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let recovered = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Once more?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
    drop(recovered.json::<Value>().await.unwrap());

    assert_eq!(
        *seen_keys.lock().unwrap(),
        vec![
            "first-key".to_owned(),
            "second-key".to_owned(),
            "second-key".to_owned(),
            "first-key".to_owned(),
            "second-key".to_owned()
        ]
    );
}

#[tokio::test]
async fn provider_level_failure_skips_the_rest_of_the_key_pool() {
    let (primary, primary_keys) = spawn_internal_error_mock().await;
    let (backup, last_backup_request) = spawn_mock().await;
    let gw = spawn_gateway_with_key_pool(primary, Some(backup)).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Beijing is sunny."
    );

    // 500 describes the endpoint, not either credential, so only one key is
    // spent before the configured route fallback runs.
    assert_eq!(*primary_keys.lock().unwrap(), vec!["first-key".to_owned()]);
    assert!(last_backup_request.lock().unwrap().is_some());

    let stats = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    let stats: Value = stats.json().await.unwrap();
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["attempts"], 1);
    assert_eq!(stats["providers"]["primary"]["keys"]["0"]["errors"], 1);
    assert_eq!(stats["providers"]["backup"]["keys"]["0"]["attempts"], 1);
    assert_eq!(stats["providers"]["backup"]["keys"]["0"]["successes"], 1);
}

/// Two keys of one provider answering one request with the same plain 403
/// is the request refused, not each key: the pool stops after the second
/// alike answer, and nothing rests (the reference's `forbiddenAlike`,
/// d5bc9f00 / plugins#243).
#[tokio::test]
async fn two_mates_alike_plain_403_is_the_request_refused_not_each_key() {
    let (upstream, seen_keys) = spawn_forbidden_mock("Forbidden").await;
    let gw = spawn_gateway_with_key_pool(upstream, None).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The first key's 403 moved to the second; the second's alike 403 ended
    // it — no third key was spent on the same refusal.
    assert_eq!(
        *seen_keys.lock().unwrap(),
        vec!["first-key".to_owned(), "second-key".to_owned()]
    );
}

/// A 403 that says why — quota wording — is the key's own failure, not the
/// request's: the next key is still asked.
#[tokio::test]
async fn a_403_that_names_quota_still_moves_to_the_next_key() {
    let (upstream, seen_keys) =
        spawn_forbidden_mock("Your usage limit is reached for this billing cycle").await;
    let gw = spawn_gateway_with_key_pool(upstream, None).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "primary/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Both keys were asked: a quota-worded 403 rests each in turn.
    assert_eq!(
        *seen_keys.lock().unwrap(),
        vec!["first-key".to_owned(), "second-key".to_owned()]
    );
}

#[tokio::test]
async fn stats_record_upstream_failures() {
    let upstream = spawn_rate_limited().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let failed = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(failed.json::<Value>().await.unwrap());

    let stats = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    let stats: Value = stats.json().await.unwrap();
    assert_eq!(stats["total_requests"], 1);
    assert_eq!(stats["errors"], 1);
    assert_eq!(stats["providers"]["mock"]["errors"], 1);
    assert_eq!(stats["recent_requests"][0]["status"], "upstream_error");
}

#[tokio::test]
async fn gemini_client_nonstream_over_chat_upstream() {
    let (upstream, last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!(
            "{gw}/v1beta/models/mock/mock-model:generateContent"
        ))
        .header("x-goog-api-key", "magpie-lite")
        .json(&json!({
            "systemInstruction": {"parts": [{"text": "You are terse."}]},
            "contents": [{"role": "user", "parts": [{"text": "Weather in Beijing?"}]}],
            "generationConfig": {"maxOutputTokens": 100}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["candidates"][0]["content"]["parts"][0]["text"],
        "Beijing is sunny."
    );
    assert_eq!(
        v["candidates"][0]["content"]["parts"][1]["functionCall"]["name"],
        "print"
    );
    assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    assert_eq!(v["usageMetadata"]["promptTokenCount"], 31);
    assert_eq!(v["usageMetadata"]["candidatesTokenCount"], 12);

    let upstream_req = last.lock().unwrap().clone().expect("upstream was called");
    assert_eq!(upstream_req["model"], "mock-model");
    assert_eq!(upstream_req["stream"], false);
    let messages = upstream_req["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are terse.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Weather in Beijing?");
}

#[tokio::test]
async fn gemini_client_stream_over_chat_upstream() {
    let (upstream, _) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!(
            "{gw}/v1beta/models/mock/mock-model:streamGenerateContent?alt=sse"
        ))
        .header("x-goog-api-key", "magpie-lite")
        .json(&json!({
            "contents": [{"role": "user", "parts": [{"text": "Weather?"}]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let text = resp.text().await.unwrap();
    let mut visible = String::new();
    let mut args = String::new();
    let mut prompt_tokens = None;
    let mut finish_reason = String::new();
    for frame in text.split("\n\n") {
        let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) else {
            continue;
        };
        let v: Value = serde_json::from_str(data).expect("Gemini SSE data is JSON");
        for part in v
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                visible.push_str(t);
            }
            if let Some(call) = part.get("functionCall") {
                assert_eq!(call["name"], "bash");
                args = call["args"]["cmd"].as_str().unwrap_or_default().to_owned();
            }
        }
        if let Some(tokens) = v.pointer("/usageMetadata/promptTokenCount") {
            prompt_tokens = tokens.as_u64();
        }
        if let Some(reason) = v
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
        {
            finish_reason = reason.to_owned();
        }
    }
    assert_eq!(visible, "Rainsoon");
    assert_eq!(args, "ls");
    assert_eq!(prompt_tokens, Some(8));
    assert_eq!(finish_reason, "STOP");
}

#[tokio::test]
async fn chat_client_nonstream_over_gemini_upstream() {
    let (upstream, last) = spawn_gemini_mock().await;
    let gw = spawn_gateway_with_protocol(upstream, Protocol::Gemini).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Hello?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "bash"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(v["usage"]["prompt_tokens"], 9);
    assert_eq!(v["usage"]["completion_tokens"], 5);

    let upstream_req = last.lock().unwrap().clone().expect("Gemini was called");
    assert_eq!(upstream_req["path"], "mock-model:generateContent");
    assert_eq!(upstream_req["api_key"], "test-key");
    assert!(upstream_req["body"].get("model").is_none());
    assert_eq!(
        upstream_req["body"]["contents"][0]["parts"][0]["text"],
        "Hello?"
    );
}

#[tokio::test]
async fn chat_client_stream_over_gemini_upstream() {
    let (upstream, last) = spawn_gemini_mock().await;
    let gw = spawn_gateway_with_protocol(upstream, Protocol::Gemini).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "Hello?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    let mut visible = String::new();
    let mut args = String::new();
    let mut usage = None;
    for frame in text.split("\n\n") {
        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let v: Value = serde_json::from_str(data).expect("chat chunk");
        if let Some(chunk) = v
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            visible.push_str(chunk);
        }
        if let Some(arg) = v
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
        {
            args.push_str(arg);
        }
        if v.get("usage").is_some() {
            usage = Some(v["usage"].clone());
        }
    }
    assert_eq!(visible, "Hello ");
    assert_eq!(args, r#"{"cmd":"pwd"}"#);
    assert_eq!(usage.expect("usage")["prompt_tokens"], 4);

    let upstream_req = last.lock().unwrap().clone().expect("Gemini was called");
    assert_eq!(upstream_req["path"], "mock-model:streamGenerateContent");
    assert_eq!(upstream_req["api_key"], "test-key");
    assert!(upstream_req["body"].get("model").is_none());
}

#[tokio::test]
async fn stats_track_nonstream_and_stream_usage() {
    let (upstream, _) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let nonstream = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(nonstream.status(), StatusCode::OK);
    drop(nonstream.json::<Value>().await.unwrap());

    let stream = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    drop(stream.text().await.unwrap());

    let stats = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    assert_eq!(stats.status(), StatusCode::OK);
    let stats: Value = stats.json().await.unwrap();
    assert_eq!(stats["total_requests"], 2);
    assert_eq!(stats["errors"], 0);
    assert_eq!(stats["providers"]["mock"]["requests"], 2);
    assert_eq!(stats["providers"]["mock"]["errors"], 0);
    assert_eq!(stats["providers"]["mock"]["input_tokens"], 39);
    assert_eq!(stats["providers"]["mock"]["output_tokens"], 16);

    let recent_usage = &stats["recent_usage"];
    assert_eq!(recent_usage["totals"]["requests"], 2);
    assert_eq!(recent_usage["totals"]["errors"], 0);
    assert_eq!(recent_usage["totals"]["requests_with_usage"], 2);
    assert_eq!(recent_usage["totals"]["input_tokens"], 39);
    assert_eq!(recent_usage["totals"]["output_tokens"], 16);
    assert!(recent_usage["totals"]["average_latency_ms"].is_u64());
    assert_eq!(recent_usage["by_provider"]["mock"]["requests"], 2);
    assert_eq!(
        recent_usage["by_model"]["mock/mock-model"]["input_tokens"],
        39
    );

    let recent = stats["recent_requests"].as_array().unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0]["client_protocol"], "chat");
    assert_eq!(recent[0]["stream"], false);
    assert_eq!(recent[0]["status"], "ok");
    assert_eq!(recent[0]["usage"]["input"], 31);
    assert_eq!(recent[1]["stream"], true);
    assert_eq!(recent[1]["status"], "ok");
    assert_eq!(recent[1]["usage"]["input"], 8);
    assert_eq!(recent[1]["usage"]["output"], 4);
    assert!(recent[1]["latency_ms"].is_u64());
}

#[tokio::test]
async fn history_survives_restart_without_conversation_or_credentials() {
    let (upstream, _) = spawn_mock().await;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let history_path = std::env::temp_dir().join(format!(
        "magpie-lite-history-e2e-{}-{unique}.jsonl",
        std::process::id()
    ));
    let first_gateway = spawn_gateway_with_history(upstream.clone(), history_path.clone()).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{first_gateway}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response.json::<Value>().await.unwrap());

    let persisted = std::fs::read_to_string(&history_path).unwrap();
    assert!(persisted.contains(r#""key_slot":0"#));
    assert!(!persisted.contains("Weather?"));
    assert!(!persisted.contains("Beijing is sunny."));
    assert!(!persisted.contains("test-key"));
    assert!(!persisted.contains("magpie-lite"));

    let second_gateway = spawn_gateway_with_history(upstream, history_path.clone()).await;
    let unauthorized = client
        .get(format!("{second_gateway}/stats"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let stats = client
        .get(format!("{second_gateway}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    assert_eq!(stats.status(), StatusCode::OK);
    let stats: Value = stats.json().await.unwrap();
    assert_eq!(stats["total_requests"], 0);
    assert_eq!(stats["errors"], 0);
    assert!(stats["providers"].as_object().unwrap().is_empty());
    assert_eq!(stats["recent_requests"].as_array().unwrap().len(), 1);
    assert_eq!(stats["recent_usage"]["totals"]["requests"], 1);
    assert_eq!(stats["recent_usage"]["totals"]["input_tokens"], 31);
    assert_eq!(
        stats["recent_usage"]["by_provider"]["mock"]["output_tokens"],
        12
    );

    let response = client
        .post(format!("{second_gateway}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Again?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response.json::<Value>().await.unwrap());

    let stats = client
        .get(format!("{second_gateway}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(stats["total_requests"], 1);
    assert_eq!(stats["providers"]["mock"]["requests"], 1);
    assert_eq!(stats["recent_usage"]["totals"]["requests"], 2);
    assert_eq!(stats["recent_usage"]["totals"]["input_tokens"], 62);
    assert_eq!(
        stats["recent_usage"]["by_model"]["mock/mock-model"]["output_tokens"],
        24
    );
    let recent = stats["recent_requests"].as_array().unwrap();
    assert_eq!(recent.len(), 2);
    let first_started = recent[0]["started_unix_ms"].as_u64().unwrap();
    assert!(recent[1]["started_unix_ms"].as_u64().unwrap() >= first_started);

    let filtered = client
        .get(format!("{second_gateway}/stats?from=0&to=1"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered: Value = filtered.json().await.unwrap();
    assert_eq!(filtered["total_requests"], 1);
    assert!(filtered["recent_requests"].as_array().unwrap().is_empty());
    assert_eq!(filtered["recent_usage"]["totals"]["requests"], 0);

    let invalid_range = client
        .get(format!("{second_gateway}/stats?from=2&to=1"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_range.status(), StatusCode::BAD_REQUEST);

    let persisted = std::fs::read_to_string(&history_path).unwrap();
    assert!(!persisted.contains("Again?"));
    std::fs::remove_file(&history_path).ok();
}
#[tokio::test]
async fn console_is_public_while_protected_data_requires_the_token() {
    let (upstream, _last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let response = client.get(format!("{gw}/")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("magpie-lite console"));
    assert!(body.contains("Authorization"));
    assert!(body.contains("Cooldowns"));
    assert!(body.contains("Routing configuration"));
    assert!(body.contains("Routing groups"));
    assert!(body.contains("Coding agents"));
    assert!(body.contains("Recover interrupted operation"));
    assert!(body.contains("Confirm restoration"));
    assert!(body.contains("id=\"stats-from\""));
    assert!(body.contains("id=\"stats-to\""));
    assert!(body.contains("id=\"stats-apply\""));
    assert!(body.contains("authorized(statsPath())"));
    assert!(!body.contains("localStorage"), "the token is not persisted");
    assert!(!body.contains("test-key"), "upstream credentials stay out");

    let alias = client.get(format!("{gw}/ui")).send().await.unwrap();
    assert_eq!(alias.status(), StatusCode::OK);

    let config = client.get(format!("{gw}/config")).send().await.unwrap();
    assert_eq!(config.status(), StatusCode::UNAUTHORIZED);

    let config = client
        .get(format!("{gw}/config"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap();
    assert_eq!(config.status(), StatusCode::OK);
    let config: Value = config.json().await.unwrap();
    assert_eq!(config["default_provider"], "mock");
    assert_eq!(config["providers"][0]["id"], "mock");
    assert_eq!(config["providers"][0]["key_count"], 1);
    assert!(!config.to_string().contains("test-key"));
    let stats = client.get(format!("{gw}/stats")).send().await.unwrap();
    assert_eq!(stats.status(), StatusCode::UNAUTHORIZED);
}
#[tokio::test]
async fn model_routes_require_the_exact_local_token() {
    let (upstream, last) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let missing = client
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let body: Value = missing.json().await.unwrap();
    assert_eq!(body["error"]["code"], 401);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid magpie-lite token")
    );

    let wrong_gemini = client
        .post(format!(
            "{gw}/v1beta/models/mock/mock-model:generateContent"
        ))
        .header("x-goog-api-key", "not-the-token")
        .json(&json!({
            "contents": [{"role": "user", "parts": [{"text": "Weather?"}]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_gemini.status(), StatusCode::UNAUTHORIZED);
    let body: Value = wrong_gemini.json().await.unwrap();
    assert_eq!(body["error"]["status"], "PERMISSION_DENIED");

    assert!(last.lock().unwrap().is_none());
}

#[tokio::test]
async fn anthropic_x_api_key_and_public_health_probe_work() {
    let (upstream, _) = spawn_mock().await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("x-api-key", "magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let health = client.get(format!("{gw}/health")).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let body: Value = health.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn refused_optional_field_is_retried_without_it_and_then_omitted() {
    let error = json!({
        "error": {
            "message": "Unsupported parameter: 'parallel_tool_calls' is unsupported",
            "code": "unsupported_parameter",
            "param": "parallel_tool_calls"
        }
    });
    let (upstream, requests) = spawn_scripted(vec![
        (400, error),
        (
            200,
            json!({
                "id": "chatcmpl-optional-ok",
                "object": "chat.completion",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2
                }
            }),
        ),
    ])
    .await;
    let gw = spawn_gateway(upstream).await;
    let client = reqwest::Client::new();
    let post = |url: String| client.post(url).bearer_auth("magpie-lite");
    let payload = json!({
        "model": "mock/mock-model",
        "tools": [{
            "type": "function",
            "function": {
                "name": "echo",
                "description": "Echo",
                "parameters": {"type": "object", "properties": {}}
            }
        }],
        "parallel_tool_calls": false,
        "messages": [{"role": "user", "content": "Weather?"}]
    });
    let first = post(format!("{gw}/v1/chat/completions"))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let body: Value = first.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    assert_eq!(*requests.lock().unwrap(), 2);

    // The refusal is remembered: the next request is without the field
    // from the start.
    let second = post(format!("{gw}/v1/chat/completions"))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    assert_eq!(*requests.lock().unwrap(), 3);
}

/// A Claude refusing a sampling parameter by wording no other rule reads
/// (#1454) is asked once more without it, on the same credential, before
/// anything reaches the client.
#[tokio::test]
async fn sampling_refusal_is_retried_without_the_sampling() {
    let error = json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "`temperature` is deprecated for this model."
        }
    });
    let (upstream, requests) =
        spawn_scripted_anthropic(vec![(400, error), (200, json!({"id": "msg-scripted", "type": "message", "role": "assistant", "model": "m", "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}))]).await;
    let gw = spawn_gateway_with_protocol(upstream, Protocol::Anthropic).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "mock/mock-model",
            "temperature": 0.2,
            "messages": [{"role": "user", "content": "Weather?"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    assert_eq!(*requests.lock().unwrap(), 2);
}

/// The request's record keeps every upstream send: the member asked, at
/// the reasoning level the send went at (the reference's tries with
/// `sentEffort`). The first route is fixed at high and refuses; the
/// agent asked low, so the fallback goes at low.
#[tokio::test]
async fn usage_records_the_effort_each_try_was_sent_at() {
    let busy = json!({"error": {"message": "busy", "type": "server_error"}});
    let (a, _) = spawn_scripted(vec![(500, busy)]).await;
    let (b, _) = spawn_mock().await;
    let gw = spawn_gateway_with_group_routes(
        a,
        b,
        RoutingPolicy::Order,
        Vec::new(),
        vec!["primary/mock-model:high".into(), "backup/mock-model".into()],
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/daily-coding",
            "reasoning_effort": "low",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let stats: Value = client
        .get(format!("{gw}/stats"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stats["recent_requests"][0]["tries"],
        json!([
            {"provider": "primary", "model": "mock-model", "key_slot": 0, "effort": "high"},
            {"provider": "backup", "model": "mock-model", "key_slot": 0, "effort": "low"},
        ])
    );
}
