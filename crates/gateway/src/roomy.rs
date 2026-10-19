//! A group member whose model is known to take less than the request isn't
//! asked it, before anything is sent.
//!
//! Ports the reference gateway's `roomy`/`estimate` pair (`0f398c48`). A
//! member whose context window is known and no larger than the request would
//! only refuse it — and a refusal that doesn't say why, for a request not far
//! enough past the window, reached the agent as the vendor's error while the
//! next member had room. It also spent a request against the member's limits.

use magpie_core::ir::{CompletionRequest, Part};
use serde_json::Value;

/// The tokens a request is estimated to hold: four bytes a token over the
/// text, tool arguments and schemas — the reference's `estimate`. Mostly code
/// counts low and mostly-CJK text counts high, so a member is passed over
/// only when the request is past its window by this count.
pub(crate) fn estimate(req: &CompletionRequest) -> u64 {
    let mut bytes = req.system.iter().map(part_bytes).sum::<u64>();
    for message in &req.messages {
        bytes += message.parts.iter().map(part_bytes).sum::<u64>();
    }
    for tool in &req.tools {
        bytes += tool.name.len() as u64 + tool.description.len() as u64;
        bytes += json_bytes(&tool.parameters);
    }
    bytes / 4
}

/// The bytes a part contributes: its text, its name, and the arguments it
/// carries, as the reference counts them.
fn part_bytes(part: &Part) -> u64 {
    part.text.len() as u64 + part.name.len() as u64 + json_bytes(&part.args)
}

/// A value's serialized size, counting an absent one as nothing.
fn json_bytes(value: &Value) -> u64 {
    if value.is_null() {
        return 0;
    }
    serde_json::to_string(value)
        .map(|text| text.len() as u64)
        .unwrap_or(0)
}

/// Which of `windows` can hold a request of about `tokens`, one flag per
/// route, or `None` when the choice cannot be judged and every route stands.
///
/// A window of zero is not known and is kept, as is one larger than the
/// estimate. Nothing is judged when only one route is among the candidates,
/// when no window is known at all, or when the estimate is below every known
/// window. A request no route holds leaves every route: the first is asked,
/// and its overflow reaches the agent as one, to compact.
///
/// Lite's route list never names one provider/model pair twice — a group's
/// routes are deduplicated by pair and a provider's fallbacks by provider — so
/// the route count is the reference's distinct-pair count.
pub(crate) fn holds(windows: &[u64], tokens: u64) -> Option<Vec<bool>> {
    if windows.len() < 2 {
        return None;
    }
    let least = windows.iter().copied().filter(|w| *w > 0).min()?;
    if tokens < least {
        return None;
    }
    let keep: Vec<bool> = windows.iter().map(|w| *w == 0 || tokens < *w).collect();
    if keep.iter().all(|kept| *kept) || keep.iter().all(|kept| !*kept) {
        return None;
    }
    Some(keep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use magpie_core::ir::{Message, Role, Tool};
    use serde_json::json;

    fn request_with(text: &str) -> CompletionRequest {
        CompletionRequest {
            model: "g".into(),
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::text(text)],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn estimate_counts_text_arguments_and_schemas_four_bytes_a_token() {
        // 4000 bytes of text is 1000 tokens.
        assert_eq!(estimate(&request_with(&"word ".repeat(800))), 1000);

        let args = json!({"cmd": "ls"});
        let mut with_args = request_with("");
        with_args.messages[0]
            .parts
            .push(Part::tool_call("call", "bash", args.clone()));
        assert_eq!(
            estimate(&with_args),
            ("bash".len() as u64 + args.to_string().len() as u64) / 4
        );

        let parameters = json!({"type": "object", "properties": {"cmd": {"type": "string"}}});
        let mut with_tool = request_with("");
        with_tool.tools.push(Tool {
            name: "bash".into(),
            namespace: None,
            description: "Run a shell command.".into(),
            parameters: parameters.clone(),
            custom: false,
            strict: false,
        });
        assert_eq!(
            estimate(&with_tool),
            ("bash".len() as u64
                + "Run a shell command.".len() as u64
                + parameters.to_string().len() as u64)
                / 4
        );
    }

    #[test]
    fn a_member_too_small_is_left_out_and_the_others_stand() {
        // free 2000, paid 10000, a request of about 2200 tokens.
        assert_eq!(holds(&[2000, 10000], 2200), Some(vec![false, true]));
        // the same request within the free member's window goes to it
        assert_eq!(holds(&[2000, 10000], 100), None);
    }

    #[test]
    fn an_unknown_window_a_single_model_and_a_hopeless_request_are_left_alone() {
        // a window not known is kept: nothing is judged
        assert_eq!(holds(&[0, 10000], 2200), None);
        // one model has nothing to choose between
        assert_eq!(holds(&[2000], 5000), None);
        // no window known at all
        assert_eq!(holds(&[0, 0], 5000), None);
        // a request no member holds: the first is asked, its overflow told
        assert_eq!(holds(&[2000, 1500], 5000), None);
    }

    #[test]
    fn an_unknown_window_beside_a_small_one_still_keeps_the_request_going() {
        // small 2000 is left out; the unknown one stands, so the request
        // still has somewhere to go.
        assert_eq!(holds(&[2000, 0], 2200), Some(vec![false, true]));
    }
}
