//! Optional request fields an OpenAI-style upstream may refuse by name.
//! A vendor that turns a request away over a field it doesn't know
//! (Mistral's `extra_forbidden`, Gemini's "Unknown name", Groq's
//! "unsupported parameter", #393) is asked once more without the
//! optional ones it named — an invalid value must reach the client
//! instead. The reference gateway's `refusedOptional` /
//! `unsupportedOptionalField` / `withoutFields`.

use std::sync::OnceLock;

use serde_json::Value;

/// Request fields OpenAI's APIs take that a request does without, plus
/// the thinking switches Kimi and Qwen take. Only these are ever
/// dropped; any other refusal is the client's to fix.
pub(crate) const OPTIONAL_FIELDS: &[&str] = &[
    "store",
    "metadata",
    "service_tier",
    "prompt_cache_key",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
    "parallel_tool_calls",
    "verbosity",
    "thinking",
    "enable_thinking",
];

/// A 400 or 422 is a refusal over the request; anything else is an
/// outage or a quota and none of this applies (the reference
/// `badRequest`).
pub(crate) fn bad_request(status: u16) -> bool {
    status == 400 || status == 422
}

/// How a vendor says it turns a field away — by the field's name in the
/// error. Structured objects (Mistral's `extra_forbidden` with its
/// `loc`, OpenAI's `param`/`code`) are read by shape; free text by the
/// field's name beside refusal words. A located validation error
/// (`loc` with a message) concerns the value, not the field's support,
/// and an input echo must never be read as one.
fn names_refusal(fault: &Value, field: &str) -> bool {
    match fault {
        Value::String(text) => string_refusal(text, field),
        Value::Array(items) => items.iter().any(|item| names_refusal(item, field)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("extra_forbidden") {
                return matches!(
                    map.get("loc").and_then(Value::as_array),
                    Some(loc)
                        if (loc.len() == 1 && loc[0] == *field)
                            || (loc.len() == 2 && loc[0] == "body" && loc[1] == *field)
                );
            }
            // A validation error over the value — not over support for
            // the field. Its `input` echo may quote other fields' names.
            if map.contains_key("loc") {
                return false;
            }
            let param = map.get("param").and_then(Value::as_str).unwrap_or("");
            if !param.is_empty() && param != field {
                return false;
            }
            if map.get("code").and_then(Value::as_str) == Some("unsupported_parameter")
                && param == field
            {
                return true;
            }
            if map.get("code").and_then(Value::as_str) == Some("unsupported_value")
                || map.get("type").and_then(Value::as_str) == Some("unsupported_value")
            {
                // The value is the fault, not the field.
                return false;
            }
            for key in [
                "error",
                "message",
                "detail",
                "details",
                "errors",
                "description",
                "metadata",
                "raw",
            ] {
                if let Some(inner) = map.get(key)
                    && names_refusal(inner, field)
                {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// How a free-text refusal names a field, with the reference gateway's
/// three wordings: an unknown or unsupported name, an unquoted
/// `unsupported ...:` form, and a field said to be unsupported.
fn string_refusal(text: &str, field: &str) -> bool {
    // A proxy can embed the vendor's JSON error in prose. Prefer its
    // structure to text matching, which could mistake an input echo or
    // a different parameter's error for a refusal.
    let mut rest = text;
    while let Some(start) = rest.find(['{', '[']) {
        let mut stream = serde_json::Deserializer::from_slice(rest.as_bytes()).into_iter::<Value>();
        match stream.next() {
            Some(Ok(inner)) if error_object(&inner) => {
                return names_refusal(&inner, field);
            }
            Some(Ok(_)) => {
                rest = &rest[stream.byte_offset()..];
            }
            _ => {
                // Not JSON there ([HTTP 400]): look on past the bracket.
                rest = &rest[start + 1..];
            }
        }
    }
    static UNKNOWN: OnceLock<regex::Regex> = OnceLock::new();
    let unknown = UNKNOWN.get_or_init(|| {
        regex::Regex::new(r#"(?i)\b(?:unknown (?:name|field|parameter)|unsupported (?:parameter|field|property|argument)|unrecognized (?:request )?(?:argument|parameter)(?: supplied)?):?\s*['"`]([a-z_]+)['"`]"#).unwrap()
    });
    for capture in unknown.captures_iter(text) {
        if capture.get(1).map(|m| m.as_str()) != Some(field) {
            continue;
        }
        // Gemini names a nested field with "at 'path'"; only an absent
        // or empty path identifies a top-level request field.
        let Some(full) = capture.get(0) else {
            continue;
        };
        let tail = text[full.end()..].trim().to_lowercase();
        if tail.starts_with("at ") && !tail.starts_with("at ''") && !tail.starts_with("at \"\"") {
            continue;
        }
        return true;
    }
    static UNQUOTED: OnceLock<regex::Regex> = OnceLock::new();
    let unquoted = UNQUOTED.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)\bunsupported (?:parameter|field|property|argument):\s*([a-z_]+)\b"#,
        )
        .unwrap()
    });
    if unquoted
        .captures_iter(text)
        .any(|capture| capture.get(1).map(|m| m.as_str()) == Some(field))
    {
        return true;
    }
    static REJECTED: OnceLock<regex::Regex> = OnceLock::new();
    let rejected = REJECTED.get_or_init(|| {
        regex::Regex::new(r#"(?i)(?:^|\b(?:parameter|field|property|argument)\s+)['"`]([a-z_]+)['"`]\s+(?:is\s+)?(?:unsupported|not supported)\b"#).unwrap()
    });
    rejected
        .captures_iter(text)
        .any(|capture| capture.get(1).map(|m| m.as_str()) == Some(field))
}

/// Whether a parsed value is an error object or an array holding one.
fn error_object(fault: &Value) -> bool {
    match fault {
        Value::Object(_) => true,
        Value::Array(items) => items.iter().any(error_object),
        _ => false,
    }
}

/// Lists the optional request fields an upstream explicitly rejects as
/// unsupported. The error `body` is the vendor's whole reply, `fault`
/// the nested error value; a proxy can embed the vendor's JSON error in
/// prose, so a string fault is searched for an embedded error object
/// before its text is matched.
pub(crate) fn refused_optional(status: u16, body: &Value, fault: &Value) -> Vec<&'static str> {
    if !bad_request(status) {
        return Vec::new();
    }
    let Some(request) = body.as_object() else {
        return Vec::new();
    };
    OPTIONAL_FIELDS
        .iter()
        .copied()
        .filter(|field| request.contains_key(*field) && names_refusal(fault, field))
        .collect()
}

/// The request body with the refused fields left out.
pub(crate) fn without_fields(body: &Value, fields: &[&str]) -> Value {
    let mut body = body.clone();
    if let Some(map) = body.as_object_mut() {
        for field in fields {
            map.remove(*field);
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Value {
        json!({
            "store": false,
            "metadata": {"session": "synthetic"},
            "service_tier": "default",
            "prompt_cache_retention": "24h",
            "thinking": {"type": "enabled"},
            "messages": [{"role": "user", "content": "hi"}]
        })
    }

    fn refused(reply: Value, status: u16) -> Vec<&'static str> {
        let fault = reply.get("error").unwrap_or(&reply).clone();
        refused_optional(status, &request(), &fault)
    }

    #[test]
    fn mistral_extra_forbidden_names_only_the_unsupported_fields() {
        // A mixed 422 detail: store refused as extra, metadata refused
        // over its value — only store is dropped.
        assert_eq!(
            refused(
                json!({"message": {"detail": [
                    {"type": "extra_forbidden", "loc": ["body", "store"],
                     "msg": "Extra inputs are not permitted", "input": false},
                    {"type": "dict_type", "loc": ["body", "metadata"],
                     "msg": "Input should be a valid dictionary", "input": "invalid"}
                ]}}),
                422
            ),
            vec!["store"]
        );
        // A located error's input echo quoting another field's name is
        // never a refusal of that field.
        assert!(
            refused(
                json!({"detail": [{"type": "dict_type", "loc": ["body", "metadata"],
                "msg": "Input should be a valid dictionary",
                "input": "Unknown name 'metadata': Cannot find field."}]}),
                422
            )
            .is_empty()
        );
        // Root location.
        assert_eq!(
            refused(
                json!({"detail": [{"type": "extra_forbidden", "loc": ["store"],
                    "msg": "Extra inputs are not permitted"}]}),
                422
            ),
            vec!["store"]
        );
        // A nested loc concerns a deeper field.
        assert!(
            refused(
                json!({"detail": [{"type": "extra_forbidden", "loc": ["body", "metadata", "store"],
                "msg": "Extra inputs are not permitted"}]}),
                422
            )
            .is_empty()
        );
    }

    #[test]
    fn openai_wording_names_the_field() {
        assert_eq!(
            refused(
                json!({"error": {"message": "Unknown name \"store\": Cannot find field."}}),
                400
            ),
            vec!["store"]
        );
        assert_eq!(
            refused(
                json!({"error": {"message": "Unrecognized request argument supplied: 'store'"}}),
                400
            ),
            vec!["store"]
        );
        assert_eq!(
            refused(json!({"error": {"message": "'store' is unsupported"}}), 400),
            vec!["store"]
        );
        assert_eq!(
            refused(
                json!({"error": {"message": "Not accepted here", "code": "unsupported_parameter", "param": "store"}}),
                400
            ),
            vec!["store"]
        );
        // Codex's unquoted form.
        assert_eq!(
            refused(
                json!({"message": "Codex: Unsupported parameter: prompt_cache_retention", "type": "invalid_request_error"}),
                400
            ),
            vec!["prompt_cache_retention"]
        );
        // Gemini's nested path is not a top-level field.
        assert!(refused(
            json!({"error": {"message": "Unknown name \"store\" at 'metadata': Cannot find field."}}),
            400
        )
        .is_empty());
        // A value refusal keeps the field: the agent must change the value.
        assert!(refused(
            json!({"error": {"message": "Unsupported value: 'service_tier' is not supported with this model.", "param": "service_tier"}}),
            400
        )
        .is_empty());
        // A param naming another field keeps it.
        assert!(refused(
            json!({"error": {"message": "Unsupported parameter: 'metadata'", "param": "model"}}),
            400
        )
        .is_empty());
        // A field every API requires is never dropped.
        assert!(
            refused(
                json!({"error": {"message": "Unknown name \"messages\": Cannot find field."}}),
                400
            )
            .is_empty()
        );
    }

    #[test]
    fn only_bad_requests_and_known_fields_apply() {
        assert!(
            refused(
                json!({"error": {"message": "Unknown name \"store\": Cannot find field."}}),
                429
            )
            .is_empty()
        );
        assert!(
            refused(
                json!({"error": {"message": "Unknown name \"store\": Cannot find field."}}),
                500
            )
            .is_empty()
        );
        // A field the request does not carry is not named.
        let fault = json!({"message": "Unknown name \"store\": Cannot find field."});
        assert!(refused_optional(400, &json!({"messages": []}), &fault).is_empty());
    }

    #[test]
    fn proxy_prose_around_embedded_json_is_read_by_shape() {
        let raw = r#"{"detail":[{"type":"extra_forbidden","loc":["body","store"],"msg":"Extra inputs are not permitted"}]}"#;
        let fault = json!({
            "error": {
                "message": format!("litellm.BadRequestError: MistralException - {raw}\nSee provider docs."),
                "type": "invalid_request_error"
            }
        });
        assert_eq!(refused_optional(400, &request(), &fault), vec!["store"]);
    }

    #[test]
    fn without_fields_leaves_out_only_the_named() {
        let body = json!({"store": false, "metadata": {"a": 1}, "messages": []});
        let out = without_fields(&body, &["store"]);
        assert!(out.get("store").is_none());
        assert!(out.get("metadata").is_some());
        assert!(out.get("messages").is_some());
    }
}
