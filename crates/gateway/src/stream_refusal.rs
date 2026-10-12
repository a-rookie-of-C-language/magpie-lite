//! Safety refusal is a routing signal, not a broken credential or empty reply.
//! Keep vendor messages out of the generated error and request history.
use magpie_core::stream::StreamEvent;
use serde_json::Value;

use crate::config::Protocol;
use crate::stream_empty::GeminiReply;

pub(crate) const REFUSAL: &str = "safety filter: upstream refused the request";

fn filter_reason(reason: &str) -> bool {
    matches!(
        reason,
        "refusal"
            | "content_filter"
            | "content_policy_violation"
            | "SAFETY"
            | "PROHIBITED_CONTENT"
            | "BLOCKLIST"
            | "SPII"
            | "IMAGE_SAFETY"
    )
}

/// Explicit policy codes, including the reference's *_policy family. Ordinary
/// invalid_prompt and messages merely discussing safety remain ordinary errors.
pub(crate) fn policy_error(value: &Value) -> bool {
    [
        value.pointer("/response/error"),
        value.get("error"),
        Some(value),
    ]
    .into_iter()
    .flatten()
    .any(|error| {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| error.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        filter_reason(code)
            || code
                .strip_suffix("_policy")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase()))
            || (code == "invalid_prompt"
                && error
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|s| {
                        let lower = s.to_ascii_lowercase();
                        lower.contains("flagged") || lower.contains("usage polic")
                    }))
    })
}

pub(crate) fn filtered(protocol: Protocol, value: &Value) -> bool {
    let reason = match protocol {
        Protocol::Chat => value.pointer("/choices/0/finish_reason"),
        Protocol::Anthropic => value
            .get("stop_reason")
            .or_else(|| value.pointer("/delta/stop_reason")),
        Protocol::Responses => value
            .pointer("/incomplete_details/reason")
            .or_else(|| value.pointer("/response/incomplete_details/reason")),
        Protocol::Gemini => {
            return [
                value.pointer("/promptFeedback/blockReason"),
                value.pointer("/prompt_feedback/block_reason"),
                value.pointer("/candidates/0/finishReason"),
                value.pointer("/candidates/0/finish_reason"),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(filter_reason);
        }
    };
    reason.and_then(Value::as_str).is_some_and(filter_reason)
}

fn nonempty(value: Option<&Value>) -> bool {
    value.is_some_and(|v| match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    })
}

fn parts_said(value: Option<&Value>, reasoning: bool) -> bool {
    value.and_then(Value::as_array).is_some_and(|parts| {
        parts.iter().any(|p| {
            match p.get("type").and_then(Value::as_str).unwrap_or_default() {
                "thinking" | "redacted_thinking" | "reasoning" => {
                    reasoning
                        && (nonempty(p.get("thinking"))
                            || nonempty(p.get("data"))
                            || nonempty(p.get("summary"))
                            || nonempty(p.get("content")))
                }
                "text" | "output_text" | "input_text" => nonempty(p.get("text")),
                "message" => parts_said(p.get("content"), reasoning),
                // Tools, images and future part types conservatively count as an
                // answer: never abandon an output that the current IR cannot carry.
                _ => true,
            }
        })
    })
}

pub(crate) fn has_answer(protocol: Protocol, value: &Value, reasoning: bool) -> bool {
    match protocol {
        Protocol::Chat => {
            let message = value
                .pointer("/choices/0/message")
                .or_else(|| value.pointer("/choices/0/delta"));
            message.is_some_and(|m| {
                nonempty(m.get("content"))
                    || nonempty(m.get("tool_calls"))
                    || nonempty(m.get("function_call"))
                    || (reasoning
                        && (nonempty(m.get("reasoning_content")) || nonempty(m.get("reasoning"))))
            })
        }
        Protocol::Anthropic => parts_said(value.get("content"), reasoning),
        Protocol::Responses => parts_said(
            value
                .get("output")
                .or_else(|| value.pointer("/response/output")),
            reasoning,
        ),
        Protocol::Gemini => {
            let mut reply = GeminiReply::default();
            reply.observe(value);
            // Unknown non-draft parts also block replay, just as the
            // reference conservatively preserves unfamiliar content blocks.
            let opaque = value
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|ps| {
                    ps.iter().any(|p| {
                        !p.get("thought").and_then(Value::as_bool).unwrap_or(false)
                            && p.as_object().is_some_and(|part| {
                                part.iter().any(|(key, value)| {
                                    !matches!(
                                        key.as_str(),
                                        "text"
                                            | "thought"
                                            | "thoughtSignature"
                                            | "thought_signature"
                                            | "functionCall"
                                            | "function_call"
                                            | "inlineData"
                                            | "inline_data"
                                            | "fileData"
                                            | "file_data"
                                    ) && nonempty(Some(value))
                                })
                            })
                    })
                });
            reply.has_answer()
                || opaque
                || (reasoning
                    && value
                        .pointer("/candidates/0/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|ps| ps.iter().any(|p| nonempty(p.get("text")))))
        }
    }
}

pub(crate) fn event_answers(event: &StreamEvent) -> bool {
    event.is_answer()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_policy_codes_only_and_sanitized_message() {
        for code in [
            "refusal",
            "content_filter",
            "content_policy_violation",
            "bio_policy",
            "SAFETY",
            "SPII",
        ] {
            for value in [
                json!({"error":{"code":code}}),
                json!({"response":{"error":{"type":code}}}),
                json!({"type":"error", "code":code}),
            ] {
                assert!(policy_error(&value), "{value}");
            }
        }
        for code in [
            "invalid_request_error",
            "rate_limit_error",
            "RECITATION",
            "NEW_FILTER",
            "_policy",
            "UPPER_policy",
        ] {
            assert!(!policy_error(
                &json!({"error":{"code":code,"message":"safety filter"}})
            ));
        }
        assert!(policy_error(
            &json!({"error":{"code":"invalid_prompt","message":"flagged by usage policy"}})
        ));
        assert!(!policy_error(
            &json!({"error":{"code":"invalid_prompt","message":"missing input"}})
        ));
        assert!(!policy_error(&json!({"message":"safety policy refusal"})));
    }

    #[test]
    fn filter_allowlist_does_not_expand_to_all_gemini_finishes() {
        for reason in [
            "STOP",
            "MAX_TOKENS",
            "RECITATION",
            "NEW_FILTER",
            "BLOCK_REASON_UNSPECIFIED",
        ] {
            assert!(!filtered(
                Protocol::Gemini,
                &json!({"candidates":[{"finishReason":reason}]})
            ));
        }
        assert!(filtered(
            Protocol::Gemini,
            &json!({"prompt_feedback":{"block_reason":"PROHIBITED_CONTENT"}})
        ));
    }
}
