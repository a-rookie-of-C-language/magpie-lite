//! Administrative provider tools run against isolated real HTTP servers.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use magpie_gateway::{AffinityStore, AppState, Config, ConfigStore, KeyRotator, build_router};
use serde_json::{Value, json};

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<(String, HeaderMap, Value)>>,
}

async fn upstream(State(mock): State<Arc<Mock>>, request: Request) -> Response {
    let uri = request.uri().to_string();
    let headers = request.headers().clone();
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    mock.seen
        .lock()
        .unwrap()
        .push((uri.clone(), headers.clone(), body));
    if uri.starts_with("/slow/") {
        tokio::time::sleep(Duration::from_secs(11)).await;
    }
    if uri.starts_with("/redirect/") {
        return (StatusCode::TEMPORARY_REDIRECT, [("location", "/v1/models")]).into_response();
    }
    if uri.starts_with("/echo/") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"SECRET-saved-key"}})),
        )
            .into_response();
    }
    if headers
        .get("authorization")
        .is_some_and(|value| value == "Bearer SECRET-bad-key")
    {
        return (StatusCode::UNAUTHORIZED, "SECRET-bad-key echoed").into_response();
    }
    if uri.starts_with("/oversize/") {
        return "x".repeat(1024 * 1024 + 1).into_response();
    }
    if uri.starts_with("/chunked/") {
        let stream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(vec![b'x'; 600_000]),
            Ok(vec![b'x'; 600_000]),
        ]);
        return Response::new(Body::from_stream(stream));
    }
    if uri.starts_with("/bad-json/") {
        return "<html>SECRET-saved-key</html>".into_response();
    }
    if uri.starts_with("/invalid/") {
        return Json(json!({"healthy":true})).into_response();
    }
    if uri.starts_with("/leak/") {
        return Json(json!({"data":[{"id":"SECRET-saved-key"}]})).into_response();
    }
    if uri.starts_with("/many/") {
        return Json(
            json!({"data":(0..5001).map(|n| json!({"id":format!("m-{n}")})).collect::<Vec<_>>()}),
        )
        .into_response();
    }
    if uri.starts_with("/repeat/") {
        return Json(json!({"models":[],"nextPageToken":"repeat"})).into_response();
    }
    if uri.starts_with("/pages/") {
        return Json(
            json!({"models":[],"nextPageToken":format!("next-{}",mock.seen.lock().unwrap().len())}),
        )
        .into_response();
    }
    if uri.contains("/models") && !uri.contains(":generateContent") {
        if uri.starts_with("/gemini/") {
            if uri.contains("pageToken=") {
                return Json(json!({"models":[{"name":"models/gemini-two","supportedGenerationMethods":["generateContent"]}]})).into_response();
            }
            return Json(json!({"models":[{"name":"models/gemini-one","supportedGenerationMethods":["generateContent"]},
                {"name":"models/embed","supportedGenerationMethods":["embedContent"]}],"nextPageToken":"page /+"})).into_response();
        }
        if uri.starts_with("/anthropic/") {
            if uri.contains("after_id=") {
                return Json(json!({"data":[{"id":"claude-two"}],"has_more":false}))
                    .into_response();
            }
            return Json(
                json!({"data":[{"id":"claude-one"}],"has_more":true,"last_id":"claude-one"}),
            )
            .into_response();
        }
        return Json(json!({"data":[{"id":"chat-one"},{"id":"chat-one"},{"id":"chat-two"}]}))
            .into_response();
    }
    let value = if uri.ends_with("/chat/completions") {
        json!({"choices":[{"message":{"role":"assistant","content":"OK"}}]})
    } else if uri.ends_with("/responses") {
        json!({"object":"response","status":"completed","output":[]})
    } else if uri.ends_with("/messages") {
        json!({"type":"message","role":"assistant","content":[{"type":"text","text":"OK"}]})
    } else {
        json!({"candidates":[{"content":{"parts":[{"text":"OK"}]},"finishReason":"STOP"}]})
    };
    Json(value).into_response()
}

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }),
    )
}

struct Fixture {
    url: String,
    upstream: String,
    mock: Arc<Mock>,
    state: Arc<AppState>,
    directory: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mock = Arc::new(Mock::default());
        let (upstream_url, up_task) =
            bind(Router::new().fallback(upstream).with_state(mock.clone())).await;
        let mut config = Config::default();
        let mut provider: magpie_gateway::ProviderConfig = serde_json::from_value(json!({
            "id":"saved","protocol":"chat","base_url":format!("{upstream_url}/v1"),"api_key":"SECRET-saved-key","models":["old"]
        })).unwrap();
        provider.key_generation = 0;
        config.set_provider(provider, true).unwrap();
        config.save(&path).unwrap();
        let state = Arc::new(AppState {
            agents: None,
            config: ConfigStore::new(config, Some(path)),
            http: magpie_gateway::upstream::http_client().unwrap(),
            requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
            key_rotation: KeyRotator::default(),
            affinity: AffinityStore::default(),
            lanes: magpie_gateway::lanes::Lanes::default(),
            rpms: magpie_gateway::lanes::Rpms::default(),
        });
        let (url, task) = bind(build_router(state.clone())).await;
        Self {
            url,
            upstream: upstream_url,
            mock,
            state,
            directory,
            tasks: vec![up_task, task],
        }
    }
    fn draft(&self, protocol: &str, prefix: &str) -> Value {
        json!({"id":"saved","protocol":protocol,"base_url":format!("{}{prefix}",self.upstream),"models":["test-model"]})
    }
    async fn call(&self, operation: &str, draft: Value, model: Option<&str>) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/config/provider-tools/{operation}", self.url))
            .bearer_auth("magpie-lite")
            .json(&json!({"provider":draft,"model":model}))
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn protocol_catalogs_paginate_filter_and_reuse_saved_credentials_without_mutation() {
    let f = Fixture::new().await;
    let disk = std::fs::read(f.directory.path().join("config.toml")).unwrap();
    let before = serde_json::to_value(f.state.config.current()).unwrap();
    for (protocol, prefix, expected) in [
        ("chat", "/v1", json!(["chat-one", "chat-two"])),
        ("responses", "/v1", json!(["chat-one", "chat-two"])),
        (
            "anthropic",
            "/anthropic",
            json!(["claude-one", "claude-two"]),
        ),
        (
            "gemini",
            "/gemini/v1beta",
            json!(["gemini-one", "gemini-two"]),
        ),
    ] {
        let response = f.call("models", f.draft(protocol, prefix), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["models"], expected);
        assert!(!value.to_string().contains("SECRET"));
    }
    let seen = f.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 6);
    assert_eq!(seen[0].1["authorization"], "Bearer SECRET-saved-key");
    assert_eq!(seen[2].1["x-api-key"], "SECRET-saved-key");
    assert_eq!(seen[2].1["anthropic-version"], "2023-06-01");
    assert!(seen[3].0.contains("after_id=claude-one"));
    assert_eq!(seen[4].1["x-goog-api-key"], "SECRET-saved-key");
    assert!(seen[5].0.contains("pageToken=page+%2F%2B"));
    assert_eq!(
        std::fs::read(f.directory.path().join("config.toml")).unwrap(),
        disk
    );
    assert_eq!(
        serde_json::to_value(f.state.config.current()).unwrap(),
        before
    );
}

#[tokio::test]
async fn minimal_probes_use_all_four_protocols_and_do_not_advance_rotation_or_usage() {
    let f = Fixture::new().await;
    let before = reqwest::Client::new()
        .get(format!("{}/stats", f.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    for (protocol, prefix) in [
        ("chat", "/v1"),
        ("responses", "/v1"),
        ("anthropic", "/anthropic"),
        ("gemini", "/gemini/v1beta"),
    ] {
        let response = f
            .call("test", f.draft(protocol, prefix), Some("chosen-model"))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["protocol"], protocol);
        assert_eq!(value["model"], "chosen-model");
    }
    let after = reqwest::Client::new()
        .get(format!("{}/stats", f.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(before, after);
    let seen = f.mock.seen.lock().unwrap();
    assert_eq!(seen[0].2["max_tokens"], 64);
    assert_eq!(seen[0].2["messages"][0]["content"], "Reply with OK.");
    assert_eq!(seen[1].2["max_output_tokens"], 64);
    assert!(seen[1].2["input"].is_array());
    assert_eq!(seen[2].2["max_tokens"], 64);
    assert!(seen[2].1.get("authorization").is_none());
    assert_eq!(seen[3].2["generationConfig"]["maxOutputTokens"], 64);
    assert!(seen[3].2.get("model").is_none());
    assert!(seen[3].0.ends_with("/models/chosen-model:generateContent"));
}

#[tokio::test]
async fn draft_keys_retry_in_isolation_and_explicit_clear_omits_authentication() {
    let f = Fixture::new().await;
    for operation in ["models", "test"] {
        let mut draft = f.draft("chat", "/v1");
        draft["id"] = json!("unsaved");
        draft["api_keys"] = json!(["SECRET-bad-key", "SECRET-draft-key"]);
        assert_eq!(
            f.call(operation, draft, None).await.status(),
            StatusCode::OK
        );
    }
    let mut draft = f.draft("chat", "/v1");
    draft["api_keys"] = json!([]);
    assert_eq!(f.call("test", draft, None).await.status(), StatusCode::OK);
    let seen = f.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].1["authorization"], "Bearer SECRET-bad-key");
    assert_eq!(seen[1].1["authorization"], "Bearer SECRET-draft-key");
    assert!(seen[4].1.get("authorization").is_none());
    assert!(f.state.config.current().provider("unsaved").is_none());
}

#[tokio::test]
async fn errors_and_redirects_never_relay_secrets_or_follow_location() {
    let f = Fixture::new().await;
    for (operation, prefix) in [
        ("models", "/redirect"),
        ("test", "/redirect"),
        ("models", "/echo"),
        ("test", "/echo"),
        ("models", "/invalid"),
        ("test", "/invalid"),
        ("models", "/bad-json"),
        ("test", "/bad-json"),
        ("models", "/leak"),
    ] {
        let response = f.call(operation, f.draft("chat", prefix), None).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{operation} {prefix}"
        );
        let text = response.text().await.unwrap();
        assert!(!text.contains("SECRET"));
        assert!(!text.contains(&f.upstream));
    }
    assert_eq!(f.mock.seen.lock().unwrap().len(), 9);
}

#[tokio::test]
async fn response_byte_model_page_and_cursor_limits_are_enforced() {
    let f = Fixture::new().await;
    for prefix in ["/oversize", "/chunked", "/many"] {
        let response = f.call("models", f.draft("chat", prefix), None).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.text().await.unwrap().contains("exceeded"));
    }
    let response = f.call("models", f.draft("gemini", "/repeat"), None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.text().await.unwrap().contains("repeated"));
    let response = f.call("models", f.draft("gemini", "/pages"), None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.text().await.unwrap().contains("20 pages"));
}

#[tokio::test]
async fn tools_require_authentication_validate_before_network_and_need_no_persistence_path() {
    let f = Fixture::new().await;
    let response = reqwest::Client::new()
        .post(format!("{}/config/provider-tools/models", f.url))
        .json(&json!({"provider":f.draft("chat","/v1")}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut draft = f.draft("chat", "/v1");
    draft["base_url"] = json!("http://SECRET@localhost/v1");
    assert_eq!(
        f.call("models", draft, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.call("test", f.draft("chat", "/v1"), Some("bad model"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut draft = f.draft("chat", "/v1");
    draft["models"] = json!([]);
    assert_eq!(
        f.call("test", draft, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(f.mock.seen.lock().unwrap().is_empty());
    let config = f.state.config.current().clone();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, task) = bind(build_router(state)).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/config/provider-tools/models"))
        .bearer_auth("magpie-lite")
        .json(&json!({"provider":f.draft("chat","/v1")}))
        .send()
        .await
        .unwrap();
    task.abort();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn timeout_is_bounded_and_sanitized() {
    let f = Fixture::new().await;
    let started = std::time::Instant::now();
    let response = f.call("test", f.draft("chat", "/slow"), None).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(!response.text().await.unwrap().contains("SECRET"));
}

#[tokio::test]
async fn checks_leave_live_rotation_and_existing_cooldowns_untouched() {
    let f = Fixture::new().await;
    f.state
        .config
        .update(|config| {
            let mut provider = config.provider("saved").unwrap().clone();
            provider.api_key = String::new();
            provider.api_keys = vec![
                "SECRET-saved-key".to_owned(),
                "SECRET-second-key".to_owned(),
                "SECRET-third-key".to_owned(),
            ];
            config.set_provider(provider, false).unwrap();
            Ok(())
        })
        .await
        .unwrap();
    let config = f.state.config.current();
    let provider = config.provider("saved").unwrap();
    let keys = provider.api_key_pool();
    assert_eq!(
        f.state
            .key_rotation
            .rotate("saved", provider.key_generation, &keys)[0]
            .0,
        0
    );
    f.state
        .key_rotation
        .cool_down("saved", provider.key_generation, 2, Duration::from_secs(60));
    for operation in ["models", "test"] {
        assert_eq!(
            f.call(operation, f.draft("chat", "/v1"), None)
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        f.state
            .key_rotation
            .rotate("saved", provider.key_generation, &keys),
        vec![
            (1, "SECRET-second-key".to_owned()),
            (0, "SECRET-saved-key".to_owned())
        ]
    );
    let seen = f.mock.seen.lock().unwrap();
    assert!(
        seen.iter()
            .all(|(_, headers, _)| headers["authorization"] == "Bearer SECRET-saved-key")
    );
}

#[tokio::test]
async fn unauthenticated_drafts_omit_headers_for_all_protocols() {
    let f = Fixture::new().await;
    for (protocol, prefix) in [
        ("chat", "/v1"),
        ("responses", "/v1"),
        ("anthropic", "/anthropic"),
        ("gemini", "/gemini/v1beta"),
    ] {
        for operation in ["models", "test"] {
            let mut draft = f.draft(protocol, prefix);
            draft["api_keys"] = json!([]);
            assert_eq!(
                f.call(operation, draft, None).await.status(),
                StatusCode::OK
            );
        }
    }
    for (_, headers, _) in f.mock.seen.lock().unwrap().iter() {
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("x-goog-api-key").is_none());
    }
}

#[tokio::test]
async fn credential_attempts_are_capped_and_malformed_json_does_not_echo_keys() {
    let f = Fixture::new().await;
    let mut draft = f.draft("chat", "/echo");
    draft["api_keys"] = json!(
        (0..10)
            .map(|n| format!("SECRET-key-{n}"))
            .collect::<Vec<_>>()
    );
    for operation in ["models", "test"] {
        let response = f.call(operation, draft.clone(), None).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.text().await.unwrap().contains("SECRET"));
    }
    assert_eq!(f.mock.seen.lock().unwrap().len(), 16);
    let response = reqwest::Client::new()
        .post(format!("{}/config/provider-tools/test", f.url))
        .bearer_auth("magpie-lite")
        .body(r#"{"provider":{"api_keys":"SECRET-malformed"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.text().await.unwrap().contains("SECRET"));
    assert_eq!(f.mock.seen.lock().unwrap().len(), 16);
}
