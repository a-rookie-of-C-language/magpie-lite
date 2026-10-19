//! Model-retirement and model-refusal heuristics for cross-route fallback.
//!
//! Ported from the reference gateway's `modelRetired` / `modelRefused`.
//! Free-form prose is matched only for these two vendor shapes, and never
//! for quota or rate-limit classification, which stay status-code driven.
//! The error `body` should be the vendor JSON error object (or a wrapper
//! around it); matching uses the raw text of that JSON so that both the
//! `"type": "ModelDeprecated"` field and the human message are visible.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// How a vendor says "this model no longer exists here".
fn retired_words() -> &'static Regex {
    static RETIRED: OnceLock<Regex> = OnceLock::new();
    RETIRED.get_or_init(|| {
        Regex::new(r#"(?i)"ModelDeprecated"|model_deprecated|model.{0,80}(has been|was|is now|is) (deprecated|retired|discontinued|decommissioned|sunset)"#)
            .expect("retired words regex")
    })
}

/// How a vendor says "this model is not one this key (or this API
/// dialect) serves" — the same model may still exist for another
/// provider, or another key on this one.
fn unserved_words() -> &'static Regex {
    static UNSERVED: OnceLock<Regex> = OnceLock::new();
    UNSERVED.get_or_init(|| {
        Regex::new(r#"(?i)model.{0,80}(not (supported|accessible|available|found|enabled|allowed)|unsupported|does ?n[o']t exist|unknown|invalid)|(no such|unknown|invalid|unsupported) model|model_not_found|模型.{0,12}(不存在|不支持|无权|未开通)"#)
            .expect("unserved words regex")
    })
}

/// OpenCode Zen's 401 for a model that left its free list: the key is
/// still good, but the model is gone. The vendor's full JSON error is
/// `{"type":"error","error":{"type":"ModelError","message":"Model
/// glm-5-free is not supported"}}`.
fn zen_model_gone() -> &'static Regex {
    static ZEN: OnceLock<Regex> = OnceLock::new();
    ZEN.get_or_init(|| {
        Regex::new(r#"(^|[\s:"\\])Model [^"\s\\]+ is not supported("|\s*$)"#)
            .expect("zen model gone regex")
    })
}

fn error_text(body: &Value) -> String {
    let error = body
        .get("error")
        .or_else(|| body.pointer("/response/error"))
        .unwrap_or(body);
    error.to_string()
}

/// The vendor says this model is retired there: another provider of the
/// same model may still serve it, so the next route is worth trying.
pub(crate) fn model_retired(status: u16, body: &Value) -> bool {
    match status {
        400 | 404 | 410 | 422 => {}
        401 => {
            return zen_model_gone().is_match(&body.to_string());
        }
        _ => return false,
    }
    let text = error_text(body);
    retired_words().is_match(&text) && !quota_words().is_match(&text)
}

/// The vendor turned this request away over its model alone: not one
/// this key's plan serves, or not one this endpoint serves. The next
/// route may still serve it.
pub(crate) fn model_refused(status: u16, body: &Value) -> bool {
    if model_retired(status, body) {
        return true;
    }
    if !matches!(status, 400 | 403 | 404 | 422) {
        return false;
    }
    let text = error_text(body);
    unserved_words().is_match(&text)
        && !quota_words().is_match(&text)
        && !refused_words().is_match(&text)
}

/// How a vendor says it can't read the request's shape — an item, field
/// or parameter it doesn't know, which another vendor's API may take:
/// xAI's 422 "Failed to deserialize the JSON body …: unknown item type"
/// (#350), OpenAI's "Unknown parameter".
///
/// It also names a vendor turning the request away with nothing but a
/// trace id: Command Code's `{"message":"invalid request error trace_id:
/// <hex>","type":"invalid_request_error"}`, a 400 (a 422 before) with no
/// code and no param (#2313). The key and the account are fine, and
/// another member may take the same request, so the group moves on and
/// this member doesn't rest. An overflow that says so before the trace
/// id, and a message that says more after it, are the request's.
fn unsaid_words() -> &'static Regex {
    static UNSAID: OnceLock<Regex> = OnceLock::new();
    UNSAID.get_or_init(|| {
        Regex::new(r#"(?i)(^|"|\\"|: )invalid request error trace_id: [0-9a-f]{16,}(\\?"|\s*$)"#)
            .expect("unsaid words regex")
    })
}

fn shape_words() -> &'static Regex {
    static SHAPE: OnceLock<Regex> = OnceLock::new();
    SHAPE.get_or_init(|| {
        Regex::new(
            r#"(?i)failed to deserialize|unknown (item |content |input )?(type|variant|field|parameter)|unknown_parameter|unrecognized (request argument|field|parameter)|extra (inputs|fields) are not permitted|additional properties are not allowed"#,
        )
        .expect("shape words regex")
    })
}

/// A request failed over its shape alone: the next member is asked, and
/// this one doesn't rest, as nothing is wrong with it. A request missing
/// what every API requires ("field required") or too long for the model
/// isn't one. An upstream saying the model is served on another endpoint
/// than the one asked is one, once no other route of this member is left
/// (#2670: a relay with a Chat URL alone, refusing tools with reasoning
/// and `none` too).
pub(crate) fn shape_refused(status: u16, body: &Value) -> bool {
    if !matches!(status, 400 | 422) {
        return false;
    }
    let text = error_text(body);
    (shape_words().is_match(&text)
        || unsaid_words().is_match(&text)
        || crate::wrong_endpoint::wrong_endpoint(status, body))
        && !quota_words().is_match(&text)
        && !unserved_words().is_match(&text)
        && !refused_words().is_match(&text)
}

fn quota_words() -> &'static Regex {
    static QUOTA: OnceLock<Regex> = OnceLock::new();
    QUOTA.get_or_init(|| {
        Regex::new(r#"(?i)quota|insufficient|balance|credit|billing|out of budget|budget (exceeded|exhausted)|exceeded|rate.?limit|usage.?limit|limit.?reached|hit your .*limit|limit.{0,24}resets|too many requests|overloaded|余额|额度|欠费|限流|频率|套餐|用量|上限"#)
            .expect("quota words regex")
    })
}

fn refused_words() -> &'static Regex {
    static REFUSED: OnceLock<Regex> = OnceLock::new();
    REFUSED.get_or_init(|| {
        Regex::new(r#"(?i)unapproved channel|illegal api invocation"#).expect("refused words regex")
    })
}

/// How a vendor says it refuses to scan the conversation at all — the
/// "response protection" layer, not this key or this request's shape.
fn protection_words() -> &'static Regex {
    static PROTECTION: OnceLock<Regex> = OnceLock::new();
    PROTECTION.get_or_init(|| {
        Regex::new(r#"(?i)response protection is unavailable"#).expect("protection words regex")
    })
}

/// How a vendor says the conversation no longer fits in the model's
/// context (the reference gateway's `tooLongRe`). Matches wording alone:
/// Lite has no token estimator, and needs none to keep a too-long
/// conversation with the route it started on.
fn too_long_words() -> &'static Regex {
    static TOO_LONG: OnceLock<Regex> = OnceLock::new();
    TOO_LONG.get_or_init(|| {
        Regex::new(r#"(?i)context_length_exceeded|prompt is too long|input is too long|(exceeds?|exceeded|over|beyond)( the)?( model'?s?)?( maximum)? context|context (length|limit|window) (exceeded|is exceeded)|maximum context length|too many (input |prompt )?tokens|上下文(长度)?(超|过长)|超(过|出)(了)?(模型)?(的)?(最大)?上下文|input token count.*exceeds the maximum|maximum prompt length is \d|reduce the length of the messages|maximum allowed input length|longer than the model'?s context length|available context size|greater than the context length|prompt token count of [\d,]+ exceeds the limit|context window exceeds limit|exceeded model token limit|configured context size|prompt too long|prompt exceeds max length|context_window_exceeded|exceeded (max |maximum )?context length|range of input length should be|token limit exceeded|input tokens? (count )?(exceeds?|exceeded) (the )?(max(imum)? )?limit"#)
            .expect("too long words regex")
    })
}

/// How a vendor paces traffic — a rate limit in disguise, not a
/// conversation that doesn't fit (the reference gateway's `paceWords`).
fn pace_words() -> &'static Regex {
    static PACE: OnceLock<Regex> = OnceLock::new();
    PACE.get_or_init(|| {
        Regex::new(r#"(?i)rate.?limit|per (minute|hour|day)|\bTP[MD]\b|please wait|try again later|throttl"#)
            .expect("pace words regex")
    })
}

/// A 4xx (but never 429) error whose words say the conversation is too
/// long for the model. One about `max_tokens` is about the reply's
/// allowance, not the conversation, and a paced one ("Too many tokens,
/// please wait") is a rate limit; neither is overflow. The reference
/// gateway's `overflowed`/`tooLong` pair, without its token-estimate
/// half: whether every vendor's phrasing of the same refusal.
pub(crate) fn context_overflowed(status: u16, body: &Value) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let text = error_text(body);
    too_long_words().is_match(&text)
        && !pace_words().is_match(&text)
        && ["max_tokens", "max_output_tokens", "max_completion_tokens"]
            .iter()
            .all(|word| !text.to_lowercase().contains(word))
}

/// The vendor refused this conversation outright (an unapproved channel
/// or illegal API invocation): another provider may accept it, and the
/// refusing one is not broken, so it doesn't rest.
pub(crate) fn prompt_refused(status: u16, body: &Value) -> bool {
    status >= 400 && refused_words().is_match(&body.to_string())
}

/// The vendor's response-protection layer is unavailable for this
/// conversation: it answers 4xx/5xx no matter what is asked. Another
/// provider may still serve it, and the refusing one doesn't rest —
/// notably a 502 here must not count as a busy outage.
pub(crate) fn protection_refused(status: u16, body: &Value) -> bool {
    status >= 400 && protection_words().is_match(&body.to_string())
}

/// A 403 that says nothing of why: no quota, credit, rate, verification,
/// restriction, a model not served, or an outright refusal (the
/// reference's `plainForbidden`, d5bc9f00 / plugins#243). Such a 403 may
/// be the key's or the request's; when two keys of one provider answer
/// one request alike, it is the request that is refused, not each key.
pub(crate) fn plain_forbidden(status: u16, body: &Value) -> bool {
    if status != 403 {
        return false;
    }
    let text = error_text(body);
    !quota_words().is_match(&text)
        && !model_refused(status, body)
        && !prompt_refused(status, body)
        && !protection_refused(status, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retired_status_codes_and_wording() {
        for status in [400, 404, 410, 422] {
            assert!(model_retired(
                status,
                &json!({"error": {"type": "ModelDeprecated", "message": "Model exo-free has been deprecated."}})
            ));
        }
        assert!(model_retired(
            410,
            &json!({"error": {"message": "model_deprecated"}})
        ));
        assert!(model_retired(
            400,
            &json!({"error": {"message": "The model gpt-x was retired on 2026-01-01."}})
        ));
        assert!(model_retired(
            422,
            &json!({"error": {"message": "this model is now discontinued"}})
        ));
        // 401 only retires through Zen's model-gone wording.
        assert!(!model_retired(
            401,
            &json!({"error": {"type": "ModelDeprecated", "message": "gone"}})
        ));
        assert!(model_retired(
            401,
            &json!({"error": {"type": "ModelError", "message": "Model glm-5-free is not supported"}})
        ));
        assert!(!model_retired(
            401,
            &json!({"error": {"message": "bad key"}})
        ));
        // Quota wording wins even on a retirement-shaped 410.
        assert!(!model_retired(
            410,
            &json!({"error": {"type": "ModelDeprecated", "message": "Model x has been deprecated, your quota is exceeded"}})
        ));
        assert!(!model_retired(
            410,
            &json!({"error": {"message": "unknown error"}})
        ));
        assert!(!model_retired(403, &json!({"error": {"message": "no"}})));
    }

    #[test]
    fn context_overflow_matches_every_vendor_shape() {
        // A full cross-vendor table of the reference gateway's
        // `TestEveryVendorsOverflow` phrasings.
        for message in [
            "This model's maximum context length is 131072 tokens. However, you requested 207000 tokens.",
            "prompt is too long: 210000 tokens > 131072 maximum",
            "input is too long for the requested model",
            "The request exceeds the maximum context length",
            "context_length_exceeded",
            "exceeded the model's context",
            "This model supports up to 131072 tokens, but the request's context length exceeded it",
            "too many prompt tokens",
            "prompt token count of 250,000 exceeds the limit of 131072",
            "context window exceeds limit: 200000 tokens requested",
            "exceeded model token limit",
            "configured context size is smaller than the prompt",
            "prompt too long",
            "prompt exceeds max length",
            "context_window_exceeded",
            "exceeded maximum context length",
            "range of input length should be between 1 and 131072",
            "token limit exceeded",
            // A relay's own cap on one request's input, under the
            // model's window, which another group member holds (#2445).
            "Input token exceed the limit",
            "Input tokens exceeded the maximum limit",
            "上下文过长",
            "超出模型最大上下文",
        ] {
            for status in [400, 413, 422] {
                assert!(
                    context_overflowed(status, &json!({"error": {"message": message}})),
                    "status {status} should overflow: {message}"
                );
            }
        }
    }

    #[test]
    fn non_overflow_errors_do_not_short_circuit_routing() {
        // A reply allowance, a rate limit in disguise, a quota, and a
        // status outside the overflow trio all stay with ordinary
        // routing.
        for (status, message) in [
            (400, "max_tokens is too large"),
            (400, "max_output_tokens: 16384 > 8192"),
            (400, "max_completion_tokens too high for this model"),
            (413, "Too many tokens, please wait before trying again"),
            (413, "You have hit your usage limit"),
            (413, "Request Entity Too Large"),
            (422, "invalid value: integer, expected max_tokens"),
            (429, "maximum context length exceeded, please slow down"),
            (400, "Your balance is exhausted"),
        ] {
            assert!(
                !context_overflowed(status, &json!({"error": {"message": message}})),
                "status {status} should not overflow: {message}"
            );
        }
    }

    #[test]
    fn refused_covers_unserved_wording_without_quota() {
        for status in [400, 403, 404, 422] {
            assert!(model_refused(
                status,
                &json!({"error": {"message": "model deepseek-v4.1-flash is not available in the current token plan"}})
            ));
        }
        assert!(!model_refused(
            400,
            &json!({"error": {"message": "model x is not supported, your credit is exhausted"}})
        ));
        assert!(!model_refused(
            500,
            &json!({"error": {"message": "model not supported"}})
        ));
        // Retirement implies refused.
        assert!(model_refused(
            410,
            &json!({"error": {"message": "The model was retired."}})
        ));
    }

    #[test]
    fn wrapper_error_objects_are_matched() {
        let body = json!({
            "type": "response.failed",
            "response": {"error": {"message": "model not_found: no such model"}}
        });
        assert!(model_refused(404, &body));
    }

    #[test]
    fn shape_refusal_matches_unknown_items_only() {
        // xAI's 422 over an input item its API doesn't know (#350).
        assert!(shape_refused(
            422,
            &json!({"error": "Failed to deserialize the JSON body into the target type: input[0]: unknown item type \"additional_tools\"; expected one of: message, reasoning, function_call"})
        ));
        // OpenAI's unknown parameter.
        assert!(shape_refused(
            400,
            &json!({"error": {"message": "Unknown parameter: 'reasoning.summary'.", "code": "unknown_parameter"}})
        ));
        // A request missing what every API requires is not a shape the
        // next vendor reads better.
        assert!(!shape_refused(
            400,
            &json!({"error": {"message": "messages: field required"}})
        ));
        // Too long for the model is not a shape failure.
        assert!(!shape_refused(
            400,
            &json!({"error": {"message": "This model's maximum context length is 128000 tokens"}})
        ));
        // The next member is asked only once, from the status itself:
        // 5xx never claims a shape.
        assert!(!shape_refused(
            500,
            &json!({"error": "Failed to deserialize the JSON body into the target type: input[0]: unknown item type \"additional_tools\""})
        ));
    }

    #[test]
    fn a_bare_trace_id_is_a_shape_refusal() {
        // Command Code's Provider API turns a request away with nothing
        // but a trace id (#2313): the key is fine and another member may
        // take the same request.
        let bare = json!({"message":"invalid request error trace_id: 251c3c29761c17fdc459480ed9c68932",
            "type":"invalid_request_error"});
        for status in [400, 422] {
            assert!(shape_refused(status, &bare));
        }
        // A provider's name in front, and a wrapped copy, read the same.
        assert!(shape_refused(
            400,
            &json!({"message":"Command Code: invalid request error trace_id: a050673b3298a747a15895803095bcf0"})
        ));
        assert!(shape_refused(
            422,
            &json!({"message":"{\"message\":\"invalid request error trace_id: f9965636e3da815215348ef4ddf2ae63\",\"type\":\"invalid_request_error\"}",
                "type":"server_error"})
        ));
        // Its overflow, a parameter it names, a message that says more
        // after the trace id, and a 200 are not this refusal.
        for (status, body) in [
            (
                400,
                json!({"message":"The input is longer than the model's context length trace_id: e811a6f10da868000cc12a043c07a73b"}),
            ),
            (
                400,
                json!({"error":{"message":"Invalid option: expected one of \"low\"|\"medium\"","type":"invalid_request_error","param":"reasoning_effort"}}),
            ),
            (
                400,
                json!({"message":"invalid request error trace_id: 251c3c29761c17fdc459480ed9c68932: messages.3.content too long"}),
            ),
            (200, bare.clone()),
        ] {
            assert!(!shape_refused(status, &body), "{status} {body}");
        }
    }

    #[test]
    fn a_model_served_on_another_endpoint_is_a_shape_refusal() {
        // A relay with a Chat URL alone, refusing tools with reasoning and
        // `none` too (#2670): the member has no other API to ask, so the
        // group's next member takes the request and this one doesn't rest.
        let body = json!({"error": {
            "message": "Function tools with reasoning_effort are not supported for gpt-6.1-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
            "type": "invalid_request_error"
        }});
        for status in [400, 422] {
            assert!(shape_refused(status, &body), "{status}");
        }
        assert!(!shape_refused(500, &body));
        // A quota or an unserved model still wins: this is neither.
        assert!(!shape_refused(
            400,
            &json!({"error": {"message": "use /v1/responses; your quota is exceeded"}})
        ));
        assert!(!shape_refused(
            400,
            &json!({"error": {"message": "use /v1/responses; model not found"}})
        ));
    }

    #[test]
    fn prompt_and_protection_refusals_match_at_any_error_status() {
        // WorkBuddy-style 400: the channel, not the key, is refused.
        let prompt_refusal =
            json!({"code": 11128, "msg": "Illegal API invocation from an unapproved channel"});
        for status in [400, 403, 502] {
            assert!(prompt_refused(status, &prompt_refusal));
        }
        assert!(!prompt_refused(200, &prompt_refusal));
        assert!(!prompt_refused(
            400,
            &json!({"error": {"message": "bad request"}})
        ));

        // ChatGPT-backend-style 502: the protection layer is unavailable.
        for body in [
            json!({"error": {"message": "response protection is unavailable", "type": "server_error"}}),
            json!({"detail": "response protection is unavailable"}),
        ] {
            for status in [400, 502] {
                assert!(protection_refused(status, &body));
            }
        }
        assert!(!protection_refused(
            200,
            &json!({"detail": "response protection is unavailable"})
        ));
        assert!(!protection_refused(
            502,
            &json!({"error": {"message": "Unable to reach the model provider"}})
        ));
        // Quota wording alone is neither refusal.
        assert!(!prompt_refused(
            429,
            &json!({"error": {"message": "rate limit"}})
        ));
        assert!(!protection_refused(
            429,
            &json!({"error": {"message": "rate limit"}})
        ));
    }
}
