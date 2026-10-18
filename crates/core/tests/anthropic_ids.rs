use magpie_core::anthropic::{self, AnthropicStreamDecoder, AnthropicStreamEncoder};
use magpie_core::chat;
use magpie_core::ir::PartKind;
use magpie_core::stream::{SseParser, StreamEvent};
use serde_json::{Value, json};

const FOREIGN_ID: &str = "Bash:0#a65b6a5e02194b87bbc796c4428c946d";

fn assert_anthropic_id(id: &str) {
    assert!(!id.is_empty());
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "unsafe Anthropic tool id: {id}"
    );
}

#[test]
fn anthropic_request_encodes_and_restores_foreign_tool_ids() {
    let request = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "run both"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": FOREIGN_ID, "type": "function",
                 "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}},
                {"id": "toolu_01AbC", "type": "function",
                 "function": {"name": "shell", "arguments": "{\"cmd\":\"pwd\"}"}},
                {"id": "mp_abc", "type": "function",
                 "function": {"name": "shell", "arguments": "{\"cmd\":\"whoami\"}"}}
            ]},
            {"role": "tool", "tool_call_id": FOREIGN_ID, "content": "main.rs"},
            {"role": "tool", "tool_call_id": "toolu_01AbC", "content": "/src"},
            {"role": "tool", "tool_call_id": "mp_abc", "content": "dev"}
        ]
    });
    let ir = chat::decode_request(&request).expect("decode chat");
    let encoded = anthropic::encode_request(&ir, "m").expect("encode Anthropic");

    let mut uses = Vec::new();
    let mut results = Vec::new();
    for message in encoded["messages"].as_array().unwrap() {
        for block in message["content"].as_array().unwrap() {
            match block["type"].as_str() {
                Some("tool_use") => uses.push(block["id"].as_str().unwrap().to_owned()),
                Some("tool_result") => {
                    results.push(block["tool_use_id"].as_str().unwrap().to_owned());
                }
                _ => {}
            }
        }
    }

    assert_eq!(uses.len(), 3);
    assert_eq!(results.len(), 3);
    for (call, result) in uses.iter().zip(&results) {
        assert_anthropic_id(call);
        assert_eq!(call, result);
    }
    assert_ne!(uses[0], FOREIGN_ID);
    assert_eq!(uses[1], "toolu_01AbC");
    assert_ne!(uses[2], "mp_abc");

    let decoded = anthropic::decode_request(&encoded).expect("decode Anthropic");
    let mut restored_uses = Vec::new();
    let mut restored_results = Vec::new();
    for message in decoded.messages {
        for part in message.parts {
            match part.kind() {
                PartKind::ToolCall => restored_uses.push(part.id),
                PartKind::ToolResult => restored_results.push(part.call_id),
                _ => {}
            }
        }
    }
    assert_eq!(restored_uses, [FOREIGN_ID, "toolu_01AbC", "mp_abc"]);
    assert_eq!(restored_results, [FOREIGN_ID, "toolu_01AbC", "mp_abc"]);
}

#[test]
fn anthropic_response_encodes_and_restores_foreign_tool_ids() {
    let ir = chat::decode_response(&json!({
        "id": "c1",
        "object": "chat.completion",
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": FOREIGN_ID,
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    }))
    .expect("decode chat response");
    let encoded = anthropic::encode_response(&ir);
    let id = encoded["content"][0]["id"].as_str().unwrap();
    assert_anthropic_id(id);
    assert_ne!(id, FOREIGN_ID);

    let decoded = anthropic::decode_response(&encoded).expect("decode Anthropic response");
    assert_eq!(decoded.message.parts[0].id, FOREIGN_ID);

    // An mp_ id that is not the canonical encoding of an id is left alone.
    let malformed = anthropic::decode_response(&json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "m",
        "content": [{
            "type": "tool_use",
            "id": "mp_abc",
            "name": "shell",
            "input": {}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .expect("decode malformed mark");
    assert_eq!(malformed.message.parts[0].id, "mp_abc");
}

#[test]
fn anthropic_stream_uses_one_safe_id_in_both_directions() {
    let mut encoder = AnthropicStreamEncoder::new("m");
    let frame = encoder
        .encode(&StreamEvent::ToolCallStart {
            index: 0,
            id: FOREIGN_ID.into(),
            name: "shell".into(),
            namespace: None,
        })
        .expect("encode tool start");

    let mut parser = SseParser::new();
    let mut remaining = frame.as_bytes();
    let mut start = None;
    while let Some(data) = parser.feed(&mut remaining).expect("parse SSE") {
        let event: Value = serde_json::from_str(&data).expect("event is JSON");
        if event["type"] == "content_block_start" {
            start = Some(event);
        }
    }
    let start = start.expect("tool content block");
    let encoded_id = start["content_block"]["id"].as_str().unwrap();
    assert_anthropic_id(encoded_id);
    assert_ne!(encoded_id, FOREIGN_ID);

    let mut decoder = AnthropicStreamDecoder::new();
    let events = decoder.decode(&start);
    let decoded_id = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::ToolCallStart { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .expect("decoded tool start");
    assert_eq!(decoded_id, FOREIGN_ID);
}
