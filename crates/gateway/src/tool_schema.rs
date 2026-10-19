//! Walk a request's tool schemas to the `pattern` strings inside them, so
//! a provider that reads one differently than JSON Schema says can be
//! handed a spelling it takes.
//!
//! A tool's schema sits wherever its dialect puts it — Chat under
//! `function.parameters`, Anthropic under `input_schema`, Gemini under
//! `functionDeclarations[].parameters`, a bare schema under `parameters`,
//! and a Responses namespace's own nested `tools` — and a schema holds
//! another only through the keywords the reference gateway names. Sample
//! data (`enum`, `const`, `default`, `examples`) is not a schema, so a
//! value that happens to be called `pattern` there keeps its text.

use serde_json::Value;

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
///
/// The walk is a whitelist, as the reference gateway's is: a tool is
/// entered through `function`, `functionDeclarations`, `parameters`,
/// `input_schema` or a namespace's nested `tools`, and a schema only
/// through the keywords that hold another schema. Lite walks the value
/// tree it just built, so unlike the reference it also reaches Gemini's
/// `functionDeclarations` and a Responses namespace's `tools`.
pub(crate) fn rewrite_patterns(
    value: &mut Value,
    in_schema: bool,
    fix: fn(&str) -> Option<String>,
) -> bool {
    match value {
        Value::Object(object) => {
            let mut changed = false;
            if in_schema {
                if let Some(Value::String(pattern)) = object.get_mut("pattern") {
                    if let Some(rewritten) = fix(pattern) {
                        *pattern = rewritten;
                        changed = true;
                    }
                }
                for key in SCHEMA_MAP_KEYS {
                    if let Some(Value::Object(schemas)) = object.get_mut(key) {
                        for schema in schemas.values_mut() {
                            changed |= rewrite_patterns(schema, true, fix);
                        }
                    }
                }
                for key in SCHEMA_KEYS {
                    if let Some(child) = object.get_mut(key) {
                        changed |= rewrite_patterns(child, true, fix);
                    }
                }
            } else {
                // A schema, wherever this dialect puts one.
                for key in ["input_schema", "parameters"] {
                    if let Some(child) = object.get_mut(key) {
                        changed |= rewrite_patterns(child, true, fix);
                    }
                }
                // A container of tools: Chat's `function` object, Gemini's
                // `functionDeclarations` array and a Responses namespace's
                // nested `tools`.
                for key in ["function", "functionDeclarations", "tools"] {
                    if let Some(child) = object.get_mut(key) {
                        changed |= rewrite_patterns(child, false, fix);
                    }
                }
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= rewrite_patterns(item, in_schema, fix);
            }
            changed
        }
        _ => false,
    }
}
