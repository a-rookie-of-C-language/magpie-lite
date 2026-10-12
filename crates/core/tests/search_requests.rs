//! Built-in search is executed by a provider, never an agent function.
use magpie_core::ir::CompletionRequest;
use magpie_core::{anthropic, chat, gemini, responses};
use serde_json::{Value, json};

fn sources() -> Vec<CompletionRequest> {
    vec![
        anthropic::decode_request(&json!({"model":"client", "messages":[],
            "tools":[{"type":"web_search_20250305","name":"web_search"}]}))
        .unwrap(),
        responses::decode_request(&json!({"model":"client", "input":"search",
            "tools":[{"type":"web_search"}]}))
        .unwrap(),
        gemini::decode_request(&json!({"model":"client", "contents":[],
            "tools":[{"googleSearch":{}}]}))
        .unwrap(),
        chat::decode_request(&json!({"model":"client", "messages":[],
            "web_search_options":{}}))
        .unwrap(),
    ]
}

#[test]
fn every_client_can_enable_responses_search_without_creating_a_function() {
    for request in sources() {
        assert!(request.tools.is_empty());
        let out = responses::encode_request(&request, "upstream").unwrap();
        assert!(
            out["tools"]
                .as_array()
                .is_some_and(|ts| ts.iter().any(|t| t["type"] == "web_search")),
            "{out}"
        );
        assert_eq!(out["include"], json!(["web_search_call.action.sources"]));
    }
}

#[test]
fn native_anthropic_search_options_survive() {
    let tool = json!({"type":"web_search_20250305", "name":"web_search", "max_uses":2,
        "allowed_domains":["example.invalid"], "user_location":{"type":"approximate","country":"CN"}});
    let request = anthropic::decode_request(&json!({"model":"client", "messages":[],
        "tools":[tool], "tool_choice":{"type":"tool","name":"web_search"}}))
    .unwrap();
    let out = anthropic::encode_request(&request, "upstream").unwrap();
    assert_eq!(out["tools"], json!([tool]));
    assert_eq!(
        out["tool_choice"],
        json!({"type":"tool","name":"web_search"})
    );
}

#[test]
fn native_gemini_search_is_not_lost_next_to_function_declarations() {
    let request = gemini::decode_request(&json!({"model":"client", "contents":[],
        "tools":[{"google_search":{"exclude_domains":["example.invalid"]},
            "functionDeclarations":[{"name":"read","parameters":{"type":"object"}}]}]}))
    .unwrap();
    let out = gemini::encode_request(&request, "upstream").unwrap();
    assert_eq!(out["tools"][0]["functionDeclarations"][0]["name"], "read");
    assert_eq!(
        out["tools"][1],
        json!({"google_search":{"exclude_domains":["example.invalid"]}})
    );
}

fn encode(target: usize, request: &CompletionRequest) -> magpie_core::Result<Value> {
    match target {
        0 => anthropic::encode_request(request, "upstream"),
        1 => responses::encode_request(request, "upstream"),
        2 => gemini::encode_request(request, "upstream"),
        3 => chat::encode_request(request, "upstream"),
        _ => unreachable!(),
    }
}

fn assert_search(target: usize, out: &Value) {
    match target {
        0 => assert!(out["tools"].as_array().unwrap().iter().any(|t| {
            t["type"]
                .as_str()
                .is_some_and(|s| s.starts_with("web_search"))
        })),
        1 => assert!(
            out["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["type"] == "web_search")
        ),
        2 => assert!(
            out["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["googleSearch"] == json!({}))
        ),
        3 => assert_eq!(out["web_search_options"], json!({})),
        _ => unreachable!(),
    }
}

#[test]
fn all_four_protocols_translate_bare_search_intent_to_all_four_upstreams() {
    for request in sources() {
        assert!(request.web_search.is_some());
        for target in 0..4 {
            let out = encode(target, &request).unwrap();
            assert_eq!(out["model"], "upstream");
            assert_search(target, &out);
        }
    }
}

#[test]
fn native_responses_options_and_allowed_tools_choice_survive() {
    let tool = json!({"type":"web_search_preview", "search_context_size":"low",
        "filters":{"allowed_domains":["private.invalid"]}, "future_policy":{"restrict":true}});
    let choice =
        json!({"type":"allowed_tools", "mode":"auto", "tools":[{"type":"web_search_preview"}]});
    let request = responses::decode_request(&json!({"model":"client", "input":"find",
        "tools":[tool], "tool_choice":choice}))
    .unwrap();
    let native = responses::encode_request(&request, "upstream").unwrap();
    assert_eq!(native["tools"], json!([tool]));
    assert_eq!(native["tool_choice"], choice);
    for target in [0, 2, 3] {
        let error = encode(target, &request).unwrap_err().to_string();
        assert!(error.contains("cannot be translated"), "{error}");
        assert!(!error.contains("private.invalid"));
    }
}

#[test]
fn configured_search_options_never_disappear_on_cross_protocol_routes() {
    let requests = [
        anthropic::decode_request(&json!({"model":"m", "messages":[], "tools":[{
            "type":"web_search_20250305","name":"web_search", "blocked_domains":["secret.invalid"]}]})).unwrap(),
        responses::decode_request(&json!({"model":"m", "tools":[{
            "type":"web_search", "user_location":{"type":"approximate","country":"CN"}}]})).unwrap(),
        gemini::decode_request(&json!({"model":"m", "tools":[{"googleSearch":{"excludeDomains":["secret.invalid"]}}]})).unwrap(),
        chat::decode_request(&json!({"model":"m", "messages":[], "web_search_options":{"search_context_size":"low"}})).unwrap(),
    ];
    for (native, request) in requests.iter().enumerate() {
        assert!(encode(native, request).is_ok());
        for target in 0..4 {
            if target != native {
                assert!(encode(target, request).is_err());
            }
        }
    }
}

#[test]
fn explicit_none_omits_hosted_search_even_if_native_options_cannot_translate() {
    let requests = [
        anthropic::decode_request(&json!({"model":"m", "messages":[], "tools":[{
            "type":"web_search_20250305","name":"web_search", "max_uses":2}], "tool_choice":{"type":"none"}})).unwrap(),
        responses::decode_request(&json!({"model":"m", "tools":[{"type":"web_search", "filters":{"allowed_domains":["private.invalid"]}}], "tool_choice":"none"})).unwrap(),
    ];
    for request in requests {
        for target in 0..4 {
            let out = encode(target, &request).unwrap();
            assert!(out.get("tools").is_none(), "{out}");
            for key in ["web_search_options", "tool_choice", "toolConfig", "include"] {
                assert!(out.get(key).is_none(), "{out}");
            }
        }
    }
}

#[test]
fn builtin_search_coexists_with_functions_and_translates_named_function_choice() {
    let request = responses::decode_request(&json!({"model":"m", "input":"find",
        "tools":[{"type":"web_search"}, {"type":"function","name":"read","parameters":{"type":"object"}}],
        "tool_choice":{"type":"function","name":"read"}})).unwrap();
    assert_eq!(request.tools.len(), 1);
    let expected_choices = [
        json!({"type":"tool","name":"read"}),
        json!({"type":"function","name":"read"}),
        json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["read"]}}),
        json!({"type":"function","function":{"name":"read"}}),
    ];
    for (target, expected) in expected_choices.iter().enumerate() {
        let out = encode(target, &request).unwrap();
        assert_search(target, &out);
        assert_eq!(
            &out[if target == 2 {
                "toolConfig"
            } else {
                "tool_choice"
            }],
            expected
        );
        assert_eq!(
            out["tools"].as_array().unwrap().len(),
            if target == 3 { 1 } else { 2 }
        );
    }
}

#[test]
fn native_gemini_multiple_function_allowlist_is_preserved_not_broadened() {
    let choice =
        json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["read","write"]}});
    let request = gemini::decode_request(&json!({"model":"m", "tools":[{"googleSearch":{},
        "functionDeclarations":[{"name":"read"},{"name":"write"}]}], "toolConfig":choice}))
    .unwrap();
    assert_eq!(
        gemini::encode_request(&request, "m").unwrap()["toolConfig"],
        choice
    );
    for target in [0, 1, 3] {
        assert!(encode(target, &request).is_err());
    }
}

#[test]
fn required_and_named_search_choices_are_never_silently_downgraded() {
    for choice in [json!("required"), json!({"type":"web_search"})] {
        let request =
            responses::decode_request(&json!({"model":"m", "tools":[{"type":"web_search"}],
            "tool_choice":choice}))
            .unwrap();
        let out = anthropic::encode_request(&request, "m").unwrap();
        assert_eq!(
            out["tool_choice"],
            if choice == "required" {
                json!({"type":"any"})
            } else {
                json!({"type":"tool","name":"web_search"})
            }
        );
        for target in [2, 3] {
            assert!(encode(target, &request).is_err());
        }
    }
    let request = anthropic::decode_request(&json!({"model":"m", "messages":[],
        "tools":[{"type":"web_search_20250305","name":"web_search"}],
        "tool_choice":{"type":"tool","name":"web_search"}}))
    .unwrap();
    assert_eq!(
        responses::encode_request(&request, "m").unwrap()["tool_choice"],
        json!({"type":"web_search"})
    );
}

#[test]
fn functions_named_web_search_are_not_builtin_tools() {
    let requests = [
        anthropic::decode_request(&json!({"model":"m", "messages":[], "tools":[{"name":"web_search","input_schema":{}}]})).unwrap(),
        responses::decode_request(&json!({"model":"m", "tools":[{"type":"function","name":"web_search","parameters":{}}]})).unwrap(),
        gemini::decode_request(&json!({"model":"m", "tools":[{"functionDeclarations":[{"name":"web_search"}]}]})).unwrap(),
        chat::decode_request(&json!({"model":"m", "messages":[], "tools":[{"type":"function","function":{"name":"web_search","parameters":{}}}]})).unwrap(),
    ];
    for request in requests {
        assert!(request.web_search.is_none());
        assert_eq!(request.tools.len(), 1);
        for target in 0..4 {
            let out = encode(target, &request).unwrap();
            assert!(out.get("web_search_options").is_none());
            assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        }
    }
}

#[test]
fn absent_null_duplicate_and_malformed_search_options_are_distinct() {
    for value in [
        json!({"model":"m","messages":[]}),
        json!({"model":"m","messages":[],"web_search_options":null}),
    ] {
        let request = chat::decode_request(&value).unwrap();
        assert!(request.web_search.is_none());
        for target in 0..4 {
            assert!(encode(target, &request).unwrap().get("tools").is_none());
        }
    }
    for invalid in [json!(false), json!(7), json!(""), json!([])] {
        assert!(
            chat::decode_request(&json!({"model":"m","messages":[],"web_search_options":invalid}))
                .is_err()
        );
        assert!(
            gemini::decode_request(&json!({"model":"m","tools":[{"googleSearch":invalid}]}))
                .is_err()
        );
    }
    assert!(gemini::decode_request(&json!({"model":"m","tools":[{"googleSearch":null}]})).is_err());
    assert!(
        responses::decode_request(
            &json!({"model":"m","tools":[{"type":"web_search"},{"type":"web_search_preview"}]})
        )
        .is_err()
    );
    assert!(anthropic::decode_request(&json!({"model":"m","messages":[],"tools":[{"type":"web_search_20250305"},{"type":"web_search_20250305"}]})).is_err());
    assert!(
        gemini::decode_request(
            &json!({"model":"m","tools":[{"googleSearch":{},"google_search":{}}]})
        )
        .is_err()
    );
    assert!(chat::decode_request(&json!({"model":"m","messages":[],"web_search_options":{},"tools":[{"type":"web_search"}]})).is_err());
    let marker =
        chat::decode_request(&json!({"model":"m","messages":[],"tools":[{"type":"web_search"}]}))
            .unwrap();
    assert_search(3, &chat::encode_request(&marker, "m").unwrap());
}

#[test]
fn chat_and_gemini_none_disable_functions_not_the_explicit_hosted_search() {
    let requests = [
        (3, chat::decode_request(&json!({"model":"m", "messages":[], "web_search_options":{},
            "tools":[{"type":"function","function":{"name":"read"}}], "tool_choice":"none"})).unwrap()),
        (2, gemini::decode_request(&json!({"model":"m", "tools":[{"googleSearch":{},
            "functionDeclarations":[{"name":"read"}]}], "toolConfig":{"functionCallingConfig":{"mode":"NONE"}}})).unwrap()),
    ];
    for (native, request) in requests {
        for target in 0..4 {
            let out = encode(target, &request).unwrap();
            assert_search(target, &out);
            let tools = out.get("tools").and_then(Value::as_array);
            assert_eq!(
                tools.map_or(0, Vec::len),
                if native == target {
                    if target == 3 { 1 } else { 2 }
                } else if target == 3 {
                    0
                } else {
                    1
                },
                "{out}"
            );
            if target == native {
                assert_eq!(
                    out[if target == 3 {
                        "tool_choice"
                    } else {
                        "toolConfig"
                    }],
                    if target == 3 {
                        json!("none")
                    } else {
                        json!({"functionCallingConfig":{"mode":"NONE"}})
                    }
                );
            }
        }
    }
}

#[test]
fn function_only_requirements_do_not_become_global_requirements() {
    let requests = [
        (3, chat::decode_request(&json!({"model":"m", "messages":[], "web_search_options":{},
            "tools":[{"type":"function","function":{"name":"read"}}], "tool_choice":"required"})).unwrap()),
        (2, gemini::decode_request(&json!({"model":"m", "tools":[{"googleSearch":{},
            "functionDeclarations":[{"name":"read"}]}], "toolConfig":{"functionCallingConfig":{"mode":"ANY"}}})).unwrap()),
    ];
    for (native, request) in requests {
        assert!(encode(native, &request).is_ok());
        for target in 0..4 {
            if target != native {
                assert!(encode(target, &request).is_err());
            }
        }
    }
}

#[test]
fn native_disabled_choice_keeps_additional_restrictions_for_remaining_functions() {
    let choice = json!({"type":"none", "disable_parallel_tool_use":true});
    let request = anthropic::decode_request(&json!({"model":"m", "messages":[],
        "tools":[{"type":"web_search_20250305","name":"web_search"},{"name":"read","input_schema":{}}],
        "tool_choice":choice})).unwrap();
    let out = anthropic::encode_request(&request, "m").unwrap();
    assert_eq!(out["tool_choice"], choice);
    assert_eq!(out["tools"].as_array().unwrap().len(), 1);
}
