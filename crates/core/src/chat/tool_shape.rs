//! Put a Chat request's tools in the Chat shape.
//!
//! A Chat Completions tool is
//! `{"type":"function","function":{name,description,parameters,strict}}`.
//! Some clients send theirs in another API's shape on the Chat route:
//! MCP's `{"name","description","inputSchema"}` (ZCode, #2622),
//! Anthropic's `input_schema`, a bare `{"name","parameters"}`, or
//! Responses' flat `{"type":"function","name,…}`. OpenAI and the strict
//! Chat APIs turn every one of those away ("field Tools[0].Type invalid",
//! "function cannot be empty"), and reading no name offered a tool with
//! none.
//!
//! A tool that already has its `function` object, a hosted tool (any
//! other type), one with no name, and a request that doesn't parse are
//! as they came. A tool's other fields stay beside its function.

use serde_json::{Map, Value, json};

/// Whether `tool` is a function tool with its name at the top rather
/// than in a `function` object.
fn unshaped(tool: &Value) -> bool {
    let Some(object) = tool.as_object() else {
        return false;
    };
    if object.contains_key("function") {
        return false;
    }
    match object.get("type").and_then(Value::as_str) {
        Some("function") | None => {}
        Some(_) => return false,
    }
    object
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
}

/// The Chat shape of one unshaped function tool.
fn shape(tool: &Value) -> Option<Value> {
    let mut object = tool.as_object()?.clone();
    let mut function = Map::new();
    for key in ["name", "description", "strict"] {
        if let Some(value) = object.remove(key) {
            function.insert(key.into(), value);
        }
    }
    for key in ["parameters", "inputSchema", "input_schema"] {
        if let Some(value) = object.remove(key) {
            function.entry("parameters").or_insert(value);
        }
    }
    object.insert("type".into(), json!("function"));
    object.insert("function".into(), Value::Object(function));
    Some(Value::Object(object))
}

/// A request body whose function tools are in the Chat shape. A body
/// with none of those shapes comes back unchanged.
pub(crate) fn shape_chat_tools(mut body: Value) -> Value {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return body;
    };
    for tool in tools.iter_mut() {
        if !unshaped(tool) {
            continue;
        }
        if let Some(shaped) = shape(tool) {
            *tool = shaped;
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_foreign_shape_becomes_a_chat_function() {
        let body = json!({"model":"m", "messages":[], "tools":[
            {"name":"mcp","description":"from MCP","inputSchema":{"type":"object"}},
            {"name":"anthropic","input_schema":{"type":"object","properties":{}}},
            {"name":"bare","parameters":{"type":"object"}},
            {"type":"function","name":"flat","description":"from Responses","parameters":{"type":"object"}}
        ]});
        let out = shape_chat_tools(body);
        let tools = out["tools"].as_array().unwrap();
        for tool in tools {
            assert_eq!(tool["type"], "function");
            assert!(
                tool["function"]["name"]
                    .as_str()
                    .is_some_and(|n| !n.is_empty())
            );
        }
        assert_eq!(tools[0]["function"]["parameters"], json!({"type":"object"}));
        assert_eq!(
            tools[1]["function"]["parameters"],
            json!({"type":"object","properties":{}})
        );
        assert_eq!(tools[3]["function"]["description"], "from Responses");
    }

    #[test]
    fn a_parameters_field_wins_over_an_input_schema() {
        let out = shape_chat_tools(json!({"tools":[{"name":"t",
            "parameters":{"kind":"chat"}, "inputSchema":{"kind":"mcp"}}]}));
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"kind":"chat"})
        );
    }

    #[test]
    fn already_shaped_hosted_and_nameless_tools_are_untouched() {
        let shaped = json!({"type":"function","function":{"name":"ok","parameters":{}}});
        let hosted = json!({"type":"web_search"});
        let nameless = json!({"description":"no name"});
        let body = json!({"tools":[shaped.clone(), hosted.clone(), nameless.clone()]});
        assert_eq!(shape_chat_tools(body.clone()), body);
        assert_eq!(shape_chat_tools(json!({"model":"m"})), json!({"model":"m"}));
    }

    #[test]
    fn other_fields_stay_beside_the_function() {
        let out = shape_chat_tools(json!({"tools":[{"name":"t","extra":{"kept":true}}]}));
        assert_eq!(out["tools"][0]["extra"], json!({"kept":true}));
    }
}
