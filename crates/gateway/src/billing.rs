//! Claude Code's per-turn billing line.
//!
//! Claude Code opens its system prompt with a billing block of its own:
//! `x-anthropic-billing-header: ...`, whose trailing id changes every
//! turn. It is for Anthropic; any other vendor's model reads it as the
//! prompt's first words, so its prompt cache misses from the first byte
//! on every turn (the reference's `billing_line.go`, #1600).
//!
//! The line goes on only to a Claude model asked on Anthropic's Messages
//! API. Everywhere else the gateway sends the prompt without it, so two
//! turns that differ only in the line's id send the same prompt.
//! Lite never manages Anthropic prompt-cache breakpoints, so there is no
//! cache-breakpoint accounting to adjust here.

use std::sync::OnceLock;

use magpie_core::ir::{Part, PartKind};

use crate::config::Protocol;

fn billing_line() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^x-anthropic-billing-header:(\s*[A-Za-z_]+=[^;\s]*;)*\s*$")
            .expect("billing line pattern")
    })
}

fn billing_header_prefix() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^x-anthropic-billing-header:(\s*[A-Za-z_]+=[^;\s]*;)*\s*")
            .expect("billing header prefix pattern")
    })
}

fn anthropic_model() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)(?:^|[/.:-])claude-").expect("Claude model pattern"))
}

/// Whether a request for `model` (the upstream's spelling) sent on
/// `target` goes without Claude Code's billing line: everything but a
/// Claude model asked on Anthropic's Messages API. Lite has no
/// remote-magpie or Code Assist providers, so those reference carve-outs
/// do not apply.
pub fn drops_billing_line(target: Protocol, model: &str) -> bool {
    target != Protocol::Anthropic || !anthropic_model().is_match(model)
}

/// The system prompt without Claude Code's billing line: a text part that
/// is the line alone is dropped, and the line heading the first text part
/// is cut from it. Anything else 鈥?a mention mid-prompt, a later part
/// heading with it, non-text parts 鈥?goes as it was sent.
pub fn strip_billing_line(system: &mut Vec<Part>) {
    system.retain(|part| !(part.kind() == PartKind::Text && billing_line().is_match(&part.text)));
    if let Some(first) = system.iter_mut().find(|part| part.kind() == PartKind::Text)
        && let Some(prefix) = billing_header_prefix().find(&first.text)
    {
        first.text.drain(..prefix.end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_part(text: &str) -> Part {
        Part {
            kind: Some(PartKind::Text),
            text: text.to_owned(),
            ..Part::default()
        }
    }

    fn texts(system: &[Part]) -> Vec<&str> {
        system.iter().map(|part| part.text.as_str()).collect()
    }

    #[test]
    fn a_lone_billing_block_is_dropped_and_the_prompt_stays() {
        let mut system = vec![
            text_part(
                "x-anthropic-billing-header: cc_version=2.1.114.aff; cc_entrypoint=sdk-cli; cch=9ebf3;",
            ),
            text_part("You are a Claude agent, built on Anthropic's Claude Agent SDK."),
            text_part("\nYou are an interactive agent <rules> & more"),
        ];
        strip_billing_line(&mut system);
        assert_eq!(
            texts(&system),
            [
                "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
                "\nYou are an interactive agent <rules> & more"
            ]
        );
    }

    #[test]
    fn a_line_heading_the_first_part_is_cut_from_it() {
        let mut system = vec![text_part(
            "x-anthropic-billing-header: cch=0218c;\nYou are it.",
        )];
        strip_billing_line(&mut system);
        assert_eq!(texts(&system), ["You are it."]);
    }

    #[test]
    fn a_system_that_is_the_line_alone_is_left_empty() {
        let mut system = vec![text_part(
            "x-anthropic-billing-header: cc_version=2.1.114.aff; cch=0218c;",
        )];
        strip_billing_line(&mut system);
        assert!(system.is_empty());
    }

    #[test]
    fn mentions_and_later_parts_go_as_they_were_sent() {
        let mut system = vec![
            text_part("You are <it>."),
            text_part("Rules mention x-anthropic-billing-header: here"),
            text_part("b x-anthropic-billing-header: cch=1;"),
        ];
        strip_billing_line(&mut system);
        assert_eq!(
            texts(&system),
            [
                "You are <it>.",
                "Rules mention x-anthropic-billing-header: here",
                "b x-anthropic-billing-header: cch=1;"
            ]
        );
    }

    #[test]
    fn non_text_parts_are_never_touched() {
        let mut system = vec![
            Part {
                kind: Some(PartKind::Image),
                text: "x-anthropic-billing-header: cch=1;".to_owned(),
                ..Part::default()
            },
            text_part("You are <it>."),
        ];
        strip_billing_line(&mut system);
        assert_eq!(
            texts(&system),
            ["x-anthropic-billing-header: cch=1;", "You are <it>."]
        );
    }

    #[test]
    fn the_line_is_kept_only_for_claude_on_anthropic() {
        assert!(drops_billing_line(Protocol::Chat, "mock-model"));
        assert!(drops_billing_line(Protocol::Chat, "claude-sonnet-4-5"));
        assert!(drops_billing_line(Protocol::Responses, "claude-sonnet-4-5"));
        assert!(drops_billing_line(Protocol::Gemini, "claude-sonnet-4-5"));
        assert!(drops_billing_line(Protocol::Anthropic, "mock-model"));
        assert!(!drops_billing_line(
            Protocol::Anthropic,
            "claude-sonnet-4-5-20250929"
        ));
        assert!(!drops_billing_line(
            Protocol::Anthropic,
            "anthropic/claude-opus-4-1"
        ));
    }
}
