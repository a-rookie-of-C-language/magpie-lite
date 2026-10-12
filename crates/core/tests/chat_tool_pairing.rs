//! Chat-bound tool transcripts from coding agents, including interrupted history.
use magpie_core::{anthropic, chat};
use serde_json::{Value, json};

fn call(id: &str) -> Value {
    json!({"type":"tool_use","id":id,"name":"run","input":{"cmd":id}})
}
fn result(id: &str, text: &str) -> Value {
    json!({"type":"tool_result","tool_use_id":id,"content":text})
}
fn assistant(parts: Vec<Value>) -> Value {
    json!({"role":"assistant","content":parts})
}
fn user(parts: Vec<Value>) -> Value {
    json!({"role":"user","content":parts})
}
fn text(s: &str) -> Value {
    json!({"type":"text","text":s})
}
fn encode(messages: Vec<Value>) -> Vec<Value> {
    let input = json!({"model":"route/model","max_tokens":64,"messages":messages});
    let ir = anthropic::decode_request(&input).unwrap();
    let before = format!("{ir:?}");
    let encoded = chat::encode_request(&ir, "model").unwrap();
    assert_eq!(
        format!("{ir:?}"),
        before,
        "encoding mutated the source transcript"
    );
    let again = chat::encode_request(&ir, "model").unwrap();
    assert_eq!(
        encoded, again,
        "repair must be deterministic within a request"
    );
    encoded["messages"].as_array().unwrap().clone()
}
fn trace(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|m| match m["role"].as_str().unwrap() {
            "assistant" if m["tool_calls"].is_array() => format!(
                "assistant({})",
                m["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["id"].as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            "tool" => format!("tool({})", m["tool_call_id"].as_str().unwrap()),
            role => role.to_owned(),
        })
        .collect()
}
fn assert_complete(messages: &[Value]) {
    let mut pending = std::collections::HashSet::new();
    for m in messages {
        if m["role"] == "tool" {
            let id = m["tool_call_id"].as_str().unwrap();
            assert!(
                !id.is_empty() && pending.remove(id),
                "unmatched/duplicate tool: {m}"
            );
        } else {
            assert!(pending.is_empty(), "unfinished calls before {m}");
            if let Some(calls) = m["tool_calls"].as_array() {
                for c in calls {
                    let id = c["id"].as_str().unwrap();
                    assert!(
                        !id.is_empty() && pending.insert(id.to_owned()),
                        "bad call id: {c}"
                    );
                }
            }
        }
    }
    assert!(pending.is_empty(), "unfinished tail");
}

#[test]
fn parallel_results_are_gathered_in_call_order_before_user_attachments() {
    let out = encode(vec![
        assistant(vec![call("A"), call("B"), call("C")]),
        user(vec![result("C", "three")]),
        user(vec![
            text("attachment"),
            result("B", "two"),
            result("A", "one"),
        ]),
    ]);
    assert_eq!(
        trace(&out),
        ["assistant(A,B,C)", "tool(A)", "tool(B)", "tool(C)", "user"]
    );
    assert_eq!(out[1]["content"], "one");
    assert_eq!(out[2]["content"], "two");
    assert_eq!(out[3]["content"], "three");
    assert_eq!(out[4]["content"], "attachment");
    assert_complete(&out);
}

#[test]
fn split_assistant_calls_are_one_exchange_with_no_synthetic_answers() {
    let out = encode(vec![
        assistant(vec![text("start"), call("A")]),
        assistant(vec![text(" next"), call("B")]),
        user(vec![result("B", "two"), result("A", "one")]),
    ]);
    assert_eq!(trace(&out), ["assistant(A,B)", "tool(A)", "tool(B)"]);
    assert_eq!(out[0]["content"], "start next");
    assert_eq!(out[1]["content"], "one");
    assert_complete(&out);
}

#[test]
fn missing_results_are_explicitly_unavailable_before_next_turn_and_at_eof() {
    for tail in [
        vec![],
        vec![user(vec![text("continue")])],
        vec![assistant(vec![text("continuing")])],
    ] {
        let mut input = vec![
            assistant(vec![call("A"), call("B")]),
            user(vec![result("B", "two")]),
        ];
        input.extend(tail);
        let out = encode(input);
        let missing = out
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == "A")
            .unwrap();
        assert!(missing["content"].as_str().unwrap().contains("unavailable"));
        let real = out
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == "B")
            .unwrap();
        assert_eq!(real["content"], "two");
        assert_complete(&out);
    }
}

#[test]
fn orphan_and_empty_id_results_are_preserved_as_user_content() {
    for id in ["orphan", ""] {
        let out = encode(vec![
            user(vec![result(id, "retained result")]),
            user(vec![text("continue")]),
        ]);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"], "retained result");
        assert_complete(&out);
    }
}

#[test]
fn late_results_do_not_reopen_an_interrupted_exchange() {
    let out = encode(vec![
        assistant(vec![call("A")]),
        user(vec![text("interrupted")]),
        user(vec![result("A", "late real result")]),
    ]);
    assert_eq!(trace(&out), ["assistant(A)", "tool(A)", "user"]);
    assert!(out[1]["content"].as_str().unwrap().contains("unavailable"));
    let content = out[2]["content"].as_str().unwrap();
    assert!(content.contains("interrupted") && content.contains("late real result"));
    assert_complete(&out);
}

#[test]
fn duplicate_results_in_one_exchange_keep_the_first_only() {
    let out = encode(vec![
        assistant(vec![call("A")]),
        user(vec![result("A", "first"), result("A", "duplicate")]),
    ]);
    assert_eq!(trace(&out), ["assistant(A)", "tool(A)"]);
    assert_eq!(out[1]["content"], "first");
    assert_complete(&out);
}

#[test]
fn orphan_closes_pending_exchange_before_delivering_user_text() {
    let out = encode(vec![
        assistant(vec![call("A"), call("B")]),
        user(vec![result("B", "two"), result("X", "orphan")]),
    ]);
    assert_eq!(out.last().unwrap()["role"], "user");
    assert_eq!(out.last().unwrap()["content"], "orphan");
    assert_complete(&out);
}

#[test]
fn empty_thinking_turn_does_not_split_pending_calls() {
    let thinking = json!({"type":"thinking","thinking":"thought","signature":"sig"});
    let out = encode(vec![
        assistant(vec![call("A")]),
        assistant(vec![thinking.clone()]),
        user(vec![result("A", "real")]),
    ]);
    assert_eq!(trace(&out), ["assistant(A)", "tool(A)"]);
    assert_eq!(out[1]["content"], "real");
    let outside = encode(vec![
        user(vec![text("hi")]),
        assistant(vec![thinking]),
        user(vec![text("continue")]),
    ]);
    assert_eq!(trace(&outside), ["user", "assistant", "user"]);
    assert_complete(&out);
}

#[test]
fn orphan_result_preserves_a_preceding_multimodal_user_attachment() {
    let image =
        json!({"type":"image","source":{"type":"url","url":"https://example.invalid/image"}});
    let out = encode(vec![
        user(vec![text("look"), image]),
        user(vec![result("orphan", "result")]),
    ]);
    assert_eq!(out.len(), 1);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["text"], "look");
    assert_eq!(blocks[1]["type"], "image_url");
    assert_eq!(blocks[2]["text"], "result");
    assert_complete(&out);
}

#[test]
fn valid_history_keeps_wire_content_and_ids_unchanged() {
    let input = json!({"model":"m","messages":[
        {"role":"system","content":"system"}, {"role":"user","content":"hi"},
        {"role":"assistant","content":"working","tool_calls":[
            {"id":"A","type":"function","function":{"name":"run","arguments":"{\"cmd\":\"ls\"}"}},
            {"id":"B","type":"function","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"A","content":"first"},
        {"role":"tool","tool_call_id":"B","content":"second"},
        {"role":"user","content":"thanks"}]});
    let ir = chat::decode_request(&input).unwrap();
    let output = chat::encode_request(&ir, "m").unwrap();
    assert_eq!(output["messages"], input["messages"]);
    assert_complete(output["messages"].as_array().unwrap());
}

#[test]
fn empty_thinking_after_a_partial_result_keeps_exchange_open() {
    let out = encode(vec![
        assistant(vec![call("A"), call("B")]),
        user(vec![result("B", "two")]),
        assistant(vec![
            json!({"type":"thinking","thinking":"thought","signature":"sig"}),
        ]),
        user(vec![result("A", "one")]),
    ]);
    assert_eq!(trace(&out), ["assistant(A,B)", "tool(A)", "tool(B)"]);
    assert_eq!(out[1]["content"], "one");
    assert_complete(&out);
}

#[test]
fn reused_call_ids_are_remapped_per_exchange_without_cross_wiring_results() {
    let out = encode(vec![
        assistant(vec![call("A")]),
        user(vec![result("A", "first")]),
        assistant(vec![call("A")]),
        user(vec![result("A", "second")]),
    ]);
    assert_eq!(out[0]["tool_calls"][0]["id"], "A");
    let second_id = &out[2]["tool_calls"][0]["id"];
    assert_ne!(second_id, "A");
    assert_eq!(out[3]["tool_call_id"], *second_id);
    assert_eq!(out[1]["content"], "first");
    assert_eq!(out[3]["content"], "second");
    assert_complete(&out);
}

#[test]
fn missing_call_ids_are_generated_without_colliding_with_later_native_ids() {
    let out = encode(vec![
        assistant(vec![call("")]),
        user(vec![text("interrupted")]),
        assistant(vec![call("call_magpie_lite_0")]),
        user(vec![result("call_magpie_lite_0", "real")]),
    ]);
    assert_eq!(out[0]["tool_calls"][0]["id"], "call_magpie_lite_1");
    assert_eq!(out[1]["tool_call_id"], "call_magpie_lite_1");
    assert!(out[1]["content"].as_str().unwrap().contains("unavailable"));
    assert_eq!(out[3]["tool_calls"][0]["id"], "call_magpie_lite_0");
    assert_eq!(out[4]["content"], "real");
    assert_complete(&out);
}

#[test]
fn ambiguous_parallel_ids_are_rejected_without_quoting_private_content() {
    let id = "PRIVATE_DUPLICATE_CALL_ID";
    let input = json!({"model":"m","messages":[assistant(vec![call(id)]),assistant(vec![call(id)]),
        user(vec![result(id,"PRIVATE_RESULT")]) ]});
    let ir = anthropic::decode_request(&input).unwrap();
    let error = chat::encode_request(&ir, "m").unwrap_err().to_string();
    assert!(error.contains("ambiguous"));
    assert!(!error.contains(id));
    assert!(!error.contains("PRIVATE_RESULT"));
}

#[test]
fn empty_orphan_output_has_a_visible_non_success_placeholder() {
    let out = encode(vec![user(vec![result("orphan", "  ")])]);
    assert_eq!(out[0]["role"], "user");
    assert_eq!(out[0]["content"], "Tool result received.");
    assert_complete(&out);
}
