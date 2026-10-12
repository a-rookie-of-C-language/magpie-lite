use magpie_core::{chat, ir::Usage, responses, stream::StreamEvent};
use serde_json::json;

#[test]
fn cumulative_usage_replaces_reported_counters_without_erasing_absent_fields() {
    let mut usage = Usage {
        input: Some(7),
        output: Some(0),
        cache_read: Some(0),
        cache_write: None,
    };
    usage.merge(&Usage {
        input: None,
        output: Some(3),
        cache_read: Some(2),
        cache_write: Some(4),
    });
    assert_eq!(
        usage,
        Usage {
            input: Some(7),
            output: Some(3),
            cache_read: Some(2),
            cache_write: Some(4)
        }
    );
    usage.merge(&Usage::default());
    assert_eq!(usage.input, Some(7));
    assert_eq!(usage.output, Some(3));
    // Explicit zero is known data, not an absent value.
    usage.merge(&Usage {
        output: Some(0),
        ..Usage::default()
    });
    assert_eq!(usage.output, Some(0));
    assert_eq!(usage.cache_read, Some(2));
}

#[test]
fn chat_stream_error_retains_usage_and_does_not_decode_answer_fields() {
    let events = chat::decode_chunk(&json!({
        "error": {"message": "rate limited"},
        "usage": {"prompt_tokens": 7, "completion_tokens": 3},
        "choices": [{"delta": {"content": "must not appear"}, "finish_reason": "stop"}]
    }));
    assert!(
        matches!(&events[0], StreamEvent::Usage(usage) if usage.input == Some(7) && usage.output == Some(3))
    );
    assert_eq!(
        &events[1..],
        &[StreamEvent::Error {
            message: "rate limited".into()
        }]
    );
    assert_eq!(
        chat::decode_chunk(&json!({"error": {}})),
        vec![StreamEvent::Error {
            message: "chat upstream error".into()
        }]
    );
}

#[test]
fn responses_stream_errors_decode_supported_envelopes_without_success() {
    for frame in [
        json!({"error": {"message": "rate limited"}}),
        json!({"type": "error", "message": "rate limited"}),
        json!({"type": "error", "error": {"message": "rate limited"}}),
        json!({"type": "response.failed", "response": {"error": {"message": "rate limited"}}}),
    ] {
        assert_eq!(
            responses::decode_chunk(&frame),
            vec![StreamEvent::Error {
                message: "rate limited".into()
            }]
        );
    }
    let events = responses::decode_chunk(&json!({"type": "response.failed", "response": {
        "error": {"message": "rate limited"}, "usage": {"input_tokens": 7, "output_tokens": 3}
    }}));
    assert!(
        matches!(&events[0], StreamEvent::Usage(usage) if usage.input == Some(7) && usage.output == Some(3))
    );
    assert_eq!(
        &events[1..],
        &[StreamEvent::Error {
            message: "rate limited".into()
        }]
    );
    assert_eq!(
        responses::decode_chunk(&json!({"type": "error"})),
        vec![StreamEvent::Error {
            message: "responses upstream error".into()
        }]
    );
}
