//! Stop a conversation stuck repeating the same tool exchange across turns.
//!
//! Every turn of such a loop carries the whole conversation, so the loop is
//! visible in the request itself: its newest turns are the same calls with
//! the same arguments answered by the same results. The guard answers the
//! next turn locally and sends nothing upstream.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use magpie_core::ir::{Message, Part, PartKind, Role};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How many consecutive identical tool exchanges stop the next turn.
pub(crate) const REPEATS: usize = 8;

/// A detected tool loop and the newest exchange's calls.
pub(crate) struct ToolLoop {
    calls: Vec<Part>,
    turns: usize,
}

/// Detect the tool loop a conversation ends in.
pub(crate) fn detect(messages: &[Message]) -> Option<ToolLoop> {
    let exchanges = exchanges(messages);
    if exchanges.len() < REPEATS {
        return None;
    }
    let last = exchanges.last()?;
    let turns = exchanges
        .iter()
        .rev()
        .take_while(|exchange| exchange.sum == last.sum)
        .count();
    (turns >= REPEATS).then(|| ToolLoop {
        calls: last.calls.clone(),
        turns,
    })
}

impl ToolLoop {
    /// Why the request was rejected, for usage and diagnostics.
    pub(crate) fn why(&self) -> String {
        format!(
            "tool loop: the same tool call got the same result {} turns in a row ({}); answered by magpie, sent nowhere",
            self.turns,
            self.called()
        )
    }

    /// The local assistant reply shown to the agent.
    pub(crate) fn notice(&self) -> String {
        format!(
            "magpie stopped this turn before it reached the model: the agent ran the same tool call {} times in a row and got the same result every time ({}). The model is stuck repeating it, and each further turn would spend your quota with no progress, so this turn was sent nowhere. Tell the agent how to go on and the conversation continues; stream_loop_guard = false turns this check off.",
            self.turns,
            self.called()
        )
    }

    fn called(&self) -> String {
        self.calls
            .iter()
            .map(|call| {
                let mut args = canonical_args(&call.args);
                if let Some((index, _)) = args.char_indices().nth(120) {
                    args.truncate(index);
                    args.push('…');
                }
                format!("{} {}", call.name, args)
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug)]
struct Exchange {
    sum: [u8; 32],
    calls: Vec<Part>,
}

/// Group consecutive same-role messages into runs, then scan backward over
/// assistant/user pairs for complete tool exchanges.
fn exchanges(messages: &[Message]) -> Vec<Exchange> {
    let mut runs: Vec<(Role, Vec<Part>)> = Vec::new();
    for message in messages {
        if let Some((role, parts)) = runs.last_mut()
            && *role == message.role
        {
            parts.extend(message.parts.iter().cloned());
            continue;
        }
        runs.push((message.role, message.parts.clone()));
    }

    let mut out = Vec::new();
    let mut index = runs.len();
    while index >= 2 {
        index -= 2;
        let (asked_role, asked) = &runs[index];
        let (said_role, said) = &runs[index + 1];
        if *said_role != Role::User || *asked_role != Role::Assistant {
            break;
        }
        let Some(exchange) = exchange_of(asked, said) else {
            break;
        };
        out.push(exchange);
    }
    out.reverse();
    out
}

/// Fingerprint one tool-use turn without call ids. Results are paired by
/// call id; orphan results still participate. User-side text and other
/// non-result parts participate separately and in order.
fn exchange_of(asked: &[Part], said: &[Part]) -> Option<Exchange> {
    let calls: Vec<Part> = asked
        .iter()
        .filter(|part| part.kind() == PartKind::ToolCall)
        .cloned()
        .collect();
    let mut results: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut rest = Vec::new();
    let mut has_result = false;
    for part in said {
        match part.kind() {
            PartKind::ToolResult => {
                has_result = true;
                results
                    .entry(&part.call_id)
                    .or_default()
                    .push(result_summary(part));
            }
            PartKind::Text => rest.push(format!("text\x00{}", part.text)),
            kind => rest.push(format!(
                "{}\x00{}{}{}",
                kind_name(kind),
                part.text,
                part.data,
                part.url
            )),
        }
    }
    if calls.is_empty() || !has_result {
        return None;
    }

    let mut keys = Vec::with_capacity(calls.len() + results.len());
    for call in &calls {
        let got = results.remove(call.id.as_str()).unwrap_or_default();
        keys.push(format!(
            "{}\x00{}\x00{}",
            call.name,
            canonical_args(&call.args),
            got.join("\x01")
        ));
    }
    for got in results.into_values() {
        keys.push(format!("\x00\x00{}", got.join("\x01")));
    }
    keys.sort();

    let mut hasher = Sha256::new();
    for key in keys {
        hasher.update(key.as_bytes());
        hasher.update([0xff]);
    }
    for item in rest {
        hasher.update(item.as_bytes());
        hasher.update([0xfe]);
    }
    Some(Exchange {
        sum: hasher.finalize().into(),
        calls,
    })
}

/// Canonical JSON with recursively sorted object keys. `serde_json`'s
/// `preserve_order` feature means serializing a decoded value as-is is not
/// enough: two equivalent objects may retain different insertion orders.
fn canonical_args(value: &Value) -> String {
    serde_json::to_string(&canonical_value(value)).unwrap_or_else(|_| value.to_string())
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_value(&object[key]));
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

fn result_summary(part: &Part) -> String {
    let mut out = String::new();
    if part.is_error {
        out.push_str("error\x00");
    }
    out.push_str(&steady_result(&part.text));
    for image in &part.images {
        out.push('\x00');
        out.push_str(&image.media_type);
        out.push_str(&image.data);
        out.push_str(&image.url);
    }
    out
}

fn steady_result(text: &str) -> String {
    let mut text = text.to_owned();
    for noise in run_noise() {
        if text.contains(noise.mark) {
            text = noise
                .regex
                .replace_all(&text, noise.replacement)
                .into_owned();
        }
    }
    text
}

struct RunNoise {
    mark: &'static str,
    regex: Regex,
    replacement: &'static str,
}

fn run_noise() -> &'static [RunNoise] {
    static NOISE: OnceLock<Vec<RunNoise>> = OnceLock::new();
    NOISE.get_or_init(|| {
        vec![
            RunNoise {
                mark: "Wall time",
                regex: Regex::new(r"(?m)^(Wall time:? )[0-9.]+( seconds?)").unwrap(),
                replacement: "${1}#${2}",
            },
            RunNoise {
                mark: "Chunk ID: ",
                regex: Regex::new(r"(?m)^(Chunk ID: )\S+").unwrap(),
                replacement: "${1}#",
            },
            RunNoise {
                mark: r#""duration_seconds""#,
                regex: Regex::new(r#"("duration_seconds"\s*:\s*)[0-9.eE+-]+"#).unwrap(),
                replacement: "${1}#",
            },
        ]
    })
}

fn kind_name(kind: PartKind) -> &'static str {
    match kind {
        PartKind::Text => "text",
        PartKind::Image => "image",
        PartKind::ToolCall => "tool_call",
        PartKind::ToolResult => "tool_result",
        PartKind::Search => "search",
        PartKind::Thinking => "thinking",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, args: Value) -> Part {
        Part::tool_call(id, name, args)
    }

    fn result(id: &str, text: &str) -> Part {
        Part::tool_result(id, text, false)
    }

    fn exchange(call: Part, result: Part) -> Exchange {
        exchange_of(&[call], &[result]).expect("a complete exchange")
    }

    #[test]
    fn equivalent_arguments_and_run_noise_match_but_real_changes_do_not() {
        let base = exchange(
            call("a", "read", json!({"path":"x","limit":2})),
            result(
                "a",
                "Wall time: 0.0246 seconds\nChunk ID: abc123\nOutput:\n{\"duration_seconds\": 1.23}",
            ),
        );
        let same = exchange(
            call("b", "read", json!({"limit":2,"path":"x"})),
            result(
                "b",
                "Wall time: 1.5 seconds\nChunk ID: fedcba\nOutput:\n{\"duration_seconds\": 4e2}",
            ),
        );
        assert_eq!(base.sum, same.sum);
        assert_eq!(
            steady_result(
                "Wall time: 0.0246 seconds\nChunk ID: abc123\nOutput:\n{\"duration_seconds\": 1.23}"
            ),
            steady_result(
                "Wall time: 1.5 seconds\nChunk ID: fedcba\nOutput:\n{\"duration_seconds\": 4e2}"
            )
        );
        assert_eq!(
            steady_result("Wall time 0.1 seconds\nOutput:\nfn x()"),
            "Wall time # seconds\nOutput:\nfn x()"
        );

        let changes = [
            exchange(
                call("c", "read", json!({"path":"y","limit":2})),
                result("c", "x"),
            ),
            exchange(
                call("d", "grep", json!({"path":"x","limit":2})),
                result("d", "x"),
            ),
            exchange(
                call("e", "read", json!({"path":"x","limit":2})),
                result("e", "y"),
            ),
            exchange(
                call("f", "read", json!({"path":"x","limit":2})),
                Part::tool_result("f", "x", true),
            ),
        ];
        for changed in changes {
            assert_ne!(base.sum, changed.sum);
        }
    }

    #[test]
    fn nested_object_keys_are_canonical_recursively() {
        let left = canonical_args(&json!({
            "outer": {"b": 2, "a": [{"z": 1, "y": 2}]},
            "first": true
        }));
        let right = canonical_args(&json!({
            "first": true,
            "outer": {"a": [{"y": 2, "z": 1}], "b": 2}
        }));
        assert_eq!(left, right);
    }

    #[test]
    fn result_images_tell_exchanges_apart() {
        let base = exchange(
            call("a", "screenshot", json!({"page": 1})),
            Part::tool_result("a", "captured", false),
        );
        let variants = [
            Part::image("image/jpeg", "same-bytes"),
            Part::image("image/png", "different-bytes"),
            Part::image_url("https://example.test/a.png"),
            Part::image_url("https://example.test/b.png"),
        ];
        for image in variants {
            let mut changed_result = result("a", "captured");
            changed_result.images.push(image);
            let changed = exchange(call("a", "screenshot", json!({"page": 1})), changed_result);
            assert_ne!(base.sum, changed.sum);
        }
    }

    #[test]
    fn only_complete_assistant_user_tool_pairs_count() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![Part::text("done")],
            },
            Message {
                role: Role::User,
                parts: vec![result("a", "x")],
            },
        ];
        assert!(exchanges(&messages).is_empty());

        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![call("a", "read", json!({}))],
            },
            Message {
                role: Role::User,
                parts: vec![Part::text("stop")],
            },
        ];
        assert!(exchanges(&messages).is_empty());
    }

    #[test]
    fn displayed_arguments_are_cut_at_120_unicode_scalars() {
        let args = format!("{{\"text\":\"{}\"}}", "你".repeat(200));
        let loop_ = ToolLoop {
            calls: vec![call("a", "read", Value::String(args))],
            turns: REPEATS,
        };
        let called = loop_.called();
        assert!(called.ends_with('…'));
        let shown = called.strip_prefix("read ").unwrap().trim_end_matches('…');
        assert_eq!(shown.chars().count(), 120);
    }
}
