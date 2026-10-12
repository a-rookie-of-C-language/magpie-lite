//! Local HTTP ingress and replay-safety fixtures, not live Anthropic requests.
use super::*;

const QUERY: &str = "private anthropic search query";
const QUERY_TWO: &str = "another private search query";
const URL: &str = "https://example.invalid/anthropic-source";
const URL_TWO: &str = "https://example.invalid/second-source";
const CIPHERTEXT: &str = "SECRET_ANTHROPIC_NATIVE_SEARCH_CIPHERTEXT";

fn frame(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn begin() -> String {
    frame(json!({"type":"message_start","message":{"model":"up-model",
        "usage":{"input_tokens":7,"output_tokens":3}}}))
}
fn call(id: &str, query: &str) -> Value {
    json!({"type":"server_tool_use","id":id,"name":"web_search","input":{"query":query}})
}
fn result(id: &str, url: &str) -> Value {
    json!({"type":"web_search_tool_result","tool_use_id":id,"content":[
        {"type":"web_search_result","title":"Search source","url":url,
         "page_age":"one day","encrypted_content":CIPHERTEXT}]})
}
fn start(index: usize, block: Value) -> String {
    frame(json!({"type":"content_block_start","index":index,"content_block":block}))
}
fn stop(index: usize) -> String {
    frame(json!({"type":"content_block_stop","index":index}))
}
fn args(index: usize, fragment: &str) -> String {
    frame(json!({"type":"content_block_delta","index":index,
        "delta":{"type":"input_json_delta","partial_json":fragment}}))
}
fn open_search() -> String {
    let mut block = call("srv_1", "");
    block["input"] = json!({});
    begin() + &start(0, block)
}
fn finish_search() -> String {
    args(0, "{\"query\":")
        + &args(0, &format!("{}{}", json!(QUERY), "}"))
        + &stop(0)
        + &start(1, result("srv_1", URL))
        + &stop(1)
}
fn content() -> Value {
    json!([call("srv_1", QUERY), result("srv_1", URL), {"type":"text","text":"between searches"},
        call("srv_2", QUERY_TWO), result("srv_2", URL_TWO)])
}
fn json_reply(blocks: Value) -> String {
    json!({"model":"up-model","content":blocks,"stop_reason":"end_turn",
        "usage":{"input_tokens":7,"output_tokens":3}})
    .to_string()
}
fn complete_search() -> String {
    open_search()
        + &finish_search()
        + &start(2, json!({"type":"text","text":""}))
        + &frame(json!({"type":"content_block_delta","index":2,
            "delta":{"type":"text_delta","text":"between searches"}}))
        + &stop(2)
        + &start(3, call("srv_2", QUERY_TWO))
        + &stop(3)
        + &start(4, result("srv_2", URL_TWO))
        + &stop(4)
        + completion(Protocol::Anthropic)
}
fn frames(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
async fn body(response: reqwest::Response) -> String {
    tokio::time::timeout(LIMIT, response.text())
        .await
        .unwrap()
        .unwrap()
}
async fn post(f: &Fixture, client: Protocol, stream: bool) -> reqwest::Response {
    let (path, request) = client_request(client, "mock/up-model", stream);
    tokio::time::timeout(
        LIMIT,
        reqwest::Client::new()
            .post(format!("{}{path}", f.gateway.url))
            .bearer_auth("secret-local-token")
            .json(&request)
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
}
fn assert_private_metadata_absent(value: &Value) {
    let wire = value.to_string();
    for private in [QUERY, QUERY_TWO, URL, URL_TWO, CIPHERTEXT] {
        assert!(
            !wire.contains(private),
            "private metadata in history: {wire}"
        );
    }
}

#[tokio::test]
async fn anthropic_search_json_and_sse_preserve_native_blocks_and_all_public_results() {
    for client in PROTOCOLS {
        for stream in [false, true] {
            let f = Fixture::new(
                Protocol::Anthropic,
                if stream {
                    complete_search()
                } else {
                    json_reply(content())
                },
                Ending::Eof,
            )
            .await;
            let history = f.persistent_history();
            let response = post(&f, client, stream).await;
            assert_eq!(response.status(), 200);
            let wire = body(response).await;
            assert!(wire.contains("between searches"), "{wire}");
            assert!(
                !wire.contains("function_call") && !wire.contains("functionCall"),
                "{wire}"
            );
            if stream {
                assert_completed(&wire, client);
            }
            if client == Protocol::Anthropic {
                assert!(wire.contains(CIPHERTEXT));
                if stream {
                    let starts: Vec<_> = frames(&wire)
                        .into_iter()
                        .filter_map(|v| v.get("content_block").cloned())
                        .collect();
                    assert_eq!(starts.len(), 5, "{wire}");
                    assert_eq!(starts[0]["id"], "srv_1");
                    assert_eq!(starts[1], result("srv_1", URL));
                    assert_eq!(starts[2]["type"], "text");
                    assert_eq!(starts[3]["id"], "srv_2");
                    assert_eq!(starts[4], result("srv_2", URL_TWO));
                } else {
                    let reply: Value = serde_json::from_str(&wire).unwrap();
                    assert_eq!(reply["content"], content());
                }
            } else {
                assert!(
                    !wire.contains(CIPHERTEXT) && !wire.contains("encrypted_content"),
                    "{wire}"
                );
                if client != Protocol::Chat {
                    for public in [QUERY, QUERY_TWO, URL, URL_TWO] {
                        assert!(wire.contains(public), "{client:?}: {wire}");
                    }
                }
            }
            let record = f.record().await;
            assert_eq!(record["status"], "ok");
            assert_eq!(record["usage"]["input"], 7);
            assert_eq!(record["usage"]["output"], 3);
            assert_private_metadata_absent(&record);
            let persisted = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
            assert_private_metadata_absent(
                &serde_json::from_str::<Value>(persisted.trim()).unwrap(),
            );
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn anthropic_search_start_satisfies_first_content_and_group_deadlines_before_args() {
    for client in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_config(
                Protocol::Anthropic,
                vec![
                    open_search(),
                    prefix(Protocol::Anthropic) + completion(Protocol::Anthropic),
                ],
                Ending::Paced(vec![(
                    Duration::from_millis(650),
                    finish_search() + completion(Protocol::Anthropic),
                )]),
                |config| {
                    configure_timeout_fallback(config, grouped);
                    config.stream_first_content_timeout_ms = 300;
                    config.stream_idle_timeout_ms = 2_000;
                    if grouped {
                        config.group[0].first_token_timeout_ms = 300;
                    }
                },
            )
            .await;
            let response = f
                .request_model(
                    client,
                    if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    },
                )
                .await;
            let wire = body(response).await;
            assert_completed(&wire, client);
            assert!(!wire.contains("timeout"), "{wire}");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn anthropic_search_start_then_idle_errors_or_loops_never_retry_keys_or_routes() {
    for client in PROTOCOLS {
        for grouped in [false, true] {
            for mode in 0..3 {
                let mut script = open_search();
                if mode == 1 {
                    script += error(Protocol::Anthropic);
                }
                if mode == 2 {
                    for _ in 0..300 {
                        script += &loop_delta(
                            Protocol::Anthropic,
                            true,
                            "Now. Writing. Go. Let me write. OK. Here. Produce.\n",
                        );
                    }
                }
                let f = Fixture::with_config(
                    Protocol::Anthropic,
                    vec![
                        script,
                        prefix(Protocol::Anthropic) + completion(Protocol::Anthropic),
                    ],
                    Ending::Hold,
                    |config| {
                        configure_timeout_fallback(config, grouped);
                        config.stream_first_content_timeout_ms = 150;
                        config.stream_idle_timeout_ms = 500;
                    },
                )
                .await;
                let history = f.persistent_history();
                let response = f
                    .request_model(
                        client,
                        if grouped {
                            "group/daily"
                        } else {
                            "mock/up-model"
                        },
                    )
                    .await;
                let wire = body(response).await;
                assert_no_success(&wire);
                if mode == 0 {
                    assert_one_stream_error(&wire, client, "upstream stream idle timeout");
                }
                if mode == 2 {
                    assert_one_stream_error(&wire, client, "reasoning was stuck in a loop");
                }
                assert_eq!(
                    *f.attempts.lock().unwrap(),
                    [0],
                    "{client:?} {grouped} {mode}: {wire}"
                );
                let record = f.record().await;
                assert_eq!(record["status"], "upstream_stream_error");
                assert_eq!(record["usage"]["input"], 7);
                assert_eq!(record["usage"]["output"], 3);
                assert_private_metadata_absent(&record);
                assert!(
                    !std::fs::read_to_string(history.path().join("requests.jsonl"))
                        .unwrap()
                        .contains(CIPHERTEXT)
                );
                f.assert_released().await;
            }
        }
    }
}

#[tokio::test]
async fn anthropic_search_then_refusal_preserves_filter_without_replaying_paid_work() {
    for client in PROTOCOLS {
        for grouped in [false, true] {
            // A closed call without a result is still hosted work, not an empty refusal.
            let script = begin()
                + &start(0, call("srv_1", QUERY))
                + &stop(0)
                + &filtered_completion(Protocol::Anthropic);
            let f = Fixture::with_config(
                Protocol::Anthropic,
                vec![
                    script,
                    prefix(Protocol::Anthropic) + completion(Protocol::Anthropic),
                ],
                Ending::Hold,
                |config| configure_timeout_fallback(config, grouped),
            )
            .await;
            let response = f
                .request_model(
                    client,
                    if grouped {
                        "group/daily"
                    } else {
                        "mock/up-model"
                    },
                )
                .await;
            let wire = body(response).await;
            assert_filtered(&wire, client);
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn anthropic_search_invalid_arguments_fail_once_without_leaking_fragments() {
    for client in PROTOCOLS {
        let script = open_search() + &args(0, "{PRIVATE_FRAGMENT") + &stop(0);
        let f = Fixture::with_config(
            Protocol::Anthropic,
            vec![
                script,
                prefix(Protocol::Anthropic) + completion(Protocol::Anthropic),
            ],
            Ending::Hold,
            |config| configure_timeout_fallback(config, false),
        )
        .await;
        let wire = body(f.request(client).await).await;
        assert_no_success(&wire);
        assert_one_stream_error(&wire, client, "upstream web search input is invalid JSON");
        assert!(!wire.contains("PRIVATE_FRAGMENT"), "{wire}");
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        let record = f.record().await;
        assert_eq!(record["status"], "upstream_stream_error");
        assert!(!record.to_string().contains("PRIVATE_FRAGMENT"));
        f.assert_released().await;
    }
}

#[tokio::test]
async fn anthropic_errors_before_search_can_retry_but_search_start_commits() {
    for client in PROTOCOLS {
        let f = Fixture::with_keys(
            Protocol::Anthropic,
            vec![begin() + error(Protocol::Anthropic), complete_search()],
            Ending::Hold,
        )
        .await;
        let wire = body(f.request(client).await).await;
        assert_completed(&wire, client);
        assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
        assert_eq!(f.record().await["status"], "ok");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn anthropic_native_search_failure_is_retained_but_not_a_successful_foreign_search() {
    let failed = json!({"type":"web_search_tool_result","tool_use_id":"srv_1",
        "content":{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}});
    for client in PROTOCOLS {
        for stream in [false, true] {
            let script = if stream {
                begin()
                    + &start(0, call("srv_1", QUERY))
                    + &stop(0)
                    + &start(1, failed.clone())
                    + &stop(1)
                    + completion(Protocol::Anthropic)
            } else {
                json_reply(json!([call("srv_1", QUERY), failed]))
            };
            let f = Fixture::new(
                Protocol::Anthropic,
                script,
                if stream { Ending::Hold } else { Ending::Eof },
            )
            .await;
            let response = post(&f, client, stream).await;
            assert_eq!(response.status(), 200);
            let wire = body(response).await;
            if stream {
                assert_completed(&wire, client);
            }
            if client == Protocol::Anthropic {
                assert!(wire.contains("max_uses_exceeded"));
            } else {
                assert!(!wire.contains(QUERY));
                assert!(
                    !wire.contains("web_search_call") && !wire.contains("groundingMetadata"),
                    "{wire}"
                );
            }
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn anthropic_search_socket_disconnect_releases_upstream_and_sanitizes_history() {
    use tokio::io::AsyncReadExt;
    for client in PROTOCOLS {
        let script = open_search()
            + &finish_search()
            + &frame(json!({"type":"content_block_delta","index":2,
                "delta":{"type":"text_delta","text":"after hosted search"}}));
        let f = Fixture::new(Protocol::Anthropic, script, Ending::Hold).await;
        let history = f.persistent_history();
        let mut socket = f.socket_request(client, "mock/up-model", true).await;
        tokio::time::timeout(LIMIT, async {
            let mut received = Vec::new();
            let marker = if client == Protocol::Chat {
                "after hosted search"
            } else {
                URL
            };
            while !String::from_utf8_lossy(&received).contains(marker) {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert_ne!(count, 0, "stream ended before search output");
                received.extend_from_slice(&bytes[..count]);
                assert!(received.len() < 256 * 1024);
            }
        })
        .await
        .unwrap();
        drop(socket);
        let record = f.record().await;
        assert_eq!(record["status"], "client_disconnected");
        assert_eq!(record["usage"]["input"], 7);
        assert_eq!(record["usage"]["output"], 3);
        assert_private_metadata_absent(&record);
        let persisted = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
        assert_private_metadata_absent(&serde_json::from_str::<Value>(persisted.trim()).unwrap());
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        f.assert_released().await;
    }
}
