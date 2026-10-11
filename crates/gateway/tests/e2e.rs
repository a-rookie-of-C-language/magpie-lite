//! End-to-end: an agent talks Anthropic (or Chat) to the gateway, the
//! gateway talks Chat to a mock upstream, and the translation survives
//! the round trip — non-streaming and streaming, tool calls included.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use magpie_gateway::{AppState, Config, Protocol, ProviderConfig, build_router};
use serde_json::{Value, json};

#[derive(Clone)]
struct MockUpstream {
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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

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
    let config = Config {
        port: 0,
        token: "magpie-lite".into(),
        default_provider: "mock".into(),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Chat,
            base_url: format!("{upstream_base}/v1"),
            api_key: "test-key".into(),
            models: vec!["mock-model".into()],
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
