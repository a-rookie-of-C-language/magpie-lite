// Real loopback checks for Grok's integral tool arguments: a Grok model writes
// a whole number as 2500.0, and a client that types its arguments strictly
// must read 2500. Requests and replies are authored in each wire dialect, not
// built with core codecs, and no paid provider is called.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
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

const TOKEN: &str = "grok-integral-fixture-token";
const KEY: &str = "grok-integral-fixture-key";
const TOOL: &str = "exec_command";

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

/// The arguments Grok writes: whole numbers with a fraction of zeros.
const ARGS: &str = r#"{"cmd":"ls","yield_time_ms":2500.0,"max_output_tokens":4000.0,"frac":2.5}"#;
/// The same arguments with the zero fractions dropped, and `frac` untouched.
const WANT: &str = r#"{"cmd":"ls","yield_time_ms":2500,"max_output_tokens":4000,"frac":2.5}"#;

/// A tool call as one protocol's reply spells it, with the arguments exactly
/// as `args` says.
fn call(protocol: Protocol, args: &str) -> Value {
    match protocol {
        Protocol::Chat => json!({
            "id":"r","model":"up-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":TOOL,"arguments":args}}]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}}),
        Protocol::Responses => json!({
            "id":"r","object":"response","model":"up-model","status":"completed",
            "output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":TOOL,"arguments":args,"status":"completed"}],
            "usage":{"input_tokens":5,"output_tokens":2}}),
        Protocol::Anthropic => json!({
            "id":"r","type":"message","role":"assistant","model":"up-model",
            "content":[{"type":"tool_use","id":"call_1","name":TOOL,"input":serde_json::from_str::<Value>(args).unwrap()}],
            "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":2}}),
        Protocol::Gemini => json!({
            "responseId":"r","modelVersion":"up-model",
            "candidates":[{"index":0,"content":{"role":"model","parts":[
                {"functionCall":{"id":"call_1","name":TOOL,"args":serde_json::from_str::<Value>(args).unwrap()}}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}),
    }
}

/// How a protocol's stream cuts the arguments into deltas: two pieces that
/// split the fraction of zeros from its number (`2500` | `.0,`), so a held
/// fraction has to survive a delta boundary.
fn pieces(args: &str) -> Vec<String> {
    let at = args.find("2500.0").unwrap();
    vec![
        args[..at + 4].to_owned(),
        args[at + 4..at + 6].to_owned(),
        args[at + 6..].to_owned(),
    ]
}

/// The upstream stream for a protocol, cutting the arguments as `pieces` says.
fn stream(protocol: Protocol, args: &str) -> String {
    let parts = pieces(args);
    let mut events: Vec<(Option<&str>, Value)> = Vec::new();
    match protocol {
        Protocol::Chat => {
            events.push((None, json!({"id":"r","model":"up-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":TOOL,"arguments":""}}]}}]})));
            for part in &parts {
                events.push((None, json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":part}}]}}]})));
            }
            events.push((None, json!({"id":"r","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})));
        }
        Protocol::Responses => {
            events.push((Some("response.created"), json!({"type":"response.created","response":{"id":"r","model":"up-model","status":"in_progress"}})));
            events.push((Some("response.output_item.added"), json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":TOOL,"arguments":"","status":"in_progress"}})));
            for part in &parts {
                events.push((Some("response.function_call_arguments.delta"), json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":part})));
            }
            // No item.done: the terminal reply has to agree with the deltas.
            events.push((
                Some("response.completed"),
                json!({"type":"response.completed","response":call(protocol, args)}),
            ));
        }
        Protocol::Anthropic => {
            events.push((Some("message_start"), json!({"type":"message_start","message":{"id":"r","model":"up-model","usage":{"input_tokens":5,"output_tokens":0}}})));
            events.push((Some("content_block_start"), json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":TOOL,"input":{}}})));
            for part in &parts {
                events.push((Some("content_block_delta"), json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":part}})));
            }
            events.push((
                Some("content_block_stop"),
                json!({"type":"content_block_stop","index":0}),
            ));
            events.push((Some("message_delta"), json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}})));
            events.push((Some("message_stop"), json!({"type":"message_stop"})));
        }
        Protocol::Gemini => {
            // One whole chunk: Gemini carries a call's arguments as an object.
            events.push((None, call(protocol, args)));
        }
    }
    let mut wire = String::new();
    for (event, value) in events {
        if let Some(name) = event {
            wire.push_str(&format!("event: {name}\n"));
        }
        wire.push_str(&format!("data: {value}\n\n"));
    }
    if protocol == Protocol::Chat {
        wire.push_str("data: [DONE]\n\n");
    }
    wire
}

#[derive(Clone)]
struct Mock {
    protocol: Protocol,
    /// The model this upstream answers with, as a relay or xAI itself names it.
    model: String,
    /// Whether the vendor ignores `stream: true` and answers whole.
    whole: bool,
    seen: Arc<Mutex<Vec<Value>>>,
}

async fn upstream(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let auth = match mock.protocol {
        Protocol::Chat | Protocol::Responses => headers["authorization"].to_str().unwrap(),
        Protocol::Anthropic => headers["x-api-key"].to_str().unwrap(),
        Protocol::Gemini => headers["x-goog-api-key"].to_str().unwrap(),
    };
    assert!(auth.contains(KEY), "unexpected upstream key {auth}");
    if mock.protocol == Protocol::Gemini {
        assert!(uri.path().starts_with("/v1beta/models/"));
    }
    let body: Value = serde_json::from_slice(&body).unwrap();
    mock.seen.lock().unwrap().push(body.clone());
    let streamed = mock.protocol != Protocol::Gemini && body["stream"] == true && !mock.whole;
    if streamed {
        return (
            [("content-type", "text/event-stream")],
            stream(mock.protocol, ARGS).replace("up-model", &mock.model),
        )
            .into_response();
    }
    let mut reply = call(mock.protocol, ARGS);
    reply["model"] = json!(mock.model);
    reply["modelVersion"] = json!(mock.model);
    Json(reply).into_response()
}

/// The reply as one protocol's client reads a call's arguments, as written.
fn client_args(protocol: Protocol, body: &str, stream: bool) -> String {
    if stream {
        let mut args = String::new();
        for line in body.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match protocol {
                Protocol::Chat => {
                    if let Some(a) = v
                        .pointer("/choices/0/delta/tool_calls/0/function/arguments")
                        .and_then(Value::as_str)
                    {
                        args.push_str(a);
                    }
                }
                Protocol::Responses => {
                    if let Some(a) = v
                        .pointer("/item/arguments")
                        .or_else(|| v.get("delta"))
                        .and_then(Value::as_str)
                        .filter(|_| v["type"] == "response.function_call_arguments.delta")
                    {
                        args.push_str(a);
                    }
                }
                Protocol::Anthropic => {
                    if let Some(a) = v.pointer("/delta/partial_json").and_then(Value::as_str) {
                        args.push_str(a);
                    }
                }
                Protocol::Gemini => {
                    if let Some(a) = v.pointer("/candidates/0/content/parts/0/functionCall/args") {
                        args = a.to_string();
                    }
                }
            }
        }
        return args;
    }
    let v: Value = serde_json::from_str(body).unwrap();
    match protocol {
        Protocol::Chat => v
            .pointer("/choices/0/message/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned(),
        Protocol::Responses => v
            .pointer("/output/0/arguments")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned(),
        Protocol::Anthropic => v.pointer("/content/0/input").unwrap().to_string(),
        Protocol::Gemini => v
            .pointer("/candidates/0/content/parts/0/functionCall/args")
            .unwrap()
            .to_string(),
    }
}

fn request(protocol: Protocol, model: &str, stream: bool) -> Value {
    let tools = match protocol {
        Protocol::Chat => {
            json!([{"type":"function","function":{"name":TOOL,"parameters":{"type":"object","properties":{"cmd":{"type":"string"},"yield_time_ms":{"type":"number"}}}}}])
        }
        Protocol::Responses => {
            json!([{"type":"function","name":TOOL,"parameters":{"type":"object","properties":{"cmd":{"type":"string"},"yield_time_ms":{"type":"number"}}}}])
        }
        Protocol::Anthropic => {
            json!([{"name":TOOL,"input_schema":{"type":"object","properties":{"cmd":{"type":"string"},"yield_time_ms":{"type":"number"}}}}])
        }
        Protocol::Gemini => {
            json!([{"functionDeclarations":[{"name":TOOL,"parameters":{"type":"object","properties":{"cmd":{"type":"string"},"yield_time_ms":{"type":"number"}}}}]}])
        }
    };
    match protocol {
        Protocol::Chat => {
            json!({"model":model,"stream":stream,"tools":tools,"messages":[{"role":"user","content":"list the files"}]})
        }
        Protocol::Responses => {
            json!({"model":model,"stream":stream,"tools":tools,"input":"list the files"})
        }
        Protocol::Anthropic => {
            json!({"model":model,"stream":stream,"max_tokens":64,"tools":tools,"messages":[{"role":"user","content":"list the files"}]})
        }
        Protocol::Gemini => {
            json!({"contents":[{"role":"user","parts":[{"text":"list the files"}]}],"tools":tools})
        }
    }
}

struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    http: reqwest::Client,
}
impl Fixture {
    async fn new(protocol: Protocol, upstream_model: &str, whole: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = serve(Router::new().fallback(post(upstream)).with_state(Mock {
            protocol,
            model: upstream_model.into(),
            whole,
            seen,
        }))
        .await;
        let base_url = match protocol {
            Protocol::Anthropic => upstream.url.clone(),
            Protocol::Gemini => format!("{}/v1beta", upstream.url),
            _ => format!("{}/v1", upstream.url),
        };
        let history = home.path().join("history.jsonl");
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(
                Config {
                    token: TOKEN.into(),
                    default_provider: "p0".into(),
                    hidden_auto_groups: Vec::new(),
                    no_auto_groups: false,
                    history_path: Some(history.clone()),
                    provider: vec![ProviderConfig {
                        id: "p0".into(),
                        name: "Relay".into(),
                        protocol,
                        capabilities: Default::default(),
                        base_url,
                        api_key: KEY.into(),
                        api_keys: Vec::new(),
                        models: vec!["grok-4.6".into(), "gpt-5.5".into()],
                        contexts: Default::default(),
                        outputs: Default::default(),
                        fallback: Vec::new(),
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
                    }],
                    group: vec![GroupConfig {
                        id: "g".into(),
                        name: "G".into(),
                        routes: vec!["p0/grok-4.6".into()],
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
            requests: Arc::new(Mutex::new(RequestLog::open(&history).unwrap())),
            key_rotation: KeyRotator::default(),
            affinity: AffinityStore::default(),
            lanes: magpie_gateway::lanes::Lanes::default(),
            rpms: magpie_gateway::lanes::Rpms::default(),
        });
        let gateway = serve(build_router(state)).await;
        Self {
            gateway,
            _upstream: upstream,
            _home: home,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    async fn ask(&self, protocol: Protocol, model: &str, stream: bool) -> String {
        let path = match protocol {
            Protocol::Chat => "/v1/chat/completions".to_owned(),
            Protocol::Responses => "/v1/responses".to_owned(),
            Protocol::Anthropic => "/v1/messages".to_owned(),
            Protocol::Gemini => format!(
                "/v1beta/models/{}:{}",
                model.rsplit('/').next().unwrap(),
                if stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                }
            ),
        };
        let response = self
            .http
            .post(format!("{}{path}", self.gateway.url))
            .bearer_auth(TOKEN)
            .json(&request(protocol, model, stream))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

const CLIENTS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Responses,
    Protocol::Anthropic,
    Protocol::Gemini,
];
const UPSTREAMS: [Protocol; 4] = [
    Protocol::Chat,
    Protocol::Responses,
    Protocol::Anthropic,
    Protocol::Gemini,
];

fn name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Chat => "chat",
        Protocol::Responses => "responses",
        Protocol::Anthropic => "messages",
        Protocol::Gemini => "gemini",
    }
}

#[tokio::test]
async fn every_client_reads_a_grok_call_with_integers() {
    for upstream in UPSTREAMS {
        for client in CLIENTS {
            for stream in [true, false] {
                let fixture = Fixture::new(upstream, "grok-4.6", false).await;
                // Gemini's client stream is one whole chunk either way.
                let streaming = stream && client != Protocol::Gemini;
                let body = fixture.ask(client, "p0/grok-4.6", streaming).await;
                let got = client_args(client, &body, streaming);
                let got: Value = serde_json::from_str(&got).unwrap_or_else(|_| json!(got));
                assert_eq!(
                    got,
                    serde_json::from_str::<Value>(WANT).unwrap(),
                    "upstream {} client {} stream {streaming}\n{body}",
                    name(upstream),
                    name(client),
                );
            }
        }
    }
}

#[tokio::test]
async fn another_models_call_keeps_its_floats() {
    for upstream in UPSTREAMS {
        for client in CLIENTS {
            let fixture = Fixture::new(upstream, "gpt-5.5", false).await;
            let body = fixture.ask(client, "p0/gpt-5.5", false).await;
            let got: Value = serde_json::from_str(&client_args(client, &body, false)).unwrap();
            assert_eq!(
                got.pointer("/yield_time_ms").unwrap().to_string(),
                "2500.0",
                "upstream {} client {}\n{body}",
                name(upstream),
                name(client)
            );
            assert!(got.pointer("/cmd").is_some(), "{body}");
        }
    }
}

/// A delta cut just after the dot must not be sent as an empty or premature
/// fraction, and the call the client reads must still add up to the whole
/// arguments.
#[tokio::test]
async fn a_fraction_split_across_deltas_does_not_lose_arguments() {
    for upstream in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic] {
        let fixture = Fixture::new(upstream, "grok-4.6", false).await;
        let body = fixture.ask(Protocol::Responses, "p0/grok-4.6", true).await;
        let got: Value =
            serde_json::from_str(&client_args(Protocol::Responses, &body, true)).unwrap();
        assert_eq!(
            got,
            serde_json::from_str::<Value>(WANT).unwrap(),
            "upstream {}\n{body}",
            name(upstream)
        );
    }
}

#[tokio::test]
async fn a_whole_reply_from_a_relay_is_replayed_with_integers() {
    // A vendor that ignores `stream: true` still answers with 2500.0, and a
    // streaming client must not read it as a float.
    for upstream in UPSTREAMS {
        for client in CLIENTS {
            let whole = upstream != Protocol::Gemini;
            let fixture = Fixture::new(upstream, "grok-4.6", whole).await;
            let streaming = client != Protocol::Gemini;
            let body = fixture.ask(client, "p0/grok-4.6", streaming).await;
            let got: Value = serde_json::from_str(&client_args(client, &body, streaming)).unwrap();
            assert_eq!(
                got,
                serde_json::from_str::<Value>(WANT).unwrap(),
                "upstream {} client {}\n{body}",
                name(upstream),
                name(client)
            );
        }
    }
}

#[tokio::test]
async fn a_relays_own_grok_name_counts_too() {
    // The client asks for a group, not a Grok model; the group's route sends
    // the request to one, and the reply is Grok's.
    let fixture = Fixture::new(Protocol::Chat, "x-ai/grok-4.6", false).await;
    let body = fixture.ask(Protocol::Responses, "group/g", false).await;
    let got: Value = serde_json::from_str(&client_args(Protocol::Responses, &body, false)).unwrap();
    assert_eq!(got, serde_json::from_str::<Value>(WANT).unwrap(), "{body}");
}
