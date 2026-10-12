//! Native opaque reasoning survives history and replies, never foreign protocols.
use magpie_core::stream::{SseParser, StreamEvent};
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

fn redacted() -> Value {
    json!({"type":"redacted_thinking","data":"PRIVATE_ANTHROPIC_CIPHERTEXT","future":{"nested":[1,true,null]}})
}
fn encrypted(id: &str, summary: &str) -> Value {
    json!({"type":"reasoning","id":id,"status":"completed",
        "summary":if summary.is_empty() {json!([])} else {json!([{"type":"summary_text","text":summary}])},
        "encrypted_content":"PRIVATE_RESPONSES_CIPHERTEXT", "future":{"nested":[2,false,null]}})
}
fn assert_no_ciphertext(v: Value) {
    let wire = v.to_string();
    assert!(!wire.contains("PRIVATE_ANTHROPIC_CIPHERTEXT"), "{wire}");
    assert!(!wire.contains("PRIVATE_RESPONSES_CIPHERTEXT"), "{wire}");
    assert!(!wire.contains("encrypted_content"), "{wire}");
    assert!(!wire.contains("redacted_thinking"), "{wire}");
}
fn frames(wire: &str) -> Vec<Value> {
    // Parse frames independently of the production SSE parser/decoders.
    wire.split(
        "

",
    )
    .filter_map(|frame| {
        frame.lines().find_map(|line| {
            line.strip_prefix("data: ")
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).unwrap())
        })
    })
    .collect()
}
#[test]
fn anthropic_native_history_and_json_reply_keep_whole_redacted_block() {
    let block = redacted();
    let req = anthropic::decode_request(&json!({"model":"m","messages":[
        {"role":"assistant","content":[{"type":"text","text":"before"},block,{"type":"text","text":"after"}]},
        {"role":"user","content":"continue"}]})).unwrap();
    let wire = anthropic::encode_request(&req, "native").unwrap();
    assert_eq!(wire["messages"][0]["content"][1], block);
    for value in [
        chat::encode_request(&req, "x").unwrap(),
        responses::encode_request(&req, "x").unwrap(),
        gemini::encode_request(&req, "x").unwrap(),
    ] {
        assert_no_ciphertext(value);
    }
    let reply = anthropic::decode_response(&json!({"model":"m","content":[block,{"type":"text","text":"answer"}],"stop_reason":"end_turn"})).unwrap();
    assert_eq!(anthropic::encode_response(&reply)["content"][0], block);
    for value in [
        chat::encode_response(&reply),
        responses::encode_response(&reply),
        gemini::encode_response(&reply),
    ] {
        assert_no_ciphertext(value);
    }
}
#[test]
fn responses_native_history_and_json_reply_keep_whole_encrypted_items() {
    let blocks = vec![
        encrypted("rs_empty", ""),
        encrypted("rs_summary", "public summary"),
    ];
    let req = responses::decode_request(&json!({"model":"m","input":blocks})).unwrap();
    assert_eq!(
        responses::encode_request(&req, "native").unwrap()["input"],
        json!(blocks)
    );
    for value in [
        chat::encode_request(&req, "x").unwrap(),
        anthropic::encode_request(&req, "x").unwrap(),
        gemini::encode_request(&req, "x").unwrap(),
    ] {
        assert_no_ciphertext(value);
    }
    let reply =
        responses::decode_response(&json!({"model":"m","status":"completed","output":blocks}))
            .unwrap();
    assert_eq!(responses::encode_response(&reply)["output"], json!(blocks));
    let foreign = anthropic::encode_response(&reply);
    assert_eq!(foreign["content"][1]["thinking"], "public summary");
    for value in [
        chat::encode_response(&reply),
        foreign,
        gemini::encode_response(&reply),
    ] {
        assert_no_ciphertext(value);
    }
}
fn anthropic_events() -> Vec<StreamEvent> {
    let mut decoder = anthropic::AnthropicStreamDecoder::default();
    [json!({"type":"message_start","message":{"model":"m"}}),
     json!({"type":"content_block_start","index":0,"content_block":redacted()}),
     json!({"type":"content_block_stop","index":0}),
     json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
     json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
     json!({"type":"content_block_stop","index":1}),
     json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),json!({"type":"message_stop"})]
     .iter().flat_map(|v| decoder.decode(v)).collect()
}
fn responses_events() -> Vec<StreamEvent> {
    let mut input = vec![json!({"type":"response.created","response":{"model":"m"}})];
    for (index, (id, summary)) in [
        ("rs_empty", ""),
        ("rs_first", "first"),
        ("rs_second", "second"),
    ]
    .iter()
    .enumerate()
    {
        input.push(json!({"type":"response.output_item.added","output_index":index,"item":{"type":"reasoning","id":id,"summary":[]}}));
        if !summary.is_empty() {
            input.push(json!({"type":"response.reasoning_summary_text.delta","item_id":id,"output_index":index,"delta":summary}));
        }
        input.push(json!({"type":"response.output_item.done","output_index":index,"item":encrypted(id,summary)}));
    }
    input.push(json!({"type":"response.completed","response":{"status":"completed","output":[]}}));
    input.iter().flat_map(responses::decode_chunk).collect()
}
#[test]
fn anthropic_sse_preserves_redacted_block_in_place_and_closes_once() {
    let mut encoder = anthropic::AnthropicStreamEncoder::new("m");
    let wire: String = anthropic_events()
        .iter()
        .filter_map(|e| encoder.encode(e))
        .collect();
    let events = frames(&wire);
    let starts: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0]["content_block"], redacted());
    assert_eq!(starts[1]["content_block"]["type"], "text");
    for index in 0..2 {
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "content_block_stop" && e["index"] == index)
                .count(),
            1
        );
    }
}
#[test]
fn responses_sse_native_ids_and_whole_items_are_not_duplicated() {
    let mut encoder = responses::ResponsesStreamEncoder::new("m");
    let wire: String = responses_events()
        .iter()
        .filter_map(|e| encoder.encode(e))
        .collect();
    let events = frames(&wire);
    let expected = json!([
        encrypted("rs_empty", ""),
        encrypted("rs_first", "first"),
        encrypted("rs_second", "second")
    ]);
    let done: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| e["item"].clone())
        .collect();
    assert_eq!(json!(done), expected);
    let added: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.added")
        .map(|e| e["item"]["id"].clone())
        .collect();
    assert_eq!(
        added,
        json!(["rs_empty", "rs_first", "rs_second"])
            .as_array()
            .unwrap()
            .clone()
    );
    let deltas: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "response.reasoning_summary_text.delta")
        .map(|e| (e["item_id"].clone(), e["delta"].clone()))
        .collect();
    assert_eq!(
        deltas,
        vec![
            (json!("rs_first"), json!("first")),
            (json!("rs_second"), json!("second"))
        ]
    );
    assert_eq!(events.last().unwrap()["response"]["output"], expected);
    // Generated SSE still parses incrementally, including tiny transport chunks.
    let mut parser = SseParser::new();
    let mut count = 0;
    for byte in wire.as_bytes() {
        let mut chunk = std::slice::from_ref(byte);
        while parser.feed(&mut chunk).unwrap().is_some() {
            count += 1;
        }
    }
    assert_eq!(count, events.len());
}
#[test]
fn sse_never_forwards_opaque_reasoning_to_foreign_clients() {
    for events in [anthropic_events(), responses_events()] {
        let mut a = anthropic::AnthropicStreamEncoder::new("m");
        let mut r = responses::ResponsesStreamEncoder::new("m");
        let mut c = chat::ChatChunkEncoder::new("m", true);
        let mut g = gemini::GeminiStreamEncoder::new("m");
        let wires: [String; 4] = [
            events.iter().filter_map(|e| a.encode(e)).collect(),
            events.iter().filter_map(|e| r.encode(e)).collect(),
            events.iter().filter_map(|e| c.encode(e)).collect(),
            events.iter().filter_map(|e| g.encode(e)).collect(),
        ];
        for (index, wire) in wires.iter().enumerate() {
            if index != 0 {
                assert!(!wire.contains("PRIVATE_ANTHROPIC_CIPHERTEXT"));
            }
            if index != 1 {
                assert!(!wire.contains("PRIVATE_RESPONSES_CIPHERTEXT"));
            }
        }
    }
}

#[test]
fn sealed_shapes_are_validated_and_metadata_is_not_first_content() {
    use magpie_core::ir::{ReasoningProtocol as P, SealedReasoning};
    for value in [
        Value::Null,
        json!("not a block"),
        json!({"type":"text","data":"secret"}),
        json!({"type":"redacted_thinking","data":5}),
    ] {
        assert!(SealedReasoning::decode(P::Anthropic, &value).is_none());
    }
    for value in [
        json!({"type":"reasoning"}),
        json!({"type":"reasoning","encrypted_content":""}),
        json!({"type":"reasoning","encrypted_content":{}}),
        redacted(),
    ] {
        assert!(SealedReasoning::decode(P::Responses, &value).is_none());
    }
    assert!(
        SealedReasoning::decode(P::Anthropic, &json!({"type":"redacted_thinking","data":""}))
            .is_some()
    );
    let seal = SealedReasoning::decode(P::Responses, &encrypted("rs_a", "")).unwrap();
    assert!(seal.native(P::Anthropic).is_none());
    let event = StreamEvent::Sealed {
        reasoning: Box::new(seal),
    };
    assert!(event.is_content());
    assert!(!event.is_answer());
    let start = StreamEvent::ThinkingStart { id: "rs_a".into() };
    assert!(!start.is_content());
    assert!(!start.is_answer());
}
#[test]
fn responses_plain_reasoning_boundaries_keep_ids_without_copying_them_abroad() {
    let mut encoder = responses::ResponsesStreamEncoder::new("m");
    let mut wire = String::new();
    for (index, id) in ["rs_plain_a", "rs_plain_b"].iter().enumerate() {
        for input in [
            json!({"type":"response.output_item.added","output_index":index,"item":{"type":"reasoning","id":id}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":index,"delta":id}),
            json!({"type":"response.output_item.done","output_index":index,"item":{"type":"reasoning","id":id,"summary":[]}}),
        ] {
            for event in responses::decode_chunk(&input) {
                if let Some(frame) = encoder.encode(&event) {
                    wire.push_str(&frame);
                }
            }
        }
    }
    wire.push_str(&encoder.encode(&StreamEvent::Done).unwrap());
    let events = frames(&wire);
    let output = events.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["id"], "rs_plain_a");
    assert_eq!(output[1]["id"], "rs_plain_b");
    assert_eq!(output[0]["summary"][0]["text"], "rs_plain_a");
    assert_eq!(output[1]["summary"][0]["text"], "rs_plain_b");
}
#[test]
fn responses_sealed_item_without_id_finishes_existing_summary_only_once() {
    let mut encoder = responses::ResponsesStreamEncoder::new("m");
    let mut item = encrypted("rs_unused", "summary");
    item.as_object_mut().unwrap().remove("id");
    let mut wire = encoder
        .encode(&StreamEvent::ThinkingDelta {
            text: "summary".into(),
        })
        .unwrap();
    for ev in responses::decode_chunk(&json!({"type":"response.output_item.done","item":item})) {
        wire.push_str(&encoder.encode(&ev).unwrap());
    }
    wire.push_str(&encoder.encode(&StreamEvent::Done).unwrap());
    let events = frames(&wire);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "response.output_item.added")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "response.output_item.done")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap()["response"]["output"], json!([item]));
}
#[test]
fn responses_sealed_done_with_different_id_never_overwrites_another_item() {
    let mut encoder = responses::ResponsesStreamEncoder::new("m");
    let events = [
        StreamEvent::ThinkingStart {
            id: "rs_public".into(),
        },
        StreamEvent::ThinkingDelta {
            text: "public".into(),
        },
        StreamEvent::Sealed {
            reasoning: Box::new(
                magpie_core::ir::SealedReasoning::decode(
                    magpie_core::ir::ReasoningProtocol::Responses,
                    &encrypted("rs_other", ""),
                )
                .unwrap(),
            ),
        },
        StreamEvent::Done,
    ];
    let wire: String = events.iter().filter_map(|e| encoder.encode(e)).collect();
    let frames = frames(&wire);
    let output = frames.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["id"], "rs_public");
    assert_eq!(output[0]["summary"][0]["text"], "public");
    assert_eq!(output[1], encrypted("rs_other", ""));
}

#[test]
fn unknown_native_summary_blocks_are_retained_but_never_become_public_reasoning() {
    let mut item = encrypted("rs_unknown", "public");
    item["summary"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"future_private","text":"PRIVATE_UNKNOWN_SUMMARY","payload":{"x":1}}));
    let req = responses::decode_request(&json!({"model":"m","input":[item]})).unwrap();
    assert_eq!(
        responses::encode_request(&req, "m").unwrap()["input"][0],
        item
    );
    for value in [
        chat::encode_request(&req, "m").unwrap(),
        anthropic::encode_request(&req, "m").unwrap(),
        gemini::encode_request(&req, "m").unwrap(),
    ] {
        assert!(!value.to_string().contains("PRIVATE_UNKNOWN_SUMMARY"));
    }
    let response = responses::decode_response(&json!({"model":"m","output":[item]})).unwrap();
    assert_eq!(responses::encode_response(&response)["output"][0], item);
    let value = anthropic::encode_response(&response);
    assert_eq!(value["content"][0]["thinking"], "public");
    assert!(!value.to_string().contains("PRIVATE_UNKNOWN_SUMMARY"));
}
#[test]
fn anthropic_repeated_redacted_blocks_close_surrounding_prose_in_order() {
    use magpie_core::ir::{ReasoningProtocol, SealedReasoning};
    let sealed = StreamEvent::Sealed {
        reasoning: Box::new(
            SealedReasoning::decode(ReasoningProtocol::Anthropic, &redacted()).unwrap(),
        ),
    };
    let events = [
        StreamEvent::TextDelta {
            text: "before".into(),
        },
        sealed.clone(),
        StreamEvent::ThinkingDelta {
            text: "middle".into(),
        },
        sealed,
        StreamEvent::TextDelta {
            text: "after".into(),
        },
        StreamEvent::Done,
    ];
    let mut encoder = anthropic::AnthropicStreamEncoder::new("m");
    let wire: String = events.iter().filter_map(|e| encoder.encode(e)).collect();
    let frames = frames(&wire);
    let blocks: Vec<_> = frames
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .collect();
    assert_eq!(blocks.len(), 5);
    let kinds: Vec<_> = blocks
        .iter()
        .map(|e| e["content_block"]["type"].clone())
        .collect();
    assert_eq!(
        kinds,
        vec![
            json!("text"),
            json!("redacted_thinking"),
            json!("thinking"),
            json!("redacted_thinking"),
            json!("text")
        ]
    );
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(block["index"], index);
        assert_eq!(
            frames
                .iter()
                .filter(|e| e["type"] == "content_block_stop" && e["index"] == index)
                .count(),
            1
        );
    }
}
