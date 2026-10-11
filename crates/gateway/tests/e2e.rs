//! End-to-end: an agent talks Anthropic (or Chat) to the gateway, the
//! gateway talks Chat to a mock upstream, and the translation survives
//! the round trip — non-streaming and streaming, tool calls included.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use magpie_gateway::{AppState, Config, Protocol, ProviderConfig, build_router};
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
use axum::http::{HeaderMap, StatusCode};
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

async fn spawn_gateway(upstream_base: String) -> String {
    spawn_gateway_with_protocol(upstream_base, Protocol::Chat).await
}

async fn spawn_gateway_with_protocol(upstream_base: String, protocol: Protocol) -> String {
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        default_provider: "mock".into(),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol,
            base_url: format!(
                "{upstream_base}/{}",
                if protocol == Protocol::Gemini {
                    "v1beta"
                } else {
                    "v1"
                }
            ),
            api_key: "test-key".into(),
            models: vec!["mock-model".into()],
            fallback: Vec::new(),
        }],
    };
    let state = Arc::new(AppState {
        config,
        http: magpie_gateway::upstream::http_client().unwrap(),
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
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_prev", "content": "5-day forecast"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_prev", "name": "forecast", "input": {"city": "Beijing"}}
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
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "call_prev");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "forecast");
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

async fn spawn_gateway_with_fallback(primary: String, backup: String) -> String {
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        default_provider: "primary".into(),
        provider: vec![
            ProviderConfig {
                id: "primary".into(),
                name: "Primary".into(),
                protocol: Protocol::Chat,
                base_url: format!("{primary}/v1"),
                api_key: "primary-key".into(),
                models: vec!["mock-model".into()],
                fallback: vec!["backup/mock-model".into()],
            },
            ProviderConfig {
                id: "backup".into(),
                name: "Backup".into(),
                protocol: Protocol::Chat,
                base_url: format!("{backup}/v1"),
                api_key: "backup-key".into(),
                models: vec!["mock-model".into()],
                fallback: Vec::new(),
            },
        ],
    };
    let state = Arc::new(AppState {
        config,
        http: magpie_gateway::upstream::http_client().unwrap(),
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
    assert_eq!(events[0]["response"]["model"], "mock/mock-model");
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
