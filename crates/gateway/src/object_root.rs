//! Fold a tool's parameter schema to an object root where an upstream
//! refuses a union there.
//!
//! Codex desktop's `mcp__codex_app__automation_update` tool has an
//! `anyOf` at the root of its parameters. xAI, Anthropic behind a relay,
//! and OpenAI-compatible validators may refuse that shape even though it
//! is valid JSON Schema. xAI's own API is always asked with the root
//! folded; another upstream is asked again only after it names the root
//! union, and that refusal is remembered for the same provider, model and
//! protocol.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::config::ProviderConfig;

/// How an upstream says it refuses a union at a tool parameter's root.
fn root_union_refusal() -> &'static Regex {
    static REFUSAL: OnceLock<Regex> = OnceLock::new();
    REFUSAL.get_or_init(|| {
        Regex::new(
            r#"(?i)tool parameter root must be an object type|does not support oneOf, allOf, or anyOf at the top level|schema must have type .object. and not have .oneOf.|schema must be a JSON Schema of .type: \\?"object\\?".(?:\n|)"#,
        )
        .expect("root union refusal regex")
    })
}

/// Whether an upstream error body is a refusal of a union at a tool's
/// parameter root.
pub(crate) fn refusal(body: &Value) -> bool {
    root_union_refusal().is_match(&body.to_string())
}

/// Whether requests to this provider/model/protocol should start with
/// tool roots folded. xAI's own API takes no root union; another
/// upstream starts folded only after it has refused one.
pub(crate) fn folds_roots(provider: &ProviderConfig, model: &str) -> bool {
    let xai = reqwest::Url::parse(&provider.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .as_deref()
        == Some("api.x.ai");
    xai || crate::unfit_fields::unfit(
        &provider.id,
        model,
        provider.protocol.as_str(),
        "root_union",
    )
}

/// Fold every tool parameter schema in a request to an object root and
/// report whether any schema changed.
pub(crate) fn fold_tool_roots(body: &mut Value) -> bool {
    let mut changed = false;
    crate::tool_schema::tool_root_schemas(body, &mut |schema| {
        let root = schema.clone();
        if object_root_in(&root, schema, 0) {
            changed = true;
        }
    });
    changed
}

/// Fold one tool parameter schema to an object root. `root` is the
/// schema's original object, used to resolve local `$ref`s while branches
/// are copied and merged.
fn object_root_in(
    root: &Map<String, Value>,
    schema: &mut Map<String, Value>,
    depth: usize,
) -> bool {
    let has_union = ["anyOf", "oneOf", "allOf"]
        .iter()
        .any(|key| schema.contains_key(*key));
    if schema.get("type").and_then(Value::as_str) == Some("object") && !has_union {
        return false;
    }

    let mut properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut required = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // allOf contributes every property and every required field. A field
    // the root already defines keeps the root's schema.
    if let Some(all_of) = schema
        .remove("allOf")
        .and_then(|value| value.as_array().cloned())
    {
        for branch in all_of {
            let Some(branch) = resolve_branch(root, &branch) else {
                continue;
            };
            if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
                for (name, value) in branch_properties {
                    if !properties.contains_key(name) {
                        properties.insert(name.clone(), value.clone());
                    }
                }
            }
            if let Some(branch_required) = branch.get("required").and_then(Value::as_array) {
                required.extend(branch_required.iter().cloned());
            }
        }
    }

    let mut branches = Vec::new();
    for key in ["anyOf", "oneOf"] {
        let Some(list) = schema
            .remove(key)
            .and_then(|value| value.as_array().cloned())
        else {
            continue;
        };
        for branch in list {
            let Some(branch) = resolve_branch(root, &branch) else {
                continue;
            };
            let mut branch = branch;
            if nested_union(&branch) && depth < 8 {
                object_root_in(root, &mut branch, depth + 1);
            }
            // A union branch that is not an object contributes no fields
            // to an object-rooted schema.
            if branch.get("type").and_then(Value::as_str) != Some("object")
                && !branch.contains_key("properties")
            {
                continue;
            }
            branches.push(branch);
        }
    }

    // A field required by every object branch stays required. Fields the
    // root does not define are merged in; differing branch schemas become
    // an anyOf, with identical schemas deduplicated.
    let mut common_required: Option<Vec<Value>> = None;
    let mut from_branches: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for branch in &branches {
        if let Some(branch_props) = branch.get("properties").and_then(Value::as_object) {
            for (name, value) in branch_props {
                if properties.contains_key(name) {
                    continue;
                }
                let values = from_branches.entry(name.clone()).or_default();
                if !values.contains(value) {
                    values.push(value.clone());
                }
            }
        }

        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        common_required = Some(match common_required {
            None => branch_required,
            Some(common) => common
                .into_iter()
                .filter(|field| branch_required.contains(field))
                .collect(),
        });
    }

    for (name, values) in from_branches {
        let value = if values.len() == 1 {
            values.into_iter().next().expect("one schema")
        } else {
            json!({"anyOf": values})
        };
        properties.insert(name, value);
    }
    required.extend(common_required.unwrap_or_default());

    let mut seen = BTreeSet::new();
    required.retain(|field| {
        field
            .as_str()
            .map(|name| seen.insert(name.to_owned()))
            .unwrap_or(true)
    });

    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if required.is_empty() {
        schema.remove("required");
    } else {
        schema.insert("required".into(), Value::Array(required));
    }
    true
}

/// Whether a branch has another union at its own root.
fn nested_union(branch: &Map<String, Value>) -> bool {
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .any(|key| branch.contains_key(*key))
}

/// Resolve a branch's local `$ref` against the tool schema's `$defs` or
/// `definitions`; a non-reference branch is copied as it is.
fn resolve_branch(root: &Map<String, Value>, branch: &Value) -> Option<Map<String, Value>> {
    let branch = branch.as_object()?;
    let Some(reference) = branch.get("$ref").and_then(Value::as_str) else {
        return Some(branch.clone());
    };
    for key in ["$defs", "definitions"] {
        let Some(name) = reference.strip_prefix(&format!("#/{key}/")) else {
            continue;
        };
        return root
            .get(key)
            .and_then(Value::as_object)
            .and_then(|definitions| definitions.get(name))
            .and_then(Value::as_object)
            .cloned();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protocol, ProviderConfig, RoutingPolicy};
    use serde_json::json;

    fn folded(schema: Value) -> Value {
        let mut body = json!({"tools": [{"type": "function", "name": "t", "parameters": schema}]});
        assert!(fold_tool_roots(&mut body));
        body["tools"][0]["parameters"].clone()
    }

    #[test]
    fn all_of_properties_and_required_are_merged() {
        let out = folded(json!({
            "allOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {"type": "object", "properties": {"b": {"type": "integer"}}, "required": ["b"]}
            ]
        }));
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert_eq!(out["properties"]["b"]["type"], "integer");
        assert_eq!(out["required"], json!(["a", "b"]));
        assert!(out.get("allOf").is_none());
    }

    #[test]
    fn any_of_keeps_only_fields_every_branch_requires() {
        let out = folded(json!({
            "anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}, "required": ["a", "b"]},
                {"type": "object", "properties": {"a": {"type": "string"}, "c": {"type": "integer"}}, "required": ["a"]}
            ]
        }));
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert_eq!(out["properties"]["b"]["type"], "string");
        assert_eq!(out["properties"]["c"]["type"], "integer");
        assert_eq!(out["required"], json!(["a"]));
        assert!(out.get("anyOf").is_none());
    }

    #[test]
    fn differing_branch_schemas_become_any_of() {
        let out = folded(json!({
            "oneOf": [
                {"type": "object", "properties": {"value": {"type": "string"}}},
                {"type": "object", "properties": {"value": {"type": "integer"}}},
                {"type": "object", "properties": {"value": {"type": "string"}}}
            ]
        }));
        assert_eq!(
            out["properties"]["value"],
            json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})
        );
    }

    #[test]
    fn nested_unions_are_folded_on_a_branch_copy() {
        let out = folded(json!({
            "anyOf": [
                {
                    "anyOf": [
                        {"type": "object", "properties": {"x": {"type": "string"}}},
                        {"type": "object", "properties": {"y": {"type": "integer"}}}
                    ]
                }
            ]
        }));
        assert_eq!(out["properties"]["x"]["type"], "string");
        assert_eq!(out["properties"]["y"]["type"], "integer");
    }

    #[test]
    fn local_refs_resolve_from_defs_and_definitions() {
        let out = folded(json!({
            "$defs": {"a": {"type": "object", "properties": {"a": {"type": "string"}}}},
            "definitions": {"b": {"type": "object", "properties": {"b": {"type": "integer"}}}},
            "anyOf": [
                {"$ref": "#/$defs/a"},
                {"$ref": "#/definitions/b"}
            ]
        }));
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert_eq!(out["properties"]["b"]["type"], "integer");
    }

    #[test]
    fn non_object_branches_are_ignored() {
        let out = folded(json!({
            "oneOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "string"}
            ]
        }));
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["a"]["type"], "string");
    }

    #[test]
    fn an_object_root_without_a_union_is_unchanged() {
        let mut body = json!({"tools": [{"type": "function", "name": "t", "parameters": {
            "type": "object", "properties": {"a": {"type": "string"}}
        }}]});
        let before = body.clone();
        assert!(!fold_tool_roots(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn every_dialects_tool_root_is_folded() {
        let union = json!({
            "anyOf": [
                {"type": "object", "properties": {"chat": {"type": "string"}}},
                {"type": "object", "properties": {"flat": {"type": "string"}}}
            ]
        });
        let mut body = json!({"tools": [
            {"type": "function", "function": {"name": "chat", "parameters": union.clone()}},
            {"name": "anthropic", "input_schema": union.clone()},
            {"functionDeclarations": [
                {"name": "gemini", "parameters": union.clone()}
            ]},
            {"type": "namespace", "name": "ns", "tools": [
                {"type": "function", "name": "flat", "parameters": union}
            ]}
        ]});
        assert!(fold_tool_roots(&mut body));
        for at in [
            "/tools/0/function/parameters",
            "/tools/1/input_schema",
            "/tools/2/functionDeclarations/0/parameters",
            "/tools/3/tools/0/parameters",
        ] {
            assert_eq!(body.pointer(at).unwrap()["type"], "object", "{at}");
            assert!(body.pointer(at).unwrap().get("anyOf").is_none(), "{at}");
        }
    }

    #[test]
    fn a_union_under_a_property_is_not_a_tool_root() {
        let mut body = json!({"tools": [{"type": "function", "name": "t", "parameters": {
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [{"type": "string"}, {"type": "integer"}]
                }
            }
        }}]});
        let before = body.clone();
        assert!(!fold_tool_roots(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn every_root_union_refusal_wording_is_known() {
        for body in [
            json!({"error": {"message": "tool parameter root must be an object type"}}),
            json!({"error": {"message": "input_schema does not support oneOf, allOf, or anyOf at the top level"}}),
            json!({"error": {"message": "schema must have type 'object' and not have 'oneOf'/'anyOf'/'allOf'/'enum'/'const'/'not' at the top level"}}),
            json!({"error": {"message": "schema must be a JSON Schema of 'type: \"object\"', got 'type: \"None\"'"}}),
        ] {
            assert!(refusal(&body), "{body}");
        }
    }

    #[test]
    fn unrelated_invalid_schema_wordings_are_not_root_union_refusals() {
        for body in [
            json!({"error": {"message": "Invalid schema for function 'read': In context=('properties', 'path'), 'format' is not permitted."}}),
            json!({"error": {"message": "Invalid 'tools[0].name': string does not match pattern."}}),
        ] {
            assert!(!refusal(&body), "{body}");
        }
    }

    fn provider(id: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Chat,
            capabilities: Default::default(),
            base_url: base_url.into(),
            api_key: "k".into(),
            api_keys: Vec::new(),
            models: vec!["m".into()],
            contexts: Default::default(),
            outputs: Default::default(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
            key_limits: Vec::new(),
        }
    }

    #[test]
    fn xai_starts_folded_and_another_vendor_does_not() {
        assert!(folds_roots(
            &provider("xai-fold-fixture", "https://api.x.ai/v1"),
            "m"
        ));
        assert!(!folds_roots(
            &provider("other-fold-fixture", "https://relay.invalid/v1"),
            "m"
        ));
    }
}
