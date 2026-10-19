//! Spell a tool's regex so DeepSeek's schema validator reads it.
//!
//! DeepSeek turns away a tool whose `pattern` holds the ECMAScript null
//! escape `\0`: "Invalid schema for function … is not a 'regex'". JSON
//! Schema's `pattern` is an ECMAScript regex, where `\0` is the NUL
//! character; DeepSeek's validator takes `\u0000` for the same character
//! instead. A Claude Code session's Artifact tool carries `^[^\0]*$`
//! (#370), and the same pattern reaches DeepSeek under an Anthropic or
//! Chat tool alike.
//!
//! A pattern needs nothing when it holds no standalone `\0`, and it is
//! only DeepSeek's — the `api.deepseek.com` host — so every other
//! provider's tools go as they came.

use serde_json::Value;

/// Replace each standalone ECMAScript null escape `\0` with the Unicode
/// escape `\u0000` DeepSeek's validator takes, or answer `None` when the
/// pattern holds none.
///
/// A pair of backslashes is literal (`\\0` is a backslash and a zero), and
/// an octal escape such as `\01` has its own meaning, so neither is
/// touched.
fn unicode_null_escape(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 == bytes.len() {
            index += 1;
            continue;
        }
        let next = bytes[index + 1];
        let octal = index + 2 < bytes.len() && bytes[index + 2].is_ascii_digit();
        if next == b'0' && !octal {
            let text = out.get_or_insert_with(|| String::with_capacity(pattern.len() + 5));
            text.push_str(&pattern[copied..index]);
            text.push_str(r"\u0000");
            index += 2;
            copied = index;
            continue;
        }
        index += 2;
    }
    if let Some(text) = out.as_mut() {
        text.push_str(&pattern[copied..]);
    }
    out
}

/// A request body with DeepSeek's tool patterns spelled for its validator,
/// or the body as it came when the provider is not DeepSeek's or nothing
/// needed respelling.
pub(crate) fn deepseek_tool_patterns(mut body: Value, host: Option<&str>) -> Value {
    if host != Some("api.deepseek.com") {
        return body;
    }
    let Some(tools) = body.get_mut("tools") else {
        return body;
    };
    crate::tool_schema::rewrite_patterns(tools, false, unicode_null_escape);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_null_escape_is_only_the_standalone_one() {
        for (pattern, want) in [
            (r"^[^\0]*$", Some(r"^[^\u0000]*$".to_string())),
            // A pair of backslashes is literal, and an octal escape has its
            // own meaning.
            (r"\\0", None),
            (r"\01", None),
            (r"\u0000", None),
            (r"^https?://", None),
            (r"\\\0", Some(r"\\\u0000".to_string())),
        ] {
            assert_eq!(unicode_null_escape(pattern), want, "{pattern}");
        }
    }

    #[test]
    fn a_standalone_null_escape_becomes_its_unicode_form() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"Artifact",
                "parameters":{"type":"object","properties":{
                    "content":{"type":"string","pattern":"^[^\\0]*$"}}}}}
        ]});
        let out = deepseek_tool_patterns(body, Some("api.deepseek.com"));
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["properties"]["content"]["pattern"],
            r"^[^\u0000]*$"
        );
        // A pattern that needs nothing comes back as it was.
        let plain = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t",
                "parameters":{"type":"object","properties":{"x":{"pattern":"^https?://"}}}}}
        ]});
        assert_eq!(
            deepseek_tool_patterns(plain.clone(), Some("api.deepseek.com")),
            plain
        );
    }

    #[test]
    fn only_deepseek_normalizes_nulls() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t",
                "parameters":{"type":"object","properties":{"x":{"pattern":"^[^\\0]*$"}}}}}
        ]});
        for host in [
            Some("api.openai.com"),
            Some("api.deepseek.com.evil.invalid"),
            None,
        ] {
            assert_eq!(deepseek_tool_patterns(body.clone(), host), body);
        }
    }
}
