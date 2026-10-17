//! OpenAI Chat Completions: decode requests into the IR, encode them out,
//! decode replies and streamed chunks, and encode streamed chunks back out.
//!
//! This shape is spoken by most vendors (DeepSeek, Kimi, GLM, OpenRouter,
//! Ollama, ...) and by agents on the OpenAI side, so it is both an upstream
//! and a client dialect.

use crate::error::{CoreError, Result};
use crate::ir::{
    CompletionRequest, CompletionResponse, Message, Part, PartKind, Role, StopReason, Tool, Usage,
};
use crate::stream::StreamEvent;
use serde_json::{Map, Value, json};

mod tool_pairing;

fn missing(field: &'static str) -> CoreError {
    CoreError::MissingField(field)
}

fn str_field(v: &Value, field: &'static str) -> Result<String> {
    v.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(missing(field))
}

/// Split a `data:` URL into its media type and base64 payload.
fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type.to_owned(), data.to_owned()))
}

/// Decode a chat `content` field (string, block array, or null) into parts.
fn decode_content(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::text(s.clone())],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(Part::text),
                Some("image_url") => b
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .map(|u| match split_data_url(u) {
                        Some((media_type, data)) => Part::image(media_type, data),
                        None => Part::image_url(u),
                    }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn part_to_content_block(p: &Part) -> Value {
    match p.kind() {
        PartKind::Text => json!({"type": "text", "text": p.text}),
        PartKind::Image if !p.data.is_empty() => json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{};base64,{}", p.media_type, p.data)}
        }),
        PartKind::Image => json!({"type": "image_url", "image_url": {"url": p.url}}),
        _ => Value::Null,
    }
}

/// Decode an OpenAI Chat Completions request body.
pub fn decode_request(v: &Value) -> Result<CompletionRequest> {
    let model = str_field(v, "model")?;
    let mut system = Vec::new();
    let mut messages = Vec::new();

    let msgs = v
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(missing("messages"))?;
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m.get("content").unwrap_or(&Value::Null);
        match role {
            "system" | "developer" => system.extend(decode_content(content)),
            "user" => messages.push(Message {
                role: Role::User,
                parts: decode_content(content),
            }),
            "assistant" => {
                let mut parts = decode_content(content);
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
                        let name = c
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let args_raw = c
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let args: Value = serde_json::from_str(args_raw)
                            .unwrap_or(Value::String(args_raw.to_owned()));
                        parts.push(Part::tool_call(id, name, args));
                    }
                }
                messages.push(Message {
                    role: Role::Assistant,
                    parts,
                });
            }
            "tool" => {
                let call_id = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let is_error = m.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let (text, images) = crate::tool_output::decode(content);
                let mut result = Part::tool_result(call_id, text, is_error);
                result.images = images;
                messages.push(Message {
                    role: Role::User,
                    parts: vec![result],
                });
            }
            _ => {}
        }
    }

    let tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
                .filter_map(|t| {
                    let f = t.get("function")?;
                    Some(Tool {
                        name: f.get("name").and_then(Value::as_str)?.to_owned(),
                        description: f
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        namespace: None,
                        parameters: f.get("parameters").cloned().unwrap_or_else(|| json!({})),
                        custom: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(CompletionRequest {
        model,
        system,
        messages,
        tools,
        web_search: crate::search::WebSearchRequest::decode(
            crate::search::SearchProtocol::Chat,
            v,
        )?,
        tool_choice: v.get("tool_choice").filter(|t| !t.is_null()).cloned(),
        tool_choice_protocol: Some(crate::ir::ToolChoiceProtocol::Chat),
        max_tokens: v.get("max_tokens").and_then(Value::as_u64),
        temperature: v.get("temperature").and_then(Value::as_f64),
        top_p: v.get("top_p").and_then(Value::as_f64),
        stop: v
            .get("stop")
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

/// Encode an IR request as an OpenAI Chat Completions body, asking for
/// `model` upstream (the routing-resolved name, not the agent's spelling).
pub fn encode_request(req: &CompletionRequest, model: &str) -> Result<Value> {
    let prepared = crate::tool_choice::prepare(req, crate::tool_choice::Protocol::Chat)?;
    let req = prepared.as_ref();
    let mut messages = Vec::new();

    let system_text: String = req
        .system
        .iter()
        .filter(|p| p.kind() == PartKind::Text)
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !system_text.is_empty() {
        messages.push(json!({"role": "system", "content": system_text}));
    }

    for m in &req.messages {
        match m.role {
            Role::User => {
                for p in &m.parts {
                    if p.kind() == PartKind::ToolResult {
                        let images: Vec<Value> = crate::tool_output::images(p)
                            .map(part_to_content_block)
                            .collect();
                        let content = if images.is_empty() {
                            json!(p.text)
                        } else {
                            let mut blocks = vec![json!({"type":"text", "text":p.text})];
                            blocks.extend(images);
                            Value::Array(blocks)
                        };
                        // The private wire normalizer relocates these image blocks
                        // after all answers, never sending a multimodal tool message.
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": p.call_id,
                            "content": content,
                        }));
                    }
                }
                let content: Vec<Value> = m
                    .parts
                    .iter()
                    .map(part_to_content_block)
                    .filter(|v| !v.is_null())
                    .collect();
                if !content.is_empty() {
                    let content =
                        if content.len() == 1 && content[0].get("type") == Some(&json!("text")) {
                            content[0].get("text").cloned().unwrap_or(Value::Null)
                        } else {
                            Value::Array(content)
                        };
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
            Role::Assistant => {
                let text: String = m
                    .parts
                    .iter()
                    .filter(|p| p.kind() == PartKind::Text)
                    .map(|p| p.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                let calls: Vec<Value> = m
                    .parts
                    .iter()
                    .filter(|p| p.kind() == PartKind::ToolCall)
                    .map(|p| {
                        json!({
                            "id": p.id,
                            "type": "function",
                            "function": {"name": p.name, "arguments": p.args.to_string()}
                        })
                    })
                    .collect();
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(msg));
            }
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert(
        "messages".into(),
        Value::Array(tool_pairing::normalize(messages)?),
    );
    if !req.tools.is_empty() {
        out.insert(
            "tools".into(),
            Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
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
        out.insert("max_tokens".into(), json!(max));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if !req.stop.is_empty() {
        out.insert("stop".into(), json!(req.stop));
    }
    out.insert("stream".into(), json!(req.stream));
    crate::search::encode(req, crate::search::SearchProtocol::Chat, &mut out)?;
    Ok(Value::Object(out))
}

fn decode_usage(v: &Value) -> Usage {
    Usage {
        input: v.get("prompt_tokens").and_then(Value::as_u64),
        output: v.get("completion_tokens").and_then(Value::as_u64),
        cache_read: v
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write: None,
    }
}

fn usage_to_json(u: &Usage) -> Value {
    let input = u.input.unwrap_or(0);
    let output = u.output.unwrap_or(0);
    let mut cached = Map::new();
    if let Some(read) = u.cache_read {
        cached.insert("cached_tokens".into(), json!(read));
    }
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), json!(input));
    usage.insert("completion_tokens".into(), json!(output));
    usage.insert("total_tokens".into(), json!(input + output));
    if !cached.is_empty() {
        usage.insert("prompt_tokens_details".into(), Value::Object(cached));
    }
    Value::Object(usage)
}

/// Read reported usage even when a refusal omits the ordinary reply fields.
pub fn response_usage(v: &Value) -> Usage {
    v.get("usage").map(decode_usage).unwrap_or_default()
}

/// Decode an OpenAI Chat Completions reply body.
pub fn decode_response(v: &Value) -> Result<CompletionResponse> {
    let model = str_field(v, "model")?;
    let choice = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or(missing("choices"))?;
    let msg = choice.get("message").ok_or(missing("message"))?;
    let mut parts = decode_content(msg.get("content").unwrap_or(&Value::Null));
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for c in calls {
            let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = c
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args_raw = c
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let args: Value =
                serde_json::from_str(args_raw).unwrap_or(Value::String(args_raw.to_owned()));
            parts.push(Part::tool_call(id, name, args));
        }
    }
    let stop = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(StopReason::from_chat)
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn response_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("chatcmpl-magpie{now}")
}

/// Encode an IR reply as an OpenAI Chat Completions body.
pub fn encode_response(resp: &CompletionResponse) -> Value {
    let text: String = resp
        .message
        .parts
        .iter()
        .filter(|p| p.kind() == PartKind::Text)
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    let calls: Vec<Value> = resp
        .message
        .parts
        .iter()
        .filter(|p| p.kind() == PartKind::ToolCall)
        .map(|p| {
            json!({
                "id": p.id,
                "type": "function",
                "function": {"name": p.name, "arguments": p.args.to_string()}
            })
        })
        .collect();
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(calls));
    }
    json!({
        "id": response_id(),
        "object": "chat.completion",
        "created": now_secs(),
        "model": resp.model,
        "choices": [{
            "index": 0,
            "message": Value::Object(msg),
            "finish_reason": resp.stop.to_chat(),
            "logprobs": Value::Null,
        }],
        "usage": usage_to_json(&resp.usage),
    })
}

/// Retain Chat opening metadata once per upstream attempt. A role-only lead
/// is a real beginning, not content; the gateway may safely hold it for retry.
#[derive(Debug, Default)]
pub struct ChatStreamDecoder {
    started: bool,
}

impl ChatStreamDecoder {
    /// Decode one chunk, including its first assistant opening when supplied.
    pub fn decode(&mut self, value: &Value) -> Vec<StreamEvent> {
        let mut events = decode_chunk(value);
        if !self.started
            && value.get("error").is_none()
            && let Some(delta) = value.pointer("/choices/0/delta").filter(|v| v.is_object())
        {
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if delta.get("role").and_then(Value::as_str) == Some("assistant") || !model.is_empty() {
                self.started = true;
                events.insert(
                    0,
                    StreamEvent::Start {
                        model: model.to_owned(),
                    },
                );
            }
        }
        events
    }
}

/// Decode one streamed chunk from an OpenAI-compatible upstream into IR
/// events. `[DONE]` is handled by the caller (it is not JSON).
pub fn decode_chunk(v: &Value) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    if let Some(u) = v.get("usage")
        && u.is_object()
    {
        out.push(StreamEvent::Usage(decode_usage(u)));
    }
    if let Some(error) = v.get("error") {
        out.push(StreamEvent::Error {
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("chat upstream error")
                .to_owned(),
        });
        return out;
    }
    let Some(choice) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    else {
        return out;
    };
    let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
    if let Some(text) = delta.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        out.push(StreamEvent::TextDelta {
            text: text.to_owned(),
        });
    }
    // DeepSeek-style reasoning deltas; harmless for clients that ignore it.
    for field in ["reasoning_content", "reasoning"] {
        if let Some(text) = delta.get(field).and_then(Value::as_str)
            && !text.is_empty()
        {
            out.push(StreamEvent::ThinkingDelta {
                text: text.to_owned(),
            });
        }
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for c in calls {
            let index = c.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = c
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !id.is_empty() || !name.is_empty() {
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: id.to_owned(),
                    name: name.to_owned(),
                    namespace: None,
                });
            }
            if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str)
                && !a.is_empty()
            {
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    args: a.to_owned(),
                });
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str)
        && !reason.is_empty()
        && reason != "null"
    {
        out.push(StreamEvent::Stop(StopReason::from_chat(reason)));
    }
    out
}

fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}

fn chunk(id: &str, model: &str, created: u64, delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "logprobs": Value::Null,
            "finish_reason": finish,
        }],
    })
}

/// Encodes IR stream events back out as OpenAI Chat Completions chunks
/// (each `encode` returns one ready-to-write SSE frame, `data: ...\n\n`).
#[derive(Debug)]
pub struct ChatChunkEncoder {
    model: String,
    id: String,
    created: u64,
    include_usage: bool,
    finished: Option<StopReason>,
    usage: crate::ir::Usage,
    usage_emitted: bool,
}

impl ChatChunkEncoder {
    /// An encoder for a client that asked for `model`, reporting usage in a
    /// final chunk when it asked for one (`stream_options.include_usage`).
    pub fn new(model: impl Into<String>, include_usage: bool) -> Self {
        Self {
            model: model.into(),
            id: response_id(),
            created: now_secs(),
            include_usage,
            finished: None,
            usage: crate::ir::Usage::default(),
            usage_emitted: false,
        }
    }

    /// Encode one event; `None` when the dialect has nothing to say for it.
    pub fn encode(&mut self, ev: &StreamEvent) -> Option<String> {
        match ev {
            StreamEvent::Start { model } => {
                if model != &self.model {
                    self.model = model.clone();
                }
                Some(sse(&chunk(
                    &self.id,
                    &self.model,
                    self.created,
                    json!({"role": "assistant", "content": ""}),
                    None,
                )))
            }
            // Chat has no portable server-side search-result event.
            StreamEvent::Signature { .. }
            | StreamEvent::ThinkingStart { .. }
            | StreamEvent::Sealed { .. } => None,
            StreamEvent::SearchStarted => None,
            StreamEvent::Search { .. } | StreamEvent::SearchUpdate { .. } => None,
            // Image output currently has a native mapping only in Gemini.
            StreamEvent::Image { .. } => None,
            StreamEvent::TextDelta { text } => Some(sse(&chunk(
                &self.id,
                &self.model,
                self.created,
                json!({"content": text}),
                None,
            ))),
            StreamEvent::ThinkingDelta { text } => Some(sse(&chunk(
                &self.id,
                &self.model,
                self.created,
                json!({"reasoning_content": text}),
                None,
            ))),
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => Some(sse(&chunk(
                &self.id,
                &self.model,
                self.created,
                json!({"tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""}
                }]}),
                None,
            ))),
            StreamEvent::ToolCallDelta { index, args } => Some(sse(&chunk(
                &self.id,
                &self.model,
                self.created,
                json!({"tool_calls": [{
                    "index": index,
                    "function": {"arguments": args}
                }]}),
                None,
            ))),
            StreamEvent::ToolCallEnd { .. } => None,
            StreamEvent::Usage(u) => {
                self.usage.merge(u);
                None
            }
            StreamEvent::Stop(reason) => {
                self.finished = Some(*reason);
                Some(sse(&chunk(
                    &self.id,
                    &self.model,
                    self.created,
                    json!({}),
                    Some(reason.to_chat()),
                )))
            }
            StreamEvent::Done => {
                let mut out = String::new();
                if self.include_usage
                    && !self.usage_emitted
                    && (self.usage.input.is_some()
                        || self.usage.output.is_some()
                        || self.usage.cache_read.is_some()
                        || self.usage.cache_write.is_some())
                {
                    self.usage_emitted = true;
                    out.push_str(&sse(&json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [],
                        "usage": usage_to_json(&self.usage),
                    })));
                }
                if self.finished.is_none() {
                    out.push_str(&sse(&chunk(
                        &self.id,
                        &self.model,
                        self.created,
                        json!({}),
                        Some("stop"),
                    )));
                }
                out.push_str("data: [DONE]\n\n");
                Some(out)
            }
            StreamEvent::Error { message } => Some(sse(&json!({
                "error": {"message": message, "type": "upstream_error"}
            }))),
        }
    }
}

#[cfg(test)]
mod stream_opening_tests {
    use super::*;

    #[test]
    fn chat_opening_is_once_per_decoder_and_preserves_usage_and_content() {
        let opening = json!({"model":"chosen", "choices":[{"delta":{"role":"assistant"}}], "usage":{"prompt_tokens":7}});
        let mut decoder = ChatStreamDecoder::default();
        assert!(
            matches!(&decoder.decode(&opening)[..], [StreamEvent::Start { model }, StreamEvent::Usage(_)] if model == "chosen")
        );
        assert!(matches!(
            &decoder.decode(&opening)[..],
            [StreamEvent::Usage(_)]
        ));
        let content = json!({"model":"chosen", "choices":[{"delta":{"content":"hi"}}]});
        assert!(
            matches!(&decoder.decode(&content)[..], [StreamEvent::TextDelta { text }] if text == "hi")
        );
        assert!(matches!(
            &ChatStreamDecoder::default().decode(&content)[..],
            [StreamEvent::Start { .. }, StreamEvent::TextDelta { .. }]
        ));
    }

    #[test]
    fn chat_errors_usage_only_and_unknown_chunks_do_not_invent_opening() {
        let mut decoder = ChatStreamDecoder::default();
        for value in [
            json!({}),
            json!({"model":"chosen", "choices":[], "usage":{"prompt_tokens":7}}),
            json!({"model":"chosen", "choices":[{"delta":{"role":"assistant"}}], "error":{"message":"failed"}}),
        ] {
            assert!(
                decoder
                    .decode(&value)
                    .iter()
                    .all(|e| !matches!(e, StreamEvent::Start { .. }))
            );
        }
        assert!(
            matches!(&decoder.decode(&json!({"choices":[{"delta":{"role":"assistant"}}]}))[..], [StreamEvent::Start { model }] if model.is_empty())
        );
    }
}
