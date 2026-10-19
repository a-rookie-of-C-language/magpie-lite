// A translated custom tool's input must reach a Responses client as it is
// written, not only when the upstream finishes the call.
use axum::{
    Json, Router,
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig,
    RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

const TOKEN: &str = "custom-stream-token";
const KEY: &str = "custom-stream-key";
const PATCH: &str = "*** Begin Patch\n*** Add File: a.txt\n+hello é 😀\n*** End Patch\n";

struct Server {
    url: String,
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
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, task }
}

#[derive(Clone, Default)]
struct Gate {
    seen_first: Arc<Notify>,
    released: Arc<AtomicBool>,
}

async fn upstream(State(gate): State<Gate>, Json(body): Json<Value>) -> Response {
    assert_eq!(body["stream"], true);
    let arguments = json!({"input": PATCH}).to_string();
    let split = arguments
        .char_indices()
        .nth(arguments.chars().count() / 3)
        .map(|(i, _)| i)
        .unwrap();
    let (first, rest) = arguments.split_at(split);
    let first = first.to_owned();
    let rest = rest.to_owned();
    let gate = gate.clone();
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(bytes(format!(
            "event: response.created\ndata: {}\n\n",
            json!({"type":"response.created","response":{"id":"r","model":"up-model"}})
        )));
        yield Ok::<_, std::io::Error>(bytes(format!(
            "event: response.output_item.added\ndata: {}\n\n",
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{
                    "type":"function_call",
                    "id":"fc_1",
                    "call_id":"call_1",
                    "name":"apply_patch",
                    "arguments":""
                }
            })
        )));
        yield Ok::<_, std::io::Error>(bytes(format!(
            "event: response.function_call_arguments.delta\ndata: {}\n\n",
            json!({
                "type":"response.function_call_arguments.delta",
                "output_index":0,
                "item_id":"fc_1",
                "delta":first
            })
        )));
        // The client must see the first piece before the upstream writes the
        // rest; otherwise the test times out.
        tokio::select! {
            _ = gate.seen_first.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
        }
        gate.released.store(true, Ordering::SeqCst);
        yield Ok::<_, std::io::Error>(bytes(format!(
            "event: response.function_call_arguments.delta\ndata: {}\n\n",
            json!({
                "type":"response.function_call_arguments.delta",
                "output_index":0,
                "item_id":"fc_1",
                "delta":rest
            })
        )));
        let whole = json!({"input": PATCH}).to_string();
        yield Ok::<_, std::io::Error>(bytes(format!(
            "event: response.function_call_arguments.done\ndata: {}\n\nevent: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","arguments":whole}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"apply_patch","arguments":whole}}),
            json!({"type":"response.completed","response":{"id":"r","model":"up-model","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":5}}})
        )));
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
        .into_response()
}

fn bytes(s: String) -> axum::body::Bytes {
    axum::body::Bytes::from(s)
}

#[tokio::test]
async fn custom_tool_input_streams_before_the_upstream_call_finishes() {
    let gate = Gate::default();
    let upstream = serve(
        Router::new()
            .fallback(post(upstream))
            .with_state(gate.clone()),
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let history = home.path().join("history.jsonl");
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(
            Config {
                token: TOKEN.into(),
                default_provider: "p0".into(),
                history_path: Some(history.clone()),
                provider: vec![ProviderConfig {
                    id: "p0".into(),
                    name: "Mock".into(),
                    protocol: Protocol::Responses,
                    capabilities: Default::default(),
                    base_url: format!("{}/v1", upstream.url),
                    api_key: KEY.into(),
                    api_keys: vec![],
                    models: vec!["up-model".into()],
                    contexts: Default::default(),
                    fallback: vec![],
                    key_cooldown_ms: None,
                    routing: RoutingPolicy::Rotate,
                    weights: Vec::new(),
                    affinity: Default::default(),
                    key_generation: 0,
                }],
                ..Config::default()
            },
            None,
        ),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(RequestLog::open(&history).unwrap())),
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
    });
    let gateway = serve(build_router(state)).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", gateway.url))
        .bearer_auth(TOKEN)
        .json(&json!({
            "model":"p0/up-model",
            "stream":true,
            "input":"edit",
            "tools":[{
                "type":"custom",
                "name":"apply_patch",
                "description":"Patch files",
                "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}
            }]
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let mut response = response;
    let mut buffer = String::new();
    let mut deltas = String::new();
    let mut saw_input_done = None;
    let mut early = false;
    loop {
        let Some(chunk) = response.chunk().await.unwrap() else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            buffer.drain(..end + 2);
            let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match event["type"].as_str() {
                Some("response.custom_tool_call_input.delta") => {
                    if deltas.is_empty() {
                        early = !gate.released.load(Ordering::SeqCst);
                        gate.seen_first.notify_one();
                    }
                    deltas.push_str(event["delta"].as_str().unwrap_or_default());
                }
                Some("response.custom_tool_call_input.done") => {
                    saw_input_done = event.get("input").cloned();
                }
                _ => {}
            }
        }
    }

    assert!(
        early,
        "no custom input reached the client before the call ended"
    );
    assert_eq!(deltas, PATCH);
    assert_eq!(saw_input_done, Some(json!(PATCH)));
}
