//! Conformance tests for the wire translators: a request decoded from one
//! dialect, encoded into the other, and checked against what that dialect
//! really expects on the wire.

use magpie_core::anthropic::{self, AnthropicStreamDecoder, AnthropicStreamEncoder};
use magpie_core::chat::{self, ChatChunkEncoder};
use magpie_core::ir::{PartKind, StopReason};
use magpie_core::stream::{SseParser, StreamEvent};
use serde_json::{Value, json};

fn chat_request() -> Value {
    json!({
        "model": "deepseek/deepseek-chat",
        "messages": [
            {"role": "system", "content": "You are terse."},
            {"role": "user", "content": "Weather in Beijing?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "weather", "arguments": "{\"city\": \"Beijing\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "Sunny, 23C"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "weather",
                "description": "Look up weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        }],
        "max_tokens": 512,
        "stream": false
    })
}

#[test]
fn chat_request_decodes_into_ir() {
    let ir = chat::decode_request(&chat_request()).expect("decode");
    assert_eq!(ir.model, "deepseek/deepseek-chat");
    assert_eq!(ir.system.len(), 1);
    assert_eq!(ir.messages.len(), 3); // user, assistant, tool-as-user
    assert_eq!(ir.messages[2].parts[0].kind(), PartKind::ToolResult);
    assert_eq!(ir.messages[2].parts[0].call_id, "call_1");
    assert_eq!(ir.messages[2].parts[0].text, "Sunny, 23C");
    assert_eq!(ir.tools.len(), 1);
    assert_eq!(ir.tools[0].name, "weather");
}

#[test]
fn chat_request_encodes_to_anthropic() {
    let ir = chat::decode_request(&chat_request()).expect("decode");
    let out = anthropic::encode_request(&ir, "deepseek-chat");
    assert_eq!(out["model"], "deepseek-chat");
    assert_eq!(out["system"], "You are terse.");
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    // The assistant turn carries a tool_use block with parsed args.
    let blocks = msgs[1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["id"], "call_1");
    assert_eq!(blocks[0]["input"]["city"], "Beijing");
    // The tool answer is a tool_result block in the following user turn.
    let answer = msgs[2]["content"].as_array().unwrap();
    assert_eq!(answer[0]["type"], "tool_result");
    assert_eq!(answer[0]["tool_use_id"], "call_1");
    // The tool crossed with its schema.
    assert_eq!(out["tools"][0]["name"], "weather");
    assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
}

#[test]
fn anthropic_request_round_trips_back_to_chat() {
    let ir = chat::decode_request(&chat_request()).expect("decode");
    let anthropic_req = anthropic::encode_request(&ir, "deepseek-chat");
    let back = anthropic::decode_request(&anthropic_req).expect("decode");
    let chat_req = chat::encode_request(&back, "deepseek-chat");
    assert_eq!(chat_req["model"], "deepseek-chat");
    let msgs = chat_req["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "weather");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_1");
}

#[test]
fn chat_response_encodes_to_anthropic() {
    let ir = chat::decode_response(&json!({
        "id": "x", "object": "chat.completion", "model": "deepseek-chat",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "It is sunny.",
                "tool_calls": [{
                    "id": "call_2", "type": "function",
                    "function": {"name": "print", "arguments": "{\"text\": \"hi\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    }))
    .expect("decode");
    assert_eq!(ir.stop, StopReason::ToolUse);
    let out = anthropic::encode_response(&ir);
    assert_eq!(out["type"], "message");
    assert_eq!(out["stop_reason"], "tool_use");
    let content = out["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["input"]["text"], "hi");
    assert_eq!(out["usage"]["input_tokens"], 11);
    assert_eq!(out["usage"]["output_tokens"], 7);
}

#[test]
fn anthropic_response_encodes_to_chat() {
    let ir = anthropic::decode_response(&json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-4-6",
        "content": [
            {"type": "text", "text": "Done."},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"cmd": "ls"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 100, "output_tokens": 20}
    }))
    .expect("decode");
    let out = chat::encode_response(&ir);
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(out["choices"][0]["message"]["content"], "Done.");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "bash"
    );
    assert_eq!(out["usage"]["prompt_tokens"], 100);
    assert_eq!(out["usage"]["completion_tokens"], 20);
}

fn anthropic_stream_events() -> Vec<Value> {
    vec![
        json!({"type": "message_start", "message": {"model": "claude-opus-4-6", "usage": {"input_tokens": 42}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "!"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "bash", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
    ]
}

#[test]
fn anthropic_stream_translates_to_chat_chunks() {
    let mut dec = AnthropicStreamDecoder::new();
    let mut enc = ChatChunkEncoder::new("claude-opus-4-6", true);
    let mut out = String::new();
    for ev in anthropic_stream_events() {
        for ir in dec.decode(&ev) {
            if let Some(frame) = enc.encode(&ir) {
                out.push_str(&frame);
            }
        }
    }
    // Every frame parses, the fragments reassemble, the tool call carries
    // over, usage is reported, and the stream ends as Chat does.
    let mut parser = SseParser::new();
    parser.extend(out.as_bytes());
    let mut texts = String::new();
    let mut args = String::new();
    let mut saw_finish = false;
    let mut saw_usage = false;
    let mut saw_done = false;
    while let Some(data) = parser.next_data() {
        if data == "[DONE]" {
            saw_done = true;
            continue;
        }
        let v: Value = serde_json::from_str(&data).expect("chunk is JSON");
        if let Some(t) = v
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            texts.push_str(t);
        }
        if let Some(a) = v
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
        {
            args.push_str(a);
        }
        if v.pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            == Some("tool_calls")
        {
            saw_finish = true;
        }
        if v.get("usage").map(Value::is_object).unwrap_or(false)
            && v["choices"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
        {
            saw_usage = true;
            assert_eq!(v["usage"]["prompt_tokens"], 42);
            assert_eq!(v["usage"]["completion_tokens"], 9);
        }
    }
    assert_eq!(texts, "Hello!");
    assert_eq!(args, r#"{"cmd":"ls"}"#);
    assert!(saw_finish);
    assert!(saw_usage);
    assert!(saw_done);
}

fn chat_stream_chunks() -> Vec<Value> {
    vec![
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [{"index": 0, "delta": {"content": "Ni"}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [{"index": 0, "delta": {"content": "hao"}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_7", "type": "function", "function": {"name": "bash", "arguments": ""}}]}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"cmd\":\"pwd\"}"}}]}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "deepseek-chat", "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}}),
    ]
}

#[test]
fn chat_stream_translates_to_anthropic_events() {
    let mut enc = AnthropicStreamEncoder::new("deepseek-chat");
    let mut out = String::new();
    if let Some(f) = enc.encode(&StreamEvent::Start {
        model: "deepseek-chat".into(),
    }) {
        out.push_str(&f);
    }
    for chunk in chat_stream_chunks() {
        for ir in chat::decode_chunk(&chunk) {
            if let Some(frame) = enc.encode(&ir) {
                out.push_str(&frame);
            }
        }
    }
    if let Some(f) = enc.encode(&StreamEvent::Done) {
        out.push_str(&f);
    }

    let mut parser = SseParser::new();
    parser.extend(out.as_bytes());
    let mut text = String::new();
    let mut args = String::new();
    let mut stop_reason = String::new();
    let mut output_tokens: Option<u64> = None;
    let mut saw_message_stop = false;
    while let Some(data) = parser.next_data() {
        let v: Value = serde_json::from_str(&data).expect("event data is JSON");
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "content_block_delta" => match v
                .pointer("/delta/type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text_delta" => text.push_str(
                    v.pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                "input_json_delta" => args.push_str(
                    v.pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                _ => {}
            },
            "message_delta" => {
                stop_reason = v
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                output_tokens = v.pointer("/usage/output_tokens").and_then(Value::as_u64);
            }
            "message_stop" => saw_message_stop = true,
            _ => {}
        }
    }
    assert_eq!(text, "Nihao");
    assert_eq!(args, r#"{"cmd":"pwd"}"#);
    assert_eq!(stop_reason, "tool_use");
    assert_eq!(output_tokens, Some(3));
    assert!(saw_message_stop);
}
