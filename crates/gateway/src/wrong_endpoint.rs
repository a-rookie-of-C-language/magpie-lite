//! How an upstream says a model exists but isn't served on the endpoint
//! asked, and how a Chat endpoint says it takes function tools only
//! without reasoning. The reference gateway's `wrongEndpoint`,
//! `asksResponses` and `toolsWithoutEffort` (#1308, #176, #2670).
//!
//! A model served on another API than the one asked is not this key's
//! fault: the reference asks its other APIs first, then lets a group's
//! next member take the request without resting the key. Lite has one
//! protocol per provider, so a wrong-endpoint refusal is a shape refusal
//! once no other route of this member can serve it.

use serde_json::Value;

fn text_of(body: &Value) -> String {
    let error = body
        .get("error")
        .or_else(|| body.pointer("/response/error"))
        .unwrap_or(body);
    // Prefer the human message over the serialized object: a JSON string
    // escapes its inner quotes, and an upstream that quotes `"none"` must
    // read the same as one that quotes `'none'`.
    let readable = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string());
    readable.to_lowercase()
}

/// An upstream's word that the model isn't served on the endpoint asked.
///
/// Matches OpenAI-compatible servers, Copilot's `/v1/messages`, OpenCode's
/// protocol errors and a relay that serves a model with tools on Responses
/// alone (#1308: "gpt-6.1-sol requires Responses for tool calls; this
/// account only supports Chat Completions").
pub(crate) fn wrong_endpoint(status: u16, body: &Value) -> bool {
    if !(400..500).contains(&status) {
        return false;
    }
    let text = text_of(body);
    [
        "not a chat model",
        "not supported in the v1/chat/completions",
        "not supported in /v1/chat/completions",
        "not supported in the v1/responses",
        "not supported in /v1/responses",
        "only supported in v1/responses",
        "only supported in /v1/responses",
        "use v1/completions",
        "use /v1/completions",
        "use v1/responses",
        "use /v1/responses",
        "use v1/chat/completions",
        "use /v1/chat/completions",
        // Copilot
        "not accessible via the",
        "unsupported_api_for_model",
        // Copilot's /v1/messages, for a model it serves on other APIs
        // alone (#754)
        "no model endpoints available given user constraints",
        // OpenCode
        "is not supported for format",
        "does not support this protocol",
        // A relay serving a model with tools on Responses alone (#1308)
        "requires responses",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// An upstream's word that the model is served, or served with what was
/// asked, on OpenAI's Responses API: where a provider has no URL for it,
/// the gateway says which URL to set (#1215, #1308, #2670).
pub(crate) fn asks_responses(body: &Value) -> bool {
    let text = text_of(body);
    [
        "requires responses",
        "use v1/responses",
        "use /v1/responses",
        "only supported in v1/responses",
        "only supported in /v1/responses",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// A Chat Completions endpoint refusing function tools with reasoning,
/// which it takes on Responses or with `reasoning_effort` "none".
///
/// Bedrock's wording for its GPT models (#176): "Function tools with
/// reasoning_effort are not supported for global.openai.gpt-6-luna in
/// /v1/chat/completions. To use function tools, use /v1/responses or set
/// reasoning_effort to 'none'." The relay in #2670 answers the same.
pub(crate) fn tools_without_effort(body: &Value) -> bool {
    let text = text_of(body);
    let Some(at) = text.find("tools with reasoning_effort are not supported") else {
        return false;
    };
    let tail = &text[at..];
    let Some(to) = tail.find("reasoning_effort to ") else {
        return false;
    };
    // The reference allows one quoting character before `none`, and the
    // wording ends at the level: a longer message about another setting
    // after it is not this refusal.
    let after = tail[to + "reasoning_effort to ".len()..].trim_start();
    let after = after.strip_prefix(['\'', '"', '`']).unwrap_or(after);
    after.starts_with("none")
}

/// An error that lists the reasoning levels a model takes, as one
/// refusing `none` does: Command Code's `expected one of
/// "low"|"medium"|"high"|"xhigh"|"max"` (#2670).
pub(crate) fn effort_levels_named(body: &Value) -> bool {
    use std::sync::OnceLock;

    static LEVELS: OnceLock<regex::Regex> = OnceLock::new();
    let levels = LEVELS.get_or_init(|| {
        regex::Regex::new(r"(?i)\blow\b\W+(?:medium|high)\b").expect("effort levels regex")
    });
    let text = text_of(body);
    levels.is_match(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wrong_endpoint_reads_the_reference_wordings() {
        for message in [
            "This is not a chat model.",
            "The model is not supported in /v1/chat/completions",
            "Model foo is only supported in /v1/responses",
            "Please use /v1/responses",
            "Model foo is not accessible via the given endpoint",
            "unsupported_api_for_model",
            "No model endpoints available given user constraints",
            "Model grok-4.7 is not supported for format anthropic",
            "Model does not support this protocol",
            "gpt-6.1-sol requires Responses for tool calls",
        ] {
            assert!(
                wrong_endpoint(400, &json!({"error": {"message": message}})),
                "{message}"
            );
        }
        // A 5xx and an unrelated bad request are not an endpoint refusal.
        assert!(!wrong_endpoint(
            500,
            &json!({"error": {"message": "use /v1/responses"}})
        ));
        assert!(!wrong_endpoint(
            400,
            &json!({"error": {"message": "messages: field required"}})
        ));
    }

    #[test]
    fn asks_responses_names_the_url_to_set() {
        for message in [
            "Function tools requires Responses",
            "To use function tools, use /v1/responses",
            "The model is only supported in /v1/responses",
        ] {
            assert!(
                asks_responses(&json!({"error": {"message": message}})),
                "{message}"
            );
        }
        assert!(!asks_responses(
            &json!({"error": {"message": "messages: field required"}})
        ));
    }

    #[test]
    fn tools_without_effort_matches_bedrocks_and_the_relays_wording() {
        for message in [
            "Amazon Bedrock: Function tools with reasoning_effort are not supported for global.openai.gpt-6-luna in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
            "Function tools with reasoning_effort are not supported for gpt-6.1-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to \"none\".",
        ] {
            assert!(
                tools_without_effort(&json!({"error": {"message": message}})),
                "{message}"
            );
        }
        // Another refusal, and one naming a different setting after none,
        // are not this.
        for message in [
            "Function tools with reasoning_effort are not supported for gpt-x",
            "Function tools with reasoning_effort are not supported; set reasoning_effort to 'low'",
        ] {
            assert!(
                !tools_without_effort(&json!({"error": {"message": message}})),
                "{message}"
            );
        }
    }

    #[test]
    fn effort_levels_named_reads_a_refusal_of_none() {
        assert!(effort_levels_named(
            &json!({"error": {"message": "Unsupported value: 'none' is not supported with the 'gpt-6.1-sol' model. Supported values are: 'low', 'medium', 'high', 'xhigh', and 'max'."}})
        ));
        assert!(effort_levels_named(
            &json!({"error": {"message": "Invalid option: expected one of \"low\"|\"medium\"|\"high\"|\"xhigh\"|\"max\""}})
        ));
        assert!(!effort_levels_named(
            &json!({"error": {"message": "Supported values are: 'low' and 'xhigh'"}})
        ));
        assert!(!effort_levels_named(
            &json!({"error": {"message": "'none' is not supported"}})
        ));
    }
}
