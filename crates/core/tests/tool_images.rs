//! Tool-returned screenshots must survive client decoding and upstream encoding.
use magpie_core::{anthropic, chat, gemini, ir::CompletionRequest, responses};
use serde_json::{Value, json};

const PNG: &str = "c2NyZWVuc2hvdA==";
const URL: &str = "https://example.invalid/private-shot.jpg";
const TEXT: &str = "PRIVATE_TOOL_TEXT";

type Decode = fn(&Value) -> magpie_core::Result<CompletionRequest>;
type Encode = fn(&CompletionRequest, &str) -> magpie_core::Result<Value>;

fn clients() -> Vec<(&'static str, Decode, Value)> {
    vec![
        (
            "Anthropic",
            anthropic::decode_request,
            json!({"model":"route/model","max_tokens":64,"messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"A","name":"screenshot","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"A","is_error":true,"content":[
                    {"type":"text","text":TEXT},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
                    {"type":"image","source":{"type":"url","url":URL}}
                ]},{"type":"text","text":"continue"}]}
            ]}),
        ),
        (
            "Chat",
            chat::decode_request,
            json!({"model":"route/model","messages":[
                {"role":"assistant","tool_calls":[{"id":"A","type":"function","function":{"name":"screenshot","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"A","is_error":true,"content":[
                    {"type":"text","text":TEXT},
                    {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}},
                    {"type":"image_url","image_url":{"url":URL}}
                ]},
                {"role":"user","content":"continue"}
            ]}),
        ),
        (
            "Responses",
            responses::decode_request,
            json!({"model":"route/model","input":[
                {"type":"function_call","call_id":"A","name":"screenshot","arguments":"{}"},
                {"type":"function_call_output","call_id":"A","output":[
                    {"type":"input_text","text":TEXT},
                    {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")},
                    {"type":"input_image","image_url":URL}
                ]},
                {"role":"user","content":"continue"}
            ]}),
        ),
        (
            "Gemini",
            gemini::decode_request,
            json!({"model":"route/model","contents":[
                {"role":"model","parts":[{"functionCall":{"id":"A","name":"screenshot","args":{}}}]},
                {"role":"user","parts":[{"functionResponse":{"id":"A","name":"screenshot","response":{"error":TEXT},"parts":[
                    {"inlineData":{"mimeType":"image/png","data":PNG}},
                    {"fileData":{"mimeType":"image/jpeg","fileUri":URL}}
                ]}},{"text":"continue"}]}
            ]}),
        ),
    ]
}

fn encoders() -> [(&'static str, Encode); 4] {
    [
        ("Chat", chat::encode_request),
        ("Anthropic", anthropic::encode_request),
        ("Responses", responses::encode_request),
        ("Gemini", gemini::encode_request),
    ]
}

fn image_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(a) => a.iter().for_each(|v| image_refs(v, out)),
        Value::Object(o) => {
            if let Some(image) = o.get("image_url")
                && let Some(url) = image
                    .as_str()
                    .or_else(|| image.get("url").and_then(Value::as_str))
            {
                out.push(url.into());
            }
            if o.get("type").is_some_and(|t| t == "image") {
                let source = &value["source"];
                if let Some(data) = source["data"].as_str() {
                    out.push(format!(
                        "data:{};base64,{data}",
                        source["media_type"].as_str().unwrap()
                    ));
                } else if let Some(url) = source["url"].as_str() {
                    out.push(url.into());
                }
            }
            if let Some(inline) = o.get("inlineData") {
                out.push(format!(
                    "data:{};base64,{}",
                    inline["mimeType"].as_str().unwrap(),
                    inline["data"].as_str().unwrap()
                ));
            }
            if let Some(file) = o.get("fileData") {
                out.push(file["fileUri"].as_str().unwrap().into());
            }
            o.values().for_each(|v| image_refs(v, out));
        }
        _ => {}
    }
}

#[test]
fn four_client_tool_images_reach_all_four_upstream_encoders() {
    for (client, decode, body) in clients() {
        let ir = decode(&body).unwrap();
        let before = format!("{ir:?}");
        for (upstream, encode) in encoders() {
            let wire = encode(&ir, "model").unwrap();
            let mut images = Vec::new();
            image_refs(&wire, &mut images);
            assert_eq!(
                images,
                [format!("data:image/png;base64,{PNG}"), URL.into()],
                "{client} -> {upstream}: {wire}"
            );
            match upstream {
                "Chat" => {
                    let messages = wire["messages"].as_array().unwrap();
                    assert_eq!(messages[1]["role"], "tool");
                    let text = messages[1]["content"].as_str().unwrap();
                    assert!(
                        text.starts_with(TEXT) && text.contains("2 images"),
                        "{wire}"
                    );
                    assert_eq!(messages[2]["role"], "user");
                    assert!(
                        messages[2]["content"][0]["text"]
                            .as_str()
                            .unwrap()
                            .contains("screenshot (tool call A)")
                    );
                    assert!(messages[2].to_string().contains("continue"), "{wire}");
                }
                "Anthropic" => {
                    let result = &wire["messages"][1]["content"][0];
                    assert_eq!(result["type"], "tool_result");
                    assert_eq!(result["content"][0]["text"], TEXT);
                    let mut nested = Vec::new();
                    image_refs(result, &mut nested);
                    assert_eq!(nested, images);
                    if client != "Responses" {
                        assert_eq!(result["is_error"], true);
                    }
                }
                "Responses" => {
                    let output = &wire["input"][1];
                    assert_eq!(output["type"], "function_call_output");
                    assert_eq!(
                        output["output"][0],
                        json!({"type":"input_text","text":TEXT})
                    );
                    let mut nested = Vec::new();
                    image_refs(output, &mut nested);
                    assert_eq!(nested, images);
                }
                "Gemini" => {
                    let result = &wire["contents"][1]["parts"][0]["functionResponse"];
                    assert_eq!(result["name"], "screenshot");
                    assert_eq!(
                        result["response"][if client == "Responses" {
                            "output"
                        } else {
                            "error"
                        }],
                        TEXT
                    );
                    let mut nested = Vec::new();
                    image_refs(result, &mut nested);
                    assert_eq!(nested, images);
                }
                _ => unreachable!(),
            }
            assert_eq!(format!("{ir:?}"), before, "source IR mutated");
            assert_eq!(
                encode(&ir, "model").unwrap(),
                wire,
                "nondeterministic output"
            );
        }
    }
}

#[test]
fn parallel_chat_tool_images_wait_for_all_results_across_client_messages() {
    let body = json!({"model":"m","messages":[
        {"role":"assistant","content":[
            {"type":"tool_use","id":"A","name":"screenshot","input":{}},
            {"type":"tool_use","id":"B","name":"Read","input":{}}
        ]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"B","content":[
            {"type":"image","source":{"type":"url","url":URL}}
        ]}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"A","content":[
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}
        ]}]},
        {"role":"user","content":"compare"}
    ]});
    let ir = anthropic::decode_request(&body).unwrap();
    let wire = chat::encode_request(&ir, "m").unwrap();
    let m = wire["messages"].as_array().unwrap();
    assert_eq!(m.len(), 4, "{wire}");
    assert_eq!(m[1]["tool_call_id"], "A");
    assert_eq!(m[2]["tool_call_id"], "B");
    assert!(!wire.to_string().contains("unavailable"));
    let mut images = Vec::new();
    image_refs(&m[3], &mut images);
    assert_eq!(images, [format!("data:image/png;base64,{PNG}"), URL.into()]);
    assert!(m[3].to_string().contains("compare"));
    assert!(m[3].to_string().contains("Read (tool call B)"));
}

#[test]
fn orphan_image_results_keep_images_without_inventing_calls() {
    let body = json!({"model":"m","messages":[{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"missing","content":[
            {"type":"image","source":{"type":"url","url":URL}}
        ]}
    ]}]});
    let wire = chat::encode_request(&anthropic::decode_request(&body).unwrap(), "m").unwrap();
    let m = wire["messages"].as_array().unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["role"], "user");
    let mut images = Vec::new();
    image_refs(&wire, &mut images);
    assert_eq!(images, [URL]);
    assert!(!wire.to_string().contains("tool_calls"));
}

#[test]
fn gemini_function_response_part_text_and_snake_case_images_are_not_lost() {
    let body = json!({"model":"m","contents":[
        {"role":"model","parts":[{"function_call":{"id":"A","name":"run","args":{}}}]},
        {"role":"user","parts":[{"function_response":{"id":"A","name":"run","response":{"output":"first"},"parts":[
            {"text":"second"},
            {"text":"private thought", "thought":true},
            {"inline_data":{"mime_type":"image/png","data":PNG}}
        ]}}]}
    ]});
    let wire = anthropic::encode_request(&gemini::decode_request(&body).unwrap(), "m").unwrap();
    let result = &wire["messages"][1]["content"][0];
    assert_eq!(result["content"][0]["text"], "first\n\nsecond");
    let mut images = Vec::new();
    image_refs(result, &mut images);
    assert_eq!(images, [format!("data:image/png;base64,{PNG}")]);
    assert!(!wire.to_string().contains("private thought"));
}

#[test]
fn responses_structured_output_text_is_not_json_stringified_with_image_payloads() {
    let (_, decode, body) = clients()
        .into_iter()
        .find(|(name, _, _)| *name == "Responses")
        .unwrap();
    let ir = decode(&body).unwrap();
    let tool = ir
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .find(|p| p.kind() == magpie_core::ir::PartKind::ToolResult)
        .unwrap();
    assert_eq!(tool.text, TEXT, "structured output is not plain-text JSON");
}

#[test]
fn tool_images_remain_owned_by_the_answer_in_shared_ir() {
    for (name, decode, body) in clients() {
        let ir = decode(&body).unwrap();
        let results: Vec<_> = ir
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| p.kind() == magpie_core::ir::PartKind::ToolResult)
            .collect();
        assert_eq!(results.len(), 1, "{name}");
        assert_eq!(results[0].images.len(), 2, "{name}");
        assert_eq!(results[0].images[0].media_type, "image/png");
        assert_eq!(results[0].images[0].data, PNG);
        assert_eq!(results[0].images[1].url, URL);
        assert!(
            !ir.messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| p.kind() == magpie_core::ir::PartKind::Image),
            "detached images: {name}"
        );
    }
}

#[test]
fn duplicate_chat_answer_does_not_deliver_its_image() {
    let body = json!({"model":"m","messages":[
        {"role":"assistant","content":[{"type":"tool_use","id":"A","name":"Read","input":{}}]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"A","content":"first"},
            {"type":"tool_result","tool_use_id":"A","content":[{"type":"image","source":{"type":"url","url":URL}}]}
        ]}
    ]});
    let wire = chat::encode_request(&anthropic::decode_request(&body).unwrap(), "m").unwrap();
    assert_eq!(wire["messages"][1]["content"], "first");
    let mut images = Vec::new();
    image_refs(&wire, &mut images);
    assert!(images.is_empty(), "duplicate answer delivered: {wire}");
}

#[test]
fn late_chat_result_preserves_its_image_after_an_interrupted_exchange() {
    let body = json!({"model":"m","messages":[
        {"role":"assistant","content":[{"type":"tool_use","id":"A","name":"Read","input":{}}]},
        {"role":"user","content":"interrupted"},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"A","content":[
            {"type":"text","text":"late"}, {"type":"image","source":{"type":"url","url":URL}}
        ]}]}
    ]});
    let wire = chat::encode_request(&anthropic::decode_request(&body).unwrap(), "m").unwrap();
    assert!(
        wire["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    assert_eq!(wire["messages"][2]["role"], "user");
    assert!(wire["messages"][2].to_string().contains("interrupted"));
    let mut images = Vec::new();
    image_refs(&wire, &mut images);
    assert_eq!(images, [URL]);
}

#[test]
fn responses_plain_strings_and_untyped_json_outputs_keep_their_content() {
    for output in [
        json!("plain"),
        json!({"answer":[1,2]}),
        json!([1, 2]),
        json!(null),
        json!([]),
    ] {
        let body = json!({"model":"m","input":[{"type":"function_call_output","call_id":"A","output":output}]});
        let ir = responses::decode_request(&body).unwrap();
        assert_eq!(
            ir.messages[0].parts[0].text,
            output
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| output.to_string())
        );
        assert!(ir.messages[0].parts[0].images.is_empty());
    }
}

#[test]
fn unsupported_file_ids_empty_images_and_nonimage_data_are_not_fabricated_images() {
    let body = json!({"model":"m","input":[{"type":"function_call_output","call_id":"A","output":[
        {"type":"input_text","text":"kept"},
        {"type":"input_image","file_id":"file_private"},
        {"type":"input_image","image_url":""},
        {"type":"input_image","image_url":"data:application/pdf;base64,cGRm"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":""}}
    ]}]});
    let ir = responses::decode_request(&body).unwrap();
    assert_eq!(ir.messages[0].parts[0].text, "kept");
    assert!(ir.messages[0].parts[0].images.is_empty());
    for (_, encode) in encoders() {
        let wire = encode(&ir, "m").unwrap();
        assert!(!wire.to_string().contains("file_private"));
        assert!(!wire.to_string().contains("cGRm"));
        let mut images = Vec::new();
        image_refs(&wire, &mut images);
        assert!(images.is_empty());
    }
}
