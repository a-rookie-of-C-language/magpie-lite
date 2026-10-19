//! Rename a tool schema's `oneOf` to `anyOf` for the routers that refuse
//! the former.
//!
//! OpenRouter serves some models behind a grammar compiler (xgrammar) that
//! turns away `oneOf` whose branches are not provably disjoint:
//! "unsupported schema keyword: oneOf with overlapping or non-provably-
// disjoint branches", a 400, and takes `anyOf`. A model choosing what to
// call reads either as "one of these"; the branches a JSON Schema `oneOf`
// holds are the ones an `anyOf` holds, and a client that meant exactly one
// still matches the union.
//!
//! Only `openrouter.ai` is rewritten — the reference also names its Cline
//! and Kilo presets, which lite has no concept of — and every other host's
//! tools go as they came.

use serde_json::map::Entry;
use serde_json::{Map, Value};

/// A request body with OpenRouter's tool schemas reading `anyOf` where
/// they said `oneOf`, or the body as it came when OpenRouter is not the
/// host.
pub(crate) fn one_of_as_any_of(mut body: Value, host: Option<&str>) -> Value {
    if host != Some("openrouter.ai") {
        return body;
    }
    crate::tool_schema::tool_schemas(&mut body, &mut |schema| rename_one_of(schema));
    body
}

/// Rename a schema's `oneOf` to `anyOf`, keeping its place among the other
/// keys. A schema that wrote both — the reference's byte rename would
/// leave it with two `anyOf` keys — has them merged into the one the
/// `oneOf` held, since the union is what either said.
fn rename_one_of(schema: &mut Map<String, Value>) {
    if !schema.contains_key("oneOf") {
        return;
    }
    let mut rebuilt = Map::with_capacity(schema.len());
    for (key, value) in std::mem::take(schema) {
        let key = if key == "oneOf" {
            "anyOf".to_string()
        } else {
            key
        };
        match rebuilt.entry(key) {
            // The key keeps this schema's position, so nothing else moves.
            Entry::Vacant(slot) => {
                slot.insert(value);
            }
            // Both keywords were written; one `anyOf` holds the union.
            Entry::Occupied(mut slot) => {
                let existing = slot.get_mut();
                if let (Value::Array(existing), Value::Array(more)) = (existing, value) {
                    existing.extend(more);
                }
            }
        }
    }
    *schema = rebuilt;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openrouters_tool_one_of_becomes_any_of_in_place() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "type":"object","properties":{
                    "x":{"oneOf":[{"type":"string"},{"type":"integer"}]},
                    "y":{"items":{"oneOf":[{"type":"string"}]}}}}}}
        ], "temperature":0.2});
        let out = one_of_as_any_of(body, Some("openrouter.ai"));
        let x = &out["tools"][0]["function"]["parameters"]["properties"]["x"];
        assert!(x.get("oneOf").is_none());
        assert_eq!(x["anyOf"], json!([{"type":"string"},{"type":"integer"}]));
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["properties"]["y"]["items"]["anyOf"],
            json!([{"type":"string"}])
        );
        assert_eq!(out["temperature"], 0.2);
    }

    #[test]
    fn the_renamed_key_keeps_its_place() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "description":"before", "oneOf":[{"type":"string"}], "title":"after"}}}
        ]});
        let out = one_of_as_any_of(body, Some("openrouter.ai"));
        let keys: Vec<&String> = out["tools"][0]["function"]["parameters"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(keys, vec!["description", "anyOf", "title"]);
    }

    #[test]
    fn an_any_of_already_written_takes_the_one_ofs_branches() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "oneOf":[{"type":"string"}], "anyOf":[{"type":"boolean"}]}}}
        ]});
        let out = one_of_as_any_of(body, Some("openrouter.ai"));
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["anyOf"],
            json!([{"type":"string"},{"type":"boolean"}])
        );
        assert!(
            out["tools"][0]["function"]["parameters"]
                .get("oneOf")
                .is_none()
        );
    }

    #[test]
    fn every_other_host_keeps_one_of() {
        let body = json!({"model":"up", "tools":[
            {"type":"function","function":{"name":"t","parameters":{
                "oneOf":[{"type":"string"}]}}}
        ]});
        for host in [
            Some("api.openai.com"),
            Some("openrouter.ai.evil.invalid"),
            None,
        ] {
            assert_eq!(one_of_as_any_of(body.clone(), host), body);
        }
    }
}
