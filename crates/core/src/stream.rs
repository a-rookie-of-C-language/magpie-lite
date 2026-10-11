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

/// Parses SSE frames out of a byte stream. Feeds it chunks as they arrive;
/// each `next_data` returns the joined `data:` payload of one frame.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    /// A parser with nothing buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffer more bytes from the wire.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Whether anything is left buffered.
    pub fn has_buffer(&self) -> bool {
        !self.buf.is_empty()
    }

    /// The next complete frame's data payload, if one is buffered.
    pub fn next_data(&mut self) -> Option<String> {
        loop {
            let (block_len, sep_len) = self.find_boundary()?;
            let block: Vec<u8> = self.buf.drain(..block_len + sep_len).collect();
            let block = String::from_utf8_lossy(&block[..block_len]);
            let mut data = String::new();
            for line in block.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    data.push('\n');
                }
            }
            if data.is_empty() {
                continue; // a comment or an event with no data
            }
            data.pop(); // the newline the loop appended
            return Some(data);
        }
    }

    /// Flush a trailing frame that the upstream ended without a blank line
    /// after (non-conforming, but cheap to tolerate).
    pub fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let block = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        let mut data = String::new();
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                data.push('\n');
            }
        }
        data.pop();
        if data.is_empty() { None } else { Some(data) }
    }

    /// Where the next frame ends: (bytes of the block, bytes of the blank
    /// separator). Accepts `\n\n` and `\r\n\r\n` (and the mixed `\n\r\n`).
    fn find_boundary(&self) -> Option<(usize, usize)> {
        let b = &self.buf;
        for i in 0..b.len().saturating_sub(1) {
            if b[i] == b'\n' {
                if b[i + 1] == b'\n' {
                    return Some((i, 2));
                }
                if i + 2 < b.len() && b[i + 1] == b'\r' && b[i + 2] == b'\n' {
                    return Some((i, 3));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_across_chunk_boundaries() {
        let mut p = SseParser::new();
        p.extend(b"data: {\"a\":");
        assert_eq!(p.next_data(), None);
        p.extend(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(p.next_data(), Some(r#"{"a":1}"#.into()));
        assert_eq!(p.next_data(), Some("[DONE]".into()));
        assert_eq!(p.next_data(), None);
    }

    #[test]
    fn parses_crlf_frames_and_ignores_comments() {
        let mut p = SseParser::new();
        p.extend(b": ping\r\ndata: one\r\n\r\nevent: x\r\ndata: two\r\n\r\n");
        assert_eq!(p.next_data(), Some("one".into()));
        assert_eq!(p.next_data(), Some("two".into()));
    }

    #[test]
    fn joins_multi_line_data() {
        let mut p = SseParser::new();
        p.extend(b"data: a\ndata: b\n\n");
        assert_eq!(p.next_data(), Some("a\nb".into()));
    }
}
