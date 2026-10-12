//! Search-result fixtures from the reference CodeAssist/Responses wire contracts.
use magpie_core::{anthropic, gemini, responses};
use serde_json::{Value, json};

fn grounding() -> Value {
    json!({"webSearchQueries":["latest go release", "go release notes"],
        "groundingChunks":[{"web":{"uri":"https://example.invalid/go","title":"go.dev"}},
            {"web":{"uri":"https://example.invalid/notes"}}, {"web":{"uri":""}}],
        "groundingSupports":[{"segment":{"startIndex":0,"endIndex":6,"text":"Latest"},
            "groundingChunkIndices":[0]}],
        "searchEntryPoint":{"renderedContent":"<div>Search attribution</div>"}})
}
fn gemini_reply() -> Value {
    json!({"modelVersion":"search-model", "candidates":[{"content":{"parts":[{"text":"Latest"}]},
        "groundingMetadata":grounding(), "finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}})
}
fn search_item() -> Value {
    json!({"id":"ws_fixture", "type":"web_search_call", "status":"completed", "action":{
        "type":"search", "query":"latest go release", "sources":[{"type":"url","url":"https://example.invalid/go"},
        {"type":"url","url":""}]}})
}
fn frames(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn assert_search_blocks(content: &[Value]) {
    let calls: Vec<_> = content
        .iter()
        .filter(|b| b["type"] == "server_tool_use")
        .collect();
    let results: Vec<_> = content
        .iter()
        .filter(|b| b["type"] == "web_search_tool_result")
        .collect();
    assert_eq!(calls.len(), 1, "{content:?}");
    assert_eq!(results.len(), 1, "{content:?}");
    assert_eq!(calls[0]["name"], "web_search");
    assert_eq!(results[0]["tool_use_id"], calls[0]["id"]);
    assert_eq!(
        results[0]["content"][0]["url"],
        "https://example.invalid/go"
    );
    assert_eq!(results[0]["content"][0]["encrypted_content"], "");
}
#[test]
fn gemini_nonstream_search_reaches_anthropic_and_retains_native_grounding() {
    let ir = gemini::decode_response(&gemini_reply()).unwrap();
    let anthropic = anthropic::encode_response(&ir);
    assert_search_blocks(anthropic["content"].as_array().unwrap());
    assert_eq!(anthropic["stop_reason"], "end_turn");
    assert_eq!(
        anthropic["content"][1]["input"]["query"],
        "latest go release; go release notes"
    );
    assert_eq!(
        gemini::encode_response(&ir)["candidates"][0]["groundingMetadata"],
        grounding()
    );
}
#[test]
fn gemini_stream_search_emits_once_before_finish_and_keeps_native_metadata() {
    let mut dec = gemini::GeminiStreamDecoder::new();
    let mut anth = anthropic::AnthropicStreamEncoder::new("client-model");
    let mut native = gemini::GeminiStreamEncoder::new("client-model");
    let mut awire = String::new();
    let mut gwire = String::new();
    let mut values = vec![json!({"candidates":[{"groundingMetadata":{}}]})];
    values.push(gemini_reply());
    values.push(json!({"candidates":[{"groundingMetadata":grounding()}]}));
    for v in values {
        for e in dec.decode(&v) {
            awire.push_str(&anth.encode(&e).unwrap_or_default());
            gwire.push_str(&native.encode(&e).unwrap_or_default());
        }
    }
    let aframes = frames(&awire);
    let blocks: Vec<Value> = aframes
        .iter()
        .filter_map(|f| f.get("content_block").cloned())
        .collect();
    assert_search_blocks(&blocks);
    assert!(awire.find("web_search_tool_result").unwrap() < awire.find("message_stop").unwrap());
    assert_eq!(
        aframes
            .iter()
            .filter(|f| f["type"] == "message_start")
            .count(),
        1
    );
    let gframes = frames(&gwire);
    let grounded: Vec<_> = gframes
        .iter()
        .filter_map(|f| f.pointer("/candidates/0/groundingMetadata"))
        .collect();
    assert_eq!(grounded, vec![&grounding()]);
}
#[test]
fn responses_search_is_not_a_client_function_call_in_either_mode() {
    let body = json!({"model":"search-model","status":"completed","output":[search_item()],
        "usage":{"input_tokens":7,"output_tokens":3}});
    let ir = responses::decode_response(&body).unwrap();
    let anth = anthropic::encode_response(&ir);
    assert_search_blocks(anth["content"].as_array().unwrap());
    assert_eq!(anth["stop_reason"], "end_turn");
    let native = responses::encode_response(&ir);
    assert_eq!(native["output"][0]["type"], "web_search_call");
    assert_eq!(native["output"][0]["action"]["query"], "latest go release");
    let mut encoder = anthropic::AnthropicStreamEncoder::new("search-model");
    let mut wire = String::new();
    for frame in [
        json!({"type":"response.output_item.done","output_index":0,"item":search_item()}),
        json!({"type":"response.completed","response":body}),
    ] {
        for e in responses::decode_chunk(&frame) {
            wire.push_str(&encoder.encode(&e).unwrap_or_default());
        }
    }
    let blocks: Vec<Value> = frames(&wire)
        .iter()
        .filter_map(|f| f.get("content_block").cloned())
        .collect();
    assert_search_blocks(&blocks);
    assert!(!wire.contains("\"type\":\"tool_use\""));
    assert!(wire.contains("end_turn"));
}

#[test]
fn snake_case_grounding_and_query_or_source_only_searches_are_visible() {
    for metadata in [
        json!({"web_search_queries":["rust", "", null]}),
        json!({"grounding_chunks":[{"web":{"uri":"https://example.invalid/go"}}]}),
    ] {
        let body = json!({"model_version":"search-model","candidates":[{"grounding_metadata":metadata,"finish_reason":"STOP"}]});
        let reply = gemini::decode_response(&body).unwrap();
        let search = reply.message.parts[0].search.as_ref().unwrap();
        assert!(search.is_answer());
        assert!(!search.query.contains("null"));
        let events = gemini::GeminiStreamDecoder::new().decode(&body);
        assert!(events.iter().any(|e| e.is_answer() && e.is_content()));
        assert_eq!(reply.stop, magpie_core::ir::StopReason::EndTurn);
    }
}
#[test]
fn empty_or_malformed_grounding_does_not_consume_the_one_search_event() {
    let mut decoder = gemini::GeminiStreamDecoder::new();
    for metadata in [
        json!({}),
        json!({"webSearchQueries":[null, ""],"groundingChunks":[{"web":{"uri":""}}]}),
        json!({"webSearchQueries":"rust","groundingChunks":[{"retrievedContext":{"uri":"https://example.invalid/go"}}]}),
    ] {
        let events = decoder.decode(&json!({"candidates":[{"groundingMetadata":metadata}]}));
        assert!(!events.iter().any(|e| e.is_answer() || e.is_content()));
    }
    let events = decoder.decode(&gemini_reply());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, magpie_core::stream::StreamEvent::Search { .. }))
            .count(),
        1
    );
}
#[test]
fn responses_search_roundtrip_closes_open_items_and_keeps_search_in_final_output() {
    use magpie_core::stream::StreamEvent;
    let mut encoder = responses::ResponsesStreamEncoder::new("client-model");
    let mut events = vec![StreamEvent::TextDelta {
        text: "before".into(),
    }];
    for index in 0..2 {
        events.extend(responses::decode_chunk(
            &json!({"type":"response.output_item.done","output_index":index,"item":search_item()}),
        ));
    }
    events.extend([
        StreamEvent::TextDelta {
            text: "after".into(),
        },
        StreamEvent::Done,
    ]);
    let wire: String = events.iter().filter_map(|e| encoder.encode(e)).collect();
    let chunks = frames(&wire);
    let added: Vec<_> = chunks
        .iter()
        .filter(|f| f["type"] == "response.output_item.added")
        .collect();
    assert_eq!(added.len(), 4, "{wire}");
    let ids: std::collections::HashSet<_> = added
        .iter()
        .map(|f| f["item"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    let output = chunks.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap();
    assert_eq!(output.len(), 4, "{wire}");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[1]["type"], "web_search_call");
    assert_eq!(output[2]["type"], "web_search_call");
    assert_eq!(output[3]["type"], "message");
    assert_eq!(chunks.last().unwrap()["type"], "response.completed");
    assert!(!wire.contains("function_call"));
}
#[test]
fn responses_non_search_actions_and_empty_searches_do_not_become_search_results() {
    for item in [
        json!({"type":"web_search_call","action":{"type":"open_page","url":"https://example.invalid/go"}}),
        json!({"type":"web_search_call","action":{"type":"find","pattern":"go"}}),
        json!({"type":"web_search_call","action":{"type":"search","query":"","sources":[{"url":""}]}}),
    ] {
        let events =
            responses::decode_chunk(&json!({"type":"response.output_item.done","item":item}));
        assert!(!events.iter().any(|e| e.is_answer()), "{events:?}");
    }
}

#[test]
fn responses_opening_usage_is_optional_and_retained_when_reported() {
    use magpie_core::stream::StreamEvent;
    let events = responses::decode_chunk(&json!({"type":"response.created", "response":{
        "model":"compat-model", "usage":{"input_tokens":7,"output_tokens":3}}}));
    assert!(
        events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.input == Some(7) && u.output == Some(3))
        )
    );
    for response in [
        json!({"model":"model"}),
        json!({"model":"model","usage":null}),
    ] {
        let events =
            responses::decode_chunk(&json!({"type":"response.created","response":response}));
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Usage(_))));
    }
}
