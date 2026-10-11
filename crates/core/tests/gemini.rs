//! Gemini generateContent conformance tests: tool history, replies, and
//! both directions of stream translation.

use magpie_core::chat::{self, ChatChunkEncoder};
use magpie_core::gemini::{self, GeminiStreamDecoder, GeminiStreamEncoder};
use magpie_core::ir::StopReason;
use magpie_core::stream::{SseParser, StreamEvent};
use serde_json::{Value, json};

fn gemini_request() -> Value {
    json!({
        "model": "mock/mock-model",
        "systemInstruction": {"parts": [{"text": "You are terse."}]},
        "contents": [
            {"role": "user", "parts": [
                {"text": "Weather in Beijing?"},
                {"inlineData": {"mimeType": "image/png", "data": "aW1n"}}
            ]},
            {"role": "model", "parts": [{
                "functionCall": {
                    "id": "call_1",
                    "name": "weather",
                    "args": {"city": "Beijing"},
                },
                "thoughtSignature": "sig-1"
            }]},
            {"role": "user", "parts": [{
                "functionResponse": {
                    "id": "call_1",
                    "name": "weather",
                    "response": {"output": "Sunny, 23C"},
                    "parts": [{"inlineData": {"mimeType": "image/jpeg", "data": "aW1nMg=="}}],
                }
            }]}
        ],
        "tools": [{"functionDeclarations": [{
            "name": "weather",
            "description": "Look up weather",
            "parameters": {"type": "OBJECT", "properties": {"city": {"type": "STRING"}}},
        }]}],
        "toolConfig": {"functionCallingConfig": {
            "mode": "ANY",
            "allowedFunctionNames": ["weather"],
        }},
        "generationConfig": {
            "maxOutputTokens": 512,
            "temperature": 0.2,
            "topP": 0.9,
            "stopSequences": ["\n"],
        }
    })
}

#[test]
fn gemini_request_decodes_tool_history_and_images() {
    let ir = gemini::decode_request(&gemini_request()).expect("decode");
    assert_eq!(ir.system[0].text, "You are terse.");
    assert_eq!(ir.messages.len(), 3);
    assert_eq!(ir.messages[0].parts[0].text, "Weather in Beijing?");
    assert_eq!(ir.messages[0].parts[1].data, "aW1n");
    assert_eq!(ir.messages[1].parts[0].id, "call_1");
    assert_eq!(ir.messages[1].parts[0].signature, "sig-1");
    assert_eq!(ir.messages[2].parts[0].text, "Sunny, 23C");
    assert_eq!(ir.messages[2].parts[1].media_type, "image/jpeg");
    assert_eq!(ir.tools[0].name, "weather");
    assert_eq!(
        ir.tool_choice,
        Some(json!({"type": "function", "function": {"name": "weather"}}))
    );
    assert_eq!(ir.max_tokens, Some(512));
    assert_eq!(ir.stop, vec!["\n"]);
}

#[test]
fn ir_request_encodes_to_gemini_and_back() {
    let ir = gemini::decode_request(&gemini_request()).expect("decode");
    let encoded = gemini::encode_request(&ir, "mock-model");
    assert_eq!(encoded["model"], "mock-model");
    assert_eq!(
        encoded["systemInstruction"]["parts"][0]["text"],
        "You are terse."
    );
    assert_eq!(encoded["contents"][0]["role"], "user");
    assert_eq!(encoded["contents"][1]["role"], "model");
    assert_eq!(
        encoded["contents"][1]["parts"][0]["functionCall"]["name"],
        "weather"
    );
    assert_eq!(
        encoded["contents"][1]["parts"][0]["thoughtSignature"],
        "sig-1"
    );
    assert_eq!(
        encoded["contents"][2]["parts"][0]["functionResponse"]["name"],
        "weather"
    );
    assert_eq!(
        encoded["contents"][2]["parts"][1]["inlineData"]["data"],
        "aW1nMg=="
    );
    assert_eq!(
        encoded["tools"][0]["functionDeclarations"][0]["name"],
        "weather"
    );
    assert_eq!(
        encoded["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
    assert_eq!(encoded["generationConfig"]["maxOutputTokens"], 512);

    let back = gemini::decode_request(&encoded).expect("decode back");
    assert_eq!(back.messages.len(), ir.messages.len());
    assert_eq!(back.messages[1].parts[0].id, "call_1");
    assert_eq!(back.messages[2].parts[1].data, "aW1nMg==");
}

#[test]
fn chat_response_encodes_to_gemini() {
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

    let out = gemini::encode_response(&ir);
    let candidate = &out["candidates"][0];
    assert_eq!(candidate["content"]["role"], "model");
    assert_eq!(candidate["content"]["parts"][0]["text"], "It is sunny.");
    assert_eq!(
        candidate["content"]["parts"][1]["functionCall"]["name"],
        "print"
    );
    assert_eq!(candidate["finishReason"], "STOP");
    assert_eq!(out["usageMetadata"]["promptTokenCount"], 11);
    assert_eq!(out["usageMetadata"]["candidatesTokenCount"], 7);

    let back = gemini::decode_response(&out).expect("decode back");
    assert_eq!(back.stop, StopReason::ToolUse);
    assert_eq!(back.message.parts[1].args["text"], "hi");
}

#[test]
fn chat_stream_translates_to_gemini_events() {
    let chunks = vec![
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{"content":"Ni"},"finish_reason":null}]}),
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{"content":"hao"},"finish_reason":null}]}),
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_7","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}),
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"pwd\"}"}}]},"finish_reason":null}]}),
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"object":"chat.completion.chunk","model":"mock-model","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}),
    ];

    let mut encoder = GeminiStreamEncoder::new("mock/mock-model");
    let mut wire = String::new();
    for chunk in chunks {
        for event in chat::decode_chunk(&chunk) {
            if let Some(frame) = encoder.encode(&event) {
                wire.push_str(&frame);
            }
        }
    }
    if let Some(f) = encoder.encode(&StreamEvent::Done) {
        wire.push_str(&f);
    }

    let mut parser = SseParser::new();
    parser.extend(wire.as_bytes());
    let mut events = Vec::new();
    while let Some(data) = parser.next_data() {
        events.push(serde_json::from_str::<Value>(&data).expect("event JSON"));
    }
    assert!(events.len() >= 4);
    assert_eq!(
        events[0]["candidates"][0]["content"]["parts"][0]["text"],
        "Ni"
    );
    let tool = events
        .iter()
        .find(|e| {
            e.pointer("/candidates/0/content/parts/0/functionCall")
                .is_some()
        })
        .expect("tool event");
    assert_eq!(
        tool.pointer("/candidates/0/content/parts/0/functionCall/name")
            .unwrap(),
        "bash"
    );
    assert_eq!(
        tool.pointer("/candidates/0/content/parts/0/functionCall/args/cmd")
            .unwrap(),
        "pwd"
    );
    let last = events.last().expect("terminal event");
    assert_eq!(last["candidates"][0]["finishReason"], "STOP");
    assert_eq!(last["usageMetadata"]["promptTokenCount"], 5);
    assert_eq!(last["usageMetadata"]["candidatesTokenCount"], 3);
}

#[test]
fn gemini_stream_translates_to_chat_chunks() {
    let upstream = vec![
        json!({
            "candidates": [{"content": {"role":"model","parts":[{"text":"Hello "}]}, "index":0}],
            "modelVersion": "mock-model",
            "responseId": "g1"
        }),
        json!({
            "candidates": [{"content": {"role":"model","parts":[
                {"text":"Gemini", "thought": false},
                {"functionCall": {"id":"call_g", "name":"bash", "args":{"cmd":"pwd"}}}
            ]}, "index":0}],
            "modelVersion": "mock-model",
            "responseId": "g1"
        }),
        json!({
            "candidates": [{"content": {"role":"model","parts":[]}, "finishReason":"STOP", "index":0}],
            "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 2, "totalTokenCount": 6},
            "modelVersion": "mock-model",
            "responseId": "g1"
        }),
    ];

    let mut decoder = GeminiStreamDecoder::new();
    let mut encoder = ChatChunkEncoder::new("mock/mock-model", true);
    let mut wire = String::new();
    for chunk in upstream {
        for event in decoder.decode(&chunk) {
            if let Some(frame) = encoder.encode(&event) {
                wire.push_str(&frame);
            }
        }
    }
    if let Some(f) = encoder.encode(&StreamEvent::Done) {
        wire.push_str(&f);
    }

    let mut parser = SseParser::new();
    parser.extend(wire.as_bytes());
    let mut text = String::new();
    let mut args = String::new();
    let mut usage: Option<Value> = None;
    let mut done = false;
    while let Some(data) = parser.next_data() {
        if data == "[DONE]" {
            done = true;
            continue;
        }
        let v: Value = serde_json::from_str(&data).expect("chat chunk");
        if let Some(t) = v
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            text.push_str(t);
        }
        if let Some(a) = v
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
        {
            args.push_str(a);
        }
        if v.get("usage").is_some() {
            usage = Some(v["usage"].clone());
        }
    }
    assert_eq!(text, "Hello Gemini");
    assert_eq!(args, r#"{"cmd":"pwd"}"#);
    let usage = usage.expect("usage");
    assert_eq!(usage["prompt_tokens"], 4);
    assert_eq!(usage["completion_tokens"], 2);
    assert!(done);
}
