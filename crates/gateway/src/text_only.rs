//! Text-only models: a provider that declares no vision
//! (`capabilities.vision == Some(false)`) is never sent image bytes.
//!
//! The reference walks the wire JSON (`image_input.go`): a user image in
//! the latest turn is rejected before any upstream call, older and
//! tool-returned images are replaced with a placeholder. Lite enforces the
//! same rule on the shared IR per attempt, so every upstream dialect gets
//! it without four wire walkers. Unknown (`None`) or `Some(true)` vision
//! leaves the request untouched; groups skip a text-only member for an
//! image-carrying request through the existing local-conversion fallback.
use magpie_core::ir::{CompletionRequest, Part, PartKind};

/// What replaces an image a text-only model must not see (the reference's
/// `imageOmitted`).
pub const PLACEHOLDER: &str = "[Image omitted: this model accepts text only.]";

/// Whether the request's last message carries a user image: the image the
/// model is asked about now. Tool results riding in the last message keep
/// their nested images out of this check; they are omitted, not rejected.
pub fn has_current_user_image(req: &CompletionRequest) -> bool {
    req.messages
        .last()
        .is_some_and(|m| m.parts.iter().any(|p| p.kind == Some(PartKind::Image)))
}

fn has_any_image(req: &CompletionRequest) -> bool {
    req.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| p.kind == Some(PartKind::Image) || !p.images.is_empty())
    })
}

/// Replace every image with the placeholder: direct parts become text, and
/// tool-returned images are dropped with the placeholder appended to the
/// result text. Requests without images are returned unchanged.
pub fn without_images(req: &CompletionRequest) -> CompletionRequest {
    if !has_any_image(req) {
        return req.clone();
    }
    let mut owned = req.clone();
    for m in &mut owned.messages {
        for p in &mut m.parts {
            if p.kind == Some(PartKind::Image) {
                *p = Part::text(PLACEHOLDER);
            }
            if !p.images.is_empty() {
                p.images.clear();
                if p.text.is_empty() {
                    p.text = PLACEHOLDER.to_owned();
                } else {
                    p.text.push_str("\n\n");
                    p.text.push_str(PLACEHOLDER);
                }
            }
        }
    }
    owned
}

#[cfg(test)]
mod tests {
    use super::*;
    use magpie_core::ir::{Message, Role};

    fn user(parts: Vec<Part>) -> Message {
        Message {
            role: Role::User,
            parts,
        }
    }

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn current_user_image_is_only_the_last_message() {
        let history = request(vec![
            user(vec![Part::image("image/png", "aGVsbG8=")]),
            user(vec![Part::text("now answer")]),
        ]);
        assert!(!has_current_user_image(&history));
        let current = request(vec![
            user(vec![Part::text("before")]),
            user(vec![
                Part::text("read"),
                Part::image_url("https://example.invalid/shot.png"),
            ]),
        ]);
        assert!(has_current_user_image(&current));
    }

    #[test]
    fn tool_result_images_are_not_current() {
        let mut result = Part::tool_result("call_1", "shot", false);
        result.images.push(Part::image("image/png", "aGVsbG8="));
        let req = request(vec![user(vec![result])]);
        assert!(!has_current_user_image(&req));
    }

    #[test]
    fn omission_replaces_and_keeps_text() {
        let mut result = Part::tool_result("call_1", "shot", false);
        result.images.push(Part::image("image/png", "aGVsbG8="));
        let req = request(vec![
            user(vec![Part::image("image/png", "aGVsbG8=")]),
            user(vec![result, Part::text("tail")]),
        ]);
        let stripped = without_images(&req);
        assert!(!has_any_image(&stripped));
        assert_eq!(stripped.messages[0].parts[0].text, PLACEHOLDER);
        let tool = &stripped.messages[1].parts[0];
        assert!(tool.images.is_empty());
        assert!(tool.text.contains("shot"));
        assert!(tool.text.contains(PLACEHOLDER));
        assert_eq!(stripped.messages[1].parts[1].text, "tail");
    }

    #[test]
    fn imageless_requests_pass_through() {
        let req = request(vec![user(vec![Part::text("hi")])]);
        let stripped = without_images(&req);
        assert_eq!(stripped.messages[0].parts[0].text, "hi");
    }
}
