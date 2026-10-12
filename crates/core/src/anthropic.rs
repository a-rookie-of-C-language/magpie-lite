//! Anthropic Messages: decode requests into the IR, encode them out,
//! decode replies and streamed events, and encode streamed events back out.
//!
//! Spoken by Claude Code as a client and by Anthropic-compatible vendors
//! as an upstream.

use std::collections::HashMap;

use crate::error::{CoreError, Result};
use crate::ir::{
    CompletionRequest, CompletionResponse, Message, Part, PartKind, ReasoningProtocol, Role,
    SealedReasoning, SearchHit, SignatureProtocol, StopReason, Tool, Usage, WebSearch,
};
use crate::stream::{SignatureTarget, StreamEvent};
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
        Some("thinking") => Some(Part {
            signature_protocol: Some(SignatureProtocol::Anthropic),
            ..Part::thinking(
                b.get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                b.get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        }),
        Some("redacted_thinking") => Some(Part {
            sealed: Some(Box::new(SealedReasoning::decode(
                ReasoningProtocol::Anthropic,
                b,
            )?)),
            ..Part::thinking("", "")
        }),
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
        Some("server_tool_use") if b.get("name").is_some_and(|n| n == "web_search") => {
            b.get("id")?.as_str().filter(|id| !id.is_empty())?;
            Some(Part::web_search(WebSearch {
                query: b
                    .pointer("/input/query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                anthropic_block: Some(b.clone()),
                ..Default::default()
            }))
        }
        Some("web_search_tool_result") => {
            b.get("tool_use_id")?.as_str().filter(|id| !id.is_empty())?;
            let content = b.get("content")?;
            if !content.is_array()
                && content
                    .get("type")
                    .is_none_or(|t| t != "web_search_tool_result_error")
            {
                return None;
            }
            let hits = content
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|hit| {
                    if hit.get("type").is_none_or(|t| t != "web_search_result") {
                        return None;
                    }
                    let url = hit.get("url")?.as_str().filter(|url| !url.is_empty())?;
                    Some(SearchHit {
                        url: url.into(),
                        title: hit
                            .get("title")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(url)
                            .into(),
                        page_age: hit
                            .get("page_age")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    })
                })
                .collect();
            Some(Part::web_search(WebSearch {
                hits,
                anthropic_block: Some(b.clone()),
                ..Default::default()
            }))
        }
        Some("tool_use") => Some(Part::tool_call(
            b.get("id").and_then(Value::as_str).unwrap_or_default(),
            b.get("name").and_then(Value::as_str).unwrap_or_default(),
            b.get("input").cloned().unwrap_or_else(|| json!({})),
        )),
        Some("tool_result") => {
            let content = b.get("content").unwrap_or(&Value::Null);
            let (text, images) = crate::tool_output::decode(content);
            let mut result = Part::tool_result(
                b.get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                text,
                b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            );
            result.images = images;
            Some(result)
        }
        _ => None,
    }
}

/// Decode an Anthropic `content` field (string or block array) into parts.
fn decode_content(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::text(s.clone())],
        Value::Array(blocks) => {
            let mut queries = HashMap::new();
            blocks
                .iter()
                .filter_map(|block| {
                    let mut part = decode_block(block)?;
                    if let Some(search) = part.search.as_deref_mut() {
                        remember_search_query(search, &mut queries);
                    }
                    Some(part)
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

// Associate a result with its own call, never with the most recent function
// or another outstanding search. Results consume the query's retained state.
fn remember_search_query(search: &mut WebSearch, queries: &mut HashMap<String, String>) {
    if let Some(block) = &search.anthropic_block {
        if let Some(id) = block.get("id").and_then(Value::as_str) {
            queries.insert(id.into(), search.query.clone());
        } else if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
            search.query = queries.remove(id).unwrap_or_default();
        }
    }
}

fn part_to_block(p: &Part) -> Option<Value> {
    if p.kind() == PartKind::Thinking
        && let Some(block) = p
            .sealed
            .as_ref()
            .and_then(|s| s.native(ReasoningProtocol::Anthropic))
    {
        return Some(block.clone());
    }
    match p.kind() {
        PartKind::Search => p.search.as_ref()?.anthropic_block.clone(),
        PartKind::Text => Some(json!({"type": "text", "text": p.text})),
        PartKind::Thinking => Some(json!({
            "type": "thinking",
            "thinking": p.text,
            "signature": p.native_signature(SignatureProtocol::Anthropic),
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
                    namespace: None,
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
        web_search: crate::search::WebSearchRequest::decode(
            crate::search::SearchProtocol::Anthropic,
            v,
        )?,
        tool_choice: v.get("tool_choice").filter(|t| !t.is_null()).cloned(),
        tool_choice_protocol: Some(crate::ir::ToolChoiceProtocol::Anthropic),
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
pub fn encode_request(req: &CompletionRequest, model: &str) -> Result<Value> {
    let prepared = crate::tool_choice::prepare(req, crate::tool_choice::Protocol::Anthropic)?;
    let req = prepared.as_ref();
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
                    let images: Vec<Value> = crate::tool_output::images(p)
                        .filter_map(part_to_block)
                        .collect();
                    let content = if images.is_empty() {
                        json!(p.text)
                    } else {
                        let mut content = Vec::new();
                        if !p.text.is_empty() {
                            content.push(json!({"type":"text", "text":p.text}));
                        }
                        content.extend(images);
                        Value::Array(content)
                    };
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": p.call_id,
                        "content": content,
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
    crate::search::encode(req, crate::search::SearchProtocol::Anthropic, &mut out)?;
    Ok(Value::Object(out))
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

fn search_blocks(search: &WebSearch, id: &str) -> Vec<Value> {
    if let Some(block) = &search.anthropic_block {
        return vec![block.clone()];
    }
    let results: Vec<_> = search
        .hits
        .iter()
        .map(|hit| {
            json!({"type":"web_search_result",
        "title":hit.title,"url":hit.url,"encrypted_content":"",
        "page_age":if hit.page_age.is_empty() { Value::Null } else { json!(hit.page_age) }})
        })
        .collect();
    vec![
        json!({"type":"server_tool_use","id":id,"name":"web_search","input":{"query":search.query}}),
        json!({"type":"web_search_tool_result","tool_use_id":id,"content":results}),
    ]
}

/// Encode an IR reply as an Anthropic Messages body.
pub fn encode_response(resp: &CompletionResponse) -> Value {
    let mut content = Vec::new();
    for part in &resp.message.parts {
        if let Some(search) = &part.search {
            content.extend(search_blocks(
                search,
                &format!("srvtoolu_{}_{}", message_id(), content.len()),
            ));
        } else if let Some(block) = part_to_block(part) {
            content.push(block);
        }
    }
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

const MAX_PENDING_SEARCH_CALLS: usize = 128;

/// Stateful Anthropic decoder. Function arguments and hosted search inputs
/// are kept separate; search starts commit replay safety before results arrive.
#[derive(Debug)]
pub struct AnthropicStreamDecoder {
    tool_blocks: HashMap<usize, ()>,
    search_blocks: HashMap<usize, SearchInput>,
    search_queries: HashMap<String, String>,
    search_limit: usize,
    search_failed: bool,
    thinking_index: Option<usize>,
}

#[derive(Debug)]
struct SearchInput {
    block: Value,
    args: String,
    whole: bool,
    bytes: usize,
}

impl Default for AnthropicStreamDecoder {
    fn default() -> Self {
        Self {
            tool_blocks: HashMap::new(),
            search_blocks: HashMap::new(),
            search_queries: HashMap::new(),
            search_limit: crate::stream::MAX_SSE_EVENT_BYTES,
            search_failed: false,
            thinking_index: None,
        }
    }
}

impl AnthropicStreamDecoder {
    /// A decoder with no blocks seen yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn search_bytes(&self) -> usize {
        self.search_blocks
            .values()
            .map(|b| b.bytes + b.args.len())
            .sum::<usize>()
            + self
                .search_queries
                .iter()
                .map(|(id, query)| id.len() + query.len())
                .sum::<usize>()
    }

    fn search_error(&mut self, message: &'static str) -> Vec<StreamEvent> {
        self.search_blocks.clear();
        self.search_queries.clear();
        self.tool_blocks.clear();
        self.search_failed = true;
        vec![StreamEvent::Error {
            message: message.into(),
        }]
    }

    /// Decode one event's `data` payload.
    pub fn decode(&mut self, v: &Value) -> Vec<StreamEvent> {
        if v.get("type").is_some_and(|ty| ty == "message_start") {
            self.tool_blocks.clear();
            self.thinking_index = None;
            self.search_blocks.clear();
            self.search_queries.clear();
            self.search_failed = false;
        } else if self.search_failed {
            return vec![];
        }
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
                if self.search_blocks.contains_key(&index) {
                    return self.search_error("upstream web search block was not closed");
                }
                self.tool_blocks.remove(&index);
                self.thinking_index = None;
                if block["type"] == "thinking" {
                    self.thinking_index = Some(index);
                    out.push(StreamEvent::ThinkingStart { id: String::new() });
                    if let Some(text) = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        out.push(StreamEvent::ThinkingDelta {
                            text: text.to_owned(),
                        });
                    }
                    if let Some(signature) = block
                        .get("signature")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        out.push(StreamEvent::Signature {
                            protocol: SignatureProtocol::Anthropic,
                            target: SignatureTarget::Thinking,
                            signature: signature.to_owned(),
                        });
                    }
                } else if let Some(reasoning) =
                    SealedReasoning::decode(ReasoningProtocol::Anthropic, &block)
                {
                    out.push(StreamEvent::Sealed {
                        reasoning: Box::new(reasoning),
                    });
                } else if block.get("type").is_some_and(|ty| ty == "server_tool_use")
                    && block.get("name").is_some_and(|name| name == "web_search")
                    && block
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
                {
                    let bytes = block.to_string().len();
                    let id = block["id"].as_str().expect("validated search id");
                    if self.search_queries.contains_key(id)
                        || self
                            .search_blocks
                            .values()
                            .any(|input| input.block["id"] == id)
                    {
                        return self.search_error("upstream web search id is already pending");
                    }
                    if self.search_blocks.len() + self.search_queries.len()
                        >= MAX_PENDING_SEARCH_CALLS
                        || self.search_bytes().saturating_add(bytes) > self.search_limit
                    {
                        return self.search_error("upstream web search state exceeds its budget");
                    }
                    let whole = block
                        .get("input")
                        .and_then(Value::as_object)
                        .is_some_and(|m| !m.is_empty());
                    self.search_blocks.insert(
                        index,
                        SearchInput {
                            block,
                            args: String::new(),
                            whole,
                            bytes,
                        },
                    );
                    out.push(StreamEvent::SearchStarted);
                } else if block
                    .get("type")
                    .is_some_and(|ty| ty == "web_search_tool_result")
                {
                    if self
                        .search_blocks
                        .values()
                        .any(|input| input.block["id"] == block["tool_use_id"])
                    {
                        return self
                            .search_error("upstream web search result precedes its closed call");
                    }
                    if let Some(mut search) = decode_block(&block).and_then(|p| p.search) {
                        remember_search_query(&mut search, &mut self.search_queries);
                        out.push(StreamEvent::Search { search });
                    }
                } else if block.get("type").and_then(Value::as_str) == Some("tool_use") {
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
                        namespace: None,
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
                    "signature_delta" => {
                        if self.thinking_index == Some(index)
                            && let Some(signature) = delta
                                .get("signature")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                        {
                            out.push(StreamEvent::Signature {
                                protocol: SignatureProtocol::Anthropic,
                                target: SignatureTarget::Thinking,
                                signature: signature.to_owned(),
                            });
                        }
                    }
                    "input_json_delta" => {
                        if let Some(a) = delta.get("partial_json").and_then(Value::as_str)
                            && !a.is_empty()
                        {
                            if let Some(input) = self.search_blocks.get(&index) {
                                if !input.whole {
                                    if self.search_bytes().saturating_add(a.len())
                                        > self.search_limit
                                    {
                                        return self.search_error(
                                            "upstream web search input exceeds its byte budget",
                                        );
                                    }
                                    self.search_blocks
                                        .get_mut(&index)
                                        .expect("known search block")
                                        .args
                                        .push_str(a);
                                }
                            } else if self.tool_blocks.contains_key(&index) {
                                out.push(StreamEvent::ToolCallDelta {
                                    index,
                                    args: a.to_owned(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.thinking_index == Some(index) {
                    self.thinking_index = None;
                }
                if let Some(mut input) = self.search_blocks.remove(&index) {
                    if !input.whole && !input.args.is_empty() {
                        let Ok(args) = serde_json::from_str::<Value>(&input.args) else {
                            return self.search_error("upstream web search input is invalid JSON");
                        };
                        if !args.is_object() {
                            return self
                                .search_error("upstream web search input must be an object");
                        }
                        input.block["input"] = args;
                    }
                    if !input.block.get("input").is_some_and(Value::is_object) {
                        return self.search_error("upstream web search input must be an object");
                    }
                    if let Some(mut search) = decode_block(&input.block).and_then(|p| p.search) {
                        let id = input.block["id"].as_str().expect("validated search id");
                        if self.search_queries.contains_key(id)
                            || self
                                .search_bytes()
                                .saturating_add(id.len())
                                .saturating_add(search.query.len())
                                > self.search_limit
                        {
                            return self
                                .search_error("upstream web search state exceeds its budget");
                        }
                        remember_search_query(&mut search, &mut self.search_queries);
                        out.push(StreamEvent::Search { search });
                    }
                } else if self.tool_blocks.remove(&index).is_some() {
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
            "message_stop" => {
                if !self.search_blocks.is_empty() {
                    return self.search_error("upstream web search block was not closed");
                }
                self.tool_blocks.clear();
                self.search_blocks.clear();
                self.search_queries.clear();
                out.push(StreamEvent::Done);
            }
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
    pending_thinking: bool,
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
            pending_thinking: false,
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
        self.pending_thinking = false;
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
            StreamEvent::ThinkingStart { .. } => {
                // Start lazily: signed-only native blocks are real, unsigned empty ones are not.
                self.pending_thinking = true;
                if self
                    .open
                    .as_ref()
                    .is_some_and(|(_, kind)| *kind == BlockKind::Thinking)
                {
                    self.close_open()
                } else {
                    None
                }
            }
            StreamEvent::Signature {
                protocol: SignatureProtocol::Anthropic,
                target: SignatureTarget::Thinking,
                signature,
            } => {
                if signature.is_empty()
                    || (!self.pending_thinking
                        && self.open.as_ref().map(|(_, kind)| *kind) != Some(BlockKind::Thinking))
                {
                    return None;
                }
                let mut out = self.start_message();
                if self.open.as_ref().map(|(_, kind)| *kind) != Some(BlockKind::Thinking) {
                    out.push_str(&self.open_block(BlockKind::Thinking, Value::Null));
                }
                let index = self.open.as_ref().expect("opened thinking block").0;
                out.push_str(&sse(
                    "content_block_delta",
                    &json!({"type":"content_block_delta","index":index,
                    "delta":{"type":"signature_delta","signature":signature}}),
                ));
                Some(out)
            }
            StreamEvent::Signature { .. } => None,
            StreamEvent::Sealed { reasoning } => {
                self.pending_thinking = false;
                let block = reasoning.native(ReasoningProtocol::Anthropic)?;
                let mut out = self.start_message();
                out.push_str(&self.close_open().unwrap_or_default());
                let index = self.next_index;
                self.next_index += 1;
                out.push_str(&sse(
                    "content_block_start",
                    &json!({
                        "type":"content_block_start", "index":index, "content_block":block
                    }),
                ));
                out.push_str(&sse(
                    "content_block_stop",
                    &json!({"type":"content_block_stop", "index":index}),
                ));
                Some(out)
            }
            StreamEvent::SearchStarted => None,
            StreamEvent::Search { search } | StreamEvent::SearchUpdate { search } => {
                if matches!(ev, StreamEvent::SearchUpdate { .. }) && !search.is_answer() {
                    return None;
                }
                let mut out = self.start_message();
                out.push_str(&self.close_open().unwrap_or_default());
                let id = format!("srvtoolu_{}_{}", self.id, self.next_index);
                for mut block in search_blocks(search, &id) {
                    let input = if block["type"] == "server_tool_use" {
                        let input = block["input"].to_string();
                        block["input"] = json!({});
                        Some(input)
                    } else {
                        None
                    };
                    let index = self.next_index;
                    self.next_index += 1;
                    out.push_str(&sse(
                        "content_block_start",
                        &json!({"type":"content_block_start","index":index,"content_block":block}),
                    ));
                    if let Some(input) = input {
                        out.push_str(&sse(
                            "content_block_delta",
                            &json!({"type":"content_block_delta","index":index,
                            "delta":{"type":"input_json_delta","partial_json":input}}),
                        ));
                    }
                    out.push_str(&sse(
                        "content_block_stop",
                        &json!({"type":"content_block_stop","index":index}),
                    ));
                }
                Some(out)
            }
            // Image output currently has a native mapping only in Gemini.
            StreamEvent::Image { .. } => None,
            StreamEvent::TextDelta { text } => {
                self.pending_thinking = false;
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
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.pending_thinking = false;
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

#[cfg(test)]
mod search_state_tests {
    use super::*;

    fn call(index: usize, id: &str) -> Value {
        json!({"type":"content_block_start","index":index,
            "content_block":{"type":"server_tool_use","id":id,"name":"web_search","input":{}}})
    }
    fn stop(index: usize) -> Value {
        json!({"type":"content_block_stop","index":index})
    }
    fn assert_failed(dec: &mut AnthropicStreamDecoder, events: &[StreamEvent]) {
        assert!(matches!(events, [StreamEvent::Error { .. }]), "{events:?}");
        assert!(dec.search_failed);
        assert!(dec.search_blocks.is_empty());
        assert!(dec.search_queries.is_empty());
        assert!(dec.tool_blocks.is_empty());
        assert!(dec.decode(&json!({"type":"message_stop"})).is_empty());
        dec.decode(&json!({"type":"message_start","message":{"model":"next"}}));
        assert!(!dec.search_failed);
    }

    #[test]
    fn accumulated_search_bytes_are_bounded_and_errors_drop_all_retained_state() {
        let start = call(0, "srv");
        let bytes = start["content_block"].to_string().len();
        let args = r#"{"query":"private"}"#;
        let mut dec = AnthropicStreamDecoder {
            search_limit: bytes + args.len(),
            ..Default::default()
        };
        assert!(matches!(
            dec.decode(&start).as_slice(),
            [StreamEvent::SearchStarted]
        ));
        assert!(
            dec.decode(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":args}}))
                .is_empty()
        );
        assert_eq!(dec.search_bytes(), dec.search_limit);
        let events = dec.decode(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":" "}}));
        assert_failed(&mut dec, &events);
        dec.search_limit = bytes - 1;
        let events = dec.decode(&start);
        assert_failed(&mut dec, &events);
    }

    #[test]
    fn pending_count_includes_both_open_calls_and_unanswered_queries() {
        let mut dec = AnthropicStreamDecoder::new();
        for index in 0..128 {
            assert!(matches!(
                dec.decode(&call(index, &format!("srv_{index}"))).as_slice(),
                [StreamEvent::SearchStarted]
            ));
            if index % 2 == 0 {
                dec.decode(&stop(index));
            }
        }
        assert_eq!(dec.search_blocks.len(), 64);
        assert_eq!(dec.search_queries.len(), 64);
        let events = dec.decode(&call(128, "overflow"));
        assert_failed(&mut dec, &events);
        assert!(matches!(
            dec.decode(&call(0, "again")).as_slice(),
            [StreamEvent::SearchStarted]
        ));
    }

    #[test]
    fn consumed_queries_release_count_and_byte_budget() {
        let mut dec = AnthropicStreamDecoder::new();
        for index in 0..256 {
            dec.decode(&call(index * 2, "reusable"));
            dec.decode(&stop(index * 2));
            let events = dec.decode(&json!({"type":"content_block_start","index":index * 2 + 1,
                "content_block":{"type":"web_search_tool_result","tool_use_id":"reusable","content":[]}}));
            assert!(matches!(events.as_slice(), [StreamEvent::Search { .. }]));
            assert_eq!(dec.search_bytes(), 0);
            assert!(!dec.search_failed);
        }
    }

    #[test]
    fn duplicate_pending_ids_and_results_before_call_stop_are_protocol_errors() {
        for closed in [false, true] {
            let mut dec = AnthropicStreamDecoder::new();
            dec.decode(&call(0, "duplicate"));
            if closed {
                dec.decode(&stop(0));
            }
            let events = dec.decode(&call(1, "duplicate"));
            assert_failed(&mut dec, &events);
        }
        let mut dec = AnthropicStreamDecoder::new();
        dec.decode(&call(0, "open"));
        let events = dec.decode(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"web_search_tool_result","tool_use_id":"open","content":[]}}));
        assert_failed(&mut dec, &events);
    }
}
