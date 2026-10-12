//! Hosted Anthropic search is not an agent-executed function call.
use magpie_core::{anthropic, gemini, ir::PartKind, responses, stream::StreamEvent};
use serde_json::{Value, json};

fn call(id: &str, query: &str) -> Value {
    json!({"type":"server_tool_use", "id":id, "name":"web_search", "input":{"query":query}})
}
fn result(id: &str) -> Value {
    json!({"type":"web_search_tool_result", "tool_use_id":id, "content":[
        {"type":"web_search_result", "url":"https://example.invalid/rust", "title":"Rust", "page_age":"2 days", "encrypted_content":"NATIVE_SECRET"},
        {"type":"web_search_result", "url":"https://example.invalid/fallback", "encrypted_content":"ANOTHER_SECRET"},
        {"type":"web_search_result", "url":""}]})
}
fn reply(content: Value) -> Value {
    json!({"model":"search-model", "content":content, "stop_reason":"end_turn", "usage":{"input_tokens":7,"output_tokens":3}})
}
fn block_start(index: usize, block: Value) -> Value {
    json!({"type":"content_block_start", "index":index, "content_block":block})
}
fn stop(index: usize) -> Value {
    json!({"type":"content_block_stop", "index":index})
}
fn frames(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[test]
fn native_search_blocks_keep_ids_ciphertext_errors_and_conversation_order() {
    let content = json!([call("srv_1", "rust release"), {"type":"text","text":"between"}, result("srv_1"),
        call("srv_2", "retry query"), {"type":"web_search_tool_result","tool_use_id":"srv_2",
        "content":{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}}]);
    let ir = anthropic::decode_response(&reply(content.clone())).unwrap();
    assert_eq!(anthropic::encode_response(&ir)["content"], content);
    let request = anthropic::decode_request(
        &json!({"model":"m","messages":[{"role":"assistant","content":content}]}),
    )
    .unwrap();
    assert_eq!(
        anthropic::encode_request(&request, "up").unwrap()["messages"][0]["content"],
        content
    );
    assert!(
        ir.message
            .parts
            .iter()
            .all(|p| p.kind() != PartKind::ToolCall && p.kind() != PartKind::ToolResult)
    );
}
#[test]
fn public_search_is_one_action_with_query_and_sources_never_native_ciphertext() {
    let ir = anthropic::decode_response(&reply(json!([
        call("srv_1", "rust release"),
        result("srv_1")
    ])))
    .unwrap();
    let out = responses::encode_response(&ir);
    let output = out["output"].as_array().unwrap();
    assert_eq!(output.len(), 1, "{out}");
    assert_eq!(output[0]["type"], "web_search_call");
    assert_eq!(output[0]["action"]["query"], "rust release");
    assert_eq!(output[0]["action"]["sources"].as_array().unwrap().len(), 2);
    let out = gemini::encode_response(&ir);
    assert_eq!(
        out["candidates"][0]["groundingMetadata"]["webSearchQueries"],
        json!(["rust release"])
    );
    assert_eq!(
        out["candidates"][0]["groundingMetadata"]["groundingChunks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for out in [out, responses::encode_response(&ir)] {
        assert!(!out.to_string().contains("SECRET"));
        assert!(!out.to_string().contains("encrypted_content"));
    }
    assert_eq!(anthropic::encode_response(&ir)["stop_reason"], "end_turn");
}
#[test]
fn search_input_fragments_never_leak_as_function_arguments_and_commit_before_results() {
    let mut dec = anthropic::AnthropicStreamDecoder::new();
    let mut search_call = call("srv_1", "");
    search_call["input"] = json!({});
    let began = dec.decode(&block_start(0, search_call));
    assert!(
        began.iter().any(StreamEvent::is_answer),
        "search start must prevent paid replay: {began:?}"
    );
    let mut all = began;
    for fragment in ["{\"query\":", "\"rust release\"}"] {
        let events = dec.decode(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":fragment}}));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, StreamEvent::ToolCallDelta { .. })),
            "{events:?}"
        );
        all.extend(events);
    }
    all.extend(dec.decode(&stop(0)));
    all.extend(dec.decode(&block_start(1, result("srv_1"))));
    all.extend(dec.decode(&stop(1)));
    all.push(StreamEvent::Done);
    let mut native = anthropic::AnthropicStreamEncoder::new("client");
    let native_wire: String = all.iter().filter_map(|e| native.encode(e)).collect();
    let native_blocks: Vec<Value> = frames(&native_wire)
        .iter()
        .filter_map(|v| v.get("content_block").cloned())
        .collect();
    assert_eq!(native_blocks.len(), 2, "{native_wire}");
    assert_eq!(native_blocks[0]["id"], "srv_1");
    assert_eq!(native_blocks[1], result("srv_1"));
    let mut encoded = responses::ResponsesStreamEncoder::new("client");
    let wire: String = all.iter().filter_map(|e| encoded.encode(e)).collect();
    let frames = frames(&wire);
    let output = frames.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap();
    assert_eq!(output.len(), 1, "{wire}");
    assert_eq!(output[0]["action"]["query"], "rust release");
    assert!(!wire.contains("function_call") && !wire.contains("SECRET"));
}
#[test]
fn unknown_server_tools_and_unopened_argument_indices_are_not_function_calls() {
    let mut dec = anthropic::AnthropicStreamDecoder::new();
    assert!(
        dec.decode(&block_start(
            0,
            json!({"type":"server_tool_use","id":"srv_other","name":"web_fetch","input":{}})
        ))
        .is_empty()
    );
    for index in [0, 42] {
        assert!(dec.decode(&json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":"{}"}})).is_empty());
    }
    assert!(matches!(
        dec.decode(&block_start(
            1,
            json!({"type":"tool_use","id":"fn_1","name":"web_search","input":{}})
        ))
        .as_slice(),
        [StreamEvent::ToolCallStart { .. }]
    ));
    assert!(matches!(dec.decode(&json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}})).as_slice(), [StreamEvent::ToolCallDelta{..}]));
    assert!(matches!(
        dec.decode(&stop(1)).as_slice(),
        [StreamEvent::ToolCallEnd { .. }]
    ));
}

#[test]
fn multiple_searches_keep_id_association_and_all_gemini_json_results() {
    let mut second = result("srv_2");
    second["content"][0]["url"] = json!("https://example.invalid/second");
    let content = json!([call("srv_1", "first query"), call("srv_2", "second query"),
        second, {"type":"text","text":"between"}, result("srv_1")]);
    let ir = anthropic::decode_response(&reply(content.clone())).unwrap();
    assert_eq!(anthropic::encode_response(&ir)["content"], content);
    let out = responses::encode_response(&ir);
    let searches: Vec<_> = out["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["type"] == "web_search_call")
        .collect();
    assert_eq!(searches.len(), 2);
    assert_eq!(searches[0]["action"]["query"], "second query");
    assert_eq!(searches[1]["action"]["query"], "first query");
    let out = gemini::encode_response(&ir);
    let metadata = &out["candidates"][0]["groundingMetadata"];
    assert_eq!(
        metadata["webSearchQueries"],
        json!(["second query", "first query"])
    );
    assert_eq!(metadata["groundingChunks"].as_array().unwrap().len(), 4);
    assert_eq!(
        metadata["groundingChunks"][0]["web"]["uri"],
        "https://example.invalid/second"
    );
}

#[test]
fn calls_and_native_errors_do_not_fabricate_successful_public_searches() {
    let content = json!([call("srv_1", "first query"),
        {"type":"web_search_tool_result","tool_use_id":"srv_1",
         "content":{"type":"web_search_tool_result_error","error_code":"unavailable"}}]);
    let ir = anthropic::decode_response(&reply(content.clone())).unwrap();
    assert_eq!(anthropic::encode_response(&ir)["content"], content);
    assert!(
        responses::encode_response(&ir)["output"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        gemini::encode_response(&ir)["candidates"][0]
            .get("groundingMetadata")
            .is_none()
    );
    let mut dec = anthropic::AnthropicStreamDecoder::new();
    let mut events = dec.decode(&block_start(0, call("srv_1", "first query")));
    events.extend(dec.decode(&stop(0)));
    events.extend(dec.decode(&block_start(1, content[1].clone())));
    for event in events {
        assert!(
            responses::ResponsesStreamEncoder::new("client")
                .encode(&event)
                .is_none()
        );
        assert!(
            gemini::GeminiStreamEncoder::new("client")
                .encode(&event)
                .is_none()
        );
    }
}

#[test]
fn whole_input_ignores_fragments_and_stream_roundtrip_keeps_interleaving() {
    let mut dec = anthropic::AnthropicStreamDecoder::new();
    let mut events = dec.decode(&block_start(0, call("srv_1", "whole query")));
    events.extend(dec.decode(&json!({"type":"content_block_delta","index":0,
        "delta":{"type":"input_json_delta","partial_json":"THIS_IS_NOT_JSON"}})));
    events.extend(dec.decode(&stop(0)));
    events.extend(dec.decode(&block_start(1, json!({"type":"text","text":""}))));
    events.extend(dec.decode(&json!({"type":"content_block_delta","index":1,
        "delta":{"type":"text_delta","text":"between"}})));
    events.extend(dec.decode(&stop(1)));
    events.extend(dec.decode(&block_start(2, result("srv_1"))));
    events.extend(dec.decode(&stop(2)));
    events.push(StreamEvent::Done);
    let mut enc = anthropic::AnthropicStreamEncoder::new("client");
    let wire: String = events
        .iter()
        .filter_map(|event| enc.encode(event))
        .collect();
    let mut decoded = anthropic::AnthropicStreamDecoder::new();
    let parts: Vec<_> = frames(&wire)
        .iter()
        .flat_map(|v| decoded.decode(v))
        .filter_map(|event| match event {
            StreamEvent::Search { search } => Some(search.anthropic_block.unwrap()),
            StreamEvent::TextDelta { text } => Some(json!({"type":"text","text":text})),
            _ => None,
        })
        .collect();
    assert_eq!(
        parts,
        vec![
            call("srv_1", "whole query"),
            json!({"type":"text","text":"between"}),
            result("srv_1")
        ]
    );
    assert!(!wire.contains("THIS_IS_NOT_JSON"));
}

#[test]
fn malformed_input_and_reused_search_indices_fail_without_secrets_and_reset() {
    for input in [Value::Null, json!([]), json!("PRIVATE_INPUT")] {
        let mut dec = anthropic::AnthropicStreamDecoder::new();
        let mut block = call("srv_1", "unused");
        block["input"] = input;
        dec.decode(&block_start(0, block));
        let events = dec.decode(&stop(0));
        assert!(
            matches!(events.as_slice(), [StreamEvent::Error { .. }]),
            "{events:?}"
        );
        assert!(!format!("{events:?}").contains("PRIVATE_INPUT"));
        assert!(
            dec.decode(&block_start(1, call("srv_2", "ignored")))
                .is_empty()
        );
        dec.decode(&json!({"type":"message_start","message":{"model":"next"}}));
        assert!(matches!(
            dec.decode(&block_start(0, call("srv_1", "next")))
                .as_slice(),
            [StreamEvent::SearchStarted]
        ));
        assert!(matches!(
            dec.decode(&stop(0)).as_slice(),
            [StreamEvent::Search { .. }]
        ));
    }
    for replacement in [
        call("srv_2", "duplicate"),
        json!({"type":"tool_use","id":"fn","name":"shell","input":{}}),
        json!({"type":"text","text":""}),
    ] {
        let mut dec = anthropic::AnthropicStreamDecoder::new();
        dec.decode(&block_start(0, call("srv_1", "first")));
        let events = dec.decode(&block_start(0, replacement));
        assert!(
            matches!(events.as_slice(), [StreamEvent::Error { .. }]),
            "{events:?}"
        );
    }
}

#[test]
fn malformed_argument_fragments_and_unclosed_search_fail_once() {
    for args in ["{PRIVATE", "[]", "null"] {
        let mut dec = anthropic::AnthropicStreamDecoder::new();
        let mut block = call("srv_1", "");
        block["input"] = json!({});
        dec.decode(&block_start(0, block));
        dec.decode(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":args}}));
        let events = dec.decode(&stop(0));
        assert!(
            matches!(events.as_slice(), [StreamEvent::Error { .. }]),
            "{events:?}"
        );
        assert!(!format!("{events:?}").contains("PRIVATE"));
        assert!(dec.decode(&json!({"type":"message_stop"})).is_empty());
    }
    let mut dec = anthropic::AnthropicStreamDecoder::new();
    dec.decode(&block_start(0, call("srv_1", "open")));
    assert!(matches!(
        dec.decode(&json!({"type":"message_stop"})).as_slice(),
        [StreamEvent::Error { .. }]
    ));
}
