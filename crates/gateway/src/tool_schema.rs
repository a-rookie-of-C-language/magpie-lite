//! Walk a request's tool schemas, so a provider that reads one differently
//! than JSON Schema says can be handed a shape it takes.
//!
//! A tool's schema sits wherever its dialect puts it — Chat under
//! `function.parameters`, Anthropic under `input_schema`, Gemini under
//! `functionDeclarations[].parameters`, a bare schema under `parameters`,
//! and a Responses namespace's own nested `tools` — and a schema holds
//! another only through the keywords the reference gateway names. Sample
//! data (`enum`, `const`, `default`, `examples`) is not a schema, so a
//! value that happens to be called `pattern` there keeps its text.

use serde_json::{Map, Value};

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

/// Replace every `pattern` of a tool's schema in place with `fix` of it,
/// and report whether anything changed. `fix` answers `None` when the
/// pattern needs nothing, so a schema that is already right costs no
/// allocation.
pub(crate) fn rewrite_patterns(
    value: &mut Value,
    in_schema: bool,
    fix: fn(&str) -> Option<String>,
) -> bool {
    let mut changed = false;
    walk(value, in_schema, &mut |schema| {
        if let Some(Value::String(pattern)) = schema.get_mut("pattern") {
            if let Some(rewritten) = fix(pattern) {
                *pattern = rewritten;
                changed = true;
            }
        }
    });
    changed
}

/// Visit every schema of the tools in a request body, root first and then
/// each nested schema, so a caller can change the ones a provider reads
/// differently. `visit` gets the schema's own object.
///
/// The walk is a whitelist, as the reference gateway's is: a tool is
/// entered through `function`, `functionDeclarations`, `parameters`,
/// `input_schema` or a namespace's nested `tools`, and a schema only
/// through the keywords that hold another schema. Lite walks the value
/// tree it just built, so unlike the reference it also reaches Gemini's
/// `functionDeclarations` and a Responses namespace's `tools`.
pub(crate) fn tool_schemas(body: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    if let Some(tools) = body.get_mut("tools") {
        walk(tools, false, visit);
    }
}

/// Visit only the root parameter schemas of the tools in a request body.
/// This is the walk a whole-schema rewrite uses when it must not turn a
/// nested property union into an object root.
pub(crate) fn tool_root_schemas(body: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    if let Some(tools) = body.get_mut("tools") {
        walk_tool_roots(tools, visit);
    }
}

fn walk_tool_roots(value: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    match value {
        Value::Object(object) => {
            for key in ["input_schema", "parameters"] {
                if let Some(Value::Object(schema)) = object.get_mut(key) {
                    visit(schema);
                }
            }
            for key in ["function", "functionDeclarations", "tools"] {
                if let Some(child) = object.get_mut(key) {
                    walk_tool_roots(child, visit);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_tool_roots(item, visit);
            }
        }
        _ => {}
    }
}

fn walk(value: &mut Value, in_schema: bool, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    match value {
        Value::Object(object) => {
            if in_schema {
                visit(object);
                for key in SCHEMA_MAP_KEYS {
                    if let Some(Value::Object(schemas)) = object.get_mut(key) {
                        for schema in schemas.values_mut() {
                            walk(schema, true, visit);
                        }
                    }
                }
                for key in SCHEMA_KEYS {
                    if let Some(child) = object.get_mut(key) {
                        walk(child, true, visit);
                    }
                }
            } else {
                // A schema, wherever this dialect puts one.
                for key in ["input_schema", "parameters"] {
                    if let Some(child) = object.get_mut(key) {
                        walk(child, true, visit);
                    }
                }
                // A container of tools: Chat's `function` object, Gemini's
                // `functionDeclarations` array and a Responses namespace's
                // nested `tools`.
                for key in ["function", "functionDeclarations", "tools"] {
                    if let Some(child) = object.get_mut(key) {
                        walk(child, false, visit);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, in_schema, visit);
            }
        }
        _ => {}
    }
}
