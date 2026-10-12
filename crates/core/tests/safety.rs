use magpie_core::ir::{CompletionResponse, StopReason};
use magpie_core::stream::StreamEvent;
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

#[test]
fn safety_finishes_remain_distinct_from_length_and_normal_completion() {
    let fixtures = [
        (
            "chat",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":"content_filter"}]}),
        ),
        (
            "anthropic",
            json!({"model":"m","content":[{"type":"text","text":"partial"}],"stop_reason":"refusal"}),
        ),
        (
            "responses",
            json!({"model":"m","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]}),
        ),
        (
            "gemini",
            json!({"modelVersion":"m","candidates":[{"content":{"parts":[]},"finishReason":"SAFETY"}]}),
        ),
    ];
    for (protocol, value) in fixtures {
        let decoded = match protocol {
            "chat" => chat::decode_response(&value),
            "anthropic" => anthropic::decode_response(&value),
            "responses" => responses::decode_response(&value),
            _ => gemini::decode_response(&value),
        }
        .unwrap();
        assert_eq!(decoded.stop.to_chat(), "content_filter", "{protocol}");
        assert_eq!(decoded.stop.to_anthropic(), "refusal", "{protocol}");
        let response = CompletionResponse {
            stop: decoded.stop,
            ..Default::default()
        };
        assert_eq!(
            chat::encode_response(&response)["choices"][0]["finish_reason"],
            "content_filter"
        );
        assert_eq!(
            anthropic::encode_response(&response)["stop_reason"],
            "refusal"
        );
        let encoded = responses::encode_response(&response);
        assert_eq!(encoded["status"], "incomplete");
        assert_eq!(encoded["incomplete_details"]["reason"], "content_filter");
        assert_eq!(
            gemini::encode_response(&response)["candidates"][0]["finishReason"],
            "SAFETY"
        );
    }
    assert_eq!(StopReason::from_chat("length"), StopReason::MaxTokens);
    let length = responses::decode_response(&json!({"model":"m","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]})).unwrap();
    assert_eq!(length.stop, StopReason::MaxTokens);
}

#[test]
fn gemini_prompt_feedback_is_a_terminal_filter_without_candidates_or_model() {
    for body in [
        json!({"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":7}}),
        json!({"prompt_feedback":{"block_reason":"PROHIBITED_CONTENT"},"usage_metadata":{"prompt_token_count":7}}),
    ] {
        let response = gemini::decode_response(&body).unwrap();
        assert_eq!(response.stop.to_chat(), "content_filter");
        assert_eq!(response.usage.input, Some(7));
        let events = gemini::GeminiStreamDecoder::new().decode(&body);
        assert!(events.iter().any(
            |ev| matches!(ev, StreamEvent::Stop(reason) if reason.to_chat() == "content_filter")
        ));
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(ev, StreamEvent::Done))
                .count(),
            1
        );
    }
    // Ordinary missing candidates remain malformed, not a synthesized refusal.
    assert!(gemini::decode_response(&json!({"modelVersion":"m"})).is_err());
}

#[test]
fn responses_stream_filter_has_the_same_terminal_reason_as_nonstream() {
    let value = json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":7,"output_tokens":3}}});
    let events = responses::decode_chunk(&value);
    let reason = events
        .iter()
        .find_map(|ev| match ev {
            StreamEvent::Stop(reason) => Some(*reason),
            _ => None,
        })
        .unwrap();
    assert_eq!(reason.to_chat(), "content_filter");
    let mut encoder = responses::ResponsesStreamEncoder::new("m");
    let mut frames = String::new();
    for ev in [StreamEvent::Stop(reason), StreamEvent::Done] {
        if let Some(frame) = encoder.encode(&ev) {
            frames.push_str(&frame);
        }
    }
    let terminal: Value = frames
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .next_back()
        .unwrap();
    assert_eq!(terminal["type"], "response.incomplete");
    assert_eq!(
        terminal["response"]["incomplete_details"]["reason"],
        "content_filter"
    );
    assert!(!frames.contains("response.completed"));
}

#[test]
fn gemini_feedback_keeps_candidate_content_and_ignores_unspecified_block() {
    for reason in ["SAFETY", "BLOCK_REASON_UNSPECIFIED", ""] {
        let body = json!({"modelVersion":"m", "promptFeedback":{"blockReason":reason}, "candidates":[{"content":{"parts":[{"text":"partial"}]}, "finishReason":"STOP"}]});
        let filtered = reason == "SAFETY";
        let response = gemini::decode_response(&body).unwrap();
        assert_eq!(response.stop == StopReason::ContentFilter, filtered);
        assert_eq!(response.message.parts[0].text, "partial");
        let events = gemini::GeminiStreamDecoder::new().decode(&body);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta { text } if text == "partial"))
        );
        assert!(events.iter().any(|event| matches!(event, StreamEvent::Stop(stop) if (*stop == StopReason::ContentFilter) == filtered)));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Done))
                .count(),
            1
        );
    }
    assert!(
        gemini::decode_response(
            &json!({"promptFeedback":{"blockReason":"BLOCK_REASON_UNSPECIFIED"}})
        )
        .is_err()
    );
}
