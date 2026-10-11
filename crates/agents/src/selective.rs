//! Reversible, explicit ownership of adapter fields. Lists/model picks are atomic.
//! Parsed values decide conflicts; format-specific editors preserve unrelated text.
use super::lifecycle::{AgentId, Image, Result, conflict};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_yaml::{Mapping, Value};
use std::str::FromStr;

type Field = Vec<String>;
fn fields(paths: &[&[&str]]) -> Vec<Field> {
    paths
        .iter()
        .map(|p| p.iter().map(|s| (*s).to_owned()).collect())
        .collect()
}
fn declared(id: AgentId, index: usize) -> Vec<Field> {
    match (id, index) {
        (AgentId::Claude, _) => fields(&[
            &["env", "ANTHROPIC_BASE_URL"],
            &["env", "ANTHROPIC_AUTH_TOKEN"],
            &["model"],
        ]),
        (AgentId::Codex, _) => fields(&[
            &["model"],
            &["model_provider"],
            &["model_providers", "magpie-lite", "name"],
            &["model_providers", "magpie-lite", "base_url"],
            &["model_providers", "magpie-lite", "wire_api"],
            &[
                "model_providers",
                "magpie-lite",
                "experimental_bearer_token",
            ],
        ]),
        (AgentId::Gemini, 0) => {
            fields(&[&["security", "auth", "selectedType"], &["model", "name"]])
        }
        (AgentId::Gemini, _) => fields(&[&["GOOGLE_GEMINI_BASE_URL"], &["GEMINI_API_KEY"]]),
        (AgentId::Crush, 0) => fields(&[
            &["providers", "magpie-lite", "type"],
            &["providers", "magpie-lite", "name"],
            &["providers", "magpie-lite", "base_url"],
            &["providers", "magpie-lite", "api_key"],
            &["providers", "magpie-lite", "models"],
            &["models", "large"],
        ]),
        (AgentId::Crush, _) => fields(&[&["models", "large"]]),
        (AgentId::Opencode, _) => fields(&[
            &["model"],
            &["provider", "magpie-lite", "npm"],
            &["provider", "magpie-lite", "name"],
            &["provider", "magpie-lite", "options", "baseURL"],
            &["provider", "magpie-lite", "options", "apiKey"],
            &["provider", "magpie-lite", "models"],
        ]),
        (AgentId::Zed, _) => fields(&[
            &[
                "language_models",
                "openai_compatible",
                "magpie-lite",
                "api_url",
            ],
            &[
                "language_models",
                "openai_compatible",
                "magpie-lite",
                "available_models",
            ],
        ]),
        (AgentId::Goose, 0) => fields(&[
            &["GOOSE_PROVIDER"],
            &["GOOSE_MODEL"],
            &["active_provider"],
            &["providers", "magpie-lite", "enabled"],
            &["providers", "magpie-lite", "model"],
            &["providers", "magpie-lite", "configured"],
        ]),
        (AgentId::Goose, _) => fields(&[
            &["name"],
            &["engine"],
            &["display_name"],
            &["description"],
            &["api_key_env"],
            &["base_url"],
            &["headers", "Authorization"],
            &["requires_auth"],
            &["skip_canonical_filtering"],
            &["models"],
            &["dynamic_models"],
        ]),
    }
}
fn text(image: &Image) -> Result<String> {
    String::from_utf8(image.bytes()?.unwrap_or_default()).map_err(|_| {
        conflict("agent configuration is not valid UTF-8; managed fields cannot be checked")
    })
}
fn parse(id: AgentId, index: usize, text: &str) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.trim().is_empty() {
        return Ok(Value::Mapping(Mapping::new()));
    }
    let value = if id == AgentId::Codex {
        toml::from_str::<toml::Value>(text)
            .ok()
            .and_then(|v| serde_yaml::to_value(v).ok())
    } else if id == AgentId::Goose && index == 0 {
        serde_yaml::from_str(text).ok()
    } else {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|v| serde_yaml::to_value(v).ok())
    }
    .ok_or_else(|| {
        conflict("agent configuration cannot be parsed; managed fields cannot be checked")
    })?;
    if id == AgentId::Goose && index == 0 && value.is_null() {
        return Ok(Value::Mapping(Mapping::new()));
    }
    if !value.is_mapping() {
        return Err(conflict(
            "agent configuration root changed; managed fields cannot be checked",
        ));
    }
    Ok(value)
}
fn at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    value
        .as_mapping()?
        .get(Value::String(path[0].clone()))
        .and_then(|v| at(v, &path[1..]))
}
fn assign(
    value: &mut Value,
    baseline: &Value,
    path: &[String],
    replacement: Option<&Value>,
) -> Result<()> {
    let key = Value::String(path[0].clone());
    let map = value
        .as_mapping_mut()
        .ok_or_else(|| conflict("agent managed container changed; refusing to overwrite it"))?;
    if path.len() == 1 {
        if let Some(v) = replacement {
            map.insert(key, v.clone());
        } else {
            map.remove(&key);
        }
    } else if let Some(child) = map.get_mut(&key) {
        let base_child = baseline.as_mapping().and_then(|m| m.get(&key));
        assign(
            child,
            base_child.unwrap_or(&Value::Null),
            &path[1..],
            replacement,
        )?;
        if child.as_mapping().is_some_and(Mapping::is_empty) && base_child.is_none() {
            map.remove(&key);
        }
    } else if replacement.is_some() {
        return Err(conflict(
            "agent managed container disappeared; refusing to overwrite it",
        ));
    }
    Ok(())
}
/// Freeze ownership at the first connection. Promote a leaf to its replaced
/// scalar/array ancestor; do not adopt unrelated fields on a later rewire.
pub(super) fn ownership(
    id: AgentId,
    index: usize,
    baseline: &Image,
    applied: &Image,
    legacy: bool,
) -> Result<Vec<Field>> {
    let mut owned = declared(id, index);
    if id == AgentId::Gemini && index == 1 {
        return Ok(owned);
    }
    let base = parse(id, index, &text(baseline)?)?;
    let set = parse(id, index, &text(applied)?)?;
    // Schema 1's Goose custom provider rewrote the complete JSON file. Record
    // fields it erased too, so upgrading cannot silently lose their backup.
    if legacy && id == AgentId::Goose && index == 1 {
        fn deleted(base: &Value, set: &Value, path: &mut Field, owned: &mut Vec<Field>) {
            if let Some(map) = base.as_mapping() {
                for (key, value) in map {
                    let Some(key) = key.as_str() else {
                        continue;
                    };
                    path.push(key.to_owned());
                    match at(set, &[key.to_owned()]) {
                        None => owned.push(path.clone()),
                        Some(next) => deleted(value, next, path, owned),
                    }
                    path.pop();
                }
            }
        }
        deleted(&base, &set, &mut Vec::new(), &mut owned);
    }
    for path in &mut owned {
        for end in 1..path.len() {
            if at(&base, &path[..end]).is_some_and(|v| !v.is_mapping()) {
                path.truncate(end);
                break;
            }
        }
    }
    owned.sort();
    owned.dedup();
    let all = owned.clone();
    owned.retain(|p| !all.iter().any(|q| q.len() < p.len() && p.starts_with(q)));
    Ok(owned)
}
pub(super) fn restore(
    id: AgentId,
    index: usize,
    baseline: &Image,
    applied: &Image,
    current: &Image,
    owned: &[Field],
) -> Result<Image> {
    if current == applied {
        return Ok(baseline.clone());
    }
    if current.data.is_none() {
        return Err(conflict(
            "agent configuration drifted: a managed file was removed",
        ));
    }
    let before = text(current)?;
    let base_text = text(baseline)?;
    let applied_text = text(applied)?;
    let after = if id == AgentId::Gemini && index == 1 {
        super::dotenv::restore(&base_text, &applied_text, &before)?
    } else {
        let base = parse(id, index, &base_text)?;
        let set = parse(id, index, &applied_text)?;
        let mut merged = parse(id, index, &before)?;
        for path in owned {
            if at(&merged, path) != at(&set, path) {
                return Err(conflict(
                    "agent configuration drifted: a managed field changed; conflicting edits are never overwritten",
                ));
            }
        }
        for path in owned {
            if at(&base, path) != at(&set, path) {
                assign(&mut merged, &base, path, at(&base, path))?;
            }
        }
        if id == AgentId::Codex {
            edit_toml(&before, &base_text, &merged)?
        } else if id == AgentId::Goose && index == 0 {
            edit_yaml(&before, &merged)?
        } else {
            if merged == base {
                return Ok(Image {
                    data: baseline.data.clone(),
                    mode: if baseline.data.is_none() {
                        None
                    } else {
                        restore_mode(baseline, applied, current)
                    },
                });
            }
            serde_json::to_string_pretty(&merged)
                .map_err(|_| conflict("cannot encode restored agent configuration"))?
        }
    };
    // Text formats can hold user-added comments even when all values are gone.
    let empty = after.trim().is_empty();
    let mode = restore_mode(baseline, applied, current);
    Ok(Image {
        data: if empty && baseline.data.is_none() {
            None
        } else {
            Some(STANDARD.encode(after))
        },
        mode: if empty && baseline.data.is_none() {
            None
        } else {
            mode
        },
    })
}
fn restore_mode(base: &Image, set: &Image, cur: &Image) -> Option<u32> {
    if cur.mode != set.mode || base.data.is_none() {
        cur.mode
    } else {
        base.mode
    }
}
fn changes(
    current: &Value,
    target: &Value,
    path: &mut Field,
    out: &mut Vec<(Field, Option<Value>)>,
) {
    if current == target {
        return;
    }
    if let (Some(a), Some(b)) = (current.as_mapping(), target.as_mapping()) {
        for key in a.keys().chain(b.keys().filter(|k| !a.contains_key(k))) {
            let Some(k) = key.as_str() else {
                continue;
            };
            let av = a.get(key);
            let bv = b.get(key);
            if av == bv {
                continue;
            }
            path.push(k.to_owned());
            if let (Some(av), Some(bv)) = (av, bv) {
                changes(av, bv, path, out);
            } else {
                out.push((path.clone(), bv.cloned()));
            }
            path.pop();
        }
    } else {
        out.push((path.clone(), Some(target.clone())));
    }
}
pub(super) fn normalized(text: &str) -> (String, bool, bool) {
    let bom = text.starts_with('\u{feff}');
    let crlf = text.contains("\r\n");
    (
        text.trim_start_matches('\u{feff}').replace("\r\n", "\n"),
        bom,
        crlf,
    )
}
pub(super) fn envelope(mut text: String, bom: bool, crlf: bool) -> String {
    if crlf {
        text = text.replace('\n', "\r\n");
    }
    if bom {
        text.insert(0, '\u{feff}');
    }
    text
}
fn edit_toml(current: &str, baseline: &str, merged: &Value) -> Result<String> {
    let (src, bom, crlf) = normalized(current);
    let base: toml_edit::DocumentMut = normalized(baseline)
        .0
        .parse()
        .map_err(|_| conflict("cannot parse original TOML backup"))?;
    let mut doc: toml_edit::DocumentMut = src
        .parse()
        .map_err(|_| conflict("cannot parse current TOML configuration"))?;
    let cur = parse(AgentId::Codex, 0, current)?;
    let mut edits = Vec::new();
    changes(&cur, merged, &mut Vec::new(), &mut edits);
    fn node<'a>(item: &'a toml_edit::Item, path: &[String]) -> Option<&'a toml_edit::Item> {
        if path.is_empty() {
            Some(item)
        } else {
            node(item.get(&path[0])?, &path[1..])
        }
    }
    for (path, replacement) in edits {
        let (key, parents) = path
            .split_last()
            .ok_or_else(|| conflict("cannot replace TOML root"))?;
        let mut table = doc.as_item_mut();
        for parent in parents {
            table = table
                .get_mut(parent)
                .ok_or_else(|| conflict("TOML managed table disappeared"))?;
        }
        let table = table
            .as_table_like_mut()
            .ok_or_else(|| conflict("TOML managed parent is not a table"))?;
        if replacement.is_some() {
            let item = node(base.as_item(), &path)
                .ok_or_else(|| conflict("TOML original field is missing"))?
                .clone();
            table.insert(key, item);
        } else {
            table.remove(key);
        }
    }
    let output = envelope(
        keep_comments(doc.to_string(), toml_comments(&src), toml_comments),
        bom,
        crlf,
    );
    if parse(AgentId::Codex, 0, &output)? != *merged {
        return Err(conflict(
            "TOML selective edit did not preserve the intended configuration",
        ));
    }
    Ok(output)
}
/// Used for Goose wiring and restoration. Never fall back to a lossy serializer;
/// validate the lossless editor's output independently with serde_yaml.
pub(super) fn edit_yaml(current: &str, target: &Value) -> Result<String> {
    let (mut src, bom, crlf) = normalized(current);
    if src.trim().is_empty() {
        src = "{}\n".to_owned();
    }
    let cur = parse(AgentId::Goose, 0, &src)?;
    let mut file = yaml_edit::YamlFile::from_str(&src)
        .map_err(|_| conflict("cannot parse YAML for selective editing"))?;
    if file.document().is_none() {
        if !src.ends_with('\n') {
            src.push('\n');
        }
        src.push_str("{}\n");
        file = yaml_edit::YamlFile::from_str(&src)
            .map_err(|_| conflict("cannot create YAML mapping"))?;
    }
    let mut edits = Vec::new();
    changes(&cur, target, &mut Vec::new(), &mut edits);
    for (path, replacement) in edits {
        let (key, parents) = path
            .split_last()
            .ok_or_else(|| conflict("cannot replace YAML root"))?;
        if parents.is_empty()
            && let Some(value) = replacement
        {
            let node = yaml_node(key, value)?;
            file.set(key.as_str(), node);
            continue;
        }
        let mut map = file
            .document()
            .and_then(|doc| doc.as_mapping())
            .ok_or_else(|| conflict("YAML root is not a mapping"))?;
        for parent in parents {
            map = map
                .get(parent.as_str())
                .and_then(|n| n.as_mapping().cloned())
                .ok_or_else(|| conflict("YAML managed parent is not a mapping"))?;
        }
        if let Some(value) = replacement {
            let node = yaml_node(key, value)?;
            map.set(key.as_str(), node);
        } else {
            map.remove(key.as_str());
        }
    }
    let output = envelope(
        keep_comments(file.to_string(), yaml_comments(&src), yaml_comments),
        bom,
        crlf,
    );
    if parse(AgentId::Goose, 0, &output)? != *target {
        return Err(conflict(
            "YAML selective edit did not preserve the intended configuration",
        ));
    }
    Ok(output)
}

// Deleting a generated empty container may detach comments attached to it.
// Retain such comments at the end, instead of discarding user-written text.
fn keep_comments(
    mut output: String,
    original: Vec<String>,
    extract: fn(&str) -> Vec<String>,
) -> String {
    let mut remaining = std::collections::HashMap::new();
    for comment in extract(&output) {
        *remaining.entry(comment).or_insert(0usize) += 1;
    }
    for comment in original {
        let count = remaining.entry(comment.clone()).or_insert(0);
        if *count > 0 {
            *count -= 1;
        } else {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&comment);
            output.push('\n');
        }
    }
    output
}
fn yaml_comments(text: &str) -> Vec<String> {
    yaml_edit::lex(text)
        .into_iter()
        .filter(|(kind, _)| *kind == yaml_edit::SyntaxKind::COMMENT)
        .map(|(_, text)| text.trim_end_matches('\r').to_owned())
        .collect()
}
fn toml_comments(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut quoted: Option<(u8, bool)> = None;
    while i < bytes.len() {
        if let Some((quote, triple)) = quoted {
            if quote == b'"' && bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == quote {
                if !triple {
                    quoted = None;
                } else if bytes[i..].starts_with(&[quote; 3]) {
                    while i < bytes.len() && bytes[i] == quote {
                        i += 1;
                    }
                    quoted = None;
                    continue;
                }
            }
        } else if bytes[i] == b'#' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(text[start..i].trim_end_matches('\r').to_owned());
            continue;
        } else if matches!(bytes[i], b'"' | b'\'') {
            let triple = bytes[i..].starts_with(&[bytes[i]; 3]);
            quoted = Some((bytes[i], triple));
            if triple {
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn yaml_node(key: &str, value: Value) -> Result<yaml_edit::YamlNode> {
    let fragment = if value.is_mapping() || value.is_sequence() {
        format!(
            "{}: {}\n",
            serde_json::to_string(key).map_err(|_| conflict("cannot encode YAML field name"))?,
            serde_json::to_string(&value)
                .map_err(|_| conflict("cannot encode YAML managed subtree"))?
        )
    } else {
        let mut one = Mapping::new();
        one.insert(Value::String(key.to_owned()), value);
        serde_yaml::to_string(&one).map_err(|_| conflict("cannot encode YAML managed field"))?
    };
    let doc = yaml_edit::Document::from_str(&fragment)
        .map_err(|_| conflict("cannot parse YAML managed field"))?;
    doc.as_mapping()
        .and_then(|m| m.get(key))
        .ok_or_else(|| conflict("YAML managed field is missing"))
}
