//! Spell a tool's regex so a Rust regex reads it as the ECMAScript regex
//! JSON Schema says it is.
//!
//! xAI turns away a tool whose `pattern` Rust's regex syntax can't parse.
//! There a `[` inside a character class opens a nested class (and `&&`,
//! `--`, `~~` are set operations), where ECMAScript reads a plain
//! character. A Claude Code session on Windows has `^[^\s[\]]+$` for a
//! breakpoint's class name and selector, so a Grok model was turned away
//! on every turn (#2317). Escaping such a character is the same character
//! to ECMAScript.
//!
//! It is Grok's whoever serves it — xAI's own API, or a model named
//! `grok…` through a relay. Every other model's tools go as they came.

use serde_json::Value;

/// Escape, inside each character class of an ECMAScript pattern, a `[`
/// and the second character of `&&`, `--` and `~~`, which a Rust regex
/// would read as a nested class or a set operation. Escapes are kept as
/// they are, and nothing outside a class changes.
fn ecma_class_for_rust(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(c);
            out.push(next);
            continue;
        }
        match c {
            '[' if !in_class => {
                in_class = true;
                out.push(c);
            }
            ']' if in_class => {
                in_class = false;
                out.push(c);
            }
            '[' if in_class => out.push_str(r"\["),
            '&' | '-' | '~' if in_class && chars.peek() == Some(&c) => {
                out.push(c);
                out.push('\\');
                out.push(c);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether this model is one xAI serves, as its id is spelled on the
/// wire: `grok-4.6`, `xai/grok-4.6`, a relay's `grok-4.6-fast`.
pub(crate) fn grok_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower
        .rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("grok"))
}

/// Keywords whose value is an object of schemas, one per name.
const SCHEMA_MAP_KEYS: [&str; 5] = [
    "properties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "patternProperties",
];

/// Keywords whose value is itself a schema, or an array of them.
const SCHEMA_KEYS: [&str; 15] = [
    "items",
    "additionalProperties",
    "contains",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
    "allOf",
    "anyOf",
    "oneOf",
    "prefixItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
];

/// Rewrite every `pattern` of a tool's schema in place, and report
/// whether anything changed.
///
/// The walk is a whitelist, as the reference gateway's is: a tool is
/// entered through `function`, `functionDeclarations`, `parameters`,
/// `input_schema` or a namespace's nested `tools`, and a schema only
/// through the keywords that hold another schema. Sample data — `enum`,
/// `const`, `default`, `examples` — is not a schema, so a value that
/// happens to be named `pattern` there keeps its text. Lite walks the
/// value tree it just built, so unlike the reference it also reaches
/// Gemini's `functionDeclarations` and a Responses namespace's `tools`.
fn rewrite(value: &mut Value, in_schema: bool) -> bool {
    match value {
        Value::Object(object) => {
            let mut changed = false;
            if in_schema {
                if let Some(Value::String(pattern)) = object.get_mut("pattern") {
                    let rewritten = ecma_class_for_rust(pattern);
                    if rewritten != *pattern {
                        *pattern = rewritten;
                        changed = true;
                    }
                }
                for key in SCHEMA_MAP_KEYS {
                    if let Some(Value::Object(schemas)) = object.get_mut(key) {
                        for schema in schemas.values_mut() {
                            changed |= rewrite(schema, true);
                        }
                    }
                }
                for key in SCHEMA_KEYS {
                    if let Some(child) = object.get_mut(key) {
                        changed |= rewrite(child, true);
                    }
                }
            } else {
                // A schema, wherever this dialect puts one.
                for key in ["input_schema", "parameters"] {
                    if let Some(child) = object.get_mut(key) {
                        changed |= rewrite(child, true);
                    }
                }
                // A container of tools: Chat's `function` object, Gemini's
                // `functionDeclarations` array and a Responses namespace's
                // nested `tools`.
                for key in ["function", "functionDeclarations", "tools"] {
                    if let Some(child) = object.get_mut(key) {
                        changed |= rewrite(child, false);
                    }
                }
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= rewrite(item, in_schema);
            }
            changed
        }
        _ => false,
    }
}

/// A request body with Grok's tool patterns spelled for its regex, or
/// the body as it came when it is not Grok's or has no tool pattern.
pub(crate) fn grok_tool_patterns(mut body: Value, model: &str, host: Option<&str>) -> Value {
    if host != Some("api.x.ai") && !grok_model(model) {
        return body;
    }
    let Some(tools) = body.get_mut("tools") else {
        return body;
    };
    rewrite(tools, false);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_nested_class_is_escaped_for_rust() {
        assert_eq!(ecma_class_for_rust(r"^[^\s[\]]+$"), r"^[^\s\[\]]+$");
        assert_eq!(ecma_class_for_rust(r"[a-z]"), r"[a-z]");
        assert_eq!(ecma_class_for_rust(r"[\[]"), r"[\[]");
        assert_eq!(ecma_class_for_rust(r"[a&&b]"), r"[a&\&b]");
        assert_eq!(ecma_class_for_rust(r"[a--b]"), r"[a-\-b]");
        assert_eq!(ecma_class_for_rust(r"[a~~b]"), r"[a~\~b]");
        // Outside a class nothing changes.
        assert_eq!(ecma_class_for_rust(r"a&&b"), r"a&&b");
    }

    #[test]
    fn only_grok_gets_its_tool_patterns_respelled() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t",
                "parameters":{"type":"object","properties":{"x":{"type":"string","pattern":"^[^\\s[\\]]+$"}}}}}
        ]});
        let out = grok_tool_patterns(body.clone(), "grok-4.6", None);
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["properties"]["x"]["pattern"],
            r"^[^\s\[\]]+$"
        );
        // xAI's own API, and a relay's model name, count too.
        assert_eq!(
            grok_tool_patterns(body.clone(), "other", Some("api.x.ai"))["tools"][0]["function"]["parameters"]
                ["properties"]["x"]["pattern"],
            r"^[^\s\[\]]+$"
        );
        // Another model's tools go as they came.
        assert_eq!(
            grok_tool_patterns(body.clone(), "claude-x", Some("relay.invalid")),
            body
        );
    }

    #[test]
    fn sample_data_named_pattern_is_not_respelled() {
        let body = json!({"model":"grok-4.6", "tools":[{"type":"function","function":{"name":"t",
            "parameters":{"type":"object","properties":{
                "x":{"type":"string","enum":[{"pattern":"[a&&b]"}]},
                "y":{"type":"string","default":{"pattern":"[a&&b]"},
                     "examples":[{"pattern":"[a&&b]"}],
                     "pattern":"[a&&b]"}}}}}], "tool_choice":"auto"});
        let out = grok_tool_patterns(body, "grok-4.6", None);
        let properties = &out["tools"][0]["function"]["parameters"]["properties"];
        // A value that merely sits under `enum`, `default` or `examples` is
        // sample data, not a schema, and keeps its text.
        assert_eq!(properties["x"]["enum"][0]["pattern"], "[a&&b]");
        assert_eq!(properties["y"]["default"]["pattern"], "[a&&b]");
        assert_eq!(properties["y"]["examples"][0]["pattern"], "[a&&b]");
        // The schema's own pattern is the one that changes.
        assert_eq!(properties["y"]["pattern"], r"[a&\&b]");
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn a_body_with_nothing_to_respell_comes_back_as_it_was() {
        let body = json!({"model":"grok-4.6", "messages":[], "tools":[
            {"type":"function","function":{"name":"t","parameters":{"type":"object"}}}]});
        assert_eq!(grok_tool_patterns(body.clone(), "grok-4.6", None), body);
        let no_tools = json!({"model":"grok-4.6", "messages":[]});
        assert_eq!(
            grok_tool_patterns(no_tools.clone(), "grok-4.6", None),
            no_tools
        );
    }

    #[test]
    fn every_dialects_tool_container_is_walked() {
        let pattern = r"^[^\s[\]]+$";
        let rewritten = r"^[^\s\[\]]+$";
        // Chat nests the schema under `function.parameters`, Anthropic under
        // `input_schema`, Gemini under `functionDeclarations[].parameters`
        // and Responses under a namespace's nested `tools`.
        let body = json!({"model":"grok-4.6", "tools":[
            {"type":"function","function":{"name":"chat","parameters":{
                "type":"object","properties":{"x":{"pattern":pattern}}}}},
            {"name":"anthropic","input_schema":{"type":"object","properties":{"x":{"pattern":pattern}}}},
            {"functionDeclarations":[
                {"name":"gemini","parameters":{"type":"object","properties":{"x":{"pattern":pattern}}}}]},
            {"type":"namespace","name":"ns","tools":[
                {"type":"function","name":"flat","parameters":{
                    "type":"object","properties":{"x":{"pattern":pattern}}}}]}
        ]});
        let out = grok_tool_patterns(body, "grok-4.6", None);
        for at in [
            "/tools/0/function/parameters/properties/x/pattern",
            "/tools/1/input_schema/properties/x/pattern",
            "/tools/2/functionDeclarations/0/parameters/properties/x/pattern",
            "/tools/3/tools/0/parameters/properties/x/pattern",
        ] {
            assert_eq!(
                out.pointer(at).and_then(Value::as_str),
                Some(rewritten),
                "{at}"
            );
        }
    }
}
