use magpie_core::ir::{CompletionRequest, PartKind};
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

fn body() -> Value {
    json!({"model":"m","input":"hello","tools":[
        {"type":"function","name":"ordinary","parameters":{"type":"object"}},
        {"type":"namespace","name":"collaboration","description":"team tools","tools":[
            {"type":"function","name":"spawn_agent","description":"Start a worker","parameters":{"type":"object","properties":{"message":{"type":"string"}}}},
            {"type":"function","name":"wait","parameters":{"type":"object"}}
        ]},
        {"type":"namespace","name":"other","tools":[{"type":"function","name":"spawn_agent","parameters":{"type":"object"}}]}
    ]})
}
fn encode(protocol: usize, req: &CompletionRequest) -> magpie_core::Result<Value> {
    match protocol {
        0 => chat::encode_request(req, "up-model"),
        1 => responses::encode_request(req, "up-model"),
        2 => anthropic::encode_request(req, "up-model"),
        _ => gemini::encode_request(req, "up-model"),
    }
}
fn names(protocol: usize, v: &Value) -> Vec<&str> {
    match protocol {
        0 => v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect(),
        1 => v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| {
                if t["type"] == "namespace" {
                    let n = t["name"].as_str().unwrap();
                    t["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|f| {
                            Box::leak(
                                format!("{n}__{}", f["name"].as_str().unwrap()).into_boxed_str(),
                            ) as &str
                        })
                        .collect::<Vec<_>>()
                } else if t["type"] == "web_search" {
                    Vec::new()
                } else {
                    vec![t["name"].as_str().unwrap()]
                }
            })
            .collect(),
        3 => v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| {
                t["functionDeclarations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| f["name"].as_str().unwrap())
            })
            .collect(),
        _ => v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect(),
    }
}
#[test]
fn namespaced_declarations_are_flattened_in_all_provider_dialects() {
    let req = responses::decode_request(&body()).unwrap();
    for p in 0..4 {
        let wire = encode(p, &req).unwrap();
        assert_eq!(
            names(p, &wire),
            [
                "ordinary",
                "collaboration__spawn_agent",
                "collaboration__wait",
                "other__spawn_agent"
            ],
            "{wire}"
        );
        assert!(wire.to_string().contains("Start a worker"));
        assert!(wire.to_string().contains("message"));
    }
}
#[test]
fn namespace_selectors_filter_the_correct_function_even_with_search() {
    for search in [false, true] {
        for choice in [
            json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","namespace":"collaboration","name":"spawn_agent"}]}),
            json!({"type":"function","namespace":"collaboration","name":"spawn_agent"}),
        ] {
            let mut v = body();
            v["tool_choice"] = choice;
            if search {
                v["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"web_search"}));
            }
            let req = responses::decode_request(&v).unwrap();
            for p in 0..4 {
                let wire = encode(p, &req).unwrap();
                assert!(names(p, &wire).contains(&"collaboration__spawn_agent") || p == 1);
                if p != 1 {
                    assert!(!wire.to_string().contains("\"namespace\""));
                }
                if req.tool_choice.as_ref().unwrap()["type"] == "allowed_tools" {
                    assert_eq!(names(p, &wire), ["collaboration__spawn_agent"]);
                    assert!(wire.to_string().find("web_search").is_none());
                }
            }
        }
    }
}
#[test]
fn namespaced_input_calls_are_flattened_without_a_current_declaration() {
    let mut v = body();
    v["tools"] = json!([]);
    v["input"] = json!([
        {"type":"function_call","call_id":"call1","namespace":"collaboration","name":"spawn_agent","arguments":"{\"message\":\"hi\"}","encrypted_function_args":[]},
        {"type":"function_call_output","call_id":"call1","output":"done"}
    ]);
    let req = responses::decode_request(&v).unwrap();
    let call = req
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .find(|p| p.kind() == PartKind::ToolCall)
        .unwrap();
    assert_eq!(call.name, "collaboration__spawn_agent");
    assert_eq!(call.args, json!({"message":"hi"}));
    for p in 0..4 {
        let wire = encode(p, &req).unwrap();
        if p != 1 {
            assert!(wire.to_string().contains("collaboration__spawn_agent"));
        }
        assert!(wire.to_string().contains("call1") || p == 3);
        if p == 1 {
            assert_eq!(wire["input"][0]["name"], "spawn_agent");
            assert_eq!(wire["input"][0]["namespace"], "collaboration");
        }
    }
}
#[test]
fn ambiguous_flat_names_and_namespace_selectors_are_rejected() {
    for duplicate in [
        json!({"type":"function","name":"collaboration__spawn_agent"}),
        json!({"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]}),
    ] {
        let mut v = body();
        v["tools"].as_array_mut().unwrap().push(duplicate);
        assert!(responses::decode_request(&v).is_err(), "{v}");
    }
    let mut v = json!({"model":"m","input":"hi","tools":[{"type":"function","name":"collaboration__spawn_agent"}]});
    v["tool_choice"] = json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","namespace":"collaboration","name":"spawn_agent"}]});
    assert!(responses::decode_request(&v).is_err());
}
#[test]
fn malformed_namespace_declarations_calls_and_selectors_fail_closed() {
    for declaration in [
        json!({"type":"namespace","name":"","tools":[]}),
        json!({"type":"namespace","name":42,"tools":[]}),
        json!({"type":"namespace","name":"n","tools":{}}),
        json!({"type":"namespace","name":"n","tools":[{"type":"function","name":""}]}),
        json!({"type":"namespace","name":"n","tools":[{"type":"namespace","name":"nested","tools":[]}]}),
        json!({"type":"namespace","name":"n","tools":[{"type":"custom","name":"freeform"}]}),
    ] {
        let mut v = body();
        v["tools"] = json!([declaration]);
        assert!(responses::decode_request(&v).is_err(), "{v}");
    }
    for namespace in [json!(""), json!(42), Value::Null] {
        let mut v = body();
        v["input"] = json!([{"type":"function_call","call_id":"c","name":"spawn_agent","namespace":namespace,"arguments":"{}"}]);
        assert!(responses::decode_request(&v).is_err(), "{v}");
        v["input"] = json!("hi");
        v["tool_choice"] = json!({"type":"function","name":"spawn_agent","namespace":namespace});
        let req = responses::decode_request(&v).unwrap();
        for p in 0..4 {
            assert!(encode(p, &req).is_err(), "protocol={p} {v}");
        }
    }
}
