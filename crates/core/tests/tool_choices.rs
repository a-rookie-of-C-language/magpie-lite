//! Tool selection and allowlists must keep the caller's constraints across dialects.
use magpie_core::{anthropic, chat, gemini, ir::CompletionRequest, responses};
use serde_json::{Value, json};

fn decode(source: usize, choice: Value, search: bool) -> CompletionRequest {
    let tools = ["safeA", "forbidden", "safeB"];
    let body = match source {
        0 => {
            json!({"model":"m","messages":[],"tools":tools.iter().map(|name| json!({"type":"function","function":{"name":name,"parameters":{"type":"object"}}})).collect::<Vec<_>>(),"tool_choice":choice})
        }
        1 => {
            json!({"model":"m","input":"hello","tools":tools.iter().map(|name| json!({"type":"function","name":name,"parameters":{"type":"object"}})).chain(search.then(|| json!({"type":"web_search"}))).collect::<Vec<_>>(),"tool_choice":choice})
        }
        2 => {
            json!({"model":"m","messages":[],"tools":tools.iter().map(|name| json!({"name":name,"input_schema":{"type":"object"}})).collect::<Vec<_>>(),"tool_choice":choice})
        }
        3 => {
            json!({"model":"m","contents":[],"tools":[{"functionDeclarations":tools.iter().map(|name| json!({"name":name,"parameters":{"type":"object"}})).collect::<Vec<_>>()}],"toolConfig":choice})
        }
        _ => unreachable!(),
    };
    match source {
        0 => chat::decode_request(&body),
        1 => responses::decode_request(&body),
        2 => anthropic::decode_request(&body),
        3 => gemini::decode_request(&body),
        _ => unreachable!(),
    }
    .unwrap()
}
fn encode(target: usize, r: &CompletionRequest) -> magpie_core::Result<Value> {
    match target {
        0 => chat::encode_request(r, "upstream"),
        1 => responses::encode_request(r, "upstream"),
        2 => anthropic::encode_request(r, "upstream"),
        3 => gemini::encode_request(r, "upstream"),
        _ => unreachable!(),
    }
}
fn choice(protocol: usize, mode: &str) -> Value {
    match (protocol, mode) {
        (0 | 1, "auto" | "none" | "required") => json!(mode),
        (0, "named") => json!({"type":"function","function":{"name":"safeB"}}),
        (1, "named") => json!({"type":"function","name":"safeB"}),
        (2, "named") => json!({"type":"tool","name":"safeB"}),
        (2, "required") => json!({"type":"any"}),
        (2, "auto" | "none") => json!({"type":mode}),
        (3, "named") => {
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeB"]}})
        }
        (3, "auto") => json!({"functionCallingConfig":{"mode":"AUTO"}}),
        (3, "none") => json!({"functionCallingConfig":{"mode":"NONE"}}),
        (3, "required") => json!({"functionCallingConfig":{"mode":"ANY"}}),
        _ => unreachable!(),
    }
}
fn names(protocol: usize, body: &Value) -> Vec<String> {
    let tools = body["tools"].as_array().unwrap();
    if protocol == 3 {
        return tools
            .iter()
            .filter_map(|t| t["functionDeclarations"].as_array())
            .flatten()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect();
    }
    tools
        .iter()
        .filter_map(|t| {
            if protocol == 0 {
                t["function"]["name"].as_str()
            } else {
                t["name"].as_str()
            }
        })
        .map(str::to_owned)
        .collect()
}
#[test]
fn all_function_choice_modes_translate_in_all_sixteen_directions() {
    for source in 0..4 {
        for mode in ["auto", "none", "required", "named"] {
            let r = decode(source, choice(source, mode), false);
            for target in 0..4 {
                let body = encode(target, &r).unwrap();
                let key = if target == 3 {
                    "toolConfig"
                } else {
                    "tool_choice"
                };
                assert_eq!(
                    body[key],
                    choice(target, mode),
                    "source={source} target={target} mode={mode} body={body}"
                );
            }
        }
    }
}
#[test]
fn responses_allowed_functions_filter_every_upstream_and_do_not_enable_search() {
    for mode in ["auto", "required"] {
        let r = decode(
            1,
            json!({"type":"allowed_tools","mode":mode,"tools":[{"type":"function","name":"safeA"},{"type":"function","name":"safeB"}]}),
            true,
        );
        for target in 0..4 {
            let body = encode(target, &r).unwrap();
            assert_eq!(names(target, &body), ["safeA", "safeB"], "{body}");
            assert!(!body.to_string().contains("web_search"), "{body}");
            assert!(!body.to_string().contains("googleSearch"), "{body}");
            assert!(!body.to_string().contains("forbidden"), "{body}");
            assert_eq!(
                body[if target == 3 {
                    "toolConfig"
                } else {
                    "tool_choice"
                }],
                if target == 1 {
                    r.tool_choice.clone().unwrap()
                } else {
                    choice(target, mode)
                }
            );
        }
    }
}
#[test]
fn gemini_multiple_allowed_functions_remain_a_subset_without_becoming_auto() {
    let r = decode(
        3,
        json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeA","safeB"]}}),
        false,
    );
    for target in 0..4 {
        let body = encode(target, &r).unwrap();
        assert_eq!(names(target, &body), ["safeA", "safeB"], "{body}");
        if target == 3 {
            assert_eq!(
                body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
                json!(["safeA", "safeB"])
            );
        } else {
            assert_eq!(body["tool_choice"], choice(target, "required"));
        }
    }
}
#[test]
fn forced_unknown_or_empty_required_allowlists_never_widen_to_all_functions() {
    for (source, c) in [
        (
            1,
            json!({"type":"allowed_tools","mode":"required","tools":[]}),
        ),
        (
            1,
            json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"unknown"}]}),
        ),
        (
            3,
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["unknown"]}}),
        ),
    ] {
        let r = decode(source, c, false);
        for target in 0..4 {
            assert!(
                encode(target, &r).is_err(),
                "source={source} target={target}"
            );
        }
    }
}
#[test]
fn gemini_validated_and_native_extra_options_are_not_silently_widened() {
    for (source, c) in [
        (
            3,
            json!({"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["safeA","safeB"]}}),
        ),
        (2, json!({"type":"any","disable_parallel_tool_use":true})),
    ] {
        let r = decode(source, c.clone(), false);
        assert_eq!(
            encode(source, &r).unwrap()[if source == 3 {
                "toolConfig"
            } else {
                "tool_choice"
            }],
            c
        );
        for target in 0..4 {
            if source != target {
                assert!(
                    encode(target, &r).is_err(),
                    "source={source} target={target}"
                );
            }
        }
    }
}

#[test]
fn malformed_or_ambiguous_selectors_are_rejected_without_echoing_values() {
    for (source, c) in [
        (3, json!({"functionCallingConfig":{"mode":42}})),
        (
            3,
            json!({"functionCallingConfig":{"mode":"ANY"},"function_calling_config":{"mode":"NONE"}}),
        ),
        (
            3,
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeA"],"allowed_function_names":["forbidden"]}}),
        ),
        (1, json!({"type":"allowed_tools","mode":false,"tools":[]})),
        (
            1,
            json!({"type":"allowed_tools","tools":[{"type":"function","name":"safeA","function":{"name":"forbidden"}}]}),
        ),
        (
            1,
            json!({"type":"allowed_tools","tools":[{"type":"web_search","private_restriction":"secret.invalid"}]}),
        ),
        (
            0,
            json!({"type":"function","name":"safeA","function":{"name":"forbidden"}}),
        ),
    ] {
        let request = decode(source, c, false);
        for target in 0..4 {
            let error = encode(target, &request).unwrap_err().to_string();
            assert!(!error.contains("secret.invalid"));
            assert!(!error.contains("forbidden"));
        }
    }
}

#[test]
fn allowlist_encoding_does_not_mutate_the_shared_request() {
    let request = decode(
        1,
        json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"safeA"}]}),
        true,
    );
    let before = format!("{request:?}");
    for target in [0, 1, 2, 3, 0] {
        let body = encode(target, &request).unwrap();
        assert_eq!(names(target, &body), ["safeA"]);
        assert_eq!(format!("{request:?}"), before);
    }
}

#[test]
fn hosted_search_cannot_bypass_gemini_native_only_or_malformed_constraints() {
    for config in [
        json!({"functionCallingConfig":{"mode":"VALIDATED"}}),
        json!({"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["safeB"]}}),
        json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeB"],"future_restriction":true}}),
    ] {
        let mut request = json!({"model":"m","contents":[],"tools":[{"googleSearch":{}},{"functionDeclarations":[{"name":"safeB","parameters":{"type":"object"}}]}]});
        request["toolConfig"] = config.clone();
        let r = gemini::decode_request(&request).unwrap();
        assert_eq!(encode(3, &r).unwrap()["toolConfig"], config);
        for target in 0..3 {
            assert!(
                encode(target, &r).is_err(),
                "target={target} config={config}"
            );
        }
    }
    for config in [
        json!({"functionCallingConfig":{"mode":42}}),
        json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["safeA"],"allowed_function_names":["safeB"]}}),
        json!({"functionCallingConfig":{"mode":"ANY"},"function_calling_config":{"mode":"NONE"}}),
    ] {
        let r = gemini::decode_request(
            &json!({"model":"m","contents":[],"tools":[{"googleSearch":{}}],"toolConfig":config}),
        )
        .unwrap();
        for target in 0..4 {
            assert!(
                encode(target, &r).is_err(),
                "target={target} config={config}"
            );
        }
    }
}
