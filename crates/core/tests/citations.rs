//! Citations and annotations survive decoding, stay in the IR, and translate
//! between Anthropic's `citations` and Responses' `url_citation` annotations.
use magpie_core::stream::StreamEvent;
use magpie_core::{anthropic, responses};
use serde_json::{Value, json};

fn citation() -> Value {
    json!({
        "type": "char_location",
        "cited_text": "magpie is a gateway",
        "document_index": 0,
        "document_title": "magpie",
        "start_char_index": 8,
        "end_char_index": 27,
    })
}
fn frames(wire: &str) -> Vec<Value> {
    wire.split("\n\n")
        .filter_map(|f| {
            f.lines().find_map(|l| {
                l.strip_prefix("data: ")
                    .filter(|d| *d != "[DONE]")
                    .map(|d| serde_json::from_str(d).unwrap())
            })
        })
        .collect()
}
fn encode_a(events: &[StreamEvent]) -> Vec<Value> {
    let mut e = anthropic::AnthropicStreamEncoder::new("m");
    frames(
        &events
            .iter()
            .filter_map(|v| e.encode(v))
            .collect::<String>(),
    )
}
fn encode_r(events: &[StreamEvent]) -> Vec<Value> {
    let mut e = responses::ResponsesStreamEncoder::new("m");
    frames(
        &events
            .iter()
            .filter_map(|v| e.encode(v))
            .collect::<String>(),
    )
}
fn annotation_events() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Start { model: "m".into() },
        StreamEvent::TextDelta {
            text: "answer".into(),
        },
        StreamEvent::Citation {
            citation: citation(),
        },
        StreamEvent::Stop(magpie_core::ir::StopReason::EndTurn),
        StreamEvent::Done,
    ]
}

#[test]
fn anthropic_whole_and_stream_citations_round_trip() {
    let reply = anthropic::decode_response(&json!({
        "id": "m", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "answer", "citations": [citation()]}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }))
    .unwrap();
    assert_eq!(reply.message.parts[0].citations, vec![citation()]);
    let block = &anthropic::encode_response(&reply)["content"][0];
    assert_eq!(block["citations"], json!([citation()]));

    let mut decoder = anthropic::AnthropicStreamDecoder::new();
    let events = [
        json!({"type":"message_start","message":{"model":"m"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":citation()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .flat_map(|v| decoder.decode(v))
    .collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Citation { citation: c } if *c == citation()))
    );
    let output = encode_a(&events);
    let deltas: Vec<_> = output
        .iter()
        .filter(|v| v["delta"]["type"] == "citations_delta")
        .collect();
    assert_eq!(deltas.len(), 1, "{output:?}");
    assert_eq!(deltas[0]["delta"]["citation"], citation());
}

#[test]
fn responses_annotations_round_trip_and_stream() {
    let annotation = json!({
        "type": "url_citation",
        "url": "https://example.com/magpie",
        "title": "magpie",
        "start_index": 8,
        "end_index": 27,
    });
    let reply = responses::decode_response(&json!({
        "id": "r", "model": "m", "status": "completed",
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "answer", "annotations": [annotation]},
        ]}],
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }))
    .unwrap();
    assert_eq!(reply.message.parts[0].citations, vec![annotation.clone()]);
    let text = &responses::encode_response(&reply)["output"][0]["content"][0];
    assert_eq!(text["annotations"], json!([annotation]));

    let mut decoder_events = Vec::new();
    decoder_events.push(json!({"type":"response.created","response":{"id":"r"}}));
    decoder_events.push(json!({"type":"response.output_item.added","output_index":0,
        "item":{"id":"m1","type":"message","role":"assistant","status":"in_progress","content":[]}}));
    decoder_events
        .push(json!({"type":"response.output_text.delta","output_index":0,"delta":"answer"}));
    decoder_events.push(
        json!({"type":"response.output_text.annotation.added","output_index":0,
        "annotation_index":0,"annotation":annotation}),
    );
    decoder_events.push(json!({"type":"response.completed","response":{"id":"r","usage":{"input_tokens":1,"output_tokens":1}}}));
    let events = decoder_events
        .iter()
        .flat_map(responses::decode_chunk)
        .collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Citation { citation } if *citation == annotation))
    );

    let output = encode_r(&annotation_events());
    let added: Vec<_> = output
        .iter()
        .filter(|v| v["type"] == "response.output_text.annotation.added")
        .collect();
    assert_eq!(added.len(), 1, "{output:?}");
    assert_eq!(added[0]["annotation"]["type"], "url_citation");
    assert_eq!(added[0]["annotation"]["cited_text"], "magpie is a gateway");
    let done = output
        .iter()
        .find(|v| v["type"] == "response.output_item.done" && v["item"]["type"] == "message")
        .expect("message done");
    let final_annotations = &done["item"]["content"][0]["annotations"];
    assert_eq!(final_annotations.as_array().unwrap().len(), 1);
    assert_eq!(final_annotations[0]["type"], "url_citation");
    assert_eq!(final_annotations[0]["cited_text"], "magpie is a gateway");
}

#[test]
fn citations_translate_across_protocols() {
    // Anthropic source stream -> Responses client: annotation.added events.
    let mut decoder = anthropic::AnthropicStreamDecoder::new();
    let events = [
        json!({"type":"message_start","message":{"model":"m"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":citation()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .flat_map(|v| decoder.decode(v))
    .collect::<Vec<_>>();
    let output = encode_r(&events);
    assert!(
        output
            .iter()
            .any(|v| v["type"] == "response.output_text.annotation.added"
                && v["annotation"]["type"] == "url_citation"
                && v["annotation"]["url"] == "")
    );

    // Responses source stream -> Anthropic client: citations_delta events.
    let annotation = json!({
        "type": "url_citation",
        "url": "https://example.com/magpie",
        "title": "magpie",
        "start_index": 8,
        "end_index": 27,
    });
    let mut events = vec![StreamEvent::Start { model: "m".into() }];
    events.push(StreamEvent::TextDelta {
        text: "answer".into(),
    });
    events.push(StreamEvent::Citation {
        citation: annotation,
    });
    events.push(StreamEvent::Stop(magpie_core::ir::StopReason::EndTurn));
    events.push(StreamEvent::Done);
    let output = encode_a(&events);
    let deltas: Vec<_> = output
        .iter()
        .filter(|v| v["delta"]["type"] == "citations_delta")
        .collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0]["delta"]["citation"]["url"],
        "https://example.com/magpie"
    );
}

#[test]
fn citations_are_not_answers_and_without_text_are_dropped() {
    let event = StreamEvent::Citation {
        citation: citation(),
    };
    assert!(!event.is_answer());
    assert!(!event.is_content());
    // No open text block: the Anthropic encoder says nothing.
    assert!(encode_a(std::slice::from_ref(&event)).is_empty());
    // No open message: the Responses encoder says nothing.
    assert!(encode_r(&[event]).is_empty());
}
