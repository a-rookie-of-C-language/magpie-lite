//! Real loopback HTTP/SSE update fixtures; no paid/live Gemini calls.
use super::*;

const QUERY: &str = "private grounding query";
const QUERY_TWO: &str = "second private grounding query";
const QUERY_THREE: &str = "third private grounding query";
const URL: &str = "https://example.invalid/grounding-first";
const URL_TWO: &str = "https://example.invalid/grounding-second";
const URL_THREE: &str = "https://example.invalid/grounding-third";
const ATTRIBUTION: &str = "PRIVATE_NATIVE_GROUNDING_ATTRIBUTION";

fn frame(metadata: &Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"modelVersion":"up-model",
        "candidates":[{"groundingMetadata":metadata}]})
    )
}
fn initial() -> Value {
    json!({"webSearchQueries":[QUERY], "groundingChunks":[{"web":{"uri":URL,"title":"First"}}]})
}
fn cumulative() -> Value {
    json!({"webSearchQueries":[QUERY,QUERY_TWO,QUERY_TWO], "groundingChunks":[
        {"web":{"uri":URL,"title":"First"}}, {"web":{"uri":URL_TWO,"title":"Second"}},
        {"web":{"uri":URL_TWO,"title":"Second"}}],
        "groundingSupports":[{"segment":{"startIndex":0,"endIndex":7,"text":"between"},
        "groundingChunkIndices":[1]}]})
}
fn citations() -> Value {
    json!({"groundingSupports":[{"segment":{"text":"between"},"groundingChunkIndices":[0]}],
        "searchEntryPoint":{"renderedContent":ATTRIBUTION}})
}
fn independent() -> Value {
    json!({"web_search_queries":[QUERY_THREE], "grounding_chunks":[{"web":{"uri":URL_THREE}}]})
}
fn frames(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn begin() -> &'static str {
    "data: {\"modelVersion\":\"up-model\",\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3}}\n\n"
}
async fn body(response: reqwest::Response) -> String {
    tokio::time::timeout(LIMIT, response.text())
        .await
        .unwrap()
        .unwrap()
}
fn route(grouped: bool) -> &'static str {
    if grouped {
        "group/daily"
    } else {
        "mock/up-model"
    }
}
fn assert_public_items(wire: &str, client: Protocol) {
    let mut queries = Vec::new();
    let mut urls = Vec::new();
    for value in frames(wire) {
        match client {
            Protocol::Anthropic => {
                if value["delta"]["type"] == "input_json_delta" {
                    let input: Value =
                        serde_json::from_str(value["delta"]["partial_json"].as_str().unwrap())
                            .unwrap();
                    queries.push(input["query"].as_str().unwrap().to_owned());
                }
                if value["content_block"]["type"] == "web_search_tool_result" {
                    for hit in value["content_block"]["content"].as_array().unwrap() {
                        urls.push(hit["url"].as_str().unwrap().to_owned());
                    }
                }
            }
            Protocol::Responses => {
                if value["type"] == "response.output_item.done"
                    && value["item"]["type"] == "web_search_call"
                {
                    let action = &value["item"]["action"];
                    queries.push(action["query"].as_str().unwrap().to_owned());
                    for hit in action["sources"].as_array().unwrap() {
                        urls.push(hit["url"].as_str().unwrap().to_owned());
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(queries, [QUERY, QUERY_TWO, QUERY_THREE], "{wire}");
    assert_eq!(urls, [URL, URL_TWO, URL_THREE], "{wire}");
}
async fn assert_history(f: &Fixture, history: &tempfile::TempDir, status: &str) {
    let record = f.record().await;
    assert_eq!(record["status"], status);
    assert_eq!(record["usage"]["input"], 7);
    assert_eq!(record["usage"]["output"], 3);
    let persisted = std::fs::read_to_string(history.path().join("requests.jsonl")).unwrap();
    for value in [record.to_string(), persisted] {
        for private in [
            QUERY,
            QUERY_TWO,
            QUERY_THREE,
            URL,
            URL_TWO,
            URL_THREE,
            ATTRIBUTION,
            "PRIVATE_OVERFLOW",
        ] {
            assert!(
                !value.contains(private),
                "private grounding in history: {value}"
            );
        }
    }
}

#[tokio::test]
async fn changed_grounding_reaches_native_and_only_new_public_items_reach_foreign_clients() {
    let mut changed_title = independent();
    changed_title["grounding_chunks"][0]["web"]["title"] = json!("Corrected title");
    let metadata = [
        citations(),
        initial(),
        cumulative(),
        citations(),
        independent(),
        changed_title,
    ];
    let mut script = begin().to_owned();
    for update in &metadata {
        script += &frame(update);
        script += &frame(update); // Exact consecutive repeats must not emit twice.
        if update == &initial() {
            script += "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"between grounding updates\"}]}}]}\n\n";
        }
    }
    script += completion(Protocol::Gemini);
    for client in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_config(
                Protocol::Gemini,
                vec![
                    script.clone(),
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Ending::Hold,
                |config| configure_timeout_fallback(config, grouped),
            )
            .await;
            let history = f.persistent_history();
            let wire = body(f.request_model(client, route(grouped)).await).await;
            assert_completed(&wire, client);
            assert!(wire.contains("between grounding updates"), "{wire}");
            match client {
                Protocol::Gemini => {
                    let values = frames(&wire);
                    let actual: Vec<_> = values
                        .iter()
                        .filter_map(|v| v.pointer("/candidates/0/groundingMetadata"))
                        .collect();
                    assert_eq!(actual, metadata.iter().collect::<Vec<_>>(), "{wire}");
                }
                Protocol::Anthropic | Protocol::Responses => assert_public_items(&wire, client),
                Protocol::Chat => {
                    assert!(!wire.contains(QUERY));
                    assert!(!wire.contains(URL));
                }
            }
            if client != Protocol::Gemini {
                assert!(!wire.contains(ATTRIBUTION), "{wire}");
                assert!(!wire.contains("groundingSupports"), "{wire}");
            }
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_history(&f, &history, "ok").await;
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn grounding_state_overflow_after_search_errors_once_without_retry_or_success() {
    let queries: Vec<_> = (0..4095).map(|i| format!("PRIVATE_OVERFLOW_{i}")).collect();
    let oversized =
        json!({"webSearchQueries":queries,"searchEntryPoint":{"renderedContent":ATTRIBUTION}});
    let script =
        begin().to_owned() + &frame(&initial()) + &frame(&oversized) + completion(Protocol::Gemini);
    for client in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_chunk_size(
                Protocol::Gemini,
                vec![
                    script.clone(),
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Ending::Hold,
                4096,
                |config| configure_timeout_fallback(config, grouped),
            )
            .await;
            let history = f.persistent_history();
            let wire = body(f.request_model(client, route(grouped)).await).await;
            assert_one_stream_error(&wire, client, "upstream grounding state exceeds its budget");
            assert_no_success(&wire);
            assert!(!wire.contains("PRIVATE_OVERFLOW"), "{wire}");
            assert!(!wire.contains(ATTRIBUTION), "{wire}");
            assert_eq!(*f.attempts.lock().unwrap(), [0]);
            assert_history(&f, &history, "upstream_stream_error").await;
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn citation_only_grounding_does_not_satisfy_first_content_or_prevent_timeout_fallback() {
    for client in PROTOCOLS {
        for grouped in [false, true] {
            let f = Fixture::with_config(
                Protocol::Gemini,
                vec![
                    frame(&citations()),
                    prefix(Protocol::Gemini) + completion(Protocol::Gemini),
                ],
                Ending::PerSlot(vec![
                    Ending::Paced(vec![(
                        Duration::from_millis(600),
                        frame(&initial()) + completion(Protocol::Gemini),
                    )]),
                    Ending::Hold,
                ]),
                |config| {
                    configure_timeout_fallback(config, grouped);
                    config.stream_first_content_timeout_ms = 150;
                    config.stream_idle_timeout_ms = 1500;
                    if grouped {
                        config.group[0].first_token_timeout_ms = 150;
                    }
                },
            )
            .await;
            let wire = body(f.request_model(client, route(grouped)).await).await;
            assert_completed(&wire, client);
            assert!(wire.contains("partial"), "{wire}");
            assert!(
                !wire.contains(ATTRIBUTION),
                "opening metadata from discarded attempt: {wire}"
            );
            assert!(!wire.contains(QUERY));
            assert_eq!(*f.attempts.lock().unwrap(), [0, 1]);
            assert_eq!(*f.sent_models.lock().unwrap(), ["up-model", "backup-model"]);
            assert_eq!(f.record().await["status"], "ok");
            f.assert_released().await;
        }
    }
}

#[tokio::test]
async fn large_native_grounding_counts_toward_opening_metadata_budget() {
    let metadata = json!({"searchEntryPoint":{"renderedContent":"x".repeat(70*1024)}});
    for client in PROTOCOLS {
        let f = Fixture::with_chunk_size(
            Protocol::Gemini,
            vec![
                frame(&metadata),
                prefix(Protocol::Gemini) + completion(Protocol::Gemini),
            ],
            Ending::Hold,
            4096,
            |config| {
                configure_timeout_fallback(config, false);
                config.stream_first_content_timeout_ms = 150;
                config.stream_idle_timeout_ms = 1500;
            },
        )
        .await;
        let wire = body(f.request(client).await).await;
        // Opening budgets intentionally commit bounded metadata-only streams;
        // they do not turn attribution into an answer or disable its deadline.
        assert_one_stream_error(&wire, client, "upstream first-content timeout");
        assert_no_success(&wire);
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        assert_eq!(f.record().await["status"], "upstream_stream_error");
        f.assert_released().await;
    }
}

#[tokio::test]
async fn socket_disconnect_after_grounding_updates_releases_upstream_and_keeps_history_private() {
    use tokio::io::AsyncReadExt;
    let script = begin().to_owned()
        + &frame(&initial())
        + &frame(&cumulative())
        + &frame(&citations())
        + "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"grounding updates received\"}]}}]}\n\n";
    for client in PROTOCOLS {
        let f = Fixture::new(Protocol::Gemini, script.clone(), Ending::Hold).await;
        let history = f.persistent_history();
        let mut socket = f.socket_request(client, "mock/up-model", true).await;
        tokio::time::timeout(LIMIT, async {
            let mut received = Vec::new();
            while !String::from_utf8_lossy(&received).contains("grounding updates received") {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert_ne!(count, 0, "stream ended before grounding updates");
                received.extend_from_slice(&bytes[..count]);
                assert!(received.len() < 256 * 1024);
            }
        })
        .await
        .unwrap();
        drop(socket);
        assert_history(&f, &history, "client_disconnected").await;
        assert_eq!(*f.attempts.lock().unwrap(), [0]);
        f.assert_released().await;
    }
}
