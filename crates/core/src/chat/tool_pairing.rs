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
}

impl Pairing {
    fn flush(&mut self) {
        for call in self.pending.drain(..) {
            self.output.push(call.answer.unwrap_or_else(|| {
                json!({
                    "role":"tool", "tool_call_id":call.wire_id, "content":INTERRUPTED
                })
            }));
        }
        self.indices.clear();
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

    fn orphan(&mut self, message: Value) {
        let text = message["content"].as_str().unwrap_or_default();
        let text = if text.trim().is_empty() {
            "Tool result received."
        } else {
            text
        };
        if let Some(last) = self.output.last_mut().filter(|m| m["role"] == "user") {
            match &mut last["content"] {
                Value::String(content) => {
                    content.push_str("\n\n");
                    content.push_str(text);
                    return;
                }
                Value::Array(blocks) => {
                    blocks.push(json!({"type":"text","text":text}));
                    return;
                }
                _ => {}
            }
        }
        self.output.push(json!({"role":"user","content":text}));
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
                pairing.output.push(message);
            }
        }
    }
    pairing.flush();
    Ok(pairing.output)
}
