//! A reply asked for past what the context window leaves is asked again
//! for what the window leaves, rather than failed as too long.
//!
//! DeepSeek counts the reply asked for (`max_tokens`) in its 1M window
//! with the prompt: "maximum context length is 1000000 tokens. However,
//! you requested N tokens (P in the messages, C in the completion)". Since
//! a client's `max_completion_tokens` reaches DeepSeek as `max_tokens`, a
//! turn asking the model's whole 384K on a ~620K+ conversation was refused
//! there and read as "too long for its model", where every other member of
//! its group answered. An endpoint speaking Anthropic's API words the same
//! refusal its own way ("input length and `max_tokens` exceed context
//! limit: P + C > W"), and Codex turns asked the model's own limit meet it
//! before the agent compacts (the reference's `replyLeft` /
//! `withReplyCap`, #b749cffb).
//!
//! Room under `LEAST_REPLY_ROOM` is left alone: anything less than a
//! useful reply means the conversation must be compacted, and the agent is
//! told it overflowed.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::config::Protocol;

/// The least room left for a reply that is worth asking for: less, and the
/// conversation needs compacting, which the refusal tells the agent as it
/// came (DeepSeek's own default reply is 8K).
pub(crate) const LEAST_REPLY_ROOM: u64 = 8192;

/// The tokens the window has left for a reply after the prompt, read from
/// a refusal that counts the reply asked for in the window: DeepSeek's and
/// vLLM's "This model's maximum context length is 1000000 tokens. However,
/// you requested 1100000 tokens (706784 in the messages, 393216 in the
/// completion)", vLLM's newer "your prompt contains at least N input
/// tokens", or Anthropic's "input length and `max_tokens` exceed context
/// limit: 970000 + 64000 > 1000000". `None` when the error is none of
/// those, or the prompt alone fills the window.
pub(crate) fn reply_left(message: &str) -> Option<u64> {
    let (window, prompt) = if let Some(parts) = window_and_prompt().captures(message) {
        let window = captured(parts.get(1));
        // Group 2 is DeepSeek's "N in the messages"; group 3 is vLLM's
        // "at least N input tokens".
        let prompt = captured(parts.get(2)).or_else(|| captured(parts.get(3)));
        (window, prompt)
    } else {
        let parts = input_plus_reply().captures(message)?;
        (captured(parts.get(2)), captured(parts.get(1)))
    };
    let (window, prompt) = (window?, prompt?);
    if window == 0 || prompt == 0 || prompt >= window {
        return None;
    }
    Some(window - prompt)
}

/// The window and the prompt's part of a refusal that adds the reply's
/// allowance to the prompt: "(N in the messages, M in the completion)", or
/// vLLM's newer "your prompt contains at least N input tokens".
fn window_and_prompt() -> &'static Regex {
    static WINDOW_AND_PROMPT: OnceLock<Regex> = OnceLock::new();
    WINDOW_AND_PROMPT.get_or_init(|| {
        Regex::new(
            r"(?is)maximum context length is (\d+) tokens.{0,200}?(?:\((\d+) in the messages, \d+ in the completion\)|your prompt contains at least (\d+) input tokens)",
        )
        .expect("the window regex")
    })
}

/// Anthropic's words for the same, which an endpoint speaking its API may
/// answer with: "input length and `max_tokens` exceed context limit:
/// 197202 + 21333 > 200000".
fn input_plus_reply() -> &'static Regex {
    static INPUT_PLUS_REPLY: OnceLock<Regex> = OnceLock::new();
    INPUT_PLUS_REPLY.get_or_init(|| {
        Regex::new(
            "(?i)input length and[^\n]*?max_tokens[^\n]*? exceed context limit:\\s*(\\d+)\\s*\\+\\s*\\d+\\s*>\\s*(\\d+)",
        )
        .expect("the input-plus-reply regex")
    })
}

/// A capture's number, or `None` when the group never matched.
fn captured(capture: Option<regex::Match<'_>>) -> Option<u64> {
    capture?.as_str().parse().ok()
}

/// Lower `body` to ask for a reply of at most `limit` tokens in
/// `protocol`'s words, whether or not it named a limit before: a request
/// that named none was given an allowance that was too large. Whether the
/// body now asks for less than it did; `false` when it asked for no more
/// already, or `protocol` keeps no such field this gateway sets.
pub(crate) fn with_reply_cap(body: &mut Value, protocol: Protocol, limit: u64) -> bool {
    if limit == 0 {
        return false;
    }
    match protocol {
        Protocol::Chat => {
            let lowered = lower(body, "max_tokens", limit);
            let lowered = lower(body, "max_completion_tokens", limit) || lowered;
            if lowered {
                return true;
            }
            if body.get("max_tokens").is_some() || body.get("max_completion_tokens").is_some() {
                return false;
            }
            insert(body, "max_tokens", limit)
        }
        Protocol::Responses => {
            if lower(body, "max_output_tokens", limit) {
                return true;
            }
            if body.get("max_output_tokens").is_some() {
                return false;
            }
            insert(body, "max_output_tokens", limit)
        }
        Protocol::Anthropic => {
            if !lower(body, "max_tokens", limit) {
                return false;
            }
            // The thinking budget must stay under the reply limit, or
            // Anthropic refuses the pair; below its least budget (1024)
            // nothing fits under the cap, so thinking goes off.
            let budget = body
                .get("thinking")
                .filter(|thinking| thinking.get("type").and_then(Value::as_str) == Some("enabled"))
                .and_then(|thinking| thinking.get("budget_tokens"))
                .and_then(token_count)
                .unwrap_or(0);
            if budget >= limit
                && let Some(object) = body.as_object_mut()
            {
                object.insert(
                    "thinking".into(),
                    if limit > 1024 {
                        json!({"type": "enabled", "budget_tokens": limit - 1})
                    } else {
                        json!({"type": "disabled"})
                    },
                );
            }
            true
        }
        Protocol::Gemini => {
            let Some(generation) = body
                .get_mut("generationConfig")
                .and_then(Value::as_object_mut)
            else {
                return false;
            };
            match generation.get("maxOutputTokens").and_then(token_count) {
                Some(current) if current > limit => {
                    generation.insert("maxOutputTokens".into(), json!(limit));
                    true
                }
                _ => false,
            }
        }
    }
}

/// Lower a top-level numeric field to `limit` when it asks for more.
fn lower(body: &mut Value, key: &str, limit: u64) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    match object.get(key).and_then(token_count) {
        Some(current) if current > limit => {
            object.insert(key.to_owned(), json!(limit));
            true
        }
        _ => false,
    }
}

/// Write a top-level field that wasn't there.
fn insert(body: &mut Value, key: &str, limit: u64) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    object.insert(key.to_owned(), json!(limit));
    true
}

/// A JSON number's value, whole or written as a float.
fn token_count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_left_reads_every_word_a_window_refusal_comes_in() {
        for (message, want) in [
            (
                "This model's maximum context length is 1000000 tokens. However, you requested 1100000 tokens (706784 in the messages, 393216 in the completion). Please reduce the length of the messages or completion.",
                Some(293216),
            ),
            (
                "This model's maximum context length is 32768 tokens. However, you requested 32000 output tokens and your prompt contains at least 769 input tokens, for a total of at least 32769 tokens.",
                Some(31999),
            ),
            // Anthropic's, from an endpoint speaking its API.
            (
                "input length and `max_tokens` exceed context limit: 970000 + 64000 > 1000000, decrease input length or `max_tokens` and try again",
                Some(30000),
            ),
            // The prompt alone is too long: nothing is left.
            (
                "This model's maximum context length is 131072 tokens. However, you requested 140000 tokens (140000 in the messages, 0 in the completion).",
                None,
            ),
            // Says no parts.
            (
                "This model's maximum context length is 131072 tokens. However, you requested 140000 tokens",
                None,
            ),
            (
                "Invalid max_tokens value, the valid range of max_tokens is [1, 8192]",
                None,
            ),
        ] {
            assert_eq!(reply_left(message), want, "{message}");
        }
    }

    #[test]
    fn a_reply_cap_lowers_or_names_each_protocols_field() {
        // Chat: a field asking for more is lowered, one asking for less is
        // left alone, and a body naming none is given the cap.
        let mut body = json!({"max_tokens": 393216, "messages": []});
        assert!(with_reply_cap(&mut body, Protocol::Chat, 293216));
        assert_eq!(body["max_tokens"], 293216);
        assert!(!with_reply_cap(&mut body, Protocol::Chat, 293216));
        let mut modest = json!({"max_tokens": 32000});
        assert!(!with_reply_cap(&mut modest, Protocol::Chat, 293216));
        let mut none = json!({"messages": []});
        assert!(with_reply_cap(&mut none, Protocol::Chat, 293216));
        assert_eq!(none["max_tokens"], 293216);
        assert!(!with_reply_cap(&mut none, Protocol::Chat, 0));

        // Responses.
        let mut body = json!({"max_output_tokens": 393216});
        assert!(with_reply_cap(&mut body, Protocol::Responses, 293216));
        assert_eq!(body["max_output_tokens"], 293216);
        let mut modest = json!({"max_output_tokens": 100});
        assert!(!with_reply_cap(&mut modest, Protocol::Responses, 293216));

        // Anthropic: the thinking budget must fit under the lowered cap.
        let mut body = json!({"max_tokens": 64000,
            "thinking": {"type": "enabled", "budget_tokens": 32000}});
        assert!(with_reply_cap(&mut body, Protocol::Anthropic, 30000));
        assert_eq!(body["max_tokens"], 30000);
        assert_eq!(body["thinking"]["budget_tokens"], 29999);
        // Below Anthropic's least budget nothing fits under it.
        let mut tight = json!({"max_tokens": 64000,
            "thinking": {"type": "enabled", "budget_tokens": 32000}});
        assert!(with_reply_cap(&mut tight, Protocol::Anthropic, 1000));
        assert_eq!(tight["thinking"], json!({"type": "disabled"}));
        // A body asking for less already is left as it was, thinking and all.
        let mut modest = json!({"max_tokens": 20000,
            "thinking": {"type": "enabled", "budget_tokens": 16000}});
        assert!(!with_reply_cap(&mut modest, Protocol::Anthropic, 30000));
        assert_eq!(modest["thinking"]["budget_tokens"], 16000);

        // Gemini.
        let mut body = json!({"generationConfig": {"maxOutputTokens": 65536}});
        assert!(with_reply_cap(&mut body, Protocol::Gemini, 30000));
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 30000);
        let mut modest = json!({"generationConfig": {"maxOutputTokens": 1000}});
        assert!(!with_reply_cap(&mut modest, Protocol::Gemini, 30000));
    }
}
