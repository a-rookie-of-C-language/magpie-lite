//! Leave out the assistant turns that say nothing.
//!
//! An agent keeps the turn the user cut off before it said anything —
//! only its thinking, an empty text, nothing — and sends it again on
//! every later turn of the conversation. DeepSeek, Kimi and the like
//! then turn the whole request away over it: "Invalid assistant
//! message: content or tool_calls must be set" (#2599). Reasoning is
//! not content to them.
//!
//! This is the reference gateway's `withoutSilentAssistants`. A prefill
//! to go on from, a refusal and an audio reply say something and stay.

use serde_json::{Value, json};

/// Whether a Chat message is an assistant turn that says nothing: no
/// content or only blank text, and no tool calls.
fn silent_assistant(m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    // A prefill to go on from, or half of one, is not silent.
    for field in ["prefix", "partial"] {
        if m.get(field).and_then(Value::as_bool) == Some(true) {
            return false;
        }
    }
    for field in ["function_call", "audio"] {
        if m.get(field).is_some_and(|v| !v.is_null()) {
            return false;
        }
    }
    if m.get("refusal")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        return false;
    }
    if m.get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
    {
        return false;
    }
    match m.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Array(blocks)) => blocks.iter().all(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|text| text.trim().is_empty())
        }),
        Some(_) => false,
    }
}

/// Leaves out of a Chat request's messages every assistant turn that
/// says nothing, and joins the user messages it stood between into one,
/// since some models' chat templates turn away two user messages in a
/// row. Nothing of the cut turn reaches the model, as nothing of it
/// reached the user. Every strict-template rule stays kept: a tool
/// message still follows the turn with its call.
pub(super) fn without_silent_assistants(messages: Vec<Value>) -> Vec<Value> {
    if !messages.iter().any(silent_assistant) {
        return messages;
    }
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut dropped = false;
    for message in messages {
        if silent_assistant(&message) {
            dropped = true;
            continue;
        }
        if dropped && message["role"] == "user" && out.last().is_some_and(|m| m["role"] == "user") {
            let last = out.last_mut().expect("checked above");
            if let Some(joined) = joined_user_content(&last["content"], &message["content"]) {
                last["content"] = joined;
                continue;
            }
        }
        dropped = false;
        out.push(message);
    }
    out
}

/// Two user messages' content as one: two texts with a blank line
/// between them, else the parts of the first and then the second's, a
/// text becoming a text part.
fn joined_user_content(a: &Value, b: &Value) -> Option<Value> {
    if let (Some(a), Some(b)) = (a.as_str(), b.as_str()) {
        return Some(Value::String(format!("{}\n\n{}", a, b)));
    }
    let parts = |content: &Value| -> Option<Vec<Value>> {
        match content {
            Value::String(text) => {
                if text.trim().is_empty() {
                    Some(Vec::new())
                } else {
                    Some(vec![json!({"type": "text", "text": text})])
                }
            }
            Value::Array(blocks) => Some(blocks.clone()),
            _ => None,
        }
    };
    let mut all = parts(a)?;
    all.extend(parts(b)?);
    Some(Value::Array(all))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(messages: &[Value]) -> Vec<&str> {
        messages
            .iter()
            .map(|m| m["role"].as_str().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_cut_off_turn_is_left_out_wherever_it_stood() {
        // What an agent keeps of a reply the user cut off before it said
        // anything: an empty text, or none at all.
        for silent in [
            json!({"role":"assistant","content":""}),
            json!({"role":"assistant","content":"   "}),
            json!({"role":"assistant","content":null}),
            json!({"role":"assistant"}),
            json!({"role":"assistant","content":[{"type":"text","text":""}]}),
            json!({"role":"assistant","content":[],"tool_calls":[]}),
        ] {
            let messages = vec![
                json!({"role":"user","content":"hi"}),
                silent.clone(),
                json!({"role":"user","content":"again"}),
            ];
            let out = without_silent_assistants(messages);
            assert_eq!(out.len(), 1, "should have dropped {silent}");
            assert_eq!(out[0]["content"], "hi\n\nagain");
        }
    }

    #[test]
    fn a_turn_that_says_something_stays() {
        for kept in [
            json!({"role":"assistant","content":"hi"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"c","function":{"name":"n","arguments":"{}"}}]}),
            json!({"role":"assistant","content":"","refusal":"no"}),
            json!({"role":"assistant","content":"","prefix":true}),
            json!({"role":"assistant","content":"","audio":{"id":"a"}}),
            json!({"role":"assistant","content":[{"type":"text","text":""},{"type":"image_url","image_url":{"url":"u"}}]}),
            json!({"role":"user","content":""}),
            json!({"role":"tool","tool_call_id":"c","content":""}),
        ] {
            let messages = vec![
                json!({"role":"user","content":"hi"}),
                kept.clone(),
                json!({"role":"user","content":"again"}),
            ];
            let out = without_silent_assistants(messages);
            assert_eq!(out.len(), 3, "should have kept {kept}");
        }
    }

    #[test]
    fn user_messages_are_joined_only_across_a_dropped_turn() {
        // Nothing dropped: the two user messages stand as they came.
        let kept = without_silent_assistants(vec![
            json!({"role":"user","content":"one"}),
            json!({"role":"user","content":"two"}),
        ]);
        assert_eq!(roles(&kept), ["user", "user"]);

        // Parts join as parts, a text becoming a text part.
        let joined = without_silent_assistants(vec![
            json!({"role":"user","content":[{"type":"text","text":"one"}]}),
            json!({"role":"assistant","content":null}),
            json!({"role":"user","content":"two"}),
        ]);
        assert_eq!(
            joined[0]["content"],
            json!([{"type":"text","text":"one"}, {"type":"text","text":"two"}])
        );

        // A non-user turn between them is not joined over.
        let split = without_silent_assistants(vec![
            json!({"role":"user","content":"one"}),
            json!({"role":"assistant","content":null}),
            json!({"role":"assistant","content":"real"}),
            json!({"role":"user","content":"two"}),
        ]);
        assert_eq!(roles(&split), ["user", "assistant", "user"]);

        // An assistant turn with calls between them keeps its answer.
        let pairing = without_silent_assistants(vec![
            json!({"role":"user","content":"one"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"n","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"out"}),
            json!({"role":"assistant","content":""}),
            json!({"role":"user","content":"two"}),
        ]);
        assert_eq!(roles(&pairing), ["user", "assistant", "tool", "user"]);
    }

    #[test]
    fn an_untouched_request_comes_back_as_it_came() {
        let messages = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":"hello"}),
        ];
        assert_eq!(without_silent_assistants(messages.clone()), messages);
    }
}
