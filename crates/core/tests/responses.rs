//! OpenAI Responses conformance tests: request and reply translation, plus
//! the SSE event shapes Codex expects.

use magpie_core::chat;
use magpie_core::ir::StopReason;
use magpie_core::responses::ResponsesStreamEncoder;
use magpie_core::stream::{SseParser, StreamEvent};
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
