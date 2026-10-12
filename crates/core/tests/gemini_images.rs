//! Native Gemini image replies, using the reference image fixture's wire shape.
use magpie_core::gemini::{self, GeminiStreamDecoder, GeminiStreamEncoder};
use serde_json::{Value, json};

fn reply() -> Value {
    json!({"modelVersion":"image-model", "candidates":[{"content":{"parts":[
        {"thought":true,"inlineData":{"mimeType":"image/png","data":"ZHJhZnQ="}},
        {"text":"Here it is."},
        {"inlineData":{"mimeType":"image/png","data":"aGVsbG8="},"thoughtSignature":"c2ln"}
    ]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":1567}})
}
fn frames(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}
#[test]
fn stream_preserves_final_image_order_and_signature_without_draft() {
    let mut decoder = GeminiStreamDecoder::new();
    let mut encoder = GeminiStreamEncoder::new("image-model");
    let wire: String = decoder
        .decode(&reply())
        .iter()
        .filter_map(|e| encoder.encode(e))
        .collect();
    let chunks = frames(&wire);
    let parts: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| {
            c.pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
        })
        .flatten()
        .collect();
    assert_eq!(parts.len(), 2, "{wire}");
    assert_eq!(parts[0]["text"], "Here it is.");
    assert_eq!(parts[1]["inlineData"]["data"], "aGVsbG8=");
    assert_eq!(parts[1]["thoughtSignature"], "c2ln");
    assert!(!wire.contains("ZHJhZnQ="));
    assert_eq!(
        chunks.last().unwrap()["usageMetadata"]["candidatesTokenCount"],
        1567
    );
}
#[test]
fn nonstream_preserves_final_image_order_and_signature_without_draft() {
    let decoded = gemini::decode_response(&reply()).unwrap();
    let encoded = gemini::encode_response(&decoded);
    let parts = encoded
        .pointer("/candidates/0/content/parts")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(parts.len(), 2, "{encoded}");
    assert_eq!(parts[0]["text"], "Here it is.");
    assert_eq!(parts[1]["inlineData"]["data"], "aGVsbG8=");
    assert_eq!(parts[1]["thoughtSignature"], "c2ln");
    assert!(!encoded.to_string().contains("ZHJhZnQ="));
}
#[test]
fn stream_preserves_snake_case_file_image_and_ignores_empty_inline() {
    let value = json!({"model_version":"image-model","candidates":[{"content":{"parts":[
        {"inline_data":{"mime_type":"image/png","data":""}},
        {"file_data":{"mime_type":"image/jpeg","file_uri":"https://example.invalid/result.jpg"},"thought_signature":"file-sig"}
    ]},"finish_reason":"STOP"}]});
    let mut decoder = GeminiStreamDecoder::new();
    let mut encoder = GeminiStreamEncoder::new("image-model");
    let wire: String = decoder
        .decode(&value)
        .iter()
        .filter_map(|e| encoder.encode(e))
        .collect();
    let chunks = frames(&wire);
    let part = &chunks[0]["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["fileData"]["mimeType"], "image/jpeg", "{wire}");
    assert_eq!(
        part["fileData"]["fileUri"],
        "https://example.invalid/result.jpg"
    );
    assert_eq!(part["thoughtSignature"], "file-sig");
    assert!(!wire.contains("inlineData"));
}

#[test]
fn empty_text_field_does_not_hide_a_complete_image_in_the_same_part() {
    let value = json!({"candidates":[{"content":{"parts":[{"text":"",
        "inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]},"finishReason":"STOP"}]});
    let mut decoder = GeminiStreamDecoder::new();
    let mut encoder = GeminiStreamEncoder::new("image-model");
    let wire: String = decoder
        .decode(&value)
        .iter()
        .filter_map(|e| encoder.encode(e))
        .collect();
    assert!(wire.contains("aGVsbG8="), "{wire}");
}

#[test]
fn image_is_visible_answer_but_draft_image_is_not() {
    use magpie_core::ir::Part;
    use magpie_core::stream::StreamEvent;
    let image = StreamEvent::Image {
        image: Box::new(Part::image("image/png", "aGVsbG8=")),
    };
    assert!(image.is_answer() && image.is_content());
    let reference = StreamEvent::Image {
        image: Box::new(Part::image_url("https://example.invalid/final.png")),
    };
    assert!(reference.is_answer() && reference.is_content());
    let empty = StreamEvent::Image {
        image: Box::new(Part::image("image/png", "")),
    };
    assert!(!empty.is_answer() && !empty.is_content());
    let reasoning = StreamEvent::ThinkingDelta {
        text: "Let me draw.".into(),
    };
    assert!(!reasoning.is_answer() && reasoning.is_content());
    let draft = json!({"candidates":[{"content":{"parts":[{"thought":true,
        "inlineData":{"mimeType":"image/png","data":"ZHJhZnQ="}}]}}]});
    assert!(
        GeminiStreamDecoder::new()
            .decode(&draft)
            .iter()
            .all(|e| !e.is_content())
    );
}

#[test]
fn image_after_tool_flushes_complete_arguments_before_its_payload() {
    use magpie_core::ir::Part;
    use magpie_core::stream::StreamEvent;
    let mut encoder = GeminiStreamEncoder::new("image-model");
    assert!(
        encoder
            .encode(&StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "draw".into()
            })
            .is_none()
    );
    assert!(
        encoder
            .encode(&StreamEvent::ToolCallDelta {
                index: 0,
                args: "{\"size\":1}".into()
            })
            .is_none()
    );
    let wire = encoder
        .encode(&StreamEvent::Image {
            image: Box::new(Part::image("image/png", "aGVsbG8=")),
        })
        .unwrap();
    let chunks = frames(&wire);
    assert_eq!(chunks.len(), 2);
    assert_eq!(
        chunks[0]["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
        json!({"size":1})
    );
    assert_eq!(
        chunks[1]["candidates"][0]["content"]["parts"][0]["inlineData"]["data"],
        "aGVsbG8="
    );
}
