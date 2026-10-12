//! Repair Chat-bound tool exchanges without mutating the shared request IR.
//!
//! Parallel/split calls belong to one assistant turn. Hold their results until
//! a real turn boundary, then emit exactly one answer per call in call order.
//! Compacted/late results remain user content, never invented successful calls.
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::{CoreError, Result};

const INTERRUPTED: &str =
    "[The result of this tool call is unavailable: the turn was interrupted.]";

struct PendingCall {
    wire_id: String,
    name: String,
    answer: Option<Value>,
}

#[derive(Default)]
struct Pairing {
    output: Vec<Value>,
    pending: Vec<PendingCall>,
    indices: HashMap<String, usize>,
    reserved: HashSet<String>,
    used: HashSet<String>,
    next_id: usize,
    visuals: Vec<Value>,
}

impl Pairing {
    fn flush(&mut self) {
        for call in self.pending.drain(..) {
            let mut answer = call.answer.unwrap_or_else(
                || json!({"role":"tool", "tool_call_id":call.wire_id, "content":INTERRUPTED}),
            );
            let (mut text, images) = split_tool_content(answer["content"].take());
            if !images.is_empty() {
                let note = if images.len() == 1 {
                    "[The tool returned an image; it follows in the next message.]".to_owned()
                } else {
                    format!(
                        "[The tool returned {} images; they follow in the next message.]",
                        images.len()
                    )
                };
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&note);
                let origin = if call.name.is_empty() {
                    format!("tool call {}", call.wire_id)
                } else {
                    format!("{} (tool call {})", call.name, call.wire_id)
                };
                self.visuals
                    .push(json!({"type":"text", "text":format!("[From the result of {origin}:]")}));
                self.visuals.extend(images);
            }
            answer["content"] = json!(text);
            self.output.push(answer);
        }
        self.indices.clear();
    }

    fn emit_visuals(&mut self, next: Option<&mut Value>) {
        if self.visuals.is_empty() {
            return;
        }
        let mut blocks = std::mem::take(&mut self.visuals);
        if let Some(next) = next.filter(|m| m["role"] == "user") {
            match next["content"].take() {
                Value::String(text) if !text.is_empty() => {
                    blocks.push(json!({"type":"text", "text":text}))
                }
                Value::Array(content) => blocks.extend(content),
                _ => {}
            }
            next["content"] = Value::Array(blocks);
        } else {
            self.output.push(json!({"role":"user", "content":blocks}));
        }
    }

    fn wire_id(&mut self, original: &str) -> String {
        if !original.is_empty() && self.used.insert(original.to_owned()) {
            return original.to_owned();
        }
        loop {
            let id = format!("call_magpie_lite_{}", self.next_id);
            self.next_id += 1;
            if !self.reserved.contains(&id) && self.used.insert(id.clone()) {
                return id;
            }
        }
    }

    fn orphan(&mut self, mut message: Value) {
        let (text, images) = split_tool_content(message["content"].take());
        let text = if text.trim().is_empty() {
            "Tool result received.".to_owned()
        } else {
            text
        };
        let content = if images.is_empty() {
            json!(text)
        } else {
            let mut blocks = vec![json!({"type":"text", "text":text})];
            blocks.extend(images);
            Value::Array(blocks)
        };
        let mut user = json!({"role":"user", "content":content});
        self.emit_visuals(Some(&mut user));
        if let Some(last) = self.output.last_mut().filter(|m| m["role"] == "user") {
            let next = user["content"].take();
            match (&mut last["content"], next) {
                (Value::String(content), Value::String(text)) => {
                    content.push_str("\n\n");
                    content.push_str(&text);
                }
                (Value::Array(blocks), Value::String(text)) => {
                    blocks.push(json!({"type":"text", "text":text}))
                }
                (Value::Array(blocks), Value::Array(next)) => blocks.extend(next),
                (content, Value::Array(mut blocks)) => {
                    if let Some(text) = content.as_str().filter(|s| !s.is_empty()) {
                        blocks.insert(0, json!({"type":"text", "text":text}));
                    }
                    *content = Value::Array(blocks);
                }
                (content, next) => *content = next,
            }
        } else {
            self.output.push(user);
        }
    }
}

// The encoder's private intermediate tool content may hold images. Move them,
// never stringify their base64/URLs into a text-only upstream tool message.
fn split_tool_content(content: Value) -> (String, Vec<Value>) {
    match content {
        Value::String(text) => (text, Vec::new()),
        Value::Array(blocks) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(value) = block["text"].as_str() {
                            text.push_str(value);
                        }
                    }
                    Some("image_url") => images.push(block),
                    _ => {}
                }
            }
            (text, images)
        }
        _ => (String::new(), Vec::new()),
    }
}

// Anthropic permits one assistant turn's calls to be split across adjacent
// assistant messages. Chat requires them together before their tool answers.
fn join_split_calls(messages: Vec<Value>) -> Vec<Value> {
    let mut joined: Vec<Value> = Vec::with_capacity(messages.len());
    for mut message in messages {
        if message["role"] == "assistant"
            && let Some(last) = joined.last_mut().filter(|last| {
                last["role"] == "assistant"
                    && last["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
            })
        {
            if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
                match &mut last["content"] {
                    Value::String(content) => content.push_str(text),
                    content => *content = json!(text),
                }
            }
            if let Some(calls) = message["tool_calls"].as_array_mut() {
                last["tool_calls"]
                    .as_array_mut()
                    .expect("joined assistant has calls")
                    .append(calls);
            }
        } else {
            joined.push(message);
        }
    }
    joined
}

pub(super) fn normalize(messages: Vec<Value>) -> Result<Vec<Value>> {
    let messages = join_split_calls(messages);
    let reserved = messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .filter_map(|call| call["id"].as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    let mut pairing = Pairing {
        reserved,
        ..Default::default()
    };
    for mut message in messages {
        match message["role"].as_str() {
            Some("assistant") => {
                if !pairing.pending.is_empty()
                    && message["tool_calls"]
                        .as_array()
                        .is_none_or(|calls| calls.is_empty())
                    && message["content"]
                        .as_str()
                        .is_none_or(|text| text.trim().is_empty())
                {
                    continue;
                }
                pairing.flush();
                pairing.emit_visuals(None);
                if let Some(calls) = message["tool_calls"].as_array_mut() {
                    for call in calls {
                        let original = call["id"].as_str().unwrap_or_default();
                        // Two simultaneous calls with the same nonempty id are
                        // ambiguous: don't silently attach a result to either.
                        if !original.is_empty() && pairing.indices.contains_key(original) {
                            return Err(CoreError::Invalid {
                                field: "tool_calls",
                                detail:
                                    "duplicate call ids within one assistant turn are ambiguous"
                                        .into(),
                            });
                        }
                        let wire_id = pairing.wire_id(original);
                        if !original.is_empty() {
                            pairing
                                .indices
                                .insert(original.to_owned(), pairing.pending.len());
                        }
                        call["id"] = json!(wire_id);
                        pairing.pending.push(PendingCall {
                            wire_id,
                            name: call["function"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                            answer: None,
                        });
                    }
                }
                pairing.output.push(message);
            }
            Some("tool") => {
                let id = message["tool_call_id"].as_str().unwrap_or_default();
                if let Some(index) = pairing.indices.get(id).copied() {
                    let pending = &mut pairing.pending[index];
                    if pending.answer.is_none() {
                        message["tool_call_id"] = json!(pending.wire_id);
                        pending.answer = Some(message);
                    }
                } else {
                    pairing.flush();
                    pairing.orphan(message);
                }
            }
            _ => {
                pairing.flush();
                pairing.emit_visuals(Some(&mut message));
                pairing.output.push(message);
            }
        }
    }
    pairing.flush();
    pairing.emit_visuals(None);
    Ok(pairing.output)
}
