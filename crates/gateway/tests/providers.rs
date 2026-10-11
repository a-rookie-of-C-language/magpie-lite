//! Persistent provider edits exercised through the actual HTTP router.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use magpie_gateway::{AppState, Config, ConfigStore, KeyRotator, build_router};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Notify;

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<(String, Value)>>,
    started: Notify,
    release: Notify,
}

async fn chat(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let key = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_owned();
    mock.seen.lock().unwrap().push((key.clone(), body.clone()));
    if key == "Bearer delayed-key" {
        mock.started.notify_one();
        mock.release.notified().await;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"rate limited"}})),
        );
    }
    (
        StatusCode::OK,
        Json(
            json!({"id":"mock", "object":"chat.completion", "model":body["model"],
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
        ),
    )
}

async fn messages(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    mock.seen.lock().unwrap().push((
        headers
            .get("x-api-key")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
        body.clone(),
    ));
    Json(
        json!({"id":"mock", "type":"message", "role":"assistant", "model":body["model"],
        "content":[{"type":"text","text":"anthropic ok"}], "stop_reason":"end_turn",
        "usage":{"input_tokens":1,"output_tokens":1}}),
    )
}

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

struct Gateway {
    url: String,
    upstream: String,
    path: PathBuf,
    mock: Arc<Mock>,
    state: Arc<AppState>,
    _directory: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn gateway() -> Gateway {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let config = Config::default();
    config.save(&path).unwrap();
    let mock = Arc::new(Mock::default());
    let (upstream, upstream_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(chat))
            .route("/v1/messages", post(messages))
            .with_state(mock.clone()),
    )
    .await;
    let state = Arc::new(AppState {
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: KeyRotator::default(),
    });
    let (url, task) = bind(build_router(state.clone())).await;
    Gateway {
        url,
        upstream,
        path,
        mock,
        state,
        _directory: directory,
        tasks: vec![task, upstream_task],
    }
}

fn provider(gw: &Gateway, id: &str) -> Value {
    json!({"id":id, "name":"Test provider", "protocol":"chat", "base_url":format!("{}/v1",gw.upstream),
        "models":["first"], "api_keys":["private-one","private-two","private-three"]})
}

async fn edit(gw: &Gateway, method: Method, path: &str, body: Value) -> reqwest::Response {
    Client::new()
        .request(method, format!("{}{path}", gw.url))
        .bearer_auth("magpie-lite")
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn snapshot(gw: &Gateway, path: &str) -> Value {
    Client::new()
        .get(format!("{}{path}", gw.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn call(gw: &Gateway, model: &str) -> reqwest::Response {
    edit(
        gw,
        Method::POST,
        "/v1/chat/completions",
        json!({"model":model, "messages":[{"role":"user","content":"hello"}]}),
    )
    .await
}

#[tokio::test]
async fn provider_crud_persists_and_changes_live_routing_without_disclosing_keys() {
    let gw = gateway().await;
    let created = edit(
        &gw,
        Method::POST,
        "/config/providers",
        provider(&gw, "mock"),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = created.text().await.unwrap();
    for secret in [
        "private-one",
        "private-two",
        "private-three",
        "magpie-lite",
        "key_generation",
    ] {
        assert!(!body.contains(secret));
    }
    assert_eq!(Config::load(&gw.path).unwrap().default_provider, "mock");
    assert_eq!(call(&gw, "first").await.status(), StatusCode::OK);
    let mut updated = provider(&gw, "mock");
    updated["protocol"] = json!("anthropic");
    updated["base_url"] = json!(gw.upstream);
    updated["models"] = json!(["second"]);
    updated["api_keys"] = json!(["rotated-secret"]);
    updated["key_cooldown_ms"] = json!(0);
    assert_eq!(
        edit(&gw, Method::PUT, "/config/providers/mock", updated)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(call(&gw, "mock/second").await.status(), StatusCode::OK);
    {
        let seen = gw.mock.seen.lock().unwrap();
        assert_eq!(seen[0].0, "Bearer private-one");
        assert_eq!(seen[1].0, "rotated-secret");
        assert_eq!(seen[1].1["model"], "second");
    }
    let catalog = snapshot(&gw, "/v1/models").await;
    assert_eq!(catalog["data"][0]["id"], "mock/second");
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/providers/mock", Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    let disk = Config::load(&gw.path).unwrap();
    assert!(disk.provider.is_empty());
    assert!(disk.default_provider.is_empty());
    assert_eq!(
        call(&gw, "mock/second").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn metadata_edit_preserves_legacy_keys_and_cooldown_then_explicit_clear_removes_auth() {
    let gw = gateway().await;
    // Existing legacy TOML remains editable without revealing its singular key.
    let mut config = Config::default();
    let p: magpie_gateway::ProviderConfig = serde_json::from_value(provider(&gw, "mock")).unwrap();
    config.provider.push(magpie_gateway::ProviderConfig {
        api_key: "legacy-secret".into(),
        api_keys: vec![],
        ..p
    });
    config.save(&gw.path).unwrap();
    // Refresh this isolated fixture's store before any edits, like gateway startup.
    let state = Arc::new(AppState {
        config: ConfigStore::new(config, Some(gw.path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: KeyRotator::default(),
    });
    let (url, task) = bind(build_router(state.clone())).await;
    let mut gw = gw;
    gw.url = url;
    gw.state = state;
    gw.tasks.push(task);
    assert_eq!(call(&gw, "mock/first").await.status(), StatusCode::OK);
    let before = gw.state.config.current().provider[0].key_generation;
    gw.state
        .key_rotation
        .cool_down("mock", before, 0, std::time::Duration::from_secs(60));
    let mut body = provider(&gw, "mock");
    body.as_object_mut().unwrap().remove("api_keys");
    body["name"] = json!("Renamed");
    assert_eq!(
        edit(&gw, Method::PUT, "/config/providers/mock", body.clone())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(gw.state.config.current().provider[0].key_generation, before);
    assert_eq!(
        Config::load(&gw.path).unwrap().provider[0].api_key,
        "legacy-secret"
    );
    assert!(snapshot(&gw, "/stats").await["key_cooldowns"]["mock"]["0"].is_object());
    body["api_keys"] = json!([]);
    assert_eq!(
        edit(&gw, Method::PUT, "/config/providers/mock", body)
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        snapshot(&gw, "/stats").await["key_cooldowns"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    let disk = Config::load(&gw.path).unwrap();
    assert!(disk.provider[0].api_key.is_empty());
    assert!(disk.provider[0].api_keys.is_empty());
    assert_eq!(
        snapshot(&gw, "/config").await["providers"][0]["credential_count"],
        0
    );
    assert_eq!(call(&gw, "mock/first").await.status(), StatusCode::OK);
    assert_eq!(gw.mock.seen.lock().unwrap().last().unwrap().0, "");
}

#[tokio::test]
async fn replacing_a_smaller_key_pool_ignores_old_requests_cooldowns() {
    let gw = gateway().await;
    let mut body = provider(&gw, "mock");
    body["api_keys"] = json!(["delayed-key", "old-two", "old-three"]);
    assert_eq!(
        edit(&gw, Method::POST, "/config/providers", body.clone())
            .await
            .status(),
        StatusCode::CREATED
    );
    let client = Client::new();
    let url = format!("{}/v1/chat/completions", gw.url);
    let old_request = tokio::spawn(async move {
        client
            .post(url)
            .bearer_auth("magpie-lite")
            .json(&json!({"model":"mock/first","messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        gw.mock.started.notified(),
    )
    .await
    .unwrap();
    // Move the old cursor beyond the new pool length before the edit.
    let old = gw.state.config.current().provider[0].clone();
    gw.state
        .key_rotation
        .rotate("mock", old.key_generation, &old.api_key_pool());
    body["api_keys"] = json!(["new-only"]);
    assert_eq!(
        edit(&gw, Method::PUT, "/config/providers/mock", body)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(call(&gw, "mock/first").await.status(), StatusCode::OK);
    gw.mock.release.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), old_request)
            .await
            .unwrap()
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert!(
        snapshot(&gw, "/stats").await["key_cooldowns"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert_eq!(call(&gw, "mock/first").await.status(), StatusCode::OK);
    let seen = gw.mock.seen.lock().unwrap();
    assert_eq!(
        seen.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
        [
            "Bearer delayed-key",
            "Bearer new-only",
            "Bearer old-two",
            "Bearer new-only"
        ]
    );
}

#[tokio::test]
async fn deletes_reject_group_and_fallback_references_and_reassign_default() {
    let gw = gateway().await;
    for id in ["first", "second"] {
        assert_eq!(
            edit(&gw, Method::POST, "/config/providers", provider(&gw, id))
                .await
                .status(),
            StatusCode::CREATED
        );
    }
    assert_eq!(
        edit(
            &gw,
            Method::POST,
            "/config/groups",
            json!({"id":"daily","routes":["first/first"]})
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/providers/first", Value::Null)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/groups/daily", Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    let mut second = provider(&gw, "second");
    second["fallback"] = json!(["first/first"]);
    assert_eq!(
        edit(&gw, Method::PUT, "/config/providers/second", second)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/providers/first", Value::Null)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        edit(
            &gw,
            Method::PUT,
            "/config/providers/second",
            provider(&gw, "second")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/providers/first", Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(snapshot(&gw, "/config").await["default_provider"], "second");
}

#[tokio::test]
async fn authentication_validation_conflicts_and_external_edits_leave_config_unchanged() {
    let gw = gateway().await;
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let path = if method == Method::POST {
            "/config/providers"
        } else {
            "/config/providers/mock"
        };
        assert_eq!(
            Client::new()
                .request(method, format!("{}{path}", gw.url))
                .json(&provider(&gw, "mock"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for (field, value) in [
        ("id", json!("group")),
        ("id", json!("with/slash")),
        ("protocol", json!("unknown")),
        ("base_url", json!("file:///secret")),
        ("base_url", json!("https://user:secret@example.com/v1")),
        ("base_url", json!("https://example.com/v1?key=private-one")),
        ("api_keys", json!(["private-one\r\nheader:value"])),
        ("fallback", json!(["missing/model"])),
        ("fallback", json!(["unlisted-bare-model"])),
        ("models", json!(["invalid model"])),
    ] {
        let mut body = provider(&gw, "mock");
        body[field] = value;
        let response = edit(&gw, Method::POST, "/config/providers", body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{field}");
        let text = response.text().await.unwrap();
        assert!(!text.contains("private-one") && !text.contains("user:secret"));
        assert!(Config::load(&gw.path).unwrap().provider.is_empty());
    }
    assert_eq!(
        edit(
            &gw,
            Method::POST,
            "/config/providers",
            provider(&gw, "mock")
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        edit(
            &gw,
            Method::POST,
            "/config/providers",
            provider(&gw, "mock")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        edit(
            &gw,
            Method::PUT,
            "/config/providers/other",
            provider(&gw, "mock")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        edit(
            &gw,
            Method::PUT,
            "/config/providers/absent",
            provider(&gw, "absent")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/providers/absent", Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut disk = Config::load(&gw.path).unwrap();
    disk.provider[0].name = "external edit".into();
    disk.save(&gw.path).unwrap();
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/providers/mock", Value::Null)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().provider[0].name,
        "external edit"
    );
    assert_eq!(
        snapshot(&gw, "/config").await["providers"][0]["name"],
        "Test provider"
    );
}

#[tokio::test]
async fn concurrent_creates_and_default_changes_are_serialized() {
    let gw = gateway().await;
    let (first, second) = tokio::join!(
        edit(&gw, Method::POST, "/config/providers", provider(&gw, "one")),
        edit(&gw, Method::POST, "/config/providers", provider(&gw, "two"))
    );
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(second.status(), StatusCode::CREATED);
    assert_eq!(Config::load(&gw.path).unwrap().provider.len(), 2);
    let mut body = provider(&gw, "two");
    body["make_default"] = json!(true);
    assert_eq!(
        edit(&gw, Method::PUT, "/config/providers/two", body)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(Config::load(&gw.path).unwrap().default_provider, "two");
    assert_eq!(snapshot(&gw, "/config").await["default_provider"], "two");
}
