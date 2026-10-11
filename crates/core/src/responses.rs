//! OpenAI Responses: decode requests into the IR, encode them out, decode
//! replies and streamed events, and encode streamed events back out.
//!
//! Spoken by Codex as a client and by OpenAI-compatible vendors that serve
//! the newer endpoint (`POST /v1/responses`).

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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- requests

/// Decode an OpenAI Responses request body. Unknown fields (reasoning
/// effort, include, store, prompts) are dropped: the gateway is stateless.
pub fn decode_request(v: &Value) -> Result<CompletionRequest> {
    let model = str_field(v, "model")?;
    let mut system = Vec::new();
    if let Some(instructions) = v.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        system.push(Part::text(instructions));
    }

    let mut messages: Vec<Message> = Vec::new();
    let input = v.get("input").unwrap_or(&Value::Null);
    match input {
        Value::String(s) => messages.push(Message {
            role: Role::User,
            parts: vec![Part::text(s.clone())],
        }),
        Value::Array(items) => {
            for item in items {
                decode_input_item(item, &mut messages);
            }
        }
        _ => {}
    }

    let tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .filter(|t| t.get("type").is_none_or(|x| x == "function"))
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
                    parameters: t.get("parameters").cloned().unwrap_or_else(|| json!({})),
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
        max_tokens: v.get("max_output_tokens").and_then(Value::as_u64),
        temperature: v.get("temperature").and_then(Value::as_f64),
        top_p: v.get("top_p").and_then(Value::as_f64),
        stop: Vec::new(),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// One item of a Responses `input` array onto the message list.
fn decode_input_item(item: &Value, messages: &mut Vec<Message>) {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match ty {
        "message" | "" => {
            let role = Role::parse(item.get("role").and_then(Value::as_str).unwrap_or("user"));
            let mut parts = Vec::new();
            match item.get("content") {
                Some(Value::String(s)) => parts.push(Part::text(s.clone())),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str).unwrap_or_default() {
                            "input_text" | "output_text" | "text" | "summary_text" => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    parts.push(Part::text(t));
                                }
                            }
                            "input_image" => {
                                if let Some(u) = b
                                    .get("image_url")
                                    .map(|x| {
                                        x.as_str()
                                            .map(str::to_owned)
                                            .unwrap_or_else(|| x.to_string())
                                    })
                                    .or_else(|| {
                                        b.get("url").and_then(Value::as_str).map(str::to_owned)
                                    })
                                {
                                    if let Some(rest) = u.strip_prefix("data:")
                                        && let Some((meta, data)) = rest.split_once(',')
                                        && let Some(media) = meta.strip_suffix(";base64")
                                    {
                                        parts.push(Part::image(media, data));
                                        continue;
                                    }
                                    parts.push(Part::image_url(u));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if !parts.is_empty() {
                messages.push(Message { role, parts });
            }
        }
        "function_call" => {
            let args_raw = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let args: Value =
                serde_json::from_str(args_raw).unwrap_or(Value::String(args_raw.to_owned()));
            messages.push(Message {
                role: Role::Assistant,
                parts: vec![Part::tool_call(
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    args,
                )],
            });
        }
        "function_call_output" => {
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            let text = match &output {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            messages.push(Message {
                role: Role::User,
                parts: vec![Part::tool_result(
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    text,
                    false,
                )],
            });
        }
        "reasoning" => {
            let text = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            messages.push(Message {
                role: Role::Assistant,
                parts: vec![Part::thinking(text, "")],
            });
        }
        _ => {}
    }
}

/// Encode an IR request as an OpenAI Responses body, asking for `model`.
pub fn encode_request(req: &CompletionRequest, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    let system: String = req
        .system
        .iter()
        .filter(|p| p.kind() == PartKind::Text)
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !system.is_empty() {
        out.insert("instructions".into(), json!(system));
    }

    let mut input: Vec<Value> = Vec::new();
    let mut open_message: Option<(String, Vec<Value>)> = None;
    for m in &crate::ir::join_split_calls(&req.messages) {
        let role = m.role.as_str().to_owned();
        for p in &m.parts {
            match p.kind() {
                PartKind::Text => {
                    let entry = open_message.get_or_insert_with(|| (role.clone(), Vec::new()));
                    entry.1.push(json!({
                        "type": if m.role == Role::Assistant { "output_text" } else { "input_text" },
                        "text": p.text,
                    }));
                }
                PartKind::Image if !p.data.is_empty() => {
                    let entry = open_message.get_or_insert_with(|| (role.clone(), Vec::new()));
                    entry.1.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", p.media_type, p.data)
                    }));
                }
                PartKind::Image if !p.url.is_empty() => {
                    let entry = open_message.get_or_insert_with(|| (role.clone(), Vec::new()));
                    entry
                        .1
                        .push(json!({"type": "input_image", "image_url": p.url}));
                }
                PartKind::Thinking => {
                    if let Some((role, content)) = open_message.take() {
                        input.push(json!({"type": "message", "role": role, "content": content}));
                    }
                    input.push(json!({
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": p.text}],
                    })); // signature fidelity is still on the roadmap
                }
                PartKind::ToolCall => {
                    if let Some((role, content)) = open_message.take() {
                        input.push(json!({"type": "message", "role": role, "content": content}));
                    }
                    input.push(json!({
                        "type": "function_call",
                        "call_id": p.id,
                        "name": p.name,
                        "arguments": p.args.to_string(),
                    }));
                }
                PartKind::ToolResult => {
                    if let Some((role, content)) = open_message.take() {
                        input.push(json!({"type": "message", "role": role, "content": content}));
                    }
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": p.call_id,
                        "output": p.text,
                    }));
                }
                PartKind::Image => {}
            }
        }
        if let Some((role, content)) = open_message.take() {
            input.push(json!({"type": "message", "role": role, "content": content}));
        }
    }
    out.insert("input".into(), Value::Array(input));

    if !req.tools.is_empty() {
        out.insert(
            "tools".into(),
            Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                            "strict": false,
                        })
                    })
                    .collect(),
            ),
        );
        out.insert(
            "tool_choice".into(),
            req.tool_choice.clone().unwrap_or(json!("auto")),
        );
    }
    if let Some(max) = req.max_tokens {
        out.insert("max_output_tokens".into(), json!(max));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    out.insert("stream".into(), json!(req.stream));
    out.insert("store".into(), json!(false));
    Value::Object(out)
}

// ---------------------------------------------------------------- replies

fn decode_usage(v: &Value) -> Usage {
    Usage {
        input: v.get("input_tokens").and_then(Value::as_u64),
        output: v.get("output_tokens").and_then(Value::as_u64),
        cache_read: v
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write: None,
    }
}

fn usage_to_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input.unwrap_or(0),
        "output_tokens": u.output.unwrap_or(0),
        "total_tokens": u.input.unwrap_or(0) + u.output.unwrap_or(0),
    })
}

/// Decode an OpenAI Responses reply body.
pub fn decode_response(v: &Value) -> Result<CompletionResponse> {
    if v.get("status") == Some(&json!("failed")) {
        let detail = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("responses upstream error");
        return Err(CoreError::Invalid {
            field: "error",
            detail: detail.to_owned(),
        });
    }
    let model = str_field(v, "model")?;
    let mut parts = Vec::new();
    let mut has_tool_call = false;
    for item in v
        .get("output")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                for b in item
                    .get("content")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    if b.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(t) = b.get("text").and_then(Value::as_str)
                    {
                        parts.push(Part::text(t));
                    }
                }
            }
            "function_call" => {
                has_tool_call = true;
                let args_raw = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let args: Value =
                    serde_json::from_str(args_raw).unwrap_or(Value::String(args_raw.to_owned()));
                parts.push(Part::tool_call(
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    args,
                ));
            }
            "reasoning" => {
                let text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                parts.push(Part::thinking(text, ""));
            }
            _ => {}
        }
    }
    let stop = if v.get("status") == Some(&json!("incomplete")) {
        StopReason::MaxTokens
    } else if has_tool_call {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };
    let usage = v.get("usage").map(decode_usage).unwrap_or_default();
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

fn response_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("resp_magpie{now}")
}

/// Encode an IR reply as an OpenAI Responses body.
pub fn encode_response(resp: &CompletionResponse) -> Value {
    let mut output: Vec<Value> = Vec::new();
    for p in &resp.message.parts {
        match p.kind() {
            PartKind::Text => output.push(json!({
                "type": "message",
                "id": format!("msg_{}", response_id()),
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": p.text, "annotations": []}],
            })),
            PartKind::ToolCall => output.push(json!({
                "type": "function_call",
                "id": format!("fc_{}", response_id()),
                "call_id": p.id,
                "name": p.name,
                "arguments": p.args.to_string(),
                "status": "completed",
            })),
            PartKind::Thinking => output.push(json!({
                "type": "reasoning",
                "id": format!("rs_{}", response_id()),
                "summary": [{"type": "summary_text", "text": p.text}],
            })),
            _ => {}
        }
    }
    let status = if resp.stop == StopReason::MaxTokens {
        "incomplete"
    } else {
        "completed"
    };
    json!({
        "id": response_id(),
        "object": "response",
        "created_at": now_secs(),
        "status": status,
        "model": resp.model,
        "output": output,
        "usage": usage_to_json(&resp.usage),
    })
}

// ---------------------------------------------------------------- streams

/// Decode one streamed event from a Responses upstream into IR events.
/// `[DONE]` never appears: the terminal `response.completed` event is it.
pub fn decode_chunk(v: &Value) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str).unwrap_or_default() {
        "response.created" => {
            if let Some(model) = v.pointer("/response/model").and_then(Value::as_str) {
                out.push(StreamEvent::Start {
                    model: model.to_owned(),
                });
            }
        }
        "response.output_item.added" => {
            let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if v.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: v
                        .pointer("/item/call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: v
                        .pointer("/item/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        "response.output_item.done" => {
            let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if v.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                out.push(StreamEvent::ToolCallEnd { index });
            }
        }
        "response.output_text.delta" => {
            if let Some(t) = v.get("delta").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(StreamEvent::TextDelta { text: t.to_owned() });
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(t) = v.get("delta").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(StreamEvent::ThinkingDelta { text: t.to_owned() });
            }
        }
        "response.function_call_arguments.delta" => {
            let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(a) = v.get("delta").and_then(Value::as_str)
                && !a.is_empty()
            {
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    args: a.to_owned(),
                });
            }
        }
        "response.completed" | "response.incomplete" => {
            let incomplete = v.get("type").and_then(Value::as_str) == Some("response.incomplete");
            if let Some(u) = v.pointer("/response/usage") {
                out.push(StreamEvent::Usage(decode_usage(u)));
            }
            let has_call = v
                .pointer("/response/output")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .any(|i| i.get("type") == Some(&json!("function_call")))
                })
                .unwrap_or(false);
            let stop = if incomplete {
                StopReason::MaxTokens
            } else if has_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            out.push(StreamEvent::Stop(stop));
            out.push(StreamEvent::Done);
        }
        "response.failed" => {
            let detail = v
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .unwrap_or("responses upstream error");
            out.push(StreamEvent::Error {
                message: detail.to_owned(),
            });
        }
        _ => {}
    }
    out
}

fn sse(v: &Value) -> String {
    // Responses streams carry both the SSE event name and the JSON type;
    // the latter is what protocol decoders route on.
    match v.get("type").and_then(Value::as_str) {
        Some(kind) => format!("event: {kind}\ndata: {v}\n\n"),
        None => format!("data: {v}\n\n"),
    }
}

fn event(kind: &str, fields: Value) -> String {
    let mut v = fields;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("type".into(), json!(kind));
    }
    sse(&v)
}

/// An output item the encoder has open: a text message or a reasoning item.
#[derive(Debug)]
struct OpenItem {
    id: String,
    output_index: usize,
    text: String,
}

/// A tool call the encoder is streaming.
#[derive(Debug)]
struct ToolItem {
    id: String,
    call_id: String,
    name: String,
    args: String,
    output_index: usize,
}

/// Encodes IR stream events back out as an OpenAI Responses SSE stream:
/// `response.created`, output items added and done, text and argument
/// deltas, and a final `response.completed` carrying the output and usage.
#[derive(Debug)]
pub struct ResponsesStreamEncoder {
    model: String,
    response_id: String,
    created_at: u64,
    sequence: u64,
    next_index: usize,
    open_message: Option<OpenItem>,
    open_reasoning: Option<OpenItem>,
    tools: HashMap<usize, ToolItem>,
    output: Vec<Value>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    started: bool,
    completed: bool,
}

impl ResponsesStreamEncoder {
    /// An encoder for a client that asked for `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            response_id: response_id(),
            created_at: now_secs(),
            sequence: 0,
            next_index: 0,
            open_message: None,
            open_reasoning: None,
            tools: HashMap::new(),
            output: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
            started: false,
            completed: false,
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.sequence += 1;
        format!("{prefix}_magpie{}_{}", self.response_id, self.sequence)
    }

    fn response_skeleton(&self, status: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.response_id));
        m.insert("object".into(), json!("response"));
        m.insert("created_at".into(), json!(self.created_at));
        m.insert("status".into(), json!(status));
        m.insert("model".into(), json!(self.model));
        m.insert("output".into(), Value::Array(self.output.clone()));
        m
    }

    fn ensure_created(&mut self) -> String {
        if self.started {
            return String::new();
        }
        self.started = true;
        event(
            "response.created",
            json!({"response": self.response_skeleton("in_progress"), "sequence_number": 0}),
        )
    }

    fn close_message(&mut self) -> String {
        let Some(item) = self.open_message.take() else {
            return String::new();
        };
        let done = json!({
            "id": item.id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": item.text, "annotations": []}],
        });
        self.output.push(done.clone());
        event(
            "response.output_item.done",
            json!({"output_index": item.output_index, "item": done}),
        )
    }

    fn close_reasoning(&mut self) -> String {
        let Some(item) = self.open_reasoning.take() else {
            return String::new();
        };
        let done = json!({
            "id": item.id,
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": item.text}],
        });
        self.output.push(done.clone());
        event(
            "response.output_item.done",
            json!({"output_index": item.output_index, "item": done}),
        )
    }

    fn close_tools(&mut self) -> String {
        let mut out = String::new();
        let indexes: Vec<usize> = self.tools.keys().copied().collect();
        for index in indexes {
            if let Some(frame) = self.encode(&StreamEvent::ToolCallEnd { index }) {
                out.push_str(&frame);
            }
        }
        out
    }

    fn complete(&mut self, reason: StopReason) -> String {
        let mut out = String::new();
        out.push_str(&self.close_message());
        out.push_str(&self.close_reasoning());
        out.push_str(&self.close_tools());
        let status = if reason == StopReason::MaxTokens {
            "incomplete"
        } else {
            "completed"
        };
        let mut response = self.response_skeleton(status);
        if reason == StopReason::MaxTokens {
            response.insert(
                "incomplete_details".into(),
                json!({"reason": "max_output_tokens"}),
            );
        }
        response.insert("usage".into(), usage_to_json(&self.usage));
        self.completed = true;
        out.push_str(&event("response.completed", json!({"response": response})));
        out
    }

    /// Encode one event; `None` when the dialect has nothing to say.
    pub fn encode(&mut self, ev: &StreamEvent) -> Option<String> {
        match ev {
            StreamEvent::Start { model } => {
                if !model.is_empty() {
                    self.model = model.clone();
                }
                Some(self.ensure_created())
            }
            StreamEvent::TextDelta { text } => {
                let mut out = self.ensure_created();
                if self.open_message.is_none() {
                    out.push_str(&self.close_reasoning());
                    let id = self.next_id("msg");
                    let index = self.next_index;
                    self.next_index += 1;
                    out.push_str(&event(
                        "response.output_item.added",
                        json!({"output_index": index, "item": {
                            "id": id, "type": "message", "role": "assistant",
                            "status": "in_progress", "content": []
                        }}),
                    ));
                    out.push_str(&event(
                        "response.content_part.added",
                        json!({"item_id": id, "output_index": index, "content_index": 0,
                               "part": {"type": "output_text", "text": "", "annotations": []}}),
                    ));
                    self.open_message = Some(OpenItem {
                        id,
                        output_index: index,
                        text: String::new(),
                    });
                }
                let item = self.open_message.as_ref().expect("just opened");
                out.push_str(&event(
                    "response.output_text.delta",
                    json!({"item_id": item.id, "output_index": item.output_index,
                           "content_index": 0, "delta": text}),
                ));
                self.open_message
                    .as_mut()
                    .expect("just opened")
                    .text
                    .push_str(text);
                Some(out)
            }
            StreamEvent::ThinkingDelta { text } => {
                let mut out = self.ensure_created();
                if self.open_reasoning.is_none() {
                    out.push_str(&self.close_message());
                    let id = self.next_id("rs");
                    let index = self.next_index;
                    self.next_index += 1;
                    out.push_str(&event(
                        "response.output_item.added",
                        json!({"output_index": index, "item": {"id": id, "type": "reasoning", "summary": []}}),
                    ));
                    self.open_reasoning = Some(OpenItem {
                        id,
                        output_index: index,
                        text: String::new(),
                    });
                }
                let item = self.open_reasoning.as_ref().expect("just opened");
                out.push_str(&event(
                    "response.reasoning_summary_text.delta",
                    json!({"item_id": item.id, "output_index": item.output_index,
                           "summary_index": 0, "delta": text}),
                ));
                self.open_reasoning
                    .as_mut()
                    .expect("just opened")
                    .text
                    .push_str(text);
                Some(out)
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                let mut out = self.ensure_created();
                out.push_str(&self.close_message());
                out.push_str(&self.close_reasoning());
                let item_id = self.next_id("fc");
                let output_index = self.next_index;
                self.next_index += 1;
                self.tools.insert(
                    *index,
                    ToolItem {
                        id: item_id.clone(),
                        call_id: id.clone(),
                        name: name.clone(),
                        args: String::new(),
                        output_index,
                    },
                );
                out.push_str(&event(
                    "response.output_item.added",
                    json!({"output_index": output_index, "item": {
                        "id": item_id, "type": "function_call", "call_id": id,
                        "name": name, "arguments": "", "status": "in_progress"
                    }}),
                ));
                Some(out)
            }
            StreamEvent::ToolCallDelta { index, args } => {
                let tool = self.tools.get(index)?;
                Some(event(
                    "response.function_call_arguments.delta",
                    json!({"item_id": tool.id, "output_index": tool.output_index, "delta": args}),
                ))
            }
            StreamEvent::ToolCallEnd { index } => {
                let tool = self.tools.remove(index)?;
                let done = json!({
                    "id": tool.id, "type": "function_call", "call_id": tool.call_id,
                    "name": tool.name, "arguments": tool.args, "status": "completed",
                });
                self.output.push(done.clone());
                Some(event(
                    "response.output_item.done",
                    json!({"output_index": tool.output_index, "item": done}),
                ))
            }
            StreamEvent::Usage(u) => {
                self.usage.merge(u);
                None
            }
            StreamEvent::Stop(reason) => {
                // Chat streams may deliver usage after the finish chunk;
                // keep the response open until Done so it is not lost.
                self.stop_reason = Some(*reason);
                None
            }
            StreamEvent::Done => {
                if self.completed {
                    return None;
                }
                let reason = self.stop_reason.take().unwrap_or(StopReason::EndTurn);
                Some(self.complete(reason))
            }
            StreamEvent::Error { message } => {
                let mut out = self.ensure_created();
                let mut response = self.response_skeleton("failed");
                response.insert(
                    "error".into(),
                    json!({"code": "upstream_error", "message": message}),
                );
                out.push_str(&event("response.failed", json!({"response": response})));
                Some(out)
            }
        }
    }
}
