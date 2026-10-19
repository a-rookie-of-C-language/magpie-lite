//! A cut reply goes on from the part of it the client already has.
//!
//! When a translated stream stops mid-reply — the connection lost, an
//! error out of nowhere — the same conversation is asked again with what
//! the client has of the reply sent back as its last assistant message:
//! the client reads one reply that finished, not one cut short, and the
//! turn doesn't fail the way it used to. Only where the upstream goes on
//! from a reply's part sent back does this run (the reference's
//! `continuation.go`): an Anthropic Messages upstream natively, a Chat
//! one in its vendor's own prefill mode — DeepSeek's `prefix` under
//! /beta, Kimi's `partial`. A Chat API that answers the message again
//! from the start, as most do, would splice a reworded re-answer onto
//! the part the client has, so a cut reply ends with its error there.
//!
//! Only a reply no tool call of has begun goes on: a call's arguments
//! can't be prefilled. A refusal is never asked again, and a model that
//! turned a prefill away isn't asked to go on.

use std::net::ToSocketAddrs;
use std::sync::OnceLock;

use magpie_core::ir::{CompletionRequest, Message, Part, Role};
use serde_json::Value;

use crate::config::{Protocol, ProviderConfig};

/// How many times a cut reply is asked to go on before it ends with the
/// error, as it used to at once (the reference's `streamRetries`).
pub(crate) const STREAM_RETRIES: usize = 2;

/// `reask_stream`'s answer when the continuation's prefill was turned
/// away: the caller ends the reply with what cut it, not with this.
pub(crate) const PREFILL_REFUSED: &str = "prefill refused";

/// The user turn asked after a conversation the client ended with the
/// assistant's message, for a model that takes no prefill
/// (the reference's `continueText`, #1447): OpenAI's Chat and Responses
/// APIs read such a conversation as a turn already said, not one to go
/// on from, so the model is asked to answer after it.
pub(crate) const CONTINUE_TEXT: &str = "Continue.";

/// Whether `req`'s conversation ends with the assistant's message rather
/// than a call it made: what a prefill is (the reference's
/// `endsWithAssistant`).
pub(crate) fn ends_with_assistant(req: &CompletionRequest) -> bool {
    let Some(last) = req.messages.last() else {
        return false;
    };
    last.role == Role::Assistant
        && !last.parts.iter().any(|p| {
            matches!(
                p.kind(),
                magpie_core::ir::PartKind::ToolCall | magpie_core::ir::PartKind::ToolResult
            )
        })
}

/// A Chat or Responses request body with a user turn after its last
/// message when that is the assistant's; `None` when it isn't (the
/// reference's `userLastBody`). A call the assistant made last still
/// waits on its results — nothing is added after it.
pub(crate) fn user_last_body(protocol: Protocol, body: &Value) -> Option<Value> {
    let chat = protocol == Protocol::Chat;
    if !chat && protocol != Protocol::Responses {
        return None;
    }
    let field = if chat { "messages" } else { "input" };
    let mut out = body.clone();
    let items = out.get_mut(field)?.as_array_mut()?;
    let last = items.last()?;
    let role = last.get("role").and_then(Value::as_str);
    let ty = last.get("type").and_then(Value::as_str);
    let has_calls = last
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    if role != Some("assistant") || ty.is_some_and(|t| t != "message") || has_calls {
        return None;
    }
    let turn = if chat {
        serde_json::json!({"role": "user", "content": CONTINUE_TEXT})
    } else {
        serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": CONTINUE_TEXT}],
        })
    };
    items.push(turn);
    Some(out)
}

/// How unfit remembers a provider's model refusing a conversation ending
/// with the assistant's message (the reference's `prefillRefused`): keyed
/// on the model, so one model's refusal doesn't tar the provider's
/// others.
pub(crate) fn prefill_refused_field(model: &str) -> String {
    format!("prefill\u{0}{model}")
}

/// Whether an upstream's answer refuses a conversation ending with the
/// assistant's message (the reference's `prefillRefusal`/`refusesPrefill`):
/// a 400 that names prefill or a trailing model turn.
pub(crate) fn refuses_prefill(status: u16, body: &Value) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)does not support (?:assistant message )?prefill|requests ending with a model turn are not supported",
        )
        .expect("prefill refusal regex")
    });
    (status == 400 || status == 422) && re.is_match(&body.to_string())
}

/// How the upstream a reply is translated to goes on from the part of it
/// the client has, sent back as the conversation's last assistant
/// message (the reference's `prefillHow`/`chatPrefill`). An Anthropic
/// Messages upstream goes on natively. A Chat one only in its vendor's
/// own mode: DeepSeek's `prefix` (its own API, where the mode is served
/// under /beta) or Kimi's `partial`. On a loopback or private relay the
/// model's own name says which, as the relay forwards either mode to the
/// vendor it fronts. `""` where the upstream answers the message again
/// from the start instead.
pub(crate) fn prefill_how(provider: &ProviderConfig, model: &str) -> &'static str {
    match provider.protocol {
        Protocol::Anthropic => "anthropic",
        Protocol::Chat => chat_prefill(provider, model),
        _ => "",
    }
}

fn chat_prefill(provider: &ProviderConfig, model: &str) -> &'static str {
    let host = reqwest::Url::parse(&provider.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
        .to_lowercase();
    if host == "api.deepseek.com" || host.ends_with(".deepseek.com") {
        return "prefix";
    }
    if host.contains("moonshot") || host.contains("kimi") {
        return "partial";
    }
    // A relay on this machine or the LAN fronts the vendor: the model
    // name says which mode it forwards.
    let local = host == "localhost"
        || host.ends_with(".local")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback() || is_private(ip))
            .unwrap_or_else(|_| {
                // A host that resolves to a private/loopback address counts.
                format!("{host}:443")
                    .to_socket_addrs()
                    .map(|mut addrs| addrs.any(|a| a.ip().is_loopback() || is_private(a.ip())))
                    .unwrap_or(false)
            });
    if !local {
        return "";
    }
    let model = model.to_lowercase();
    if model.contains("deepseek") {
        return "prefix";
    }
    if model.contains("kimi") {
        return "partial";
    }
    ""
}

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private(),
        std::net::IpAddr::V6(v6) => {
            v6.segments()[0] & 0xfe00 == 0xfc00 // fc00::/7 unique-local
        }
    }
}

/// What of a translated reply the client already has, so a cut reply's
/// next ask goes on from there and only what is new goes to the client.
pub(crate) struct Continuation {
    think: String,
    text: String,
    /// A tool call was begun: the reply can't go on.
    tools: bool,
    /// How the upstream goes on from a prefill (prefill_how), "" where it
    /// can't.
    pub(crate) mode: &'static str,
    /// This try is a continuation.
    pub(crate) resume: bool,
    /// The text it was prefilled with: a full echo of it is dropped.
    echo: String,
    /// What of a possible echo has come.
    seen: String,
}

impl Continuation {
    pub(crate) fn new(mode: &'static str) -> Self {
        Self {
            think: String::new(),
            text: String::new(),
            tools: false,
            mode,
            resume: false,
            echo: String::new(),
            seen: String::new(),
        }
    }

    /// Journal an event the client has.
    pub(crate) fn add(&mut self, event: &magpie_core::stream::StreamEvent) {
        use magpie_core::stream::StreamEvent as E;
        match event {
            E::ThinkingDelta { text } => self.think.push_str(text),
            E::TextDelta { text } => self.text.push_str(text),
            E::ToolCallStart { .. } | E::ToolCallDelta { .. } => self.tools = true,
            _ => {}
        }
    }

    /// Whether the reply can go on: the upstream goes on from a prefill
    /// at all, the client has something of it, and no tool call of it
    /// was begun.
    pub(crate) fn possible(&self) -> bool {
        !self.mode.is_empty() && !self.tools && (!self.think.is_empty() || !self.text.is_empty())
    }

    /// Whether the cut reply goes on with another try, and readies it:
    /// not a refusal (a 4xx the same ask gets again), the tries not up.
    /// `status` is the failure's HTTP status, 0 for a transport cut.
    pub(crate) fn again(&mut self, status: u16, refusal: bool, tries: usize) -> bool {
        if refusal
            || status == 400
            || status == 422
            || status == 404
            || tries >= STREAM_RETRIES
            || !self.possible()
        {
            return false;
        }
        self.resume = true;
        self.echo = self.prefill_text();
        self.seen.clear();
        true
    }

    /// The reply's text as the prefill carries it: trailing whitespace
    /// trimmed, which an Anthropic upstream turns the whole request away
    /// for ("final assistant content cannot end with trailing
    /// whitespace"). The client has it already, so the reply loses
    /// nothing.
    fn prefill_text(&self) -> String {
        self.text.trim_end().to_owned()
    }

    /// `orig` with what the client has of the reply sent back as the
    /// last message, for the model to go on from.
    pub(crate) fn request(&self, orig: &CompletionRequest) -> CompletionRequest {
        let mut request = orig.clone();
        let mut parts = Vec::new();
        if !self.think.is_empty() {
            parts.push(Part::thinking(self.think.clone(), ""));
        }
        let text = self.prefill_text();
        if !text.is_empty() {
            parts.push(Part::text(text));
        }
        request.messages.push(Message {
            role: Role::Assistant,
            parts,
        });
        request
    }

    /// The encoded body of a continuation request, marked with the
    /// upstream's own prefill mode: DeepSeek's `prefix` (served under
    /// /beta), Kimi's `partial`, and the reasoning the client has goes
    /// with it where the upstream reads `reasoning_content` back. An
    /// Anthropic upstream takes the trailing assistant message natively,
    /// so nothing is marked there.
    pub(crate) fn mark(&self, body: &mut Value) {
        if self.mode == "anthropic" || self.mode.is_empty() {
            return;
        }
        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        // A thinking-only prefill's message is a silent assistant the
        // encoder leaves out: put it back marked, so the model still goes
        // on from the reasoning the client has.
        let last = messages
            .iter_mut()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
        if last.is_none() {
            let text = self.prefill_text();
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::String(text) },
            }));
        }
        let last = messages
            .iter_mut()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .expect("the prefill message is there");
        match self.mode {
            "prefix" => {
                last["prefix"] = Value::Bool(true);
            }
            "partial" => {
                last["partial"] = Value::Bool(true);
            }
            _ => {}
        }
        if !self.think.is_empty() {
            last["reasoning_content"] = Value::String(self.think.clone());
        }
    }

    /// Whether the continuation's ask goes to a different path: DeepSeek
    /// serves its prefix mode under /beta, beside the /v1 the base
    /// carries (the reference's `req.Resume` path rewrite).
    pub(crate) fn base_url<'a>(&self, provider: &'a ProviderConfig) -> String {
        let base = provider.base_url.trim_end_matches('/');
        if self.mode == "prefix" {
            if let Some(root) = base.strip_suffix("/v1") {
                return format!("{root}/beta");
            }
        }
        base.to_owned()
    }

    /// Drop a full echo of the text the reply was prefilled with,
    /// holding what could still be one until it reads either way. What
    /// the model says instead of echoing goes whole.
    pub(crate) fn unecho(&mut self, delta: String) -> String {
        if self.echo.is_empty() {
            return delta;
        }
        self.seen.push_str(&delta);
        if self.seen.len() < self.echo.len() && self.echo.starts_with(&self.seen) {
            return String::new();
        }
        let out = if self.seen.starts_with(&self.echo) {
            self.seen.split_off(self.echo.len())
        } else {
            std::mem::take(&mut self.seen)
        };
        self.echo.clear();
        self.seen.clear();
        out
    }
}
