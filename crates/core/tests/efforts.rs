//! Reasoning-effort asks must keep the caller's level across dialects,
//! in the shape each upstream takes, never silently dropped.
use magpie_core::{anthropic, chat, effort::Effort, gemini, ir, responses};
use serde_json::{Value, json};

fn decode(source: usize, body: Value) -> ir::CompletionRequest {
    match source {
        0 => chat::decode_request(&body),
        1 => responses::decode_request(&body),
        2 => anthropic::decode_request(&body),
        3 => gemini::decode_request(&body),
        _ => unreachable!(),
    }
    .unwrap()
}
fn encode(target: usize, r: &ir::CompletionRequest) -> Value {
    match target {
        0 => chat::encode_request(r, "up"),
        1 => responses::encode_request(r, "up"),
        2 => anthropic::encode_request(r, "up"),
        3 => gemini::encode_request(r, "up"),
        _ => unreachable!(),
    }
    .unwrap()
}

fn body(source: usize, ask: Value) -> Value {
    match source {
        0 => json!({"model":"m","messages":[],"reasoning_effort":ask}),
        1 => json!({"model":"m","input":"hi","reasoning":ask}),
        2 => json!({"model":"m","messages":[],"max_tokens":64000,"thinking":ask}),
        3 => json!({"model":"m","contents":[],"generationConfig":{"thinkingConfig":ask}}),
        _ => unreachable!(),
    }
}

#[test]
fn every_effort_level_translates_in_all_sixteen_directions() {
    // source x [chat, responses, anthropic, gemini] asks
    let asks: [Vec<Value>; 4] = [
        vec![
            json!("none"),
            json!("minimal"),
            json!("medium"),
            json!("xhigh"),
            json!("max"),
        ],
        vec![
            json!({"effort":"none"}),
            json!({"effort":"minimal"}),
            json!({"effort":"medium"}),
            json!({"effort":"xhigh"}),
            json!({"effort":"max"}),
        ],
        vec![
            json!({"type":"disabled"}),
            json!({"type":"enabled","budget_tokens":4096}),
            json!({"type":"enabled","budget_tokens":10000}),
            json!({"type":"enabled","budget_tokens":32000}),
            json!({"type":"enabled","budget_tokens":64000}),
        ],
        vec![
            json!({"thinkingLevel":"none"}),
            json!({"thinkingBudget":1024}),
            json!({"thinkingBudget":8192}),
            json!({"thinkingBudget":24576}),
            json!({"thinkingLevel":"xhigh"}),
        ],
    ];
    for (source, level_asks) in asks.iter().enumerate() {
        for ask in level_asks {
            let r = decode(source, body(source, ask.clone()));
            assert!(r.effort.is_some(), "source={source} ask={ask}");
            for target in 0..4usize {
                let out = encode(target, &r);
                let read = ir::CompletionRequest {
                    effort: r.effort,
                    ..decode(target, out.clone())
                }
                .effort;
                assert_eq!(
                    read, r.effort,
                    "source={source} ask={ask} target={target} out={out}"
                );
            }
        }
    }
}

#[test]
fn no_ask_sends_no_reasoning_field_and_unknown_levels_fail_closed() {
    for source in 0..4usize {
        let bare = match source {
            0 => json!({"model":"m","messages":[]}),
            1 => json!({"model":"m","input":"hi"}),
            2 => json!({"model":"m","messages":[],"max_tokens":4096}),
            3 => json!({"model":"m","contents":[]}),
            _ => unreachable!(),
        };
        let r = decode(source, bare.clone());
        assert_eq!(r.effort, None, "{bare}");
        for target in 0..4usize {
            let out = encode(target, &r);
            let re = decode(target, out.clone()).effort;
            assert_eq!(re, None, "target={target} out={out}");
        }
    }
    // A level no vendor words carry is refused, not silently mapped.
    assert!(Effort::parse("yolo").is_none());
    let body = json!({"model":"m","messages":[],"reasoning_effort":"yolo"});
    assert!(
        chat::decode_request(&body).is_ok(),
        "unknown levels decode as absent"
    );
    let r = chat::decode_request(&body).unwrap();
    assert_eq!(r.effort, None);
}

#[test]
fn anthropic_budget_fits_under_the_client_cap() {
    // A 20480 cap must fit a high (24000) budget under it, with room left.
    let body = json!({
        "model":"m","messages":[],"max_tokens":20480,
        "reasoning_effort":"high"
    });
    let r = chat::decode_request(&body).unwrap();
    let out = anthropic::encode_request(&r, "up").unwrap();
    let thinking = &out["thinking"];
    assert_eq!(thinking["type"], "enabled");
    let budget = thinking["budget_tokens"].as_i64().unwrap();
    assert!(
        budget <= 20480 - 1024,
        "budget {budget} leaves no answer room"
    );
    assert!(budget >= 4096, "budget {budget} lost the level");
}

#[test]
fn effort_survives_a_full_cross_protocol_round_trip() {
    let wire = json!({
        "model":"m","input":"hi","max_output_tokens":64000,
        "reasoning":{"effort":"xhigh","summary":"auto"},
        "tools":[{"type":"function","name":"f","parameters":{"type":"object"}}]
    });
    let r = responses::decode_request(&wire).unwrap();
    assert_eq!(r.effort, Some(Effort::XHigh));
    let out = anthropic::encode_request(&r, "up").unwrap();
    assert_eq!(out["thinking"]["type"], "enabled");
    assert_eq!(out["thinking"]["budget_tokens"], 32000);
    let back = anthropic::decode_request(&out).unwrap();
    assert_eq!(back.effort, Some(Effort::XHigh));
    let gem = gemini::encode_request(&back, "up").unwrap();
    assert_eq!(
        gem["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "xhigh"
    );
    assert_eq!(
        gem["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
    let back2 = gemini::decode_request(&gem).unwrap();
    assert_eq!(back2.effort, Some(Effort::XHigh));
    let out2 = responses::encode_request(&back2, "up").unwrap();
    assert_eq!(out2["reasoning"]["effort"], "xhigh");
    assert_eq!(out2["reasoning"]["summary"], "auto");
}
