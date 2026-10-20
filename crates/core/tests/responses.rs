//! OpenAI Responses conformance tests: request and reply translation, plus
//! the SSE event shapes Codex expects.

use magpie_core::ir::StopReason;
use magpie_core::responses::{ResponsesStreamDecoder, ResponsesStreamEncoder};
use magpie_core::stream::{SseParser, StreamEvent};
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

fn responses_request() -> Value {
    json!({
        "model": "mock/mock-model",
        "instructions": "You are terse.",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Weather in Beijing?"}
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"Beijing\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "Sunny, 23C"}
        ],
        "tools": [{
            "type": "function",
            "name": "weather",
            "description": "Look up weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }],
        "max_output_tokens": 512,
        "stream": false
    })
}

#[test]
fn responses_request_round_trips_tool_history() {
    let ir = magpie_core::responses::decode_request(&responses_request()).expect("decode");
    assert_eq!(ir.system.len(), 1);
    assert_eq!(ir.messages.len(), 3);
    assert_eq!(
        ir.messages[1].parts[0].kind(),
        magpie_core::ir::PartKind::ToolCall
    );
    assert_eq!(ir.messages[2].parts[0].call_id, "call_1");
    assert_eq!(ir.tools[0].name, "weather");

    let encoded = magpie_core::responses::encode_request(&ir, "mock-model").unwrap();
    assert_eq!(encoded["model"], "mock-model");
    assert_eq!(encoded["instructions"], "You are terse.");
    let input = encoded["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["arguments"], r#"{"city":"Beijing"}"#);
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], "Sunny, 23C");
    assert_eq!(encoded["tools"][0]["name"], "weather");
    assert_eq!(encoded["tools"][0]["parameters"]["type"], "object");
}

#[test]
fn chat_response_encodes_to_responses() {
    let ir = chat::decode_response(&json!({
        "id": "x", "object": "chat.completion", "model": "mock-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "It is sunny.",
                "tool_calls": [{
                    "id": "call_2", "type": "function",
                    "function": {"name": "print", "arguments": "{\"text\":\"hi\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    }))
    .expect("decode");
    assert_eq!(ir.stop, StopReason::ToolUse);

    let out = magpie_core::responses::encode_response(&ir);
    assert_eq!(out["object"], "response");
    assert_eq!(out["status"], "completed");
    let output = out["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "It is sunny.");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "call_2");
    assert_eq!(output[1]["arguments"], r#"{"text":"hi"}"#);
    assert_eq!(out["usage"]["input_tokens"], 11);
    assert_eq!(out["usage"]["output_tokens"], 7);
}

#[test]
fn chat_stream_translates_to_responses_events() {
    let chunks = vec![
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[{"index":0, "delta":{"role":"assistant"}, "finish_reason":null}]}),
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[{"index":0, "delta":{"content":"Ni"}, "finish_reason":null}]}),
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[{"index":0, "delta":{"content":"hao"}, "finish_reason":null}]}),
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[{"index":0, "delta":{"tool_calls":[{"index":0, "id":"call_7", "type":"function", "function":{"name":"bash", "arguments":""}}]}, "finish_reason":null}]}),
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[{"index":0, "delta":{"tool_calls":[{"index":0, "function":{"arguments":"{\"cmd\":\"pwd\"}"}}]}, "finish_reason":null}]}),
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[{"index":0, "delta":{}, "finish_reason":"tool_calls"}]}),
        json!({"id":"c", "object":"chat.completion.chunk", "model":"mock-model", "choices":[], "usage":{"prompt_tokens":5, "completion_tokens":3, "total_tokens":8}}),
    ];

    let mut encoder = ResponsesStreamEncoder::new("mock-model");
    let mut wire = String::new();
    for chunk in chunks {
        for event in chat::decode_chunk(&chunk) {
            if let Some(frame) = encoder.encode(&event) {
                wire.push_str(&frame);
            }
        }
    }
    if let Some(frame) = encoder.encode(&StreamEvent::Done) {
        wire.push_str(&frame);
    }

    let mut parser = SseParser::new();
    let mut remaining = wire.as_bytes();
    let mut events = Vec::new();
    while let Some(data) = parser.feed(&mut remaining).unwrap() {
        events.push(serde_json::from_str::<Value>(&data).expect("event JSON"));
    }

    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[0]["response"]["model"], "mock-model");
    assert!(
        events.iter().any(|e| {
            e["type"] == "response.output_item.added" && e["item"]["type"] == "message"
        })
    );
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect();
    assert_eq!(text, "Nihao");
    let args: String = events
        .iter()
        .filter(|e| e["type"] == "response.function_call_arguments.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect();
    assert_eq!(args, r#"{"cmd":"pwd"}"#);
    assert!(events.iter().any(|e| {
        e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call"
    }));

    let completed = events
        .iter()
        .rev()
        .find(|e| e["type"] == "response.completed")
        .expect("terminal event");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 5);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 3);
    assert!(
        completed["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "call_7")
    );
}

#[test]
fn responses_stream_namespaces_round_trip_and_flatten_foreign_dialects() {
    let added = json!({
        "type": "response.output_item.added",
        "output_index": 2,
        "item": {
            "type": "function_call",
            "call_id": "call_ns",
            "name": "spawn_agent",
            "namespace": "collaboration",
            "arguments": ""
        }
    });
    assert_eq!(
        responses::decode_chunk(&added),
        [StreamEvent::ToolCallStart {
            index: 2,
            id: "call_ns".into(),
            name: "collaboration__spawn_agent".into(),
            namespace: Some("collaboration".into()),
        }]
    );

    let done = json!({
        "type": "response.output_item.done",
        "output_index": 2,
        "item": {
            "type": "function_call",
            "call_id": "call_ns",
            "name": "spawn_agent",
            "namespace": "collaboration",
            "arguments": "{\"message\":\"hi\"}"
        }
    });
    assert_eq!(
        responses::decode_chunk(&done),
        [StreamEvent::ToolCallEnd { index: 2 }]
    );

    let start = StreamEvent::ToolCallStart {
        index: 0,
        id: "call_ns".into(),
        name: "collaboration__spawn_agent".into(),
        namespace: Some("collaboration".into()),
    };
    let mut encoder = ResponsesStreamEncoder::new("mock-model");
    let mut wire = String::new();
    for event in [
        start.clone(),
        StreamEvent::ToolCallDelta {
            index: 0,
            args: "{\"message\":".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            args: "\"hi\"}".into(),
        },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Stop(StopReason::ToolUse),
        StreamEvent::Done,
    ] {
        if let Some(frame) = encoder.encode(&event) {
            wire.push_str(&frame);
        }
    }
    let mut parser = SseParser::new();
    let mut remaining = wire.as_bytes();
    let mut events = Vec::new();
    while let Some(data) = parser.feed(&mut remaining).unwrap() {
        events.push(serde_json::from_str::<Value>(&data).unwrap());
    }
    let added_item = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .unwrap();
    assert_eq!(added_item["item"]["name"], "spawn_agent");
    assert_eq!(added_item["item"]["namespace"], "collaboration");
    let done_item = events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call"
        })
        .unwrap();
    assert_eq!(done_item["item"]["name"], "spawn_agent");
    assert_eq!(done_item["item"]["namespace"], "collaboration");
    assert_eq!(done_item["item"]["arguments"], r#"{"message":"hi"}"#);
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output[0]["name"], "spawn_agent");
    assert_eq!(output[0]["namespace"], "collaboration");

    let foreign = StreamEvent::ToolCallStart {
        index: 0,
        id: "call_ns".into(),
        name: "collaboration__spawn_agent".into(),
        namespace: Some("collaboration".into()),
    };
    let mut chat_encoder = chat::ChatChunkEncoder::new("mock-model", false);
    let chat_wire = chat_encoder.encode(&foreign).unwrap();
    assert!(chat_wire.contains("collaboration__spawn_agent"));
    assert!(!chat_wire.contains("\"namespace\""));

    let mut anthropic_encoder = anthropic::AnthropicStreamEncoder::new("mock-model");
    let anthropic_wire = anthropic_encoder.encode(&foreign).unwrap();
    assert!(anthropic_wire.contains("collaboration__spawn_agent"));
    assert!(!anthropic_wire.contains("\"namespace\""));

    let mut gemini_encoder = gemini::GeminiStreamEncoder::new("mock-model");
    assert!(gemini_encoder.encode(&foreign).is_none());
    let gemini_wire = gemini_encoder
        .encode(&StreamEvent::ToolCallEnd { index: 0 })
        .unwrap();
    assert!(gemini_wire.contains("collaboration__spawn_agent"));
    assert!(!gemini_wire.contains("\"namespace\""));
}

#[test]
fn responses_stream_namespace_errors_are_fail_closed() {
    for namespace in [json!(""), json!(42), json!(null), json!("bad__name")] {
        let event = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "private",
                "name": "spawn_agent",
                "namespace": namespace
            }
        });
        let decoded = responses::decode_chunk(&event);
        assert!(
            matches!(decoded.as_slice(), [StreamEvent::Error { .. }]),
            "{event}"
        );
        assert!(
            !decoded
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
        );
    }
}

fn stream_frames(wire: &str) -> Vec<Value> {
    let mut parser = SseParser::new();
    let mut remaining = wire.as_bytes();
    let mut events = Vec::new();
    while let Some(data) = parser.feed(&mut remaining).unwrap() {
        events.push(serde_json::from_str::<Value>(&data).unwrap());
    }
    events
}

#[test]
fn custom_tool_input_streams_through_function_arguments() {
    let patch = "*** Begin Patch\n+\"quoted\" \\ é 😀\n*** End Patch\n";
    let arguments = json!({"input": patch}).to_string();
    let mut mid = arguments.len() / 2;
    while !arguments.is_char_boundary(mid) {
        mid += 1;
    }
    let events = [
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": ""
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": &arguments[..mid]
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": &arguments[mid..]
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "arguments": arguments
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": arguments
            }
        }),
        json!({"type":"response.completed","response":{"model":"m","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
    ];

    let request = responses::decode_request(&json!({
        "model": "m",
        "input": "edit",
        "tools": [{
            "type": "custom",
            "name": "apply_patch",
            "description": "Patch files"
        }]
    }))
    .unwrap();
    let mut decoder = ResponsesStreamDecoder::new();
    let mut encoder = ResponsesStreamEncoder::for_request("m", &request);
    let mut wire = String::new();
    for event in events {
        for decoded in decoder.decode(&event) {
            if let Some(frame) = encoder.encode(&decoded) {
                wire.push_str(&frame);
            }
        }
    }
    wire.push_str(&encoder.encode(&StreamEvent::Done).unwrap_or_default());
    let frames = stream_frames(&wire);

    let deltas: String = frames
        .iter()
        .filter(|frame| frame["type"] == "response.custom_tool_call_input.delta")
        .filter_map(|frame| frame["delta"].as_str())
        .collect();
    assert_eq!(deltas, patch);

    let input_done = frames
        .iter()
        .find(|frame| frame["type"] == "response.custom_tool_call_input.done")
        .expect("custom input done");
    assert_eq!(input_done["input"], patch);
    let item_done = frames
        .iter()
        .find(|frame| {
            frame["type"] == "response.output_item.done"
                && frame["item"]["type"] == "custom_tool_call"
        })
        .expect("custom item done");
    assert_eq!(item_done["item"]["input"], patch);
}

#[test]
fn argument_deltas_that_complete_json_wait_for_done() {
    fn collect(got: &mut Vec<String>, events: Vec<StreamEvent>) {
        got.extend(events.into_iter().filter_map(|event| match event {
            StreamEvent::ToolCallDelta { args, .. } => Some(args),
            _ => None,
        }));
    }
    let mut decoder = ResponsesStreamDecoder::new();
    let mut got = Vec::new();
    collect(
        &mut got,
        decoder.decode(&json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","call_id":"c1","name":"f"}
        })),
    );
    for delta in [r#"{"cmd":"a}"#, r#"\"]{","#, r#""n":[1,{}]"#] {
        collect(
            &mut got,
            decoder.decode(&json!({
                "type":"response.function_call_arguments.delta",
                "delta":delta
            })),
        );
    }
    assert_eq!(got.concat(), r#"{"cmd":"a}\"]{","n":[1,{}]"#);
    collect(
        &mut got,
        decoder.decode(&json!({
            "type":"response.function_call_arguments.delta",
            "delta":"}"
        })),
    );
    assert_eq!(got.concat(), r#"{"cmd":"a}\"]{","n":[1,{}]"#);
    collect(
        &mut got,
        decoder.decode(&json!({
            "type":"response.output_item.done",
            "item":{
                "type":"function_call",
                "call_id":"c1",
                "name":"f",
                "arguments":"{\"cmd\":\"a}\\\"]{\",\"n\":[1,{}],\"more\":true}"
            }
        })),
    );
    assert_eq!(got.concat(), r#"{"cmd":"a}\"]{","n":[1,{}],"more":true}"#);
}

#[test]
fn native_custom_input_events_are_translated_and_streamed() {
    let request = responses::decode_request(&json!({
        "model":"m",
        "input":"edit",
        "tools":[{"type":"custom","name":"apply_patch"}]
    }))
    .unwrap();
    let patch = "*** Begin Patch\n+é 😀\n*** End Patch\n";
    let (a, b) = patch.split_at(12);
    let events = [
        json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"apply_patch","input":""}
        }),
        json!({
            "type":"response.custom_tool_call_input.delta",
            "output_index":0,
            "item_id":"ctc_1",
            "delta":a
        }),
        json!({
            "type":"response.custom_tool_call_input.delta",
            "output_index":0,
            "item_id":"ctc_1",
            "delta":b
        }),
        json!({
            "type":"response.custom_tool_call_input.done",
            "output_index":0,
            "item_id":"ctc_1",
            "input":patch
        }),
        json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"apply_patch","input":patch}
        }),
    ];
    let mut decoder = ResponsesStreamDecoder::new();
    let mut encoder = ResponsesStreamEncoder::for_request("m", &request);
    let mut wire = String::new();
    for event in events {
        for decoded in decoder.decode(&event) {
            if let Some(frame) = encoder.encode(&decoded) {
                wire.push_str(&frame);
            }
        }
    }
    let frames = stream_frames(&wire);
    let deltas: String = frames
        .iter()
        .filter(|frame| frame["type"] == "response.custom_tool_call_input.delta")
        .filter_map(|frame| frame["delta"].as_str())
        .collect();
    assert_eq!(deltas, patch);
    assert!(frames.iter().any(
        |frame| frame["type"] == "response.custom_tool_call_input.done" && frame["input"] == patch
    ));
}

#[test]
fn file_id_only_input_image_is_refused_not_silently_dropped() {
    // A vendor-hosted reference names an image only its own Responses API
    // can open; the reference answers 400 on a cross-protocol route, and
    // lite cannot carry it on a native route either.
    let body = json!({"model":"m","input":[
        {"role":"user","content":[
            {"type":"input_text","text":"read"},
            {"type":"input_image","file_id":"file_private"}
        ]}
    ]});
    let error = responses::decode_request(&body).expect_err("file_id image must not decode");
    assert!(error.to_string().contains("image_url"), "{error:?}");
    // Portable images still decode.
    for image in [
        json!({"type":"input_image","image_url":"https://example.invalid/shot.jpg"}),
        json!({"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}),
    ] {
        let body = json!({"model":"m","input":[{"role":"user","content":[{"type":"input_text","text":"read"}, image]}]});
        let ir = responses::decode_request(&body).expect("portable image decodes");
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].parts.len(), 2);
    }
}
