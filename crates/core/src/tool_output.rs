//! Shared parsing for image-bearing tool outputs, not arbitrary attachments.
use serde_json::Value;

use crate::ir::{Part, PartKind};

pub(crate) fn image_url(url: &str) -> Option<Part> {
    if url.is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media = meta.strip_suffix(";base64")?;
        return (media.starts_with("image/") && !data.is_empty()).then(|| Part::image(media, data));
    }
    Some(Part::image_url(url))
}

pub(crate) fn decode(content: &Value) -> (String, Vec<Part>) {
    if let Some(text) = content.as_str() {
        return (text.to_owned(), Vec::new());
    }
    let mut text = String::new();
    let mut images = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text" | "input_text" | "output_text") => {
                if let Some(value) = block["text"].as_str() {
                    text.push_str(value);
                }
            }
            Some("image") => {
                let source = &block["source"];
                if let (Some(media), Some(data)) =
                    (source["media_type"].as_str(), source["data"].as_str())
                {
                    if media.starts_with("image/") && !data.is_empty() {
                        images.push(Part::image(media, data));
                    }
                } else if let Some(image) = source["url"].as_str().and_then(image_url) {
                    images.push(image);
                }
            }
            Some("input_image" | "image_url") => {
                if let Some(image) = block["image_url"]
                    .as_str()
                    .or_else(|| block["image_url"]["url"].as_str())
                    .and_then(image_url)
                {
                    images.push(image);
                }
            }
            _ => {}
        }
    }
    (text, images)
}

/// Child parts are public IR, so don't accidentally encode nonimages or empty
/// payloads as images when a caller constructs a tool result directly.
pub(crate) fn images(part: &Part) -> impl Iterator<Item = &Part> {
    part.images.iter().filter(|p| {
        p.kind() == PartKind::Image
            && ((!p.data.is_empty() && p.media_type.starts_with("image/"))
                || (!p.url.is_empty()
                    && (p.media_type.is_empty() || p.media_type.starts_with("image/"))))
    })
}
