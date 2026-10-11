//! Dotenv records, including exported/duplicate keys, BOM and quoted multiline values.
//! Unmanaged records are copied byte-for-byte; no general-purpose env expansion.
use super::lifecycle::{Result, conflict};
const KEYS: [&str; 2] = ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"];
struct Record<'a> {
    raw: &'a str,
    key: Option<&'a str>,
    equals: usize,
}
fn key(line: &str) -> Option<(&str, usize)> {
    let equals = line.find('=')?;
    let mut key = line[..equals].trim();
    if let Some(rest) = key.strip_prefix("export")
        && rest.starts_with(char::is_whitespace)
    {
        key = rest.trim();
    }
    if key.is_empty()
        || !key
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    {
        return None;
    }
    Some((key, equals))
}
fn quoted_closed(value: &str) -> bool {
    let value = value.trim_start();
    let Some(quote @ ('\'' | '"')) = value.chars().next() else {
        return true;
    };
    let mut escape = false;
    for ch in value[quote.len_utf8()..].chars() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && quote == '"' {
            escape = true;
            continue;
        }
        if ch == quote {
            return true;
        }
    }
    false
}
fn records(text: &str) -> Result<Vec<Record<'_>>> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = text[start..]
            .find('\n')
            .map_or(text.len(), |i| start + i + 1);
        let found = key(&text[start..end]);
        if let Some((_, equals)) = found {
            while !quoted_closed(&text[start + equals + 1..end]) {
                if end == text.len() {
                    return Err(conflict("agent dotenv has an unterminated quoted value"));
                }
                end = text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
            }
        }
        out.push(Record {
            raw: &text[start..end],
            key: found.map(|(k, _)| k),
            equals: found.map_or(0, |(_, e)| e),
        });
        start = end;
    }
    Ok(out)
}
fn body(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}
fn quote(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '#' | '$' | '"' | '\'' | '\\'))
    {
        format!("{value:?}")
    } else {
        value.to_owned()
    }
}
pub(super) fn wire(text: &str, values: &[(&str, String)]) -> Result<String> {
    let mut out = if text.starts_with('\u{feff}') {
        "\u{feff}".to_owned()
    } else {
        String::new()
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut seen = std::collections::HashSet::new();
    for record in records(body(text))? {
        if let Some((k, value)) = values.iter().find(|(k, _)| record.key == Some(k)) {
            seen.insert(*k);
            out.push_str(&record.raw[..record.equals + 1]);
            out.push_str(&quote(value));
            // Keep the record's original line-ending convention.
            if record.raw.ends_with("\r\n") {
                out.push_str("\r\n");
            } else if record.raw.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(record.raw);
        }
    }
    for (k, value) in values {
        if !seen.contains(k) {
            if !out.is_empty() && !out.ends_with('\n') && out != "\u{feff}" {
                out.push_str(newline);
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&quote(value));
            out.push_str(newline);
        }
    }
    Ok(out)
}
pub(super) fn restore(baseline: &str, applied: &str, current: &str) -> Result<String> {
    let base = records(body(baseline))?;
    let set = records(body(applied))?;
    let cur = records(body(current))?;
    for key in KEYS {
        let values = |rs: &[Record<'_>]| {
            rs.iter()
                .filter(|r| r.key == Some(key))
                .map(|r| r.raw[r.equals + 1..].trim_end().to_owned())
                .collect::<Vec<_>>()
        };
        if values(&set) != values(&cur) {
            return Err(conflict(
                "agent configuration drifted: a managed dotenv key changed; conflicting edits are never overwritten",
            ));
        }
    }
    let mut out = if current.starts_with('\u{feff}') {
        "\u{feff}".to_owned()
    } else {
        String::new()
    };
    let mut counts = std::collections::HashMap::new();
    for record in cur {
        if let Some(key) = record.key.filter(|k| KEYS.contains(k)) {
            let count = counts.entry(key).or_insert(0);
            if let Some(original) = base.iter().filter(|r| r.key == Some(key)).nth(*count) {
                if !out.is_empty() && !out.ends_with('\n') && out != "\u{feff}" {
                    out.push('\n');
                }
                out.push_str(original.raw);
            }
            *count += 1;
        } else {
            if !out.is_empty() && !out.ends_with('\n') && out != "\u{feff}" {
                out.push('\n');
            }
            out.push_str(record.raw);
        }
    }
    Ok(out)
}
