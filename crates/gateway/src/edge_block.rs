//! Vendor edge-firewall block pages for cross-route fallback.
//!
//! Ported from the reference gateway's `EdgeBlocked` / `BlockedHint`.
//! A firewall block page is not the vendor's API answering and nothing
//! in the request is at fault: another provider of the same model goes
//! another way. Matched on the raw response text, so HTML pages that
//! never parse as JSON are still recognized.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// What a vendor's edge firewall blocking this address means, in plain
/// words (Alibaba Cloud's ESA answers a 405 HTML page, "Sorry, your
/// request has been blocked due to unusual activity").
pub(crate) const BLOCKED_HINT: &str = "the provider's network firewall blocked requests from this IP; wait a while, or switch to another network or proxy";

fn blocked_words() -> &'static Regex {
    static BLOCKED: OnceLock<Regex> = OnceLock::new();
    BLOCKED.get_or_init(|| {
        Regex::new(
            r"(?i)request has been blocked|errors\.aliyun\.com|firewall blocked requests from this IP",
        )
        .expect("edge block regex")
    })
}

/// Says whether raw upstream error text is a firewall's block page.
pub(crate) fn edge_blocked_text(text: &str) -> bool {
    blocked_words().is_match(text)
}

/// Says whether a parsed error body is a firewall's block page.
pub(crate) fn edge_blocked(body: &Value) -> bool {
    if body.is_null() {
        return false;
    }
    edge_blocked_text(&body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_aliyun_block_pages_and_plain_hints() {
        assert!(edge_blocked_text(
            "<html><body>Sorry, your request has been blocked due to unusual activity. errors.aliyun.com</body></html>"
        ));
        assert!(edge_blocked_text(BLOCKED_HINT));
        assert!(edge_blocked(&json!({
            "error": {"message": "Sorry, your request has been blocked"}
        })));
        assert!(!edge_blocked(&json!({
            "error": {"message": "your request was blocked by content policy"}
        })));
        assert!(!edge_blocked_text("normal API error"));
        assert!(!edge_blocked(&Value::Null));
    }
}
