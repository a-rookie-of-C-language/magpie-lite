//! Bounded text/reasoning repetition detection, after wire decoding.
//!
//! Matches the reference's 256-unit/24-distinct window and two 4096-byte
//! run-on units. Tools and metadata are deliberately not prose. No content is
//! copied into errors or request history.

use std::collections::{HashMap, VecDeque};

use magpie_core::stream::StreamEvent;

const WINDOW: usize = 256;
const DISTINCT: usize = 24;
const UNIT_MOST: usize = 4096;
const KEY_MOST: usize = 256;
const RUN_ON_UNITS: usize = 2;
const RUN_ON_DISTINCT: usize = 8;

#[derive(Default)]
pub(crate) struct LoopGuard {
    reasoning: UnitRun,
    text: UnitRun,
}

pub(crate) struct LoopTrip {
    reasoning: bool,
    run_on: bool,
}

impl LoopTrip {
    pub(crate) fn message(&self) -> &'static str {
        // Unlike the reference, do not quote repeated reply text: the same
        // error may travel to an agent log or a proxy's diagnostics.
        match (self.reasoning, self.run_on) {
            (false, false) => {
                "magpie-lite ended this reply: the model's text was stuck in a loop (256 lines/sentences among at most 24 distinct units); ask again or set stream_loop_guard = false"
            }
            (true, false) => {
                "magpie-lite ended this reply: the model's reasoning was stuck in a loop (256 lines/sentences among at most 24 distinct units); ask again or set stream_loop_guard = false"
            }
            (false, true) => {
                "magpie-lite ended this reply: the model's text was stuck in a loop (repeated words without a break across two 4096-byte units); ask again or set stream_loop_guard = false"
            }
            (true, true) => {
                "magpie-lite ended this reply: the model's reasoning was stuck in a loop (repeated words without a break across two 4096-byte units); ask again or set stream_loop_guard = false"
            }
        }
    }
}

impl LoopGuard {
    pub(crate) fn observe(&mut self, event: &StreamEvent) -> Option<LoopTrip> {
        let (run, text, reasoning) = match event {
            StreamEvent::TextDelta { text } => (&mut self.text, text, false),
            StreamEvent::ThinkingDelta { text } => (&mut self.reasoning, text, true),
            _ => return None,
        };
        run.add(text).map(|run_on| LoopTrip { reasoning, run_on })
    }
}

#[derive(Default)]
struct UnitRun {
    current: String,
    stop: bool,
    lettered: bool,
    // Full bounded keys avoid hash-collision-induced false loop detections.
    ring: VecDeque<String>,
    counts: HashMap<String, usize>,
    run_on: usize,
}

impl UnitRun {
    fn add(&mut self, text: &str) -> Option<bool> {
        for c in text.chars() {
            if c == '\n' || self.stop && c.is_whitespace() {
                if self.end() {
                    return Some(false);
                }
                continue;
            }
            self.current.push(c);
            match c {
                '.' | '!' | '?' => self.stop = self.lettered,
                '。' | '！' | '？' => {
                    if self.end() {
                        return Some(false);
                    }
                    continue;
                }
                _ => {
                    self.stop = false;
                    self.lettered |= c.is_alphabetic();
                }
            }
            if self.current.len() >= UNIT_MOST {
                if run_on_words(&self.current) {
                    self.run_on += 1;
                    if self.run_on >= RUN_ON_UNITS {
                        return Some(true);
                    }
                } else {
                    self.run_on = 0;
                }
                if self.end() {
                    return Some(false);
                }
            }
        }
        None
    }

    fn end(&mut self) -> bool {
        if self.current.len() < UNIT_MOST
            && (self.current.len() < UNIT_MOST / 8 || !run_on_words(&self.current))
        {
            self.run_on = 0;
        }
        let key = unit_key(&self.current);
        self.current.clear();
        self.stop = false;
        self.lettered = false;
        let Some(key) = key else { return false };
        if self.ring.len() == WINDOW {
            let old = self.ring.pop_front().expect("full window");
            let count = self.counts.get_mut(&old).expect("ring key counted");
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&old);
            }
        }
        *self.counts.entry(key.clone()).or_default() += 1;
        self.ring.push_back(key);
        self.ring.len() == WINDOW && self.counts.len() <= DISTINCT
    }
}

fn word_char(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric() || c == '_'
}

/// A run with a letter but no number, not a hex value, version or hash.
fn has_word(text: &str) -> bool {
    text.split(|c| !word_char(c))
        .any(|word| word.chars().any(char::is_alphabetic) && !word.chars().any(char::is_numeric))
}

fn unit_key(raw: &str) -> Option<String> {
    let text = raw
        .trim()
        .trim_end_matches(|c: char| c.is_whitespace() || ".!?。！？,，;；:：…".contains(c));
    if !has_word(text) {
        return None;
    }
    let mut key = text.to_lowercase();
    let mut end = key.len().min(KEY_MOST);
    while !key.is_char_boundary(end) {
        end -= 1;
    }
    key.truncate(end);
    Some(key)
}

fn run_on_words(raw: &str) -> bool {
    if !has_word(raw) {
        return false;
    }
    let bytes = raw.as_bytes();
    for period in 1..=64.min(bytes.len() / 4) {
        if (period..bytes.len()).all(|i| bytes[i] == bytes[i - period]) {
            return true;
        }
    }
    let text = raw.to_lowercase();
    let mut counts = HashMap::new();
    let mut words = 0;
    for word in text
        .split(|c| !word_char(c))
        .filter(|word| !word.is_empty())
    {
        words += 1;
        counts.entry(word).or_insert(());
        if counts.len() > RUN_ON_DISTINCT {
            return false;
        }
    }
    words * 32 >= raw.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(guard: &mut LoopGuard, text: &str, reasoning: bool, width: usize) -> Option<LoopTrip> {
        let mut chunk = String::new();
        for c in text.chars() {
            chunk.push(c);
            if chunk.len() >= width {
                let event = if reasoning {
                    StreamEvent::ThinkingDelta {
                        text: std::mem::take(&mut chunk),
                    }
                } else {
                    StreamEvent::TextDelta {
                        text: std::mem::take(&mut chunk),
                    }
                };
                if let Some(trip) = guard.observe(&event) {
                    return Some(trip);
                }
            }
        }
        guard.observe(&if reasoning {
            StreamEvent::ThinkingDelta { text: chunk }
        } else {
            StreamEvent::TextDelta { text: chunk }
        })
    }

    #[test]
    fn detects_line_sentence_and_unicode_loops_independent_of_chunk_boundaries() {
        for width in [1, 3, 7, 64, 100_000] {
            for reasoning in [false, true] {
                for cycle in [
                    "Now.\nWriting.\nGo.\nLet me write.\nOK.\nHere.\nProduce.\n",
                    "Now. Writing. Go. Let me write. OK. Here. Produce. ",
                    "好。现在写。开始。让我写。好的。这里。输出。",
                ] {
                    let mut guard = LoopGuard::default();
                    let trip =
                        feed(&mut guard, &cycle.repeat(100), reasoning, width).expect("loop");
                    assert_eq!(trip.reasoning, reasoning);
                    assert!(!trip.run_on);
                }
            }
        }
    }

    #[test]
    fn exact_window_and_distinct_threshold_and_sliding_eviction() {
        let mut run = UnitRun::default();
        for i in 0..WINDOW - 1 {
            assert!(run.add(&format!("Unit {}\n", i % DISTINCT)).is_none());
        }
        assert_eq!(run.add("Unit 0\n"), Some(false));
        let mut run = UnitRun::default();
        for i in 0..WINDOW * 2 {
            assert!(run.add(&format!("Unit {}\n", i % (DISTINCT + 1))).is_none());
        }
        assert_eq!(run.ring.len(), WINDOW);
        assert_eq!(run.counts.len(), DISTINCT + 1);
        for i in 0..WINDOW {
            if run.add("Different ending\n").is_some() {
                assert!(i > 0);
                return;
            }
        }
        panic!("old diverse units must leave the window");
    }

    #[test]
    fn detects_spaced_unspaced_and_unicode_run_on_loops() {
        for cycle in [
            "mcp ",
            "mcp, MCP ",
            "mcpmcpmcp",
            "好好好",
            "now writing go ",
        ] {
            for reasoning in [false, true] {
                for width in [1, 7, 8192] {
                    let trip = feed(
                        &mut LoopGuard::default(),
                        &cycle.repeat(10_000),
                        reasoning,
                        width,
                    )
                    .expect("run-on loop");
                    assert!(trip.run_on);
                    assert_eq!(trip.reasoning, reasoning);
                }
            }
        }
        // Short natural breaks reset the run-on counter; long repeated lines do not.
        let text = format!("{}\n", "mcp ".repeat(1500)).repeat(4);
        assert!(
            feed(&mut LoopGuard::default(), &text, false, 7)
                .unwrap()
                .run_on
        );
        let text = format!("{}\nOK\n", "mcp ".repeat(1024)).repeat(4);
        assert!(feed(&mut LoopGuard::default(), &text, false, 7).is_none());
    }

    #[test]
    fn channels_are_independent_and_tools_and_metadata_are_not_prose() {
        let mut guard = LoopGuard::default();
        let text = "Now\n".repeat(WINDOW / 2);
        assert!(feed(&mut guard, &text, true, 3).is_none());
        assert!(feed(&mut guard, &text, false, 3).is_none());
        for _ in 0..300 {
            assert!(
                guard
                    .observe(&StreamEvent::ToolCallDelta {
                        index: 0,
                        args: "Now\n".repeat(300)
                    })
                    .is_none()
            );
            assert!(
                guard
                    .observe(&StreamEvent::Start {
                        model: "Now\n".repeat(300)
                    })
                    .is_none()
            );
        }
        assert!(
            feed(&mut guard, &"Now\n".repeat(WINDOW / 2), true, 3)
                .unwrap()
                .reasoning
        );
        assert!(
            !feed(&mut guard, &"Now\n".repeat(WINDOW / 2), false, 3)
                .unwrap()
                .reasoning
        );
    }

    #[test]
    fn normal_code_tables_logs_numeric_and_long_distinct_text_do_not_trip() {
        let mut normal = String::new();
        for i in 0..2000 {
            normal += &format!(
                "fn example_{i}() {{ return {i}; }}\n| item {i} | column {i} |\n{i}. Test step {i}.\n"
            );
        }
        normal += &"{}\n---\n0x00 0x01\n1234567890\n".repeat(1000);
        normal += &(0..4000)
            .map(|i| format!("identifier_{i} "))
            .collect::<String>();
        for reasoning in [false, true] {
            assert!(feed(&mut LoopGuard::default(), &normal, reasoning, 7).is_none());
        }
        for source in [
            include_str!("server.rs"),
            include_str!("console.html"),
            include_str!("stream_timeout.rs"),
            include_str!("../../../README.md"),
        ] {
            assert!(feed(&mut LoopGuard::default(), source, false, 13).is_none());
        }
    }

    #[test]
    fn bounded_keys_and_current_unit_keep_unicode_boundaries_and_memory_bounded() {
        let mut run = UnitRun::default();
        for i in 0..20_000 {
            assert!(run.add(&format!("{i} {}\n", "你好".repeat(100))).is_none());
        }
        assert_eq!(run.ring.len(), WINDOW);
        assert_eq!(run.counts.len(), WINDOW);
        assert!(run.ring.iter().all(|key| key.len() <= KEY_MOST));
        assert!(run.current.capacity() <= 2 * UNIT_MOST);
        let text = (0..4000).map(|i| format!("变量_{i} ")).collect::<String>();
        assert!(run.add(&text).is_none());
        assert!(run.current.len() < UNIT_MOST + 4);
        assert!(run.current.capacity() <= 2 * UNIT_MOST);
    }

    #[test]
    fn key_normalization_keeps_numbers_and_excludes_nonwords() {
        assert_eq!(unit_key("  NOW.!? "), Some("now".into()));
        assert_eq!(unit_key("好。"), Some("好".into()));
        assert_ne!(unit_key("row 1"), unit_key("row 2"));
        for text in ["", "{}", "123 456", "0x00", "v2 abc123", "----"] {
            assert!(unit_key(text).is_none(), "{text}");
        }
        assert!(!run_on_words(&"0x00 ".repeat(1000)));
        assert!(!run_on_words(
            &"a".repeat(65)
                .chars()
                .chain("b".repeat(65).chars())
                .collect::<String>()
                .repeat(100)
        ));
    }
}
