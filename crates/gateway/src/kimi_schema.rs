//! Spell a request so Kimi and Moonshot read it.
//!
//! Moonshot's "flavored JSON Schema" refuses a tool schema whose `enum` or
//! `const` names no `type`: 400 "type is not defined" (#886), and the whole
//! request fails though the tool is never called. The type is the one every
//! value has; values of mixed types, or a `null` among them, are left as
//! they were.
//!
//! Since kimi-k2.5 the server locks `temperature` and `top_p` to one value,
//! and a client sending its own defaults (VS Code Copilot's chat sends
//! `temperature` 0.1 and `top_p` 1) is refused 400 before any token is read.
//! Left out, the server fills in the value the model and mode want.

use serde_json::{Map, Value};

/// The hosts Kimi and Moonshot serve.
const KIMI_HOSTS: [&str; 4] = [
    "api.moonshot.ai",
    "api.moonshot.cn",
    "api.kimi.com",
    "api.kimi.ai",
];

/// Whether this host is Moonshot's.
pub(crate) fn kimi_host(host: Option<&str>) -> bool {
    host.is_some_and(|host| KIMI_HOSTS.contains(&host))
}

/// A request body whose tool schemas name the type of each untyped `enum`
/// or `const`, or the body as it came when it is not Kimi's.
pub(crate) fn kimi_tool_enum_types(mut body: Value, host: Option<&str>) -> Value {
    if !kimi_host(host) {
        return body;
    }
    crate::tool_schema::tool_schemas(&mut body, &mut add_enum_type);
    body
}

/// Write the type of a schema's `enum` (or `const`) when it names none.
fn add_enum_type(schema: &mut Map<String, Value>) {
    if schema.contains_key("type") {
        return;
    }
    let values: Vec<&Value> = match (schema.get("enum"), schema.get("const")) {
        (Some(Value::Array(values)), _) => values.iter().collect(),
        (_, Some(value)) => vec![value],
        _ => return,
    };
    let Some(kind) = values_type(&values) else {
        return;
    };
    schema.insert("type".into(), Value::String(kind.into()));
}

/// The JSON Schema type every value has, or `None` when they differ, the
/// list is empty, or a `null` is among them. Whole numbers among fractions
/// make the type `number`.
fn values_type(values: &[&Value]) -> Option<&'static str> {
    let mut kind: Option<&'static str> = None;
    for value in values {
        let this = match value {
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            _ => return None,
        };
        kind = match (kind, this) {
            (None, this) => Some(this),
            (Some("integer"), "number") | (Some("number"), "integer") => Some("number"),
            (Some(seen), this) if seen == this => Some(seen),
            _ => return None,
        };
    }
    kind
}

/// A request body without `temperature` and `top_p` when the model's
/// server locks them, or the body as it came when it is not Kimi's or the
/// model takes a range.
pub(crate) fn kimi_locked_sampling(mut body: Value, host: Option<&str>, model: &str) -> Value {
    if !kimi_host(host) || !kimi_locked_model(model) {
        return body;
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("temperature");
        object.remove("top_p");
    }
    body
}

/// Whether the model's server locks `temperature` and `top_p`:
/// `kimi-for-coding*`, `kimi-k2.5` and later (`kimi-k2.6`, `kimi-k3`, …),
/// and a bare `k3*`. A leading `vendor/` is ignored.
pub(crate) fn kimi_locked_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name.starts_with("kimi-for-coding") {
        return true;
    }
    let rest = if let Some(rest) = name.strip_prefix("kimi-k") {
        rest
    } else if let Some(rest) = name.strip_prefix('k') {
        rest
    } else {
        return false;
    };
    let (major, rest) = leading_int(rest);
    let Some(major) = major else {
        return false;
    };
    if major != 2 {
        return major >= 3;
    }
    // kimi-k2-thinking and kimi-k2-0905-preview take a range.
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    leading_int(rest).0.is_some_and(|minor| minor >= 5)
}

/// The decimal integer `text` begins with, and what follows it.
fn leading_int(text: &str) -> (Option<u32>, &str) {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return (None, text);
    }
    (text[..digits].parse().ok(), &text[digits..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_untyped_enum_gets_the_type_its_values_have() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "type":"object","properties":{
                    "kind":{"enum":["text","icon"]},
                    "count":{"const":3},
                    "ratio":{"enum":[1,2.5]},
                    "mixed":{"enum":["a",1]},
                    "nullable":{"enum":[null,"a"]},
                    "typed":{"type":"string","enum":["a"]}}}}}
        ]});
        let out = kimi_tool_enum_types(body, Some("api.moonshot.ai"));
        let properties = &out["tools"][0]["function"]["parameters"]["properties"];
        assert_eq!(properties["kind"]["type"], "string");
        assert_eq!(properties["count"]["type"], "integer");
        assert_eq!(properties["ratio"]["type"], "number");
        // Mixed values and a null are left as they were.
        assert!(properties["mixed"].get("type").is_none());
        assert!(properties["nullable"].get("type").is_none());
        assert_eq!(properties["typed"]["type"], "string");
    }

    #[test]
    fn only_kimis_hosts_get_the_type_written() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "type":"object","properties":{"kind":{"enum":["text"]}}}}}
        ]});
        for host in [
            Some("api.openai.com"),
            Some("api.moonshot.ai.evil.invalid"),
            None,
        ] {
            assert_eq!(kimi_tool_enum_types(body.clone(), host), body);
        }
    }

    #[test]
    fn a_locked_model_loses_its_sampling_fields() {
        for model in [
            "kimi-k2.5",
            "moonshot/kimi-k2.6",
            "kimi-k3",
            "k3",
            "kimi-for-coding",
        ] {
            assert!(kimi_locked_model(model), "{model}");
        }
        for model in [
            "kimi-k2-thinking",
            "kimi-k2-0905-preview",
            "moonshot-v1-8k",
            "kimi-latest",
        ] {
            assert!(!kimi_locked_model(model), "{model}");
        }
        let body = json!({"model":"kimi-k2.5", "temperature":0.1, "top_p":1,
            "messages":[]});
        let out = kimi_locked_sampling(body, Some("api.moonshot.ai"), "kimi-k2.5");
        assert!(out.get("temperature").is_none());
        assert!(out.get("top_p").is_none());
    }

    #[test]
    fn a_range_model_keeps_its_sampling() {
        let body = json!({"model":"kimi-k2-0905-preview", "temperature":0.1,
            "top_p":1, "messages":[]});
        assert_eq!(
            kimi_locked_sampling(
                body.clone(),
                Some("api.moonshot.ai"),
                "kimi-k2-0905-preview"
            ),
            body
        );
        // A non-Kimi host keeps them too, whatever the model is called.
        assert_eq!(
            kimi_locked_sampling(body.clone(), Some("api.openai.com"), "kimi-k2.5"),
            body
        );
    }
}
