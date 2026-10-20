//! Spell a request so Kimi and Moonshot read it.
//!
//! Moonshot's "flavored JSON Schema" refuses a tool schema whose `enum` or
//! `const` names no `type`: 400 "type is not defined" (#886), and the whole
//! request fails though the tool is never called. The type is the one every
//! value has, or that type and `"null"` when a null is among them. It also
//! expands each `$ref` and refuses a target that names no type, so a target
//! a tool reaches is given the type its keywords or branches imply.
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

/// A request body whose tool schemas give a type to each untyped schema a
/// local `$ref` reaches, or the body as it came when it is not Kimi's.
///
/// Moonshot expands a `$ref` and refuses a target with no type: 400 "not a
/// valid moonshot flavored json schema … At path 'properties.b': detected
/// infinite recursion without termination condition". Codex sends zod
/// `$defs` that leave a `oneOf` or `allOf` target untyped, so the whole
/// request fails though the tool is never called.
pub(crate) fn kimi_tool_ref_types(mut body: Value, host: Option<&str>) -> Value {
    if !kimi_host(host) || !body_has_ref(&body) {
        return body;
    }
    crate::tool_schema::tool_root_schemas(&mut body, &mut add_ref_types);
    body
}

/// Whether any `$ref` appears in the body, so a body without one costs no
/// second schema walk.
fn body_has_ref(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| key == "$ref" || body_has_ref(value)),
        Value::Array(items) => items.iter().any(body_has_ref),
        _ => false,
    }
}

/// Write a type into each untyped schema a local `$ref` under `root` reaches.
///
/// A target that is `{}`, an `anyOf` (Moonshot refuses a type beside one),
/// or a `$ref` with nothing but notes beside it is left as it was.
fn add_ref_types(root: &mut Map<String, Value>) {
    let mut schema = Value::Object(std::mem::take(root));
    let snapshot = schema.clone();
    let mut targets = Vec::new();
    collect_ref_targets(&snapshot, &snapshot, &mut targets);
    for (pointer, kind) in targets {
        let Some(target) = schema.pointer_mut(&pointer) else {
            continue;
        };
        let Some(object) = target.as_object_mut() else {
            continue;
        };
        object.insert("type".into(), kind);
    }
    if let Value::Object(object) = schema {
        *root = object;
    }
}

/// Collect each distinct untyped `$ref` target under `node` and the type it
/// should carry. `pointer` names the target in `root`.
fn collect_ref_targets(root: &Value, node: &Value, out: &mut Vec<(String, Value)>) {
    walk_schema(node, &mut |schema| {
        let Some(Value::String(reference)) = schema.get("$ref") else {
            return;
        };
        let Some(pointer) = ref_target(root, reference) else {
            return;
        };
        let Some(target) = root.pointer(&pointer) else {
            return;
        };
        let Some(target) = target.as_object() else {
            return;
        };
        if !untyped_for_kimi(target) {
            return;
        }
        let Some(kind) = kimi_schema_type(root, &Value::Object(target.clone())) else {
            return;
        };
        if !out.iter().any(|(seen, _)| *seen == pointer) {
            out.push((pointer, kind));
        }
    });
}

/// Visit every schema under `node`, through the keywords that hold schemas.
fn walk_schema(node: &Value, visit: &mut impl FnMut(&Map<String, Value>)) {
    match node {
        Value::Object(object) => {
            visit(object);
            for key in crate::tool_schema::SCHEMA_MAP_KEYS {
                if let Some(Value::Object(schemas)) = object.get(key) {
                    for schema in schemas.values() {
                        walk_schema(schema, visit);
                    }
                }
            }
            for key in crate::tool_schema::SCHEMA_KEYS {
                if let Some(schema) = object.get(key) {
                    walk_schema(schema, visit);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_schema(item, visit);
            }
        }
        _ => {}
    }
}

/// The JSON Pointer a local `$ref` names, or `None` for another document.
fn ref_target(root: &Value, reference: &str) -> Option<String> {
    if reference == "#" {
        return Some(String::new());
    }
    let pointer = reference.strip_prefix('#')?;
    if !pointer.starts_with('/') {
        return None;
    }
    root.pointer(pointer)?;
    Some(pointer.to_owned())
}

/// Whether Moonshot refuses this `$ref` target as untyped: it names no type,
/// isn't `{}`, isn't an `anyOf`, and isn't only another `$ref` with notes.
fn untyped_for_kimi(target: &Map<String, Value>) -> bool {
    if target.contains_key("type") || target.contains_key("anyOf") || target.is_empty() {
        return false;
    }
    let only_ref = target.keys().all(|key| {
        matches!(
            key.as_str(),
            "$ref" | "description" | "title" | "default" | "examples" | "$comment"
        )
    });
    !(only_ref && target.contains_key("$ref"))
}

/// Every JSON Schema type, which Moonshot takes for a schema that may be
/// anything.
fn all_kimi_types() -> Value {
    Value::Array(
        [
            "string", "number", "integer", "boolean", "object", "array", "null",
        ]
        .into_iter()
        .map(|kind| Value::String(kind.into()))
        .collect(),
    )
}

/// The type an untyped schema is given: the one its enum, keywords or
/// branches say, or every type when they say nothing.
fn kimi_schema_type(root: &Value, schema: &Value) -> Option<Value> {
    if let Some(Value::Array(values)) = schema.get("enum") {
        return enum_type(&values.iter().collect::<Vec<_>>());
    }
    if let Some(value) = schema.get("const") {
        return enum_type(&[value]);
    }
    let types = schema_types(root, schema, 0);
    let Some(types) = types else {
        return Some(all_kimi_types());
    };
    let mut names = Vec::new();
    for kind in [
        "string", "number", "integer", "boolean", "object", "array", "null",
    ] {
        if types.contains(&kind) {
            names.push(kind);
        }
    }
    if names.contains(&"integer") && names.contains(&"number") {
        names.retain(|kind| *kind != "integer");
    }
    match names.as_slice() {
        [] => Some(all_kimi_types()),
        [one] => Some(Value::String((*one).into())),
        _ => Some(Value::Array(
            names
                .into_iter()
                .map(|kind| Value::String(kind.into()))
                .collect(),
        )),
    }
}

/// The types a schema can have, as its type, keywords, values or branches
/// (through their `$ref`s) say, or `None` when they say nothing.
fn schema_types(root: &Value, schema: &Value, depth: usize) -> Option<Vec<&'static str>> {
    if depth > 16 {
        return None;
    }
    let object = schema.as_object()?;
    if let Some(kind) = object.get("type") {
        let mut out = Vec::new();
        match kind {
            Value::String(kind) => out.push(kind_name(kind)?),
            Value::Array(kinds) => {
                for kind in kinds {
                    out.push(kind_name(kind.as_str()?)?);
                }
            }
            _ => return None,
        }
        return Some(out);
    }
    if let Some(Value::String(reference)) = object.get("$ref") {
        let pointer = ref_target(root, reference)?;
        return schema_types(root, root.pointer(&pointer)?, depth + 1);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return Some(value_types(values));
    }
    if let Some(value) = object.get("const") {
        return Some(value_types(std::slice::from_ref(value)));
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        let Some(branches) = object.get(key).and_then(Value::as_array) else {
            continue;
        };
        let mut out = Vec::new();
        let mut known = false;
        for branch in branches {
            let Some(types) = schema_types(root, branch, depth + 1) else {
                if key != "allOf" {
                    return None;
                }
                continue;
            };
            for kind in types {
                if !out.contains(&kind) {
                    out.push(kind);
                }
                known = true;
            }
        }
        if known {
            return Some(out);
        }
    }
    let has = |keys: &[&str]| keys.iter().any(|key| object.contains_key(*key));
    let kind = if has(&[
        "properties",
        "required",
        "additionalProperties",
        "patternProperties",
        "minProperties",
        "maxProperties",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "unevaluatedProperties",
    ]) {
        "object"
    } else if has(&[
        "items",
        "prefixItems",
        "minItems",
        "maxItems",
        "uniqueItems",
        "contains",
        "unevaluatedItems",
    ]) {
        "array"
    } else if has(&[
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "contentEncoding",
        "contentMediaType",
    ]) {
        "string"
    } else if has(&[
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ]) {
        "number"
    } else {
        return None;
    };
    Some(vec![kind])
}

fn kind_name(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "string" => "string",
        "number" => "number",
        "integer" => "integer",
        "boolean" => "boolean",
        "object" => "object",
        "array" => "array",
        "null" => "null",
        _ => return None,
    })
}

/// The types a list of JSON values has.
fn value_types(values: &[Value]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for value in values {
        let kind = match value {
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Null => "null",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    out
}

/// Write the type of a schema's `enum` (or `const`) when it names none.
///
/// One type plus `null` becomes `[T, "null"]`, the one type array Moonshot
/// takes beside an enum. Other mixed values are left as they were.
fn add_enum_type(schema: &mut Map<String, Value>) {
    if schema.contains_key("type") {
        return;
    }
    let values: Vec<&Value> = match (schema.get("enum"), schema.get("const")) {
        (Some(Value::Array(values)), _) => values.iter().collect(),
        (_, Some(value)) => vec![value],
        _ => return,
    };
    let Some(kind) = enum_type(&values) else {
        return;
    };
    schema.insert("type".into(), kind);
}

/// The type an enum or const is written with for Moonshot: the one every
/// non-null value has, or that type and `null`, or `None` when the values
/// have no single type or are all null.
fn enum_type(values: &[&Value]) -> Option<Value> {
    let mut null = false;
    let mut rest = Vec::with_capacity(values.len());
    for value in values {
        if value.is_null() {
            null = true;
        } else {
            rest.push(*value);
        }
    }
    let kind = values_type(&rest)?;
    if null {
        Some(Value::Array(vec![
            Value::String(kind.into()),
            Value::String("null".into()),
        ]))
    } else {
        Some(Value::String(kind.into()))
    }
}

/// The JSON Schema type every value has, or `None` when they differ or the
/// list is empty. Whole numbers among fractions make the type `number`.
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
        // One type plus null becomes the type array Moonshot takes.
        assert!(properties["mixed"].get("type").is_none());
        assert_eq!(properties["nullable"]["type"], json!(["string", "null"]));
        assert_eq!(properties["typed"]["type"], "string");
    }

    #[test]
    fn a_ref_target_gets_the_type_its_keywords_and_branches_say() {
        let schema = json!({
            "type":"object",
            "properties":{
                "a":{"$ref":"#/$defs/empty"},
                "b":{"$ref":"#/$defs/union"},
                "c":{"items":{"$ref":"#/$defs/all"},"type":"array"},
                "d":{"anyOf":[{"$ref":"#/$defs/props"},{"type":"null"}]},
                "e":{"$ref":"#/$defs/minlen"},
                "f":{"$ref":"#/definitions/anything"},
                "g":{"$ref":"#/$defs/refs"},
                "h":{"$ref":"#/$defs/any"},
                "i":{"$ref":"#/$defs/typed"},
                "j":{"$ref":"#/$defs/enum"}
            },
            "$defs":{
                "empty":{},
                "union":{"oneOf":[{"type":"string"},{"type":"number"}]},
                "all":{"allOf":[{"type":"string"}]},
                "props":{"properties":{"x":{"type":"string"}}},
                "minlen":{"minLength":1},
                "refs":{"oneOf":[{"$ref":"#/$defs/props"},{"$ref":"#/definitions/anything"}]},
                "any":{"anyOf":[{"type":"string"},{"$ref":"#/$defs/any"}]},
                "typed":{"type":"string","enum":["x"]},
                "enum":{"enum":["a",null]}
            },
            "definitions":{"anything":{"description":"anything"}}
        });
        let body = json!({"model":"up","tools":[
            {"type":"function","function":{"name":"t","parameters":schema}}
        ]});
        let out = kimi_tool_ref_types(body, Some("api.moonshot.ai"));
        let defs = &out["tools"][0]["function"]["parameters"]["$defs"];
        let anything = json!([
            "string", "number", "integer", "boolean", "object", "array", "null"
        ]);
        // An empty target, an anyOf, and a pure $ref chain stay as they were.
        assert!(defs["empty"].get("type").is_none());
        assert_eq!(defs["union"]["type"], json!(["string", "number"]));
        assert_eq!(defs["all"]["type"], "string");
        assert_eq!(defs["props"]["type"], "object");
        assert_eq!(defs["minlen"]["type"], "string");
        assert_eq!(defs["refs"]["type"], anything);
        assert!(defs["any"].get("type").is_none());
        assert_eq!(defs["typed"]["type"], "string");
        assert_eq!(defs["enum"]["type"], json!(["string", "null"]));
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["definitions"]["anything"]["type"],
            anything
        );
    }

    #[test]
    fn only_kimis_hosts_get_ref_targets_typed() {
        let body = json!({"model":"up","tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "type":"object",
                "properties":{"a":{"$ref":"#/$defs/a"}},
                "$defs":{"a":{"oneOf":[{"type":"string"},{"type":"number"}]}}
            }}}
        ]});
        for host in [
            Some("api.openai.com"),
            Some("api.moonshot.ai.evil.invalid"),
            None,
        ] {
            assert_eq!(kimi_tool_ref_types(body.clone(), host), body);
        }
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
