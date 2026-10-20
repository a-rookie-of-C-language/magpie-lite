//! Max reply tokens the user set per model.
//!
//! A request asking for more than the user's Max output for the model is
//! lowered to it on any route (the reference's `userOutput`), and a limit
//! left unnamed and translated to Anthropic's Messages API – which needs
//! `max_tokens` – is filled with it (the reference's `askedOutput`, #1792:
//! a flat 16384 cut a long Codex thinking and tool turn short). Only the
//! user's own figure ever lowers or fills a request; a vendor's list may
//! be stale. The effort budget fits under the resulting limit when the
//! request is encoded, as under one the agent named.

use magpie_core::ir::CompletionRequest;

use crate::config::Protocol;

/// Fit `req`'s reply limit to the user's Max output `limit` for the model
/// being asked: a named limit above it is lowered on any target, and an
/// unnamed one headed for Anthropic's API is filled with it. A zero limit,
/// or any other request, goes as the agent sent it.
pub fn apply_user_output(req: &mut CompletionRequest, target: Protocol, limit: u64) {
    if limit == 0 {
        return;
    }
    match req.max_tokens {
        Some(named) if named > limit => req.max_tokens = Some(limit),
        None if target == Protocol::Anthropic => req.max_tokens = Some(limit),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(max_tokens: Option<u64>) -> CompletionRequest {
        CompletionRequest {
            max_tokens,
            ..CompletionRequest::default()
        }
    }

    #[test]
    fn a_named_limit_above_the_users_is_lowered_on_any_target() {
        for target in [
            Protocol::Chat,
            Protocol::Anthropic,
            Protocol::Responses,
            Protocol::Gemini,
        ] {
            let mut req = request(Some(128_000));
            apply_user_output(&mut req, target, 32_000);
            assert_eq!(req.max_tokens, Some(32_000), "{target:?}");
        }
    }

    #[test]
    fn a_named_limit_within_the_users_goes_as_it_is() {
        let mut req = request(Some(16_000));
        apply_user_output(&mut req, Protocol::Chat, 32_000);
        assert_eq!(req.max_tokens, Some(16_000));
        let mut exact = request(Some(32_000));
        apply_user_output(&mut exact, Protocol::Anthropic, 32_000);
        assert_eq!(exact.max_tokens, Some(32_000));
    }

    #[test]
    fn an_unnamed_limit_is_filled_only_for_anthropic() {
        let mut anthropic = request(None);
        apply_user_output(&mut anthropic, Protocol::Anthropic, 32_000);
        assert_eq!(anthropic.max_tokens, Some(32_000));
        for target in [Protocol::Chat, Protocol::Responses, Protocol::Gemini] {
            let mut req = request(None);
            apply_user_output(&mut req, target, 32_000);
            assert_eq!(req.max_tokens, None, "{target:?}");
        }
    }

    #[test]
    fn no_user_limit_leaves_the_request_alone() {
        let mut named = request(Some(128_000));
        apply_user_output(&mut named, Protocol::Anthropic, 0);
        assert_eq!(named.max_tokens, Some(128_000));
        let mut unnamed = request(None);
        apply_user_output(&mut unnamed, Protocol::Anthropic, 0);
        assert_eq!(unnamed.max_tokens, None);
    }
}
