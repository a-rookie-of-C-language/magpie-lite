//! Agent configuration JSON must have an unambiguous object-key mapping.
//! Preserve serde_json syntax/number/depth validation and reject duplicates
//! recursively before lossless JSON/JSONC edits or selective restoration.
use anyhow::{Context, Result, bail};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::{collections::HashMap, fmt, path::Path};

/// JSONC is supported only for agents whose native configuration supports it.
#[derive(Clone, Copy)]
pub(super) enum Syntax {
    Json,
    Jsonc,
}

const MAX_BYTES: usize = 4 * 1024 * 1024;

struct UniqueValue(Value);
struct UniqueVisitor;
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor).map(Self)
    }
}
impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite JSON number"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(UniqueValue(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                // Do not put the key or its values (potential credentials) in errors.
                return Err(de::Error::custom(
                    "duplicate key in agent JSON configuration",
                ));
            }
            let UniqueValue(value) = map.next_value()?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
pub(super) fn from_str(text: &str) -> serde_json::Result<Value> {
    from_slice(text.as_bytes())
}
pub(super) fn from_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let UniqueValue(value) = UniqueValue::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(value)
}

/// Validate with serde's bounded, duplicate-refusing decoder before building a
/// CST. JSONC accepts only comments and trailing commas, not JSON5 extensions.
pub(super) fn parse(text: &str, syntax: Syntax) -> Result<Value> {
    if text.len() > MAX_BYTES {
        bail!("agent JSON exceeds the size limit");
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match syntax {
        Syntax::Json => Ok(from_str(text)?),
        Syntax::Jsonc => {
            let (normalized, _) = normalize(text)?;
            Ok(from_slice(&normalized)?)
        }
    }
}

/// Strip comments to whitespace, keeping string escapes and line boundaries.
/// This deliberately small lexical pass does not parse or evaluate JSON values.
fn normalize(text: &str) -> Result<(Vec<u8>, Vec<String>)> {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut comments = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
        } else if bytes[i..].starts_with(b"//") || bytes[i..].starts_with(b"/*") {
            let start = i;
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && !matches!(bytes[i], b'\r' | b'\n') {
                    i += 1;
                }
            } else {
                i += 2;
                while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                    i += 1;
                }
                if i == bytes.len() {
                    bail!("unterminated JSONC comment");
                }
                i += 2;
            }
            comments.push(text[start..i].to_owned());
            for b in &mut out[start..i] {
                if !matches!(*b, b'\r' | b'\n') {
                    *b = b' ';
                }
            }
        } else {
            i += 1;
        }
    }
    // Only commas following a value may be trailing. Do not turn {,}, [,],
    // double commas, or a missing value into a valid document.
    let mut quoted = false;
    let mut escaped = false;
    let mut prior = None;
    for i in 0..out.len() {
        let b = out[i];
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
                prior = Some(b);
            }
            continue;
        }
        if b == b'"' {
            quoted = true;
        }
        if b == b',' {
            let next = out[i + 1..]
                .iter()
                .copied()
                .find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']'))
                && prior.is_some_and(|p| !matches!(p, b'[' | b'{' | b',' | b':'))
            {
                out[i] = b' ';
            }
        }
        if !b.is_ascii_whitespace() {
            prior = Some(b);
        }
    }
    Ok((out, comments))
}

fn options(syntax: Syntax) -> ParseOptions {
    let jsonc = matches!(syntax, Syntax::Jsonc);
    ParseOptions {
        allow_comments: jsonc,
        allow_trailing_commas: jsonc,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}
fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(a) => CstInputValue::Array(a.iter().map(input).collect()),
        Value::Object(o) => {
            CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), input(v))).collect())
        }
    }
}
fn edit_object(
    object: &CstObject,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
) -> Result<()> {
    for (key, old) in before {
        if after.get(key) == Some(old) {
            continue;
        }
        let prop = object.get(key).context("JSON editor property missing")?;
        match after.get(key) {
            None => prop.remove(),
            Some(new) => {
                if let (Some(a), Some(b)) = (old.as_object(), new.as_object()) {
                    let child = prop.object_value().context("JSON editor object missing")?;
                    edit_object(&child, a, b)?;
                } else {
                    prop.set_value(input(new));
                }
            }
        }
    }
    for (key, new) in after {
        if !before.contains_key(key) {
            object.append(key, input(new));
        }
    }
    Ok(())
}

/// Apply a semantic target to the original syntax tree. Existing foreign values,
/// spelling/key order and trivia are never serialized through a Value. Only
/// changed leaves/subtrees are replaced. A touched compact object may expand.
/// Comments inside a removed/replaced subtree are retained at the document end.
pub(super) fn edit(text: &str, target: &Value, syntax: Syntax) -> Result<String> {
    if text.len() > MAX_BYTES {
        bail!("agent JSON exceeds the size limit");
    }
    let bom = text.starts_with('\u{feff}');
    let source = text.strip_prefix('\u{feff}').unwrap_or(text);
    let blank = source.trim().is_empty();
    let before = if blank {
        Value::Object(Map::new())
    } else {
        parse(source, syntax)?
    };
    let old = before
        .as_object()
        .context("agent JSON root must be an object")?;
    let new = target
        .as_object()
        .context("agent JSON target must be an object")?;
    // Bound the target as well, before CST recursion/insertion.
    parse(&serde_json::to_string(target)?, Syntax::Json)?;
    if before == *target && !blank {
        return Ok(text.to_owned());
    }
    let seed;
    let source = if blank {
        let newline = if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        seed = format!("{source}{{{newline}}}{newline}");
        seed.as_str()
    } else {
        source
    };
    let root = CstRootNode::parse(source, &options(syntax))
        .map_err(|_| anyhow::anyhow!("agent JSON syntax tree could not be parsed"))?;
    let object = root
        .object_value()
        .context("agent JSON root must be an object")?;
    edit_object(&object, old, new)?;
    let mut out = root.to_string();
    if matches!(syntax, Syntax::Jsonc) {
        let (_, original_comments) = normalize(source)?;
        let (_, retained_comments) = normalize(&out)?;
        let mut counts = HashMap::<String, usize>::new();
        for comment in retained_comments {
            *counts.entry(comment).or_default() += 1;
        }
        let newline = if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        for comment in original_comments {
            let count = counts.entry(comment.clone()).or_default();
            if *count > 0 {
                *count -= 1;
            } else {
                if !out.ends_with(['\r', '\n']) {
                    out.push_str(newline);
                }
                out.push_str(&comment);
                out.push_str(newline);
            }
        }
    }
    if bom {
        out.insert(0, '\u{feff}');
    }
    // Independent syntax, duplicate-key, number/depth/byte and semantic checks.
    if parse(&out, syntax)? != *target {
        bail!("agent JSON editor semantic mismatch");
    }
    Ok(out)
}

pub(super) fn write(path: &Path, target: &Value, syntax: Syntax) -> Result<()> {
    let original = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let out = edit(&original, target, syntax)?;
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_keys_are_rejected_recursively_after_escape_decoding() {
        for text in [
            r#"{"model":1,"model":2}"#,
            r#"{"model":1,"\u006dodel":2}"#,
            r#"{"foreign":{"key":1,"key":2}}"#,
            r#"{"foreign":[{"key":1,"key":2}]}"#,
            r#"{"credential-private":1,"credential-private":2}"#,
        ] {
            let error = from_str(text).unwrap_err().to_string();
            assert!(error.contains("duplicate key"));
            assert!(!error.contains("credential-private"));
        }
    }
    #[test]
    fn unique_json_keeps_normal_values_and_validation_bounds() {
        let text = r#"{"first":{"key":1},"second":{"key":2},"values":[null,true,false,-9223372036854775808,18446744073709551615,1.25,"中文\n"]}"#;
        assert_eq!(
            from_str(text).unwrap(),
            serde_json::from_str::<Value>(text).unwrap()
        );
        assert_eq!(
            from_str(&format!("\u{feff}{text}")).unwrap(),
            from_str(text).unwrap()
        );
        for text in [
            r#"{"x":1} {"x":2}"#,
            r#"{"x":1,}"#,
            r#"{"x":1e999}"#,
            r#"{"x":NaN}"#,
        ] {
            assert!(from_str(text).is_err());
        }
        let deeply_nested = format!("{}0{}", "[".repeat(130), "]".repeat(130));
        assert!(from_str(&deeply_nested).is_err());
    }

    #[test]
    fn jsonc_lexing_is_bounded_and_not_json5_or_last_key_wins() {
        let valid = r#"/* header { */ { "url": "https://x//y/*z*/", "q": "\"//",
            "unicode": "中文", "list": [1, 2, /*tail*/], } // footer"#;
        assert_eq!(
            parse(valid, Syntax::Jsonc).unwrap()["list"],
            serde_json::json!([1, 2])
        );
        assert!(parse(valid, Syntax::Json).is_err());
        for source in [
            r#"{,}"#,
            r#"[,]"#,
            r#"[1,,]"#,
            r#"{"x":,}"#,
            r#"{"x":1,,}"#,
            r#"{"x":1} {}"#,
            r#"{unquoted:1}"#,
            r#"{'x':1}"#,
            r#"{"x":+1}"#,
            r#"{"x":0x10}"#,
            "[.5]",
            "[NaN]",
            "[Infinity]",
            "[1e999]",
            "[1 2]",
            r#"{"x":"\x41"}"#,
            "/* unterminated",
            r#"{"x":1, /*comment*/ "\u0078":2,}"#,
            r#"{"list":[{"x":1,"x":2}]}"#,
        ] {
            assert!(parse(source, Syntax::Jsonc).is_err(), "{source}");
        }
        assert!(
            parse(
                &format!("{}0{}", "[".repeat(130), "]".repeat(130)),
                Syntax::Jsonc
            )
            .is_err()
        );
        assert!(parse(&" ".repeat(MAX_BYTES + 1), Syntax::Jsonc).is_err());
    }

    #[test]
    fn edit_keeps_foreign_bytes_escaped_keys_order_bom_and_crlf() {
        let source = concat!(
            "\u{feff}/* header { */\r\n{\r\n",
            "\t\"z-last\": 1e+02, // keep number spelling\r\n",
            "\t\"\\u006dodel\": /* between */ \"old\",\r\n",
            "\t\"foreign\": {\"url\":\"https://a//b/*c*/\",\"enabled\":true},\r\n",
            "}\r\n// footer\r\n"
        );
        let mut target = parse(source, Syntax::Jsonc).unwrap();
        target["model"] = serde_json::json!("新模型\n\"\\");
        target["new"] = serde_json::json!({"nested":[null, true, 18446744073709551615u64]});
        let out = edit(source, &target, Syntax::Jsonc).unwrap();
        assert!(out.starts_with("\u{feff}/* header { */\r\n"));
        assert!(out.ends_with("// footer\r\n"));
        for exact in [
            "\t\"z-last\": 1e+02, // keep number spelling\r\n",
            "\"\\u006dodel\": /* between */ ",
            "\t\"foreign\": {\"url\":\"https://a//b/*c*/\",\"enabled\":true},\r\n",
        ] {
            assert!(out.contains(exact), "lost {exact:?} in {out}");
        }
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert!(out.find("z-last").unwrap() < out.find("foreign").unwrap());
        assert_eq!(parse(&out, Syntax::Jsonc).unwrap(), target);
    }

    #[test]
    fn nested_edit_delete_and_atomic_replacement_keep_every_comment() {
        let source = r#"{
          // top note
          "provider": {"id": /* id note */ "old", "remove": { /* lost container */ "x": 1}},
          "array": [1, /* inside array */ 2],
          "theme": "dark" // theme note
        }"#;
        let mut target = parse(source, Syntax::Jsonc).unwrap();
        target["provider"]["id"] = serde_json::json!("new");
        target["provider"].as_object_mut().unwrap().remove("remove");
        target["array"] = serde_json::json!([3]);
        let out = edit(source, &target, Syntax::Jsonc).unwrap();
        for comment in [
            "// top note",
            "/* id note */",
            "/* lost container */",
            "/* inside array */",
            "// theme note",
        ] {
            assert_eq!(out.matches(comment).count(), 1, "{out}");
        }
        assert!(out.contains("\"theme\": \"dark\" // theme note"));
        assert_eq!(parse(&out, Syntax::Jsonc).unwrap(), target);
        let empty = edit(&out, &serde_json::json!({}), Syntax::Jsonc).unwrap();
        assert_eq!(parse(&empty, Syntax::Jsonc).unwrap(), serde_json::json!({}));
        for comment in [
            "// top note",
            "/* id note */",
            "/* lost container */",
            "/* inside array */",
            "// theme note",
        ] {
            assert_eq!(empty.matches(comment).count(), 1, "{empty}");
        }
    }

    #[test]
    fn no_op_is_exact_and_comment_multiplicity_is_retained() {
        let source = r#"{"a": [/* same */1], "b": {/* same */"x":2}, /* same */}"#;
        let value = parse(source, Syntax::Jsonc).unwrap();
        assert_eq!(edit(source, &value, Syntax::Jsonc).unwrap(), source);
        let out = edit(source, &serde_json::json!({"a":0}), Syntax::Jsonc).unwrap();
        assert_eq!(out.matches("/* same */").count(), 3);
        assert_eq!(
            parse(&out, Syntax::Jsonc).unwrap(),
            serde_json::json!({"a":0})
        );
    }

    #[test]
    fn shape_matrix_and_empty_documents_produce_independently_valid_targets() {
        let sources = [
            r#"{}"#,
            r#"{"a":null}"#,
            r#"{"a":false,"foreign":1e2}"#,
            r#"{"a":{"b":1,"c":[true]},"foreign":"中文"}"#,
            r#"{"a":[1,{"x":2}]}"#,
        ];
        for source in sources {
            for target in [
                serde_json::json!({}),
                serde_json::json!({"a":0}),
                serde_json::json!({"a":{"b":1,"d":false},"foreign":true}),
                serde_json::json!({"a":[{"x":2}],"new":"escaped\n"}),
            ] {
                let out = edit(source, &target, Syntax::Json).unwrap();
                assert_eq!(
                    from_str(&out).unwrap(),
                    target,
                    "source {source}, output {out}"
                );
            }
        }
        for source in ["", " \r\n\r\n", "\u{feff}\r\n"] {
            let target = serde_json::json!({"model":"mock/model"});
            let out = edit(source, &target, Syntax::Json).unwrap();
            assert_eq!(from_str(&out).unwrap(), target);
            if source.contains("\r\n") {
                assert!(!out.replace("\r\n", "").contains('\n'));
            }
            assert_eq!(out.starts_with('\u{feff}'), source.starts_with('\u{feff}'));
        }
    }
}
