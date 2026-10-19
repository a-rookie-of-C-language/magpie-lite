//! Grok writes a whole number in a tool call's arguments with a fraction of
//! zeros (`"yield_time_ms":2500.0`, `"session_id":17277.0`), whichever
//! service answers for it: xAI's API, OpenRouter's `x-ai/grok-*`, or a relay
//! in front of any of them. Codex reads such fields as u64, usize, i32 or
//! i64 (exec_command, write_stdin, shell's timeout_ms, sleep, tool_search's
//! limit, wait), though their schemas say `number`, and serde turns the
//! whole call away: "failed to parse function arguments: invalid type:
//! floating point `2500.0`, expected u64" (#1431). Any client that types its
//! arguments strictly does the same.
//!
//! In a reply from a Grok model, a number written with a fraction of zeros
//! and no exponent loses that fraction: 2500.0 is written 2500. JSON has one
//! kind of number, so 2500 is the same value to every client, and one that
//! took 2500.0 takes 2500; a tool's schema isn't asked, since Codex's own say
//! `number` for the integers it reads. Strings, keys, other fractions (2.5)
//! and exponents (1.0e3) stay as written, and so does every other model's
//! reply.
//!
//! The rule is applied to the arguments as a stream of text, so it gives the
//! same arguments whether they come whole or in deltas cut anywhere: what the
//! deltas of a call add up to is what its done events say.

use std::collections::HashMap;

use magpie_core::ir::{CompletionResponse, PartKind};
use magpie_core::stream::StreamEvent;
use serde_json::Value;

use crate::grok_schema::grok_model;

/// Whether a request is Grok's, by the id the client asked for or the name it
/// is sent upstream under (`xai/grok-4.5`, a relay's `grok-4.6-fast`).
pub(crate) fn grok_served(client_model: &str, upstream_model: &str) -> bool {
    grok_model(client_model) || grok_model(upstream_model)
}

/// Drops the zero fraction of each number in a stream of JSON text, fed to
/// it a piece at a time. It holds only a fraction of zeros (".0", ".00")
/// until it knows whether the number ends there, in which case the fraction
/// is dropped, or goes on (2500.05, 2500.0e3), in which case it is written as
/// it came; so a piece never waits on the next one for anything but such a
/// fraction, and nothing is held at the end.
#[derive(Debug, Default, Clone)]
pub(crate) struct WholeNumbers {
    /// In a string.
    in_string: bool,
    /// Just after the string's backslash.
    escaped: bool,
    /// In a number.
    in_number: bool,
    /// The rest of this number goes as written.
    as_is: bool,
    /// A fraction of zeros held: its dot, then its zeros.
    zeros: usize,
}

impl WholeNumbers {
    /// Rewrite one piece of JSON text, together with whatever an earlier
    /// piece left held.
    pub(crate) fn feed(&mut self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if self.in_number {
                if self.zeros > 0 && c == '0' {
                    self.zeros += 1;
                    continue;
                }
                if self.zeros > 0 && (c.is_ascii_digit() || c == 'e' || c == 'E') {
                    // Not a fraction of zeros: what was held goes as it came.
                    out.push('.');
                    for _ in 1..self.zeros {
                        out.push('0');
                    }
                    self.zeros = 0;
                    self.as_is = true;
                    out.push(c);
                    continue;
                }
                if self.zeros == 0 && !self.as_is && c == '.' {
                    self.zeros = 1;
                    continue;
                }
                if self.zeros == 0
                    && (c.is_ascii_digit()
                        || c == '.'
                        || c == 'e'
                        || c == 'E'
                        || c == '+'
                        || c == '-')
                {
                    if c == 'e' || c == 'E' || c == '.' {
                        self.as_is = true;
                    }
                    out.push(c);
                    continue;
                }
                // The number ended: a fraction of zeros held is dropped.
                self.in_number = false;
                self.as_is = false;
                self.zeros = 0;
            }
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if c == '\\' {
                    self.escaped = true;
                } else if c == '"' {
                    self.in_string = false;
                }
            } else if c == '"' {
                self.in_string = true;
            } else if c == '-' || c.is_ascii_digit() {
                self.in_number = true;
            }
            out.push(c);
        }
        out
    }
}

/// A call's whole arguments with the zero fraction of each of their numbers
/// dropped.
pub(crate) fn whole_args(s: &str) -> String {
    if !s.contains('.') {
        return s.to_owned();
    }
    let mut n = WholeNumbers::default();
    n.feed(s)
}

/// The arguments of one call, read back after the rule. Arguments that are
/// not a JSON object or array are left as they came.
fn whole_args_value(args: &Value) -> Value {
    if !matches!(args, Value::Object(_) | Value::Array(_)) {
        return args.clone();
    }
    // The arguments are re-serialized here, so a number keeps the spelling
    // the IR gave it; the rule is the same one the stream applies.
    let text = args.to_string();
    let fixed = whole_args(&text);
    if fixed == text {
        return args.clone();
    }
    serde_json::from_str(&fixed).unwrap_or_else(|_| args.clone())
}

/// A whole reply from a Grok model with its calls' arguments' zero fractions
/// dropped.
pub(crate) fn whole_reply(reply: &mut CompletionResponse) {
    for part in &mut reply.message.parts {
        if part.kind() != PartKind::ToolCall || part.args.is_null() {
            continue;
        }
        part.args = whole_args_value(&part.args);
    }
}

/// The rule applied to a Grok model's reply as it streams: each call's
/// arguments are rewritten where they are cut into deltas, so what the deltas
/// add up to is what the call says when it is done.
#[derive(Debug, Default)]
pub(crate) struct GrokIntegral {
    /// Whether this reply is a Grok model's at all.
    applies: bool,
    /// A streamed call's arguments so far, by its index.
    calls: HashMap<usize, WholeNumbers>,
}

impl GrokIntegral {
    /// State for one reply, given whether its model is Grok's.
    pub(crate) fn new(applies: bool) -> Self {
        Self {
            applies,
            calls: HashMap::new(),
        }
    }

    /// Forget the arguments streamed so far. A reasked reply starts its calls
    /// over, so a held fraction from the abandoned one must not leak into it.
    pub(crate) fn reset(&mut self) {
        self.calls.clear();
    }

    /// Rewrite one event in place and say whether it still carries something
    /// to send. Only a delta that was nothing but a held fraction is dropped;
    /// every other event goes on unchanged.
    pub(crate) fn event(&mut self, event: &mut StreamEvent) -> bool {
        if !self.applies {
            return true;
        }
        match event {
            StreamEvent::ToolCallStart { index, .. } => {
                self.calls.insert(*index, WholeNumbers::default());
            }
            StreamEvent::ToolCallDelta { index, args } => {
                let numbers = self.calls.entry(*index).or_default();
                *args = numbers.feed(args);
                return !args.is_empty();
            }
            StreamEvent::ToolCallEnd { index } => {
                self.calls.remove(index);
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The arguments of a tool-call delta, for the tests.
    fn args_of(event: &StreamEvent) -> &str {
        match event {
            StreamEvent::ToolCallDelta { args, .. } => args,
            other => panic!("not a tool-call delta: {other:?}"),
        }
    }

    /// Every way a text can be cut into two pieces, and into single bytes.
    fn cuts(s: &str) -> Vec<Vec<String>> {
        let mut out = vec![vec![s.to_owned()]];
        out.push(s.chars().map(|c| c.to_string()).collect());
        for (i, _) in s.char_indices().skip(1) {
            out.push(vec![s[..i].to_owned(), s[i..].to_owned()]);
        }
        out
    }

    #[test]
    fn a_zero_fraction_is_dropped_however_the_text_is_cut() {
        for (input, want) in [
            (
                r#"{"yield_time_ms":2500.0,"max_output_tokens":4000.0}"#,
                r#"{"yield_time_ms":2500,"max_output_tokens":4000}"#,
            ),
            (
                r#"{"a":-3.00,"b":[1.0,2.0],"c":0.0,"d":-0.0}"#,
                r#"{"a":-3,"b":[1,2],"c":0,"d":-0}"#,
            ),
            (
                r#"{"a":2.5,"b":2.50,"c":2.05,"d":2.500e1}"#,
                r#"{"a":2.5,"b":2.50,"c":2.05,"d":2.500e1}"#,
            ),
            (
                r#"{"a":1.0e3,"b":1.0E-3,"c":2.5e3,"d":1e3,"e":10}"#,
                r#"{"a":1.0e3,"b":1.0E-3,"c":2.5e3,"d":1e3,"e":10}"#,
            ),
            (
                r#"{"1.0":"2.0","s":"a\"1.0\\","t":"\\\\"," n":3.0 }"#,
                r#"{"1.0":"2.0","s":"a\"1.0\\","t":"\\\\"," n":3 }"#,
            ),
            (
                r#"{"ok":true,"no":null,"x":false}"#,
                r#"{"ok":true,"no":null,"x":false}"#,
            ),
            ("[100.000, 7.0]", "[100, 7]"),
        ] {
            assert_eq!(whole_args(input), want, "{input}");
            for pieces in cuts(input) {
                let mut numbers = WholeNumbers::default();
                let mut got = String::new();
                for piece in &pieces {
                    got.push_str(&numbers.feed(piece));
                }
                assert_eq!(got, want, "{input} cut {pieces:?}");
            }
        }
    }

    #[test]
    fn a_piece_that_is_only_a_held_fraction_carries_nothing() {
        let mut integral = GrokIntegral::new(true);
        let mut start = StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "exec_command".into(),
            namespace: None,
        };
        assert!(integral.event(&mut start));
        let mut whole = StreamEvent::ToolCallDelta {
            index: 0,
            args: r#"{"yield_time_ms":2500.0"#.into(),
        };
        assert!(integral.event(&mut whole));
        assert_eq!(args_of(&whole), r#"{"yield_time_ms":2500"#);
        let mut fraction = StreamEvent::ToolCallDelta {
            index: 0,
            args: "0".into(),
        };
        assert!(!integral.event(&mut fraction), "a held zero is not sent");
        let mut close = StreamEvent::ToolCallDelta {
            index: 0,
            args: "}".into(),
        };
        assert!(integral.event(&mut close));
        assert_eq!(args_of(&close), "}");
        // A call that goes on is written as it came.
        let mut goes_on = StreamEvent::ToolCallDelta {
            index: 1,
            args: r#"{"a":2.5"#.into(),
        };
        assert!(integral.event(&mut goes_on));
        assert_eq!(args_of(&goes_on), r#"{"a":2.5"#);
    }

    #[test]
    fn another_models_reply_is_untouched() {
        let mut integral = GrokIntegral::new(false);
        let mut delta = StreamEvent::ToolCallDelta {
            index: 0,
            args: r#"{"yield_time_ms":2500.0}"#.into(),
        };
        assert!(integral.event(&mut delta));
        assert_eq!(args_of(&delta), r#"{"yield_time_ms":2500.0}"#);
    }

    #[test]
    fn a_whole_reply_gets_its_calls_integers() {
        let mut reply = CompletionResponse {
            model: "grok-4.6".into(),
            message: magpie_core::ir::Message {
                role: magpie_core::ir::Role::Assistant,
                parts: vec![
                    magpie_core::ir::Part::text("running it"),
                    magpie_core::ir::Part::tool_call(
                        "call_1",
                        "exec_command",
                        json!({"cmd": "ls", "yield_time_ms": 2500.0, "n": -3.0}),
                    ),
                ],
            },
            stop: Default::default(),
            usage: Default::default(),
        };
        whole_reply(&mut reply);
        assert_eq!(
            reply.message.parts[1].args,
            json!({"cmd": "ls", "yield_time_ms": 2500, "n": -3})
        );
    }

    #[test]
    fn grok_is_named_by_the_client_or_the_upstream() {
        assert!(grok_served("relay/grok-4.6", "grok-4.6"));
        assert!(grok_served("relay/fast", "x-ai/grok-4"));
        assert!(grok_served("grok-4.6", "grok-4.6"));
        assert!(!grok_served("relay/gpt-5.5", "gpt-5.5"));
    }
}
