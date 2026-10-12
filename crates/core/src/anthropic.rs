//! Anthropic Messages: decode requests into the IR, encode them out,
//! decode replies and streamed events, and encode streamed events back out.
//!
//! Spoken by Claude Code as a client and by Anthropic-compatible vendors
//! as an upstream.

use std::collections::HashMap;

use crate::error::{CoreError, Result};
use crate::ir::{
    CompletionRequest, CompletionResponse, Message, Part, PartKind, Role, StopReason, Tool, Usage,
};
use crate::stream::StreamEvent;
use serde_json::{Map, Value, json};

fn missing(field: &'static str) -> CoreError {
    CoreError::MissingField(field)
}

fn str_field(v: &Value, field: &'static str) -> Result<String> {
    v.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(missing(field))
}

/// Decode an Anthropic content block into a part.
fn decode_block(b: &Value) -> Option<Part> {
    match b.get("type").and_then(Value::as_str) {
        Some("text") => b.get("text").and_then(Value::as_str).map(Part::text),
        Some("thinking") => Some(Part::thinking(
            b.get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            b.get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )),
        Some("redacted_thinking") => {
            // The sealed payload cannot cross protocols; keep the block as
            // empty thinking so the turn shape survives. TODO: pass sealed
            // reasoning through when both ends speak Anthropic.
            Some(Part::thinking("", ""))
        }
        Some("image") => {
            let src = b.get("source")?;
            match src.get("type").and_then(Value::as_str) {
                Some("base64") => Some(Part::image(
                    src.get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png"),
                    src.get("data").and_then(Value::as_str).unwrap_or_default(),
                )),
                Some("url") => src.get("url").and_then(Value::as_str).map(Part::image_url),
                _ => None,
            }
        }
        Some("tool_use") => Some(Part::tool_call(
            b.get("id").and_then(Value::as_str).unwrap_or_default(),
            b.get("name").and_then(Value::as_str).unwrap_or_default(),
            b.get("input").cloned().unwrap_or_else(|| json!({})),
        )),
        Some("tool_result") => {
            let content = b.get("content").unwrap_or(&Value::Null);
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|x| x.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            Some(Part::tool_result(
                b.get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                text,
                b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            ))
        }
        _ => None,
    }
}

/// Decode an Anthropic `content` field (string or block array) into parts.
fn decode_content(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::text(s.clone())],
        Value::Array(blocks) => blocks.iter().filter_map(decode_block).collect(),
        _ => Vec::new(),
    }
}

fn part_to_block(p: &Part) -> Option<Value> {
    match p.kind() {
        PartKind::Text => Some(json!({"type": "text", "text": p.text})),
        PartKind::Thinking => Some(json!({
            "type": "thinking",
            "thinking": p.text,
            "signature": p.signature,
        })),
        PartKind::Image if !p.data.is_empty() => Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": p.media_type, "data": p.data}
        })),
        PartKind::Image if !p.url.is_empty() => {
            Some(json!({"type": "image", "source": {"type": "url", "url": p.url}}))
        }
        PartKind::ToolCall => Some(json!({
            "type": "tool_use",
            "id": p.id,
            "name": p.name,
            "input": p.args,
        })),
        _ => None,
    }
}

/// Decode an Anthropic Messages request body.
pub fn decode_request(v: &Value) -> Result<CompletionRequest> {
    let model = str_field(v, "model")?;
    let system = v.get("system").map(decode_content).unwrap_or_default();
    let mut messages = Vec::new();
    for m in v
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(missing("messages"))?
    {
        let role = Role::parse(m.get("role").and_then(Value::as_str).unwrap_or("user"));
        messages.push(Message {
            role,
            parts: decode_content(m.get("content").unwrap_or(&Value::Null)),
        });
    }
    let tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .filter(|t| t.get("type").is_none_or(|x| x == "custom"))
                .map(|t| Tool {
                    name: t
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    parameters: t.get("input_schema").cloned().unwrap_or_else(|| json!({})),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(CompletionRequest {
        model,
        system,
        messages,
        tools,
        tool_choice: v.get("tool_choice").filter(|t| !t.is_null()).cloned(),
        max_tokens: v.get("max_tokens").and_then(Value::as_u64),
        temperature: v.get("temperature").and_then(Value::as_f64),
        top_p: v.get("top_p").and_then(Value::as_f64),
        stop: v
            .get("stop_sequences")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Encode an IR request as an Anthropic Messages body, asking for `model`
/// upstream (the routing-resolved name).
pub fn encode_request(req: &CompletionRequest, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("max_tokens".into(), json!(req.max_tokens.unwrap_or(16384)));
    if !req.system.is_empty() {
        let text: String = req
            .system
            .iter()
            .filter(|p| p.kind() == PartKind::Text)
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let blocks: Vec<Value> = req.system.iter().filter_map(part_to_block).collect();
        if !text.is_empty() {
            out.insert("system".into(), json!(text));
        } else if !blocks.is_empty() {
            out.insert("system".into(), Value::Array(blocks));
        }
    }
    let mut messages = Vec::new();
    for m in &crate::ir::join_split_calls(&req.messages) {
        let mut blocks: Vec<Value> = Vec::new();
        if m.role == Role::User {
            // tool_result blocks come first: Anthropic wants each answered
            // call inside the user turn that follows it.
            for p in &m.parts {
                if p.kind() == PartKind::ToolResult {
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": p.call_id,
                        "content": p.text,
                        "is_error": p.is_error,
                    }));
                }
            }
        }
        for p in &m.parts {
            match p.kind() {
                PartKind::ToolCall => blocks.push(json!({
                    "type": "tool_use",
                    "id": p.id,
                    "name": p.name,
                    "input": p.args,
                })),
                PartKind::ToolResult => {}
                _ => {
                    if let Some(b) = part_to_block(p) {
                        blocks.push(b);
                    }
                }
            }
        }
        if blocks.is_empty() {
            blocks.push(json!({"type": "text", "text": ""}));
        }
        messages.push(json!({"role": m.role.as_str(), "content": blocks}));
    }
    out.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        out.insert(
            "tools".into(),
            Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            ),
        );
        out.insert(
            "tool_choice".into(),
            req.tool_choice.clone().unwrap_or(json!({"type": "auto"})),
        );
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if !req.stop.is_empty() {
        out.insert("stop_sequences".into(), json!(req.stop));
    }
    out.insert("stream".into(), json!(req.stream));
    Value::Object(out)
}

fn decode_usage(v: &Value) -> Usage {
    Usage {
        input: v.get("input_tokens").and_then(Value::as_u64),
        output: v.get("output_tokens").and_then(Value::as_u64),
        cache_read: v.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_write: v.get("cache_creation_input_tokens").and_then(Value::as_u64),
    }
}

fn usage_to_json(u: &Usage) -> Value {
    let mut usage = Map::new();
    if let Some(x) = u.input {
        usage.insert("input_tokens".into(), json!(x));
    }
    if let Some(x) = u.output {
        usage.insert("output_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_read {
        usage.insert("cache_read_input_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_write {
        usage.insert("cache_creation_input_tokens".into(), json!(x));
    }
    Value::Object(usage)
}

/// Read reported usage even when a refusal omits the ordinary reply fields.
pub fn response_usage(v: &Value) -> Usage {
    v.get("usage").map(decode_usage).unwrap_or_default()
}

/// Decode an Anthropic Messages reply body.
pub fn decode_response(v: &Value) -> Result<CompletionResponse> {
    if v.get("type") == Some(&json!("error")) {
        let detail = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("anthropic upstream error");
        return Err(CoreError::Invalid {
            field: "error",
            detail: detail.to_owned(),
        });
    }
    let model = str_field(v, "model")?;
    let parts = decode_content(v.get("content").unwrap_or(&Value::Null));
    let stop = v
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(StopReason::from_anthropic)
        .unwrap_or_default();
    let usage = response_usage(v);
    Ok(CompletionResponse {
        model,
        message: Message {
            role: Role::Assistant,
            parts,
        },
        stop,
        usage,
    })
}

fn message_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("msg_magpie{now}")
}

/// Encode an IR reply as an Anthropic Messages body.
pub fn encode_response(resp: &CompletionResponse) -> Value {
    let content: Vec<Value> = resp
        .message
        .parts
        .iter()
        .filter_map(part_to_block)
        .collect();
    json!({
        "id": message_id(),
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": resp.stop.to_anthropic(),
        "stop_sequence": Value::Null,
        "usage": usage_to_json(&resp.usage),
    })
}

/// Decodes streamed Anthropic events into IR events. Stateful: it remembers
/// which content blocks are tool calls so their `content_block_stop` can be
/// told apart from text and thinking blocks.
#[derive(Debug, Default)]
pub struct AnthropicStreamDecoder {
    tool_blocks: HashMap<usize, ()>,
}

impl AnthropicStreamDecoder {
    /// A decoder with no blocks seen yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one event's `data` payload.
    pub fn decode(&mut self, v: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => {
                let model = v
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                out.push(StreamEvent::Start { model });
                if let Some(u) = v.pointer("/message/usage") {
                    out.push(StreamEvent::Usage(decode_usage(u)));
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.tool_blocks.insert(index, ());
                    out.push(StreamEvent::ToolCallStart {
                        index,
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    });
                }
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str)
                            && !t.is_empty()
                        {
                            out.push(StreamEvent::TextDelta { text: t.to_owned() });
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta.get("thinking").and_then(Value::as_str)
                            && !t.is_empty()
                        {
                            out.push(StreamEvent::ThinkingDelta { text: t.to_owned() });
                        }
                    }
                    "input_json_delta" => {
                        if let Some(a) = delta.get("partial_json").and_then(Value::as_str)
                            && !a.is_empty()
                        {
                            out.push(StreamEvent::ToolCallDelta {
                                index,
                                args: a.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.tool_blocks.remove(&index).is_some() {
                    out.push(StreamEvent::ToolCallEnd { index });
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage") {
                    out.push(StreamEvent::Usage(decode_usage(u)));
                }
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    out.push(StreamEvent::Stop(StopReason::from_anthropic(reason)));
                }
            }
            "message_stop" => out.push(StreamEvent::Done),
            "error" => {
                let detail = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("anthropic upstream error");
                out.push(StreamEvent::Error {
                    message: detail.to_owned(),
                });
            }
            _ => {}
        }
        out
    }
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The kind of Anthropic content block currently open in a stream we are
/// encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

/// Encodes IR stream events back out as an Anthropic Messages SSE stream.
/// Stateful: it opens and closes content blocks as the kinds change, and
/// maps IR tool-call indexes to Anthropic block indexes.
#[derive(Debug)]
pub struct AnthropicStreamEncoder {
    model: String,
    id: String,
    started: bool,
    stopped: bool,
    stop_reason: Option<StopReason>,
    next_index: usize,
    open: Option<(usize, BlockKind)>,
    tool_blocks: HashMap<usize, usize>, // IR call index -> block index
    usage: Usage,
}

impl AnthropicStreamEncoder {
    /// An encoder for a client that asked for `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            id: message_id(),
            started: false,
            stopped: false,
            stop_reason: None,
            next_index: 0,
            open: None,
            tool_blocks: HashMap::new(),
            usage: Usage::default(),
        }
    }

    fn start_message(&mut self) -> String {
        if self.started {
            return String::new();
        }
        self.started = true;
        let usage = if self.usage.input.is_some() {
            usage_to_json(&self.usage)
        } else {
            json!({"input_tokens": 0})
        };
        sse(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": usage,
                }
            }),
        )
    }

    fn close_open(&mut self) -> Option<String> {
        let (index, _) = self.open.take()?;
        Some(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ))
    }

    fn open_block(&mut self, kind: BlockKind, extra: Value) -> String {
        let mut out = self.close_open().unwrap_or_default();
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some((index, kind));
        let mut block = Map::new();
        match kind {
            BlockKind::Text => {
                block.insert("type".into(), json!("text"));
                block.insert("text".into(), json!(""));
            }
            BlockKind::Thinking => {
                block.insert("type".into(), json!("thinking"));
                block.insert("thinking".into(), json!(""));
            }
            BlockKind::ToolUse => {
                block.insert("type".into(), json!("tool_use"));
                block.extend(
                    extra
                        .as_object()
                        .map(|m| m.clone().into_iter())
                        .into_iter()
                        .flatten(),
                );
            }
        }
        out.push_str(&sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": block}),
        ));
        out
    }

    /// Encode one event; `None` when there is nothing to send. A return can
    /// carry several SSE frames (a block close, open, and delta).
    pub fn encode(&mut self, ev: &StreamEvent) -> Option<String> {
        match ev {
            StreamEvent::Start { model } => {
                if !model.is_empty() {
                    self.model = model.clone();
                }
                Some(self.start_message())
            }
            StreamEvent::TextDelta { text } => {
                let mut out = self.start_message();
                if self.open.as_ref().map(|(_, k)| *k) != Some(BlockKind::Text) {
                    out.push_str(&self.open_block(BlockKind::Text, Value::Null));
                }
                let index = self.open.as_ref().expect("just opened").0;
                out.push_str(&sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text},
                    }),
                ));
                Some(out)
            }
            StreamEvent::ThinkingDelta { text } => {
                let mut out = self.start_message();
                if self.open.as_ref().map(|(_, k)| *k) != Some(BlockKind::Thinking) {
                    out.push_str(&self.open_block(BlockKind::Thinking, Value::Null));
                }
                let index = self.open.as_ref().expect("just opened").0;
                out.push_str(&sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "thinking_delta", "thinking": text},
                    }),
                ));
                Some(out)
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                let mut out = self.start_message();
                let block_index = self.next_index;
                self.next_index += 1;
                self.tool_blocks.insert(*index, block_index);
                self.open = Some((block_index, BlockKind::ToolUse));
                out.push_str(&sse(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    }),
                ));
                Some(out)
            }
            StreamEvent::ToolCallDelta { index, args } => {
                let block_index = *self.tool_blocks.get(index)?;
                Some(sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {"type": "input_json_delta", "partial_json": args},
                    }),
                ))
            }
            StreamEvent::ToolCallEnd { index } => {
                let block_index = self.tool_blocks.remove(index)?;
                if self.open.as_ref().map(|(i, _)| *i) == Some(block_index) {
                    self.open = None;
                }
                Some(sse(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": block_index}),
                ))
            }
            StreamEvent::Usage(u) => {
                self.usage.merge(u);
                None
            }
            StreamEvent::Stop(reason) => {
                self.stopped = true;
                self.stop_reason = Some(*reason);
                self.close_open()
            }
            StreamEvent::Done => {
                let mut out = String::new();
                if !self.started {
                    out.push_str(&self.start_message());
                }
                if !self.stopped {
                    out.push_str(&self.close_open().unwrap_or_default());
                    self.stopped = true;
                    self.stop_reason = Some(StopReason::EndTurn);
                }
                if let Some(reason) = self.stop_reason {
                    let mut usage = Map::new();
                    if let Some(x) = self.usage.input {
                        usage.insert("input_tokens".into(), json!(x));
                    }
                    usage.insert(
                        "output_tokens".into(),
                        json!(self.usage.output.unwrap_or(0)),
                    );
                    out.push_str(&sse(
                        "message_delta",
                        &json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": reason.to_anthropic(), "stop_sequence": Value::Null},
                            "usage": Value::Object(usage),
                        }),
                    ));
                }
                out.push_str(&sse("message_stop", &json!({"type": "message_stop"})));
                Some(out)
            }
            StreamEvent::Error { message } => Some(sse(
                "error",
                &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
            )),
        }
    }
}
