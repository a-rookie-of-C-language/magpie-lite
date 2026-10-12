//! Streaming: the protocol-neutral event set every SSE dialect is decoded
//! into, and a small server-sent-events frame parser.

use crate::ir::{StopReason, Usage};

/// One thing that happened in a streamed completion.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The upstream message began (its model name, as it reported it).
    Start {
        /// Model that answered.
        model: String,
    },
    /// A fragment of visible text.
    TextDelta {
        /// The fragment.
        text: String,
    },
    /// A fragment of reasoning text.
    ThinkingDelta {
        /// The fragment.
        text: String,
    },
    /// A tool call opened. `index` numbers the call within the message.
    ToolCallStart {
        /// Call index.
        index: usize,
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
    },
    /// A fragment of a tool call's JSON arguments.
    ToolCallDelta {
        /// Call index.
        index: usize,
        /// Argument fragment, as spelled on the wire.
        args: String,
    },
    /// A tool call closed (its arguments are complete).
    ToolCallEnd {
        /// Call index.
        index: usize,
    },
    /// Token accounting, as it arrived.
    Usage(Usage),
    /// The message ended, with why.
    Stop(StopReason),
    /// The stream is over.
    Done,
    /// The upstream reported an error.
    Error {
        /// Human-readable detail.
        message: String,
    },
}

/// Default byte budget for one raw SSE event, including nonempty lines and
/// their line endings, but excluding the terminating blank line.
/// The reference gateway accepts 32 MiB per line; we also bound the full event.
pub const MAX_SSE_EVENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("upstream SSE event exceeds {limit} bytes")]
    EventTooLarge { limit: usize },
}

/// Incremental, bounded SSE parser. Feed a borrowed transport chunk repeatedly
/// until it is consumed; each call returns at most one data payload. No queue
/// or copy of the whole transport chunk is retained. LF, CRLF and CR are valid
/// line endings, even when split across chunks; a single leading BOM is ignored.
#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    max_event_bytes: usize,
    skip_lf: bool,
    first_line: bool,
    failed: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_limit(MAX_SSE_EVENT_BYTES)
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a per-event byte budget. Transport chunks may contain any number of
    /// individually bounded events and may be much larger than this limit.
    pub fn with_limit(max_event_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            event_bytes: 0,
            max_event_bytes,
            skip_lf: false,
            first_line: true,
            failed: false,
        }
    }

    pub fn has_buffer(&self) -> bool {
        self.event_bytes != 0
    }

    /// Consume only through the next data event, advancing the caller's slice.
    /// A size error is terminal and must never be followed by a successful EOF.
    pub fn feed(&mut self, bytes: &mut &[u8]) -> Result<Option<String>, SseError> {
        if self.failed {
            return Err(self.size_error());
        }
        while !bytes.is_empty() {
            if self.skip_lf {
                self.skip_lf = false;
                if bytes[0] == b'\n' {
                    *bytes = &bytes[1..];
                    // CRLF after a nonempty line belongs to that event. The LF
                    // of a blank separator does not belong to the next event.
                    if self.event_bytes != 0 {
                        self.account(1)?;
                    }
                    continue;
                }
            }
            let end = bytes
                .iter()
                .position(|b| matches!(b, b'\r' | b'\n'))
                .unwrap_or(bytes.len());
            self.account(end)?;
            self.line.extend_from_slice(&bytes[..end]);
            *bytes = &bytes[end..];
            if bytes.is_empty() {
                break;
            }
            let newline = bytes[0];
            *bytes = &bytes[1..];
            self.skip_lf = newline == b'\r';
            // A BOM alone before the first newline is also a blank line.
            if self.line.is_empty() || (self.first_line && self.line == b"\xef\xbb\xbf") {
                self.line.clear();
                self.first_line = false;
                if let Some(data) = self.dispatch() {
                    return Ok(Some(data));
                }
            } else {
                self.account(1)?;
                self.finish_line();
            }
        }
        Ok(None)
    }

    /// Tolerate a final event without its blank separator, as the existing
    /// gateway does. A stream size error can never be flushed into a payload.
    pub fn flush(&mut self) -> Result<Option<String>, SseError> {
        if self.failed {
            return Err(self.size_error());
        }
        if !self.line.is_empty() {
            self.finish_line();
        }
        Ok(self.dispatch())
    }

    fn size_error(&self) -> SseError {
        SseError::EventTooLarge {
            limit: self.max_event_bytes,
        }
    }

    fn account(&mut self, count: usize) -> Result<(), SseError> {
        if count > self.max_event_bytes.saturating_sub(self.event_bytes) {
            self.failed = true;
            self.line = Vec::new();
            self.data = Vec::new();
            self.event_bytes = 0;
            return Err(self.size_error());
        }
        self.event_bytes += count;
        Ok(())
    }

    fn finish_line(&mut self) {
        let mut line = self.line.as_slice();
        if self.first_line {
            self.first_line = false;
            line = line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line);
        }
        // A colon-less field has an empty value. Only one optional space after
        // the colon is removed; comments and unknown fields are ignored.
        let value = if line == b"data" {
            Some(&b""[..])
        } else {
            line.strip_prefix(b"data:")
                .map(|value| value.strip_prefix(b" ").unwrap_or(value))
        };
        if let Some(value) = value {
            self.data.extend_from_slice(value);
            self.data.push(b'\n');
        }
        self.line.clear();
    }

    fn dispatch(&mut self) -> Option<String> {
        self.event_bytes = 0;
        if self.data.is_empty() {
            return None;
        }
        self.data.pop();
        let payload = String::from_utf8_lossy(&self.data).into_owned();
        self.data.clear();
        Some(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8], chunk_size: usize) -> Vec<String> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for chunk in input.chunks(chunk_size) {
            let mut remaining = chunk;
            while let Some(event) = parser.feed(&mut remaining).unwrap() {
                events.push(event);
            }
            assert!(remaining.is_empty());
        }
        if let Some(event) = parser.flush().unwrap() {
            events.push(event);
        }
        events
    }

    #[test]
    fn parses_frames_across_chunk_boundaries() {
        let input = b"data: {\"a\":1}\n\ndata: [DONE]\n\n";
        for chunk in 1..=input.len() {
            assert_eq!(
                parse(input, chunk),
                [r#"{"a":1}"#.to_owned(), "[DONE]".into()]
            );
        }
    }

    #[test]
    fn parses_all_line_endings_bom_unicode_and_multiline_data() {
        for ending in ["\n", "\r\n", "\r"] {
            let input = format!(
                "\u{feff}: ping{ending}event: x{ending}data: 中文 🦀{ending}data:  two{ending}{ending}data: done{ending}{ending}"
            );
            for chunk in 1..=input.len() {
                assert_eq!(parse(input.as_bytes(), chunk), ["中文 🦀\n two", "done"]);
            }
        }
    }

    #[test]
    fn ignores_comments_and_unknown_fields_but_keeps_empty_data() {
        let input = b": ping\n\nid: unused\n\nevent: unused\n\ndata\n\ndata:\n\ndata: \n\ndata : ignored\n\ndata:  space\n\n";
        assert_eq!(parse(input, 1), ["", "", "", " space"]);
    }

    #[test]
    fn tolerates_unterminated_final_data_and_replaces_invalid_utf8() {
        assert_eq!(parse(b"data: one\rdata: two\xff", 1), ["one\ntwo\u{fffd}"]);
        assert!(parse(b": comment without newline", 2).is_empty());
        // Only the start of the stream may have a BOM.
        assert_eq!(
            parse("data: one\n\n\u{feff}data: ignored\n\n".as_bytes(), 1),
            ["one"]
        );
    }

    #[test]
    fn bounds_full_events_including_ignored_lines_without_retaining_input() {
        for input in [
            b"data: 123456\n\n".as_slice(),
            b": 1234567890\n\n",
            b"x: 123456789\n\n",
            b"data: a\ndata: b\n\n",
        ] {
            for size in 1..=input.len() {
                let mut parser = SseParser::with_limit(10);
                let mut failed = false;
                for chunk in input.chunks(size) {
                    let mut remaining = chunk;
                    if parser.feed(&mut remaining).is_err() {
                        failed = true;
                        break;
                    }
                }
                assert!(failed);
                assert!(parser.line.is_empty() && parser.data.is_empty());
                assert!(parser.flush().is_err());
                assert!(parser.feed(&mut b"data: ok\n\n".as_slice()).is_err());
            }
        }
    }

    #[test]
    fn leaves_later_frames_borrowed_and_never_reads_past_a_returned_event() {
        let input = format!("data: ok\n\ndata: {}", "x".repeat(100_000));
        let mut remaining = input.as_bytes();
        let mut parser = SseParser::with_limit(9);
        assert_eq!(parser.feed(&mut remaining).unwrap(), Some("ok".into()));
        assert_eq!(remaining, &input.as_bytes()[10..]);
        assert!(parser.line.is_empty() && parser.data.is_empty());
        assert_eq!(
            parser.feed(&mut remaining),
            Err(SseError::EventTooLarge { limit: 9 })
        );
    }

    #[test]
    fn bom_only_first_line_does_not_join_the_following_event_budget() {
        for ending in ["\n", "\r\n", "\r"] {
            let input = format!("\u{feff}{ending}data: ok{ending}{ending}");
            for chunk_size in 1..=input.len() {
                let mut parser = SseParser::with_limit(8 + ending.len());
                let mut events = Vec::new();
                for chunk in input.as_bytes().chunks(chunk_size) {
                    let mut remaining = chunk;
                    while let Some(data) = parser.feed(&mut remaining).unwrap() {
                        events.push(data);
                    }
                }
                assert_eq!(events, ["ok"]);
                assert!(!parser.has_buffer());
            }
        }
    }

    #[test]
    fn fragmented_exact_limits_are_independent_of_crlf_chunk_boundaries() {
        for ending in ["\n", "\r\n", "\r"] {
            let input = format!("data: ok{ending}{ending}");
            let limit = 8 + ending.len();
            for size in 1..=input.len() {
                for (budget, succeeds) in [(limit, true), (limit - 1, false)] {
                    let mut parser = SseParser::with_limit(budget);
                    let mut events = Vec::new();
                    let mut failed = false;
                    for chunk in input.as_bytes().chunks(size) {
                        let mut remaining = chunk;
                        loop {
                            match parser.feed(&mut remaining) {
                                Ok(Some(event)) => events.push(event),
                                Ok(None) => break,
                                Err(_) => {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                        if failed {
                            break;
                        }
                    }
                    assert_eq!(
                        !failed, succeeds,
                        "ending={ending:?}, size={size}, budget={budget}"
                    );
                    if succeeds {
                        assert_eq!(events, ["ok"]);
                    }
                }
            }
        }
    }

    #[test]
    fn accepts_exact_limit_and_many_coalesced_frames_beyond_chunk_budget() {
        for ending in ["\n", "\r\n", "\r"] {
            let frame = format!("data: ok{ending}{ending}");
            let input = frame.repeat(1000);
            let mut remaining = input.as_bytes();
            let mut parser = SseParser::with_limit(8 + ending.len());
            let mut count = 0;
            while let Some(data) = parser.feed(&mut remaining).unwrap() {
                assert_eq!(data, "ok");
                count += 1;
            }
            assert_eq!(count, 1000);
            assert!(remaining.is_empty());
            assert!(!parser.has_buffer());
        }
    }
}
