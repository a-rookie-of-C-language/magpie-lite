//! Native signatures stay attached to their own blocks and protocol.
use magpie_core::stream::StreamEvent;
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

const A: &str = "PRIVATE_ANTHROPIC_SIGNATURE";
const G: &str = "PRIVATE_GEMINI_SIGNATURE";
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
fn encode_g(events: &[StreamEvent]) -> Vec<Value> {
    let mut e = gemini::GeminiStreamEncoder::new("m");
    frames(
        &events
            .iter()
            .filter_map(|v| e.encode(v))
            .collect::<String>(),
    )
}
fn gemini_parts(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|v| {
            v.pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
        })
        .flatten()
        .cloned()
        .collect()
}
fn anthropic_events() -> Vec<StreamEvent> {
    let mut decoder = anthropic::AnthropicStreamDecoder::new();
    let mut values = vec![json!({"type":"message_start","message":{"model":"m"}})];
    // Signed-only first block, then adjacent thinking blocks with initial and delta signatures.
    for (index, text, sig) in [(0, "", A), (1, "first", "initial-"), (2, "second", "")] {
        values.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":text,"signature":sig}}));
        if index == 1 {
            for fragment in ["fragment-", A] {
                values.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":fragment}}));
            }
        }
        if index == 2 {
            values.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":A}}));
        }
        values.push(json!({"type":"content_block_stop","index":index}));
    }
    values.extend([json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_stop","index":3}),json!({"type":"message_stop"})]);
    values.iter().flat_map(|v| decoder.decode(v)).collect()
}
#[test]
fn anthropic_signed_only_and_adjacent_blocks_keep_initial_and_fragmented_signatures() {
    let output = encode_a(&anthropic_events());
    let starts: Vec<_> = output
        .iter()
        .filter(|v| v["type"] == "content_block_start")
        .collect();
    assert_eq!(starts.len(), 4, "{output:?}");
    for (index, expected) in [A.to_owned(), format!("initial-fragment-{A}"), A.to_owned()]
        .iter()
        .enumerate()
    {
        assert_eq!(starts[index]["content_block"]["type"], "thinking");
        let signature: String = output
            .iter()
            .filter(|v| v["index"] == index && v["delta"]["type"] == "signature_delta")
            .filter_map(|v| v["delta"]["signature"].as_str())
            .collect();
        assert_eq!(&signature, expected);
        assert_eq!(
            output
                .iter()
                .filter(|v| v["type"] == "content_block_stop" && v["index"] == index)
                .count(),
            1
        );
    }
    let thinking: Vec<String> = (0..3)
        .map(|index| {
            output
                .iter()
                .filter(|v| v["index"] == index)
                .filter_map(|v| v["delta"]["thinking"].as_str())
                .collect()
        })
        .collect();
    assert_eq!(thinking, ["", "first", "second"]);
}
fn signed_gemini() -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[
        {"text":"thought","thought":true,"thoughtSignature":G},
        {"text":"answer","thought_signature":G},
        {"text":"","thoughtSignature":G},
        {"functionCall":{"id":"call_one","name":"one","args":{"x":1}},"thoughtSignature":G},
        {"function_call":{"id":"call_two","name":"two","args":{"x":2}},"thought_signature":"second-tool-signature"}
    ]},"finishReason":"STOP"}],"modelVersion":"m"})
}
#[test]
fn gemini_sse_preserves_thought_text_signature_only_and_each_tool_signature() {
    let mut decoder = gemini::GeminiStreamDecoder::new();
    let events = decoder.decode(&signed_gemini());
    let parts = gemini_parts(&encode_g(&events));
    let calls: Vec<_> = parts
        .iter()
        .filter(|p| p.get("functionCall").is_some())
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["thoughtSignature"], G);
    assert_eq!(calls[1]["thoughtSignature"], "second-tool-signature");
    let thought_sigs: Vec<_> = parts
        .iter()
        .filter(|p| p["thought"] == true)
        .filter_map(|p| p["thoughtSignature"].as_str())
        .collect();
    assert_eq!(thought_sigs, [G]);
    let text_sigs: Vec<_> = parts
        .iter()
        .filter(|p| p["thought"] != true && p.get("text").is_some())
        .filter_map(|p| p["thoughtSignature"].as_str())
        .collect();
    assert_eq!(text_sigs, [G, G]);
    assert_eq!(
        parts
            .iter()
            .filter(|p| p["thought"] != true)
            .filter_map(|p| p["text"].as_str())
            .collect::<String>(),
        "answer"
    );
}
#[test]
fn gemini_native_json_and_history_keep_all_signature_kinds() {
    let reply = gemini::decode_response(&signed_gemini()).unwrap();
    let encoded = gemini::encode_response(&reply);
    let parts = encoded["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap();
    assert_eq!(parts.len(), 5);
    for p in parts.iter().take(4) {
        assert_eq!(p["thoughtSignature"], G);
    }
    let request = gemini::decode_request(
        &json!({"model":"m", "contents":[signed_gemini()["candidates"][0]["content"].clone()]}),
    )
    .unwrap();
    let history = gemini::encode_request(&request, "m").unwrap();
    // The decoded history opens with the model's turn, so a continuing
    // user turn goes before it; the signatures ride the model's own turn.
    assert_eq!(
        history["contents"][0]["parts"][0]["text"],
        "[Continuing from previous AI thoughts...]"
    );
    for p in history["contents"][1]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .take(4)
    {
        assert_eq!(p["thoughtSignature"], G);
    }
}
#[test]
fn signatures_never_translate_into_foreign_protocols() {
    let events = anthropic_events();
    assert!(!format!("{:?}", encode_g(&events)).contains(A));
    let mut decoder = gemini::GeminiStreamDecoder::new();
    let events = decoder.decode(&signed_gemini());
    assert!(!format!("{:?}", encode_a(&events)).contains(G));
    let mut c = chat::ChatChunkEncoder::new("m", true);
    let mut r = responses::ResponsesStreamEncoder::new("m");
    for e in &events {
        assert!(!c.encode(e).unwrap_or_default().contains(G));
        assert!(!r.encode(e).unwrap_or_default().contains(G));
    }
    let req = gemini::decode_request(
        &json!({"model":"m","contents":[signed_gemini()["candidates"][0]["content"].clone()]}),
    )
    .unwrap();
    assert!(
        !anthropic::encode_request(&req, "m")
            .unwrap()
            .to_string()
            .contains(G)
    );
    assert!(
        !anthropic::encode_response(&gemini::decode_response(&signed_gemini()).unwrap())
            .to_string()
            .contains(G)
    );
}

#[test]
fn native_signatures_commit_content_but_are_not_answers() {
    use magpie_core::{ir::SignatureProtocol, stream::SignatureTarget};
    for protocol in [SignatureProtocol::Anthropic, SignatureProtocol::Gemini] {
        for target in [
            SignatureTarget::Thinking,
            SignatureTarget::Text,
            SignatureTarget::Tool { index: 0 },
        ] {
            for signature in ["", "opaque"] {
                let e = StreamEvent::Signature {
                    protocol,
                    target,
                    signature: signature.into(),
                };
                assert_eq!(e.is_content(), !signature.is_empty());
                assert!(!e.is_answer());
            }
        }
    }
    assert!(!StreamEvent::ThinkingStart { id: String::new() }.is_content());
}

#[test]
fn anthropic_stray_closed_and_previous_message_signatures_are_ignored() {
    let mut d = anthropic::AnthropicStreamDecoder::new();
    let signature = |index| json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":A}});
    let start = json!({"type":"content_block_start","index":3,"content_block":{"type":"thinking","thinking":""}});
    assert!(d.decode(&signature(3)).is_empty());
    d.decode(&start);
    assert!(d.decode(&signature(4)).is_empty());
    assert!(
        d.decode(&signature(3))
            .iter()
            .any(|e| matches!(e, StreamEvent::Signature { .. }))
    );
    d.decode(&json!({"type":"content_block_stop","index":3}));
    assert!(d.decode(&signature(3)).is_empty());
    d.decode(&start);
    d.decode(&json!({"type":"message_start","message":{"model":"next"}}));
    assert!(d.decode(&signature(3)).is_empty());
}

#[test]
fn unsigned_thinking_metadata_does_not_fabricate_a_native_block() {
    let events = [
        StreamEvent::ThinkingStart { id: String::new() },
        StreamEvent::TextDelta {
            text: "answer".into(),
        },
        StreamEvent::Done,
    ];
    let output = encode_a(&events);
    let starts: Vec<_> = output
        .iter()
        .filter(|v| v["type"] == "content_block_start")
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["content_block"]["type"], "text");
    assert!(
        !output
            .iter()
            .any(|v| v["delta"]["type"] == "signature_delta")
    );
}

#[test]
fn gemini_fragmented_tool_signatures_only_attach_to_the_matching_call() {
    use magpie_core::{ir::SignatureProtocol, stream::SignatureTarget};
    let sig = |index, signature: &str| StreamEvent::Signature {
        protocol: SignatureProtocol::Gemini,
        target: SignatureTarget::Tool { index },
        signature: signature.into(),
    };
    let events = [
        StreamEvent::ToolCallStart {
            index: 2,
            id: "call_two".into(),
            name: "inspect".into(),
            namespace: None,
        },
        StreamEvent::ToolCallDelta {
            index: 2,
            args: "{}".into(),
        },
        sig(1, "wrong-call"),
        sig(2, "first-"),
        sig(2, "second"),
        StreamEvent::ToolCallEnd { index: 2 },
        sig(2, "closed-call"),
        StreamEvent::Done,
    ];
    let parts = gemini_parts(&encode_g(&events));
    let calls: Vec<_> = parts
        .iter()
        .filter(|p| p.get("functionCall").is_some())
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["thoughtSignature"], "first-second");
}

#[test]
fn anthropic_json_signatures_round_trip_natively_but_not_to_gemini() {
    let content = json!([{"type":"thinking","thinking":"public reasoning","signature":A},{"type":"text","text":"answer"}]);
    let request = anthropic::decode_request(
        &json!({"model":"m","max_tokens":100,"messages":[{"role":"assistant","content":content}]}),
    )
    .unwrap();
    let native = anthropic::encode_request(&request, "m").unwrap();
    assert_eq!(native["messages"][0]["content"][0]["signature"], A);
    assert!(
        !gemini::encode_request(&request, "m")
            .unwrap()
            .to_string()
            .contains(A)
    );
    let reply = anthropic::decode_response(
        &json!({"model":"m","role":"assistant","content":content,"stop_reason":"end_turn"}),
    )
    .unwrap();
    assert_eq!(
        anthropic::encode_response(&reply)["content"][0]["signature"],
        A
    );
    assert!(!gemini::encode_response(&reply).to_string().contains(A));
}
