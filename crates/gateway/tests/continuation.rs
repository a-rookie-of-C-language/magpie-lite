//! A reply the upstream's stream cuts mid-way goes on from the part of it
//! the client already has (the reference's `continuation.go`): the same
//! candidate is asked again with the reply so far sent back as the
//! conversation's last assistant message — an Anthropic upstream
//! natively, a Chat one in its vendor's own prefill mode — and the
//! client reads one reply that finished, not one cut short. A Chat API
//! that answers the message again from the start is never asked, a reply
//! with a tool call begun ends as it used to, and a model that turns the
//! prefill away is remembered and not asked again.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, KeyRotator, Protocol, ProviderConfig,
    RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const TOKEN: &str = "continuation-token";

/// What each upstream call was asked and which path it was asked on.
#[derive(Clone, Default)]
struct Journal {
    bodies: Arc<Mutex<Vec<Value>>>,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Journal {
    fn calls(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }
    fn path(&self, call: usize) -> String {
        self.paths.lock().unwrap()[call].clone()
    }
    fn body(&self, call: usize) -> Value {
        self.bodies.lock().unwrap()[call].clone()
    }
    /// The last message a call was asked with, when it's the
    /// assistant's: the reply so far, sent back to go on from.
    fn last_assistant(&self, call: usize) -> Option<Value> {
        let messages = self.body(call)["messages"].as_array()?.clone();
        let last = messages.last()?.clone();
        (last["role"] == "assistant").then_some(last)
    }
}

/// A Chat upstream that severs the reply mid-stream on the calls `cuts`
/// names: the events given go out, then the connection just ends —
/// nothing more, no [DONE]. Other calls answer `whole` and finish.
struct CutChat {
    journal: Journal,
    cuts: Vec<Vec<String>>,
    whole: Vec<String>,
    fails: Vec<(u16, String)>,
}

async fn chat(
    State(f): State<Arc<CutChat>>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    let streaming = body["stream"] == true;
    let call = {
        let mut bodies = f.journal.bodies.lock().unwrap();
        f.journal.paths.lock().unwrap().push(uri.path().to_owned());
        let call = bodies.len();
        bodies.push(body);
        call
    };
    if let Some((status, fail)) = f.fails.get(call).filter(|(s, _)| *s != 0) {
        return (
            axum::http::StatusCode::from_u16(*status).unwrap(),
            Json(serde_json::from_str::<Value>(fail).unwrap()),
        )
            .into_response();
    }
    let (events, cut) = if call < f.cuts.len() && !f.cuts[call].is_empty() {
        (f.cuts[call].clone(), true)
    } else {
        (f.whole.clone(), false)
    };
    if !streaming {
        // A non-stream ask answers the whole reply at once.
        return Json(json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "answer"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
        .into_response();
    }
    let stream = async_stream::stream! {
        for ev in events {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {ev}\n\n")));
        }
        if !cut {
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
        // A cut just ends: no sentinel, the connection closed under the
        // client.
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

/// An Anthropic upstream that severs the reply the same way.
struct CutAnthropic {
    journal: Journal,
    cuts: Vec<Vec<String>>,
    whole: Vec<String>,
    fails: Vec<(u16, String)>,
}

async fn anthropic(
    State(f): State<Arc<CutAnthropic>>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    let call = {
        let mut bodies = f.journal.bodies.lock().unwrap();
        f.journal.paths.lock().unwrap().push(uri.path().to_owned());
        let call = bodies.len();
        bodies.push(body);
        call
    };
    if let Some((status, fail)) = f.fails.get(call).filter(|(s, _)| *s != 0) {
        return (
            axum::http::StatusCode::from_u16(*status).unwrap(),
            Json(serde_json::from_str::<Value>(fail).unwrap()),
        )
            .into_response();
    }
    let events = if call < f.cuts.len() && !f.cuts[call].is_empty() {
        f.cuts[call].clone()
    } else {
        f.whole.clone()
    };
    // A cut just ends: no message_stop, the connection closed under the
    // client — the way an upstream dying mid-reply leaves the stream.
    let stream = async_stream::stream! {
        for ev in events {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {ev}\n\n")));
        }
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

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

fn provider(protocol: Protocol, url: String, model: &str) -> ProviderConfig {
    ProviderConfig {
        id: "up".into(),
        name: "UP".into(),
        protocol,
        base_url: url,
        api_key: "fixture-key".into(),
        api_keys: Vec::new(),
        models: vec![model.into()],
        contexts: BTreeMap::new(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: RoutingPolicy::Order,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
    }
}

async fn gateway(p: ProviderConfig) -> Server {
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(
            Config {
                token: TOKEN.into(),
                default_provider: p.id.clone(),
                provider: vec![p],
                ..Config::default()
            },
            None,
        ),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(RequestLog::default())),
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    serve(build_router(state)).await
}

/// POST the client's ask, streaming; its body back whole.
async fn ask(url: &str, protocol: Protocol, model: &str) -> String {
    let (path, body) = match protocol {
        Protocol::Anthropic => (
            "/v1/messages",
            json!({"model": model, "max_tokens": 32000, "stream": true,
                "messages": [{"role": "user", "content": "hi"}]}),
        ),
        _ => (
            "/v1/chat/completions",
            json!({"model": model, "stream": true,
                "messages": [{"role": "user", "content": "hi"}]}),
        ),
    };
    let resp = reqwest::Client::new()
        .post(format!("{url}{path}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    resp.text().await.unwrap()
}

/// The stream's frames, as JSON (the [DONE] sentinel dropped).
fn frames(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
        })
        .collect()
}

/// A Messages reply's text deltas, concatenated; its thinking likewise.
fn anthropic_text(body: &str) -> String {
    frames(body)
        .iter()
        .filter(|ev| ev["type"] == "content_block_delta" && ev["delta"]["type"] == "text_delta")
        .filter_map(|ev| ev["delta"]["text"].as_str().map(str::to_owned))
        .collect()
}
fn anthropic_think(body: &str) -> String {
    frames(body)
        .iter()
        .filter(|ev| ev["type"] == "content_block_delta" && ev["delta"]["type"] == "thinking_delta")
        .filter_map(|ev| ev["delta"]["thinking"].as_str().map(str::to_owned))
        .collect()
}
/// (message_start, message_stop, error) counts in a Messages reply.
fn anthropic_counts(body: &str) -> (usize, usize, usize) {
    let mut out = (0, 0, 0);
    for ev in frames(body) {
        match ev["type"].as_str() {
            Some("message_start") => out.0 += 1,
            Some("message_stop") => out.1 += 1,
            Some("error") => out.2 += 1,
            _ => {}
        }
    }
    out
}
/// A Chat reply's content deltas, concatenated; its reasoning likewise.
fn chat_text(body: &str) -> String {
    chat_delta(body, "content")
}
fn chat_delta(body: &str, field: &str) -> String {
    let mut out = String::new();
    for ev in frames(body) {
        if let Some(choices) = ev["choices"].as_array() {
            for choice in choices {
                if let Some(s) = choice["delta"][field].as_str() {
                    out.push_str(s);
                }
            }
        }
    }
    out
}

/// One Chat chunk of reasoning, of text, and the stop with usage.
fn chat_think_ev(model: &str, s: &str) -> String {
    json!({"id":"c1","model":model,"choices":[{"index":0,"delta":{"reasoning_content":s}}]})
        .to_string()
}
fn chat_text_ev(model: &str, s: &str) -> String {
    json!({"id":"c1","model":model,"choices":[{"index":0,"delta":{"content":s}}]}).to_string()
}
fn chat_stop(model: &str) -> String {
    json!({"id":"c1","model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":9,"completion_tokens":4}})
    .to_string()
}

/// Anthropic stream events: start (input billed), a thinking and a text
/// delta, and the end (output billed).
fn anth_start(model: &str, input: u64) -> String {
    json!({"type":"message_start","message":{"id":"m1","type":"message","role":"assistant",
        "model":model,"content":[],"usage":{"input_tokens":input,"output_tokens":0}}})
    .to_string()
}
fn anth_text(s: &str) -> String {
    json!({"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":s}})
    .to_string()
}
fn anth_stop(output: u64) -> String {
    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
        "usage":{"output_tokens":output}})
    .to_string()
}
fn anth_end() -> String {
    json!({"type":"message_stop"}).to_string()
}

/// A reply the upstream cut after some text goes on: the same
/// conversation is asked again with what the client has sent back — in
/// Kimi's `partial` mode, the loopback relay taken to front the vendor —
/// and the client reads one whole reply, its thinking with it.
#[tokio::test]
async fn a_reply_cut_mid_way_goes_on_from_what_the_client_has() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![vec![
            chat_think_ev("kimi-k3", "think-1"),
            chat_text_ev("kimi-k3", "hel"),
        ]],
        whole: vec![
            chat_think_ev("kimi-k3", "think-2"),
            chat_text_ev("kimi-k3", "lo"),
            chat_stop("kimi-k3"),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(
        Protocol::Chat,
        format!("{}/v1", up.url),
        "kimi-k3",
    ))
    .await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/kimi-k3").await;

    assert_eq!(f.journal.calls(), 2, "the cut reply wasn't asked again");
    let m = f
        .journal
        .last_assistant(1)
        .expect("the second ask ends with the assistant's message");
    assert_eq!(m["content"], "hel");
    assert_eq!(m["reasoning_content"], "think-1");
    assert_eq!(m["partial"], true, "Kimi's prefill mode");
    assert_eq!(anthropic_text(&body), "hello");
    // A continuation's fresh thinking isn't shown a second time.
    assert_eq!(anthropic_think(&body), "think-1");
    assert_eq!(anthropic_counts(&body), (1, 1, 0), "{body}");
}

/// A model that says what it was prefilled with again before going on has
/// the echo dropped: the client reads the text once.
#[tokio::test]
async fn a_continued_replys_echo_of_the_prefill_is_dropped() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![vec![chat_text_ev("kimi-k3", "hel")]],
        whole: vec![
            chat_text_ev("kimi-k3", "hel"),
            chat_text_ev("kimi-k3", "lo"),
            chat_stop("kimi-k3"),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(
        Protocol::Chat,
        format!("{}/v1", up.url),
        "kimi-k3",
    ))
    .await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/kimi-k3").await;

    assert_eq!(f.journal.calls(), 2);
    assert_eq!(anthropic_text(&body), "hello", "echo not dropped: {body}");
    assert_eq!(anthropic_counts(&body).2, 0, "{body}");
}

/// A reply a tool call of has begun can't be prefilled and go on: it
/// ends with the error, as it used to.
#[tokio::test]
async fn a_reply_cut_after_a_tool_call_begun_ends_as_before() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![vec![
            chat_text_ev("kimi-k3", "hel"),
            json!({"id":"c1","model":"kimi-k3","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"ci"}}]}}]}).to_string(),
        ]],
        whole: vec![chat_text_ev("kimi-k3", "lo"), chat_stop("kimi-k3")],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(
        Protocol::Chat,
        format!("{}/v1", up.url),
        "kimi-k3",
    ))
    .await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/kimi-k3").await;

    assert_eq!(
        f.journal.calls(),
        1,
        "a reply with a tool call begun was asked again"
    );
    assert_eq!(anthropic_counts(&body).2, 1, "{body}");
}

/// A reply whose every ask is cut ends with the error after the tries
/// are up, as it used to after one.
#[tokio::test]
async fn a_reply_cut_again_and_again_goes_on_only_so_long() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![
            vec![chat_text_ev("kimi-k3", "hel")],
            vec![chat_text_ev("kimi-k3", "hel")],
            vec![chat_text_ev("kimi-k3", "hel")],
        ],
        whole: vec![chat_text_ev("kimi-k3", "lo"), chat_stop("kimi-k3")],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(
        Protocol::Chat,
        format!("{}/v1", up.url),
        "kimi-k3",
    ))
    .await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/kimi-k3").await;

    assert_eq!(f.journal.calls(), 3, "initial + streamRetries");
    assert_eq!(anthropic_counts(&body).2, 1, "{body}");
    assert_eq!(anthropic_text(&body), "hel");
}

/// A reply cut while it was only thinking goes on from the thinking the
/// client has, where the upstream reads it back; the client reads its
/// thinking once, then the answer.
#[tokio::test]
async fn a_reply_cut_mid_thinking_goes_on_from_the_thinking() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![vec![chat_think_ev("kimi-k3", "think-1")]],
        whole: vec![
            chat_think_ev("kimi-k3", "rethink"),
            chat_text_ev("kimi-k3", "done"),
            chat_stop("kimi-k3"),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(
        Protocol::Chat,
        format!("{}/v1", up.url),
        "kimi-k3",
    ))
    .await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/kimi-k3").await;

    assert_eq!(f.journal.calls(), 2);
    let m = f.journal.last_assistant(1).unwrap();
    assert_eq!(m["reasoning_content"], "think-1");
    assert_eq!(m["partial"], true);
    assert_eq!(anthropic_think(&body), "think-1");
    assert_eq!(anthropic_text(&body), "done");
}

/// DeepSeek goes on from the reply's part in its `prefix` mode: the
/// continuation is asked of the /beta endpoint with "prefix": true on the
/// message.
#[tokio::test]
async fn deepseek_goes_on_in_its_prefix_mode_under_beta() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![vec![chat_text_ev("deepseek-chat", "hel")]],
        whole: vec![
            chat_text_ev("deepseek-chat", "lo"),
            chat_stop("deepseek-chat"),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(
        Protocol::Chat,
        format!("{}/v1", up.url),
        "deepseek-chat",
    ))
    .await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/deepseek-chat").await;

    assert_eq!(f.journal.calls(), 2);
    assert_eq!(f.journal.path(1), "/beta/chat/completions");
    let m = f.journal.last_assistant(1).unwrap();
    assert_eq!(m["content"], "hel");
    assert_eq!(m["prefix"], true);
    assert_eq!(anthropic_text(&body), "hello");
}

/// An Anthropic upstream goes on from the reply's part natively: the
/// continuation is the same conversation with it as the last, assistant,
/// message — no mode field — and the client reads one whole reply.
#[tokio::test]
async fn an_anthropic_upstream_goes_on_natively() {
    let f = Arc::new(CutAnthropic {
        journal: Journal::default(),
        cuts: vec![vec![anth_start("m", 7), anth_text("hel")]],
        whole: vec![
            anth_start("m", 11),
            anth_text("lo"),
            anth_stop(4),
            anth_end(),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(anthropic))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(Protocol::Anthropic, up.url.clone(), "m")).await;

    let body = ask(&gw.url, Protocol::Chat, "up/m").await;

    assert_eq!(f.journal.calls(), 2);
    let last = f.journal.body(1)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["role"], "assistant");
    assert_eq!(chat_text(&body), "hello");
}

/// A prefill's trailing whitespace, which Anthropic turns the request
/// away for, is trimmed: the client has it already.
#[tokio::test]
async fn a_prefills_trailing_whitespace_is_trimmed() {
    let f = Arc::new(CutAnthropic {
        journal: Journal::default(),
        cuts: vec![vec![anth_start("m", 7), anth_text("hel\n")]],
        whole: vec![
            anth_start("m", 11),
            anth_text("lo"),
            anth_stop(4),
            anth_end(),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(anthropic))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(Protocol::Anthropic, up.url.clone(), "m")).await;

    let body = ask(&gw.url, Protocol::Chat, "up/m").await;

    assert_eq!(f.journal.calls(), 2);
    let last = f.journal.body(1)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let text: String = last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect();
    assert_eq!(text, "hel", "the prefill kept its trailing whitespace");
    assert_eq!(chat_text(&body), "hel\nlo");
}

/// An upstream that turns the prefill itself away ends the reply with
/// what cut it, as a reply that can't go on does — not with the refusal
/// of an ask the client never made.
#[tokio::test]
async fn a_prefill_refused_ends_the_reply_with_what_cut_it() {
    let f = Arc::new(CutAnthropic {
        journal: Journal::default(),
        cuts: vec![vec![anth_start("m", 7), anth_text("hel")]],
        whole: vec![],
        fails: vec![
            (0, String::new()),
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"This model does not support assistant message prefill."}}"#
                    .into(),
            ),
        ],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(anthropic))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(Protocol::Anthropic, up.url.clone(), "m")).await;

    let body = ask(&gw.url, Protocol::Chat, "up/m").await;

    assert_eq!(f.journal.calls(), 2);
    assert_eq!(chat_text(&body), "hel");
    assert!(
        body.contains("ended before a completion marker") || body.contains("error"),
        "{body}"
    );
}

/// A Chat upstream without a prefill mode answers the reply's part sent
/// back again from the start, reworded — spliced onto the part the client
/// has, with no error, the client would keep the garble as the reply.
/// There the cut reply ends with the error, as it used to.
#[tokio::test]
async fn a_plain_chat_upstream_is_never_asked_to_go_on() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![vec![chat_text_ev(
            "m",
            "Go's garbage collector is a concurrent, tri-color",
        )]],
        whole: vec![
            chat_text_ev("m", "Go uses a concurrent, tri-color mark-and-sweep"),
            chat_stop("m"),
        ],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    // "m" names neither vendor's model: on this relay nothing fronts one.
    let gw = gateway(provider(Protocol::Chat, format!("{}/v1", up.url), "m")).await;

    let body = ask(&gw.url, Protocol::Anthropic, "up/m").await;

    assert_eq!(
        f.journal.calls(),
        1,
        "an upstream that re-answers was asked to go on"
    );
    assert_eq!(
        anthropic_text(&body),
        "Go's garbage collector is a concurrent, tri-color"
    );
    assert_eq!(anthropic_counts(&body).2, 1, "{body}");
}

/// A model that turned the trailing assistant message away is remembered:
/// its conversation is asked once more with a user turn after it, and a
/// later request goes out that way from the start.
#[tokio::test]
async fn a_prefill_refusal_is_answered_with_a_user_turn_and_remembered() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![],
        whole: vec![chat_text_ev("m", "ok"), chat_stop("m")],
        fails: vec![
            (
                400,
                r#"{"error":{"message":"This model does not support assistant message prefill."}}"#
                    .into(),
            ),
            (0, String::new()),
            (0, String::new()),
        ],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(Protocol::Chat, format!("{}/v1", up.url), "m")).await;

    let ask = || {
        let url = gw.url.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{url}/v1/chat/completions"))
                .bearer_auth(TOKEN)
                .json(&json!({
                    "model": "up/m",
                    "messages": [
                        {"role": "user", "content": "hi"},
                        {"role": "assistant", "content": "a beginning"},
                    ]
                }))
                .send()
                .await
                .unwrap()
        }
    };

    let first = ask().await;
    let status = first.status();
    let text = first.text().await.unwrap();
    assert!(status.is_success(), "{status}: {text}");
    assert_eq!(f.journal.calls(), 2, "the refused ask is sent once more");
    assert_eq!(
        f.journal.body(1)["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"],
        "Continue."
    );

    // The refusal is remembered: the next ask goes out with the turn
    // already after the assistant's message.
    let second = ask().await;
    assert!(second.status().is_success());
    assert_eq!(f.journal.calls(), 3);
    assert_eq!(
        f.journal.body(2)["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"],
        "Continue."
    );
}

/// An Anthropic client's own trailing assistant message is its prefill,
/// not a turn said: nothing is added after it, and a refusal is the
/// client's own ask's answer.
#[tokio::test]
async fn an_anthropic_clients_prefill_is_its_own_ask() {
    let f = Arc::new(CutChat {
        journal: Journal::default(),
        cuts: vec![],
        whole: vec![chat_text_ev("m", "ok"), chat_stop("m")],
        fails: vec![],
    });
    let up = serve(
        Router::new()
            .route("/{*p}", post(chat))
            .with_state(f.clone()),
    )
    .await;
    let gw = gateway(provider(Protocol::Chat, format!("{}/v1", up.url), "m")).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", gw.url))
        .bearer_auth(TOKEN)
        .json(&json!({
            "model": "up/m",
            "max_tokens": 32000,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "a beginning"},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    assert_eq!(f.journal.calls(), 1);
    let body0 = f.journal.body(0);
    let messages = body0["messages"].as_array().unwrap();
    assert_eq!(
        messages.last().unwrap()["role"],
        "assistant",
        "an Anthropic client's prefill got a user turn after it: {messages:?}"
    );
}
