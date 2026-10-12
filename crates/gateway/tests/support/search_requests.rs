//! Request-to-upstream fixtures, reusing the lifecycle suite's real HTTP stack.
use super::*;

fn enable_search(client: Protocol, body: &mut Value, functions: bool) {
    match client {
        Protocol::Chat => {
            body["web_search_options"] = json!({});
            if functions {
                body["tools"] = json!([{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]);
            }
        }
        Protocol::Anthropic => {
            body["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]);
            if functions {
                body["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name":"read","input_schema":{"type":"object"}}));
            }
        }
        Protocol::Responses => {
            body["tools"] = json!([{"type":"web_search"}]);
            if functions {
                body["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"function","name":"read","parameters":{"type":"object"}}));
            }
        }
        Protocol::Gemini => {
            body["tools"] = json!([{"googleSearch":{}}]);
            if functions {
                body["tools"].as_array_mut().unwrap().push(json!({"functionDeclarations":[{"name":"read","parameters":{"type":"object"}}]}));
            }
        }
    }
}

fn assert_upstream_search(protocol: Protocol, body: &Value, functions: bool) {
    let tools = body.get("tools").and_then(Value::as_array);
    match protocol {
        Protocol::Chat => assert_eq!(body["web_search_options"], json!({})),
        Protocol::Anthropic => assert!(
            tools
                .unwrap()
                .iter()
                .any(|t| t["type"] == "web_search_20250305" && t["name"] == "web_search")
        ),
        Protocol::Responses => {
            assert!(tools.unwrap().iter().any(|t| t["type"] == "web_search"));
            assert_eq!(body["include"], json!(["web_search_call.action.sources"]));
        }
        Protocol::Gemini => {
            assert!(
                body.get("model").is_none(),
                "model belongs in the Gemini URL"
            );
            assert!(
                tools
                    .unwrap()
                    .iter()
                    .any(|t| t["googleSearch"] == json!({}))
            );
        }
    }
    let function_count = tools
        .map(|tools| {
            tools
                .iter()
                .filter(|t| match protocol {
                    Protocol::Chat | Protocol::Responses => t["type"] == "function",
                    Protocol::Anthropic => t.get("type").is_none(),
                    Protocol::Gemini => t.get("functionDeclarations").is_some(),
                })
                .count()
        })
        .unwrap_or_default();
    assert_eq!(function_count, usize::from(functions), "{body}");
}

async fn post(f: &Fixture, path: &str, body: &Value) -> reqwest::Response {
    tokio::time::timeout(
        LIMIT,
        reqwest::Client::new()
            .post(format!("{}{path}", f.gateway.url))
            .bearer_auth("secret-local-token")
            .json(body)
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
}
async fn text(response: reqwest::Response) -> String {
    tokio::time::timeout(LIMIT, response.text())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn search_requests_enable_real_upstream_search_and_return_results_in_json_and_sse() {
    for upstream in [Protocol::Gemini, Protocol::Responses, Protocol::Anthropic] {
        for client in PROTOCOLS {
            for stream in [false, true] {
                let script = if stream {
                    search_frame(upstream) + completion(upstream)
                } else {
                    search_reply(upstream).to_string()
                };
                let f = Fixture::new(upstream, script, Ending::Eof).await;
                let (path, mut body) = client_request(client, "mock/up-model", stream);
                enable_search(client, &mut body, false);
                let response = post(&f, &path, &body).await;
                assert_eq!(response.status(), 200);
                let reply = text(response).await;
                if client != Protocol::Chat {
                    assert_search_output(&reply, client);
                }
                if stream {
                    assert_completed(&reply, client);
                }
                {
                    let sent = f.sent_bodies.lock().unwrap();
                    assert_eq!(sent.len(), 1);
                    assert_upstream_search(upstream, &sent[0], false);
                }
                assert_eq!(*f.attempts.lock().unwrap(), [0]);
                let record = f.record().await;
                assert_eq!(record["status"], "ok");
                assert_eq!(record["usage"]["input"], 7);
                assert_eq!(record["usage"]["output"], 3);
                assert!(!record.to_string().contains(SEARCH_QUERY));
                assert!(!record.to_string().contains(SEARCH_URL));
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn search_requests_keep_function_declarations_on_every_client_and_upstream_pair() {
    for upstream in PROTOCOLS {
        for client in PROTOCOLS {
            let f = Fixture::new(
                upstream,
                prefix(upstream) + completion(upstream),
                Ending::Eof,
            )
            .await;
            let (path, mut body) = client_request(client, "mock/up-model", true);
            enable_search(client, &mut body, true);
            let response = post(&f, &path, &body).await;
            assert_eq!(response.status(), 200);
            assert_completed(&text(response).await, client);
            assert_upstream_search(upstream, &f.sent_bodies.lock().unwrap()[0], true);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn search_requests_none_does_not_enable_paid_search_but_keeps_disabled_functions() {
    for upstream in PROTOCOLS {
        for client in [Protocol::Anthropic, Protocol::Responses] {
            let f = Fixture::new(
                upstream,
                prefix(upstream) + completion(upstream),
                Ending::Eof,
            )
            .await;
            let (path, mut body) = client_request(client, "mock/up-model", true);
            enable_search(client, &mut body, true);
            if client == Protocol::Gemini {
                body["toolConfig"] = json!({"functionCallingConfig":{"mode":"NONE"}});
            } else {
                body["tool_choice"] = if client == Protocol::Anthropic {
                    json!({"type":"none"})
                } else {
                    json!("none")
                };
            }
            let response = post(&f, &path, &body).await;
            assert_eq!(response.status(), 200);
            assert_completed(&text(response).await, client);
            let sent = f.sent_bodies.lock().unwrap()[0].clone();
            assert!(sent.get("web_search_options").is_none());
            assert!(sent.get("include").is_none());
            assert_eq!(sent["tools"].as_array().unwrap().len(), 1, "{sent}");
            match upstream {
                Protocol::Anthropic => assert_eq!(sent["tool_choice"], json!({"type":"none"})),
                Protocol::Gemini => assert_eq!(
                    sent["toolConfig"],
                    json!({"functionCallingConfig":{"mode":"NONE"}})
                ),
                _ => assert_eq!(sent["tool_choice"], "none"),
            }
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn search_requests_untranslatable_options_return_400_without_network_or_key_errors() {
    for upstream in [Protocol::Anthropic, Protocol::Gemini, Protocol::Chat] {
        for stream in [false, true] {
            let f =
                Fixture::with_keys(upstream, vec![String::new(), String::new()], Ending::Eof).await;
            let history = f.persistent_history();
            let (path, mut body) = client_request(Protocol::Responses, "mock/up-model", stream);
            body["tools"] =
                json!([{"type":"web_search", "filters":{"allowed_domains":["private.invalid"]}}]);
            let response = post(&f, &path, &body).await;
            assert_eq!(response.status(), 400);
            let reply = text(response).await;
            assert!(reply.contains("cannot be translated"), "{reply}");
            assert!(!reply.contains("private.invalid"));
            assert!(f.attempts.lock().unwrap().is_empty());
            assert!(f.sent_bodies.lock().unwrap().is_empty());
            assert!(
                f.state
                    .key_rotation
                    .cooldown_snapshot(&f.state.config.current())
                    .is_empty()
            );
            let record = f.record().await;
            assert!(record["usage"]["input"].is_null());
            assert!(record["usage"]["output"].is_null());
            assert!(
                f.log.lock().unwrap().snapshot().providers["mock"]
                    .keys
                    .is_empty()
            );
            let persisted = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
            assert!(!persisted.contains("private.invalid"));
            assert!(!persisted.contains("allowed_domains"));
        }
    }
}

#[tokio::test]
async fn search_requests_skip_incompatible_key_pools_and_preserve_options_on_native_fallback() {
    for grouped in [false, true] {
        for stream in [false, true] {
            let script = if stream {
                search_frame(Protocol::Responses) + completion(Protocol::Responses)
            } else {
                search_reply(Protocol::Responses).to_string()
            };
            let f = Fixture::with_config(
                Protocol::Anthropic,
                vec![script.clone(), script],
                Ending::Eof,
                |config| {
                    configure_timeout_fallback(config, grouped);
                    config.provider[1].protocol = Protocol::Responses;
                },
            )
            .await;
            let model = if grouped {
                "group/daily"
            } else {
                "mock/up-model"
            };
            let (path, mut body) = client_request(Protocol::Responses, model, stream);
            let tool = json!({"type":"web_search", "filters":{"allowed_domains":["private.invalid"]},"search_context_size":"low"});
            body["tools"] = json!([tool]);
            let response = post(&f, &path, &body).await;
            assert_eq!(response.status(), 200);
            let reply = text(response).await;
            assert_search_output(&reply, Protocol::Responses);
            if stream {
                assert_completed(&reply, Protocol::Responses);
            }
            assert_eq!(*f.attempts.lock().unwrap(), [1]);
            assert_eq!(*f.sent_models.lock().unwrap(), ["backup-model"]);
            assert_eq!(f.sent_bodies.lock().unwrap()[0]["tools"], json!([tool]));
            let record = f.record().await;
            assert_eq!(record["provider"], "spare");
            assert_eq!(record["status"], "ok");
            assert_eq!(record["usage"]["input"], 7);
            assert!(
                f.state
                    .key_rotation
                    .cooldown_snapshot(&f.state.config.current())
                    .is_empty()
            );
            assert!(
                !f.log
                    .lock()
                    .unwrap()
                    .snapshot()
                    .providers
                    .contains_key("mock")
            );
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn search_requests_native_gemini_reask_preserves_search_config_and_same_candidate() {
    for stream in [false, true] {
        let first = if stream {
            "data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}]}\n\n"
                .into()
        } else {
            json!({"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}).to_string()
        };
        let second = if stream {
            search_frame(Protocol::Gemini) + completion(Protocol::Gemini)
        } else {
            search_reply(Protocol::Gemini).to_string()
        };
        let f = Fixture::with_keys(
            Protocol::Gemini,
            vec![String::new()],
            Ending::PerRequest(vec![first, second], Box::new(Ending::Eof)),
        )
        .await;
        let (path, mut body) = client_request(Protocol::Gemini, "mock/up-model", stream);
        let tool = json!({"googleSearch":{"excludeDomains":["private.invalid"]}});
        body["tools"] = json!([tool]);
        let response = post(&f, &path, &body).await;
        assert_eq!(response.status(), 200);
        let reply = text(response).await;
        assert_search_output(&reply, Protocol::Gemini);
        assert_eq!(*f.attempts.lock().unwrap(), [0, 0]);
        assert_eq!(*f.sent_models.lock().unwrap(), ["up-model", "up-model"]);
        for sent in f.sent_bodies.lock().unwrap().iter() {
            assert_eq!(sent["tools"], json!([tool]));
        }
        let record = f.record().await;
        assert_eq!(record["status"], "ok");
        assert!(!record.to_string().contains("private.invalid"));
        f.assert_released().await;
    }
}

#[tokio::test]
async fn search_requests_function_only_none_keeps_hosted_search_on_native_and_translated_routes() {
    for upstream in PROTOCOLS {
        for client in [Protocol::Chat, Protocol::Gemini] {
            let f = Fixture::new(
                upstream,
                prefix(upstream) + completion(upstream),
                Ending::Eof,
            )
            .await;
            let (path, mut body) = client_request(client, "mock/up-model", true);
            enable_search(client, &mut body, true);
            if client == Protocol::Gemini {
                body["toolConfig"] = json!({"functionCallingConfig":{"mode":"NONE"}});
            } else {
                body["tool_choice"] = json!("none");
            }
            let response = post(&f, &path, &body).await;
            assert_eq!(response.status(), 200);
            assert_completed(&text(response).await, client);
            // On native routes, preserve disabled function declarations. On
            // translated routes, remove them so only the hosted search can run.
            assert_upstream_search(
                upstream,
                &f.sent_bodies.lock().unwrap()[0],
                upstream == client,
            );
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}
