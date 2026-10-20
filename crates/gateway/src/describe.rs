//! Image descriptions for models that cannot see.
//!
//! A provider that declares no vision (`capabilities.vision ==
//! Some(false)`) is never sent image bytes; by default such images are
//! refused (a current user image) or omitted with a note (older and
//! tool-returned ones). When the gateway names an Image recognition
//! model (`vision` in the config: `provider/model`, never `off` or
//! unset), each image is instead described once by that model and the
//! description goes in the image's place — in the current turn, older
//! turns and tool results alike (the reference's `internal/gateway/
//! vision.go`, without its Settings UI or account-tier auto-pick: lite
//! has no catalog vision metadata to pick from, so only an explicit
//! model ref describes).
//!
//! The call goes through lite itself with the vision User-Agent, so any
//! model lite serves can describe, with its failover and usage. When
//! the describer cannot say, the request falls back to the refusal and
//! omission it would have had without one.
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use magpie_core::ir::{CompletionRequest, Part, PartKind};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The User-Agent lite's own describe calls carry (the reference's
/// `VisionAgent`).
pub const VISION_AGENT: &str = "magpie-vision/1";
/// How long one image's description may take (the reference's
/// `visionTimeout`).
pub const VISION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a failed describer rests before it is asked again (the
/// reference rests its classifier; a description call that fails is
/// retried on the next request past this, not on every image).
const DESCRIBE_REST: Duration = Duration::from_secs(30);
/// Most descriptions remembered at once (the reference's `sightsKept`).
const SIGHTS_KEPT: usize = 256;

/// What the describer is told: transcribe everything, add nothing (the
/// reference's `visionSystem`).
pub const VISION_PROMPT: &str = "You describe images for an AI model that cannot see them. It will answer the user from your description alone, so leave nothing out that it may need.\n- Transcribe all text exactly as written, keeping its layout: code, terminal output, error messages, logs, UI labels, menus, file names, numbers.\n- For a screenshot of an app or page: which app or page it is, its layout, and the state of what is on it (selected, disabled, checked, highlighted, error states).\n- For a chart or table: its kind, axes and labels, and every value you can read.\n- For a diagram: its elements and how they are connected.\n- For a photo or drawing: what it shows, with the details that matter.\nDescribe only what is there. Don't guess at what can't be read, say it can't be read. Don't answer questions or give advice. No preamble.";

/// One remembered description, keyed by model and image source.
#[derive(Default)]
struct Sights {
    texts: HashMap<String, String>,
    order: VecDeque<String>,
    rested: HashMap<String, Instant>,
}
static SIGHTS: LazyLock<Mutex<Sights>> = LazyLock::new(|| Mutex::new(Sights::default()));
/// Forget every remembered description (tests).
#[doc(hidden)]
pub fn reset() {
    *SIGHTS.lock().unwrap() = Sights::default();
}

/// Whether the describer failed too recently to ask again: while rested
/// the request falls back at once instead of stalling on another call.
pub fn rested(model: &str) -> bool {
    SIGHTS
        .lock()
        .unwrap()
        .rested
        .get(model)
        .is_some_and(|at| at.elapsed() < DESCRIBE_REST)
}

/// Rest the describer after a failure; a success wakes it (`unrest`).
pub fn rest(model: &str) {
    SIGHTS
        .lock()
        .unwrap()
        .rested
        .insert(model.to_owned(), Instant::now());
}

/// A success wakes the describer.
pub fn unrest(model: &str) {
    SIGHTS.lock().unwrap().rested.remove(model);
}

/// What identifies an image for the description cache: the model asked
/// to describe it and the inline bytes or URL (the reference keys its
/// sights by model and source).
pub fn sight_key(model: &str, image: &Part) -> String {
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update(b"\x00");
    if !image.data.is_empty() {
        h.update(image.media_type.as_bytes());
        h.update(b"\x00");
        h.update(image.data.as_bytes());
    } else {
        h.update(image.url.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A remembered description for this model and image, if one was kept.
pub fn remembered(model: &str, image: &Part) -> Option<String> {
    SIGHTS
        .lock()
        .unwrap()
        .texts
        .get(&sight_key(model, image))
        .cloned()
}

/// Remember a description, keeping at most [`SIGHTS_KEPT`] sights: an
/// agent sends a conversation's images again every turn.
pub fn remember(model: &str, image: &Part, text: String) {
    let key = sight_key(model, image);
    let mut sights = SIGHTS.lock().unwrap();
    if sights.texts.contains_key(&key) {
        return;
    }
    sights.texts.insert(key.clone(), text);
    sights.order.push_back(key);
    while sights.order.len() > SIGHTS_KEPT {
        if let Some(old) = sights.order.pop_front() {
            sights.texts.remove(&old);
        }
    }
}

/// Whether this part is an image with something to describe.
pub fn describable(part: &Part) -> bool {
    part.kind == Some(PartKind::Image) && (!part.data.is_empty() || !part.url.is_empty())
}

/// Whether the request carries any image a text-only model must not see,
/// direct or tool-returned.
pub fn has_any_image(req: &CompletionRequest) -> bool {
    req.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| describable(p) || p.images.iter().any(describable))
    })
}

/// Every image in the request, direct and tool-returned, in order.
pub fn collect_images(req: &CompletionRequest) -> Vec<Part> {
    let mut out = Vec::new();
    for m in &req.messages {
        for p in &m.parts {
            if describable(p) {
                out.push(p.clone());
            }
            for image in &p.images {
                if describable(image) {
                    out.push(image.clone());
                }
            }
        }
    }
    out
}

/// The request with every image replaced by its description, said as such
/// so the model knows it did not see the image itself. Tool-returned
/// images are dropped with their descriptions appended to the result text.
/// The caller describes every image first: a missing description falls back
/// to the text-only placeholder rather than stalling the request.
pub fn with_descriptions(
    req: &CompletionRequest,
    mut describe: impl FnMut(&Part) -> Option<String>,
) -> CompletionRequest {
    let mut owned = req.clone();
    for m in &mut owned.messages {
        for p in &mut m.parts {
            if describable(p) {
                let note = describe(p)
                    .map(|text| description_note(&text))
                    .unwrap_or_else(|| crate::text_only::PLACEHOLDER.to_owned());
                *p = Part::text(note);
            }
            if p.images.iter().any(describable) {
                let mut notes = Vec::new();
                for image in &p.images {
                    if !describable(image) {
                        continue;
                    }
                    notes.push(
                        describe(image)
                            .map(|text| description_note(&text))
                            .unwrap_or_else(|| crate::text_only::PLACEHOLDER.to_owned()),
                    );
                }
                p.images.clear();
                for note in notes {
                    if p.text.is_empty() {
                        p.text = note;
                    } else {
                        p.text.push_str("\n\n");
                        p.text.push_str(&note);
                    }
                }
            }
        }
    }
    owned
}

/// The image as a Chat content part, the shape lite's own Chat codec
/// sends and reads.
fn image_content(image: &Part) -> Value {
    if !image.data.is_empty() {
        let media = if image.media_type.is_empty() {
            "image/png"
        } else {
            &image.media_type
        };
        json!({"type": "image_url", "image_url": {"url": format!("data:{media};base64,{}", image.data)}})
    } else {
        json!({"type": "image_url", "image_url": {"url": image.url}})
    }
}

/// The Chat request asking `model` to describe one image (the
/// reference's describe call); a model that reasons does so before it
/// answers, and room is left for a full transcription.
pub fn describe_body(model: &str, image: &Part) -> Value {
    json!({
        "model": model,
        "messages": [
            {"role": "system", "content": VISION_PROMPT},
            {"role": "user", "content": [
                {"type": "text", "text": "Describe this image for a model that cannot see it."},
                image_content(image),
            ]},
        ],
        "stream": false,
        "temperature": 0,
        "max_tokens": 4096,
    })
}

/// The description a describe answer carries: the reply's text (the
/// reference reads the description out of the answer the same way).
pub fn read_description(answer: &Value) -> Result<String, String> {
    let text = match answer.pointer("/choices/0/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => {
            return Err("the describer answered without text".to_owned());
        }
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("the describer answered with empty text".to_owned());
    }
    Ok(text)
}

/// The text that goes where a described image was: the description, said
/// as such so the model knows it did not see the image itself.
pub fn description_note(text: &str) -> String {
    format!("[Image description: {text}]")
}

/// Resolve the configured Image recognition model to the provider and
/// model to ask: an explicit `provider/model` ref whose provider exists
/// and is not itself text-only (describing through one would refuse
/// again). Unset, `off`, a bare name, or a missing or text-only
/// provider describes nothing — the reference's `seer`, without its
/// auto-pick: lite has no catalog vision metadata to pick from.
pub fn resolve(config: &crate::config::Config) -> Option<(String, String)> {
    let want = config.vision.as_deref()?;
    if want.is_empty() || want == "off" {
        return None;
    }
    let (provider, model) = want.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    let found = config.provider(provider)?;
    if found.capabilities.vision == Some(false) {
        return None;
    }
    Some((found.id.clone(), model.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Part {
        Part {
            kind: Some(PartKind::Image),
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
            ..Default::default()
        }
    }

    #[test]
    fn describe_body_carries_the_image_and_the_brief() {
        let body = describe_body("eye/see", &image());
        assert_eq!(body["model"], json!("eye/see"));
        assert_eq!(body["stream"], json!(false));
        let content = body
            .pointer("/messages/1/content")
            .expect("user content")
            .as_array()
            .expect("content parts");
        assert_eq!(content.len(), 2);
        assert_eq!(
            content[1]["image_url"]["url"],
            json!("data:image/png;base64,aGVsbG8=")
        );
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .expect("system prompt")
                .contains("cannot see them")
        );
    }

    #[test]
    fn url_images_ride_as_urls() {
        let mut image = image();
        image.data.clear();
        image.url = "https://example.invalid/shot.png".into();
        let body = describe_body("eye/see", &image);
        assert_eq!(
            body.pointer("/messages/1/content/1/image_url/url")
                .expect("url"),
            &json!("https://example.invalid/shot.png")
        );
    }

    #[test]
    fn descriptions_read_as_text_and_empty_is_no_answer() {
        let answer = json!({"choices": [{"message": {"content": "  a red square  "}}]});
        assert_eq!(read_description(&answer).expect("text"), "a red square");
        let empty = json!({"choices": [{"message": {"content": "   "}}]});
        assert!(read_description(&empty).is_err());
        let none = json!({"choices": [{}]});
        assert!(read_description(&none).is_err());
    }

    #[test]
    fn the_cache_keys_by_model_and_source_and_forgets_oldest() {
        reset();
        let image = image();
        assert_eq!(remembered("a/m", &image), None);
        remember("a/m", &image, "hello".into());
        assert_eq!(remembered("a/m", &image).as_deref(), Some("hello"));
        // Another model describing the same bytes asks again.
        assert_eq!(remembered("b/m", &image), None);
    }

    #[test]
    fn images_are_found_direct_and_tool_returned() {
        use magpie_core::ir::{CompletionRequest, Message, Role};
        let plain = CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::text("hi")],
            }],
            ..Default::default()
        };
        assert!(!has_any_image(&plain));
        assert!(collect_images(&plain).is_empty());
        let mut result = Part::tool_result("call_1", "shot", false);
        result.images.push(image());
        let req = CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                parts: vec![image(), result],
            }],
            ..Default::default()
        };
        assert!(has_any_image(&req));
        assert_eq!(collect_images(&req).len(), 2);
    }

    #[test]
    fn descriptions_replace_images_where_they_were() {
        use magpie_core::ir::{CompletionRequest, Message, Role};
        let mut result = Part::tool_result("call_1", "shot", false);
        result.images.push(image());
        let req = CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                parts: vec![image(), result],
            }],
            ..Default::default()
        };
        let out = with_descriptions(&req, |_| Some("a red square".to_owned()));
        assert!(collect_images(&out).is_empty());
        assert_eq!(
            out.messages[0].parts[0].text,
            "[Image description: a red square]"
        );
        let tool = &out.messages[0].parts[1];
        assert!(tool.images.is_empty());
        assert!(tool.text.contains("shot"));
        assert!(tool.text.contains("[Image description: a red square]"));
    }
}
