//! Spell a tool schema as Gemini takes it.
//!
//! Gemini's function-calling schema is an OpenAPI subset, not full JSON
//! Schema: a list `type` (`["string","null"]`, as zod-to-json-schema writes
//! a nullable field), an `array` with no `items`, `items` that are a list
//! or `true`, `properties` or an `enum` with no `type` are all refused —
//! Devin's Gemini models answer the same shapes with `invalid_argument`
//! (the reference's `devinGeminiSchema`, `1530c5ef`, 7eleven on Discord).
//! Lite has no Devin backend, but every Gemini-protocol upstream applies
//! the same restriction to `functionDeclarations[].parameters`, so the
//! sanitizer runs on the protocol, not on a host.
//!
//! A list type becomes its one type, `nullable` when it had `"null"`, or
//! `anyOf` of each type; such an array's `items` become `{}` (a
//! one-schema list becomes that schema); a schema with `properties`, or a
//! root with no type, is an `object`; an `enum`'s type comes from its
//! values, and an `enum` of mixed values is dropped. A schema needing
//! none of it is sent as it came.

use serde_json::{Map, Value, json};

/// The deepest a tool schema nests before the walk stops: past that a
/// schema is handed to the vendor as it is, so a recursive `$defs` tree
/// cannot park the request.
const SCHEMA_DEPTH: usize = 32;

/// A request body whose Gemini tool schemas are spelled as Gemini reads
/// them, or the body as it came when the upstream is not Gemini.
pub(crate) fn gemini_tool_schemas(mut body: Value, gemini: bool) -> Value {
    if !gemini {
        return body;
    }
    crate::tool_schema::tool_root_schemas(&mut body, &mut |schema| {
        gemini_schema_root(schema);
    });
    body
}

/// The root of one tool's parameters. A root with no `type` at all is an
/// `object` — Gemini requires it — and then the whole schema is walked.
fn gemini_schema_root(schema: &mut Map<String, Value>) {
    if !schema.contains_key("type") {
        schema.insert("type".into(), json!("object"));
        if !schema.contains_key("properties") {
            schema.insert("properties".into(), json!({}));
        }
    }
    gemini_schema_in(schema, 0);
}

/// Rewrite `schema` in place as Gemini takes it.
fn gemini_schema_in(schema: &mut Map<String, Value>, depth: usize) {
    if depth > SCHEMA_DEPTH {
        return;
    }
    for key in crate::tool_schema::SCHEMA_MAP_KEYS {
        if let Some(Value::Object(schemas)) = schema.get_mut(key) {
            for nested in schemas.values_mut() {
                if let Value::Object(nested) = nested {
                    gemini_schema_in(nested, depth + 1);
                }
            }
        }
    }
    for key in [
        "anyOf",
        "oneOf",
        "allOf",
        "prefixItems",
        "additionalProperties",
        "not",
        "if",
        "then",
        "else",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
    ] {
        match schema.get_mut(key) {
            Some(Value::Object(nested)) => gemini_schema_in(nested, depth + 1),
            Some(Value::Array(list)) => {
                for item in list {
                    if let Value::Object(item) = item {
                        gemini_schema_in(item, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    // A list type becomes its one type, `nullable` when it had "null",
    // else `anyOf` of each.
    if let Some(Value::Array(list)) = schema.get("type").cloned() {
        let mut some: Vec<String> = Vec::new();
        let mut null = false;
        for t in &list {
            let Some(t) = t.as_str() else { continue };
            if t.is_empty() || some.iter().any(|s| s == t) {
                continue;
            }
            if t == "null" {
                null = true;
            } else {
                some.push(t.to_owned());
            }
        }
        if null && !some.is_empty() {
            schema.insert("nullable".into(), json!(true));
        }
        match some.len() {
            1 => {
                schema.insert("type".into(), json!(some[0]));
            }
            0 if null => {
                schema.insert("type".into(), json!("null"));
            }
            0 => {
                schema.remove("type");
            }
            _ => {
                schema.remove("type");
                let mut branches = match schema.remove("anyOf") {
                    Some(Value::Array(branches)) => branches,
                    _ => Vec::new(),
                };
                for t in some {
                    let mut branch = json!({"type": t});
                    if t == "array" {
                        branch["items"] = json!({});
                    }
                    branches.push(branch);
                }
                schema.insert("anyOf".into(), Value::Array(branches));
            }
        }
    }

    let typed = schema.contains_key("type");

    // An enum's type comes from its values; one of mixed values is
    // dropped rather than refused.
    if let Some(Value::Array(values)) = schema.get("enum").cloned() {
        if !typed {
            let mut kinds = std::collections::BTreeSet::new();
            let mut kept = Vec::new();
            for v in values {
                match &v {
                    Value::Null => {
                        schema.insert("nullable".into(), json!(true));
                        continue;
                    }
                    Value::String(_) => {
                        kinds.insert("string");
                    }
                    Value::Bool(_) => {
                        kinds.insert("boolean");
                    }
                    Value::Number(n) => {
                        if n.is_f64() {
                            kinds.insert("number");
                        } else {
                            kinds.insert("integer");
                        }
                    }
                    _ => {
                        kinds.insert("other");
                    }
                }
                kept.push(v);
            }
            let one = match kinds.len() {
                1 => kinds.iter().next().copied(),
                2 if kinds.contains("integer") && kinds.contains("number") => Some("number"),
                _ => None,
            };
            match one {
                Some(kind) if kind != "other" => {
                    schema.insert("type".into(), json!(kind));
                    schema.insert("enum".into(), Value::Array(kept));
                }
                _ => {
                    schema.remove("enum");
                }
            }
        }
    }

    let typed = schema.contains_key("type");

    // A schema with properties is an object.
    if matches!(schema.get("properties"), Some(Value::Object(_))) && !typed {
        schema.insert("type".into(), json!("object"));
    }

    // An array without items, or whose items are a list or true, gets a
    // free one (a one-schema list becomes that schema).
    let has_items = schema.contains_key("items");
    let is_array = schema.get("type").and_then(Value::as_str) == Some("array")
        || (!schema.contains_key("type") && has_items);
    if is_array {
        match schema.get_mut("items") {
            Some(Value::Object(items)) => gemini_schema_in(items, depth + 1),
            Some(Value::Array(list)) => {
                let single = if list.len() == 1 {
                    list.first_mut()
                        .and_then(Value::as_object_mut)
                        .map(|first| {
                            gemini_schema_in(first, depth + 1);
                            Value::Object(first.clone())
                        })
                } else {
                    None
                };
                schema.insert("items".into(), single.unwrap_or_else(|| json!({})));
            }
            _ => {
                schema.insert("items".into(), json!({}));
            }
        }
        if schema.get("type").and_then(Value::as_str) != Some("array") {
            schema.insert("type".into(), json!("array"));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What Gemini refuses in a schema, at any depth — the checks the
    /// reference test made against the real account's refusals.
    fn faults(schema: &Value, at: &str) -> Vec<String> {
        let Some(m) = schema.as_object() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if matches!(m.get("type"), Some(Value::Array(_))) {
            out.push(format!("{at} list type"));
        }
        let typed = m.contains_key("type");
        if !typed && (m.contains_key("properties") || m.contains_key("enum")) {
            out.push(format!("{at} no type"));
        }
        if m.get("type").and_then(Value::as_str) == Some("array")
            && !matches!(m.get("items"), Some(Value::Object(_)))
        {
            out.push(format!("{at} items"));
        }
        if let Some(Value::Object(props)) = m.get("properties") {
            for (k, v) in props {
                out.extend(faults(v, &format!("{at}.{k}")));
            }
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            if let Some(Value::Array(list)) = m.get(key) {
                for (i, v) in list.iter().enumerate() {
                    out.extend(faults(v, &format!("{at}.{key}[{i}]")));
                }
            }
        }
        if let Some(items) = m.get("items") {
            out.extend(faults(items, &format!("{at}[]")));
        }
        out
    }

    /// Rewrite one schema as a tool root and return it.
    fn rewrite(schema: Value) -> Value {
        let mut body = json!({
            "tools": [{"functionDeclarations": [{"name": "t", "parameters": schema}]}]
        });
        body = gemini_tool_schemas(body, true);
        body.pointer("/tools/0/functionDeclarations/0/parameters")
            .expect("parameters")
            .clone()
    }

    /// The schemas the reference's real account refused, one each
    /// (devinGeminiRefused, `1530c5ef`).
    #[test]
    fn the_schemas_gemini_refused_all_go_through() {
        let refused = [
            (
                "typeList",
                json!({"type":"object","properties":{"a":{"type":["string","null"]}}}),
            ),
            (
                "typeListOne",
                json!({"type":"object","properties":{"a":{"type":["string"]}}}),
            ),
            (
                "typeListTwo",
                json!({"type":"object","properties":{"a":{"type":["string","number"]}}}),
            ),
            (
                "noItems",
                json!({"type":"object","properties":{"a":{"type":"array"}}}),
            ),
            (
                "itemsTrue",
                json!({"type":"object","properties":{"a":{"type":"array","items":true}}}),
            ),
            (
                "itemsList",
                json!({"type":"object","properties":{"a":{"type":"array","items":[{"type":"string"}]}}}),
            ),
            (
                "deepNoItems",
                json!({"type":"object","properties":{"a":{"type":"object","properties":{"b":{"type":"array"}}}}}),
            ),
            (
                "unionNoItems",
                json!({"type":"object","properties":{"a":{"anyOf":[{"type":"array"},{"type":"null"}]}}}),
            ),
            (
                "propsNoType",
                json!({"type":"object","properties":{"a":{"properties":{"b":{"type":"string"}}}}}),
            ),
            (
                "enumNoType",
                json!({"type":"object","properties":{"a":{"enum":["x","y"]}}}),
            ),
            ("rootNoType", json!({"properties":{"a":{"type":"string"}}})),
            ("emptyRoot", json!({})),
        ];
        for (name, schema) in refused {
            let out = rewrite(schema);
            assert_eq!(
                out.get("type").and_then(Value::as_str),
                Some("object"),
                "{name}: the root is an object"
            );
            assert!(
                faults(&out, "$").is_empty(),
                "{name}: faults left in {}",
                serde_json::to_string(&out).unwrap()
            );
        }
    }

    /// A list type's rewrites: one type stays itself, "null" marks it
    /// nullable, several become anyOf branches.
    #[test]
    fn a_list_type_becomes_one_nullable_or_an_any_of() {
        let out = rewrite(json!({"type":"object","properties":{
            "a":{"type":["string","null"]},
            "b":{"type":["string"]},
            "c":{"type":["string","number"]}
        }}));
        let props = out["properties"].as_object().unwrap();
        assert_eq!(props["a"]["type"], "string");
        assert_eq!(props["a"]["nullable"], true);
        assert_eq!(props["b"]["type"], "string");
        assert!(props["b"].get("nullable").is_none());
        assert!(props["c"].get("type").is_none());
        let branches = props["c"]["anyOf"].as_array().unwrap();
        assert_eq!(
            branches,
            &vec![json!({"type":"string"}), json!({"type":"number"})]
        );
    }

    /// An array's items: a one-schema list becomes that schema, anything
    /// else a free object.
    #[test]
    fn an_arrays_items_are_filled_or_listed_once() {
        let out = rewrite(json!({"type":"object","properties":{
            "a":{"type":"array"},
            "b":{"type":"array","items":[{"type":"string"}]},
            "c":{"type":"array","items":[{"type":"string"},{"type":"number"}]}
        }}));
        let props = out["properties"].as_object().unwrap();
        assert_eq!(props["a"]["items"], json!({}));
        assert_eq!(props["b"]["items"], json!({"type":"string"}));
        assert_eq!(props["c"]["items"], json!({}));
    }

    /// An enum's type comes from its values; a mixed one is dropped.
    #[test]
    fn an_enum_takes_its_values_type_or_goes() {
        let out = rewrite(json!({"type":"object","properties":{
            "a":{"enum":["x","y"]},
            "b":{"enum":[1,2,3]},
            "c":{"enum":["x",1]},
            "d":{"enum":["x",null]}
        }}));
        let props = out["properties"].as_object().unwrap();
        assert_eq!(props["a"]["type"], "string");
        assert_eq!(props["b"]["type"], "integer");
        assert!(props["c"].get("enum").is_none(), "a mixed enum is dropped");
        assert_eq!(props["d"]["type"], "string");
        assert_eq!(props["d"]["nullable"], true);
        assert_eq!(props["d"]["enum"], json!(["x"]));
    }

    /// A schema needing nothing is sent as it came — the walk is not a
    /// rewrite for its own sake.
    #[test]
    fn a_schema_gemini_already_takes_is_untouched() {
        let schema = json!({"type":"object","properties":{
            "a":{"type":"string","enum":["x","y"]},
            "b":{"type":"array","items":{"type":"number"}}
        },"required":["a"]});
        assert_eq!(rewrite(schema.clone()), schema);
    }

    /// Only a Gemini upstream is spelled so: a Chat one keeps its list
    /// type and untyped enum.
    #[test]
    fn a_non_gemini_body_is_untouched() {
        let schema = json!({"type":"object","properties":{"a":{"type":["string","null"]}}});
        let mut body = json!({
            "tools": [{"functionDeclarations": [{"name": "t", "parameters": schema}]}]
        });
        let before = body.clone();
        body = gemini_tool_schemas(body, false);
        assert_eq!(body, before);
    }
}
