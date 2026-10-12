//! Authenticated group editing persists before publishing and immediately changes routing.
use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, routing::post};
use magpie_gateway::{AppState, Config, ConfigStore, Protocol, ProviderConfig, build_router};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    path: std::path::PathBuf,
    last: Arc<Mutex<Option<Value>>>,
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

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

async fn mock(
    State(last): State<Arc<Mutex<Option<Value>>>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    *last.lock().unwrap() = Some(body);
    Json(
        json!({"id":"mock", "object":"chat.completion", "model":"mock",
        "choices":[{"index":0, "message":{"role":"assistant", "content":"ok"}, "finish_reason":"stop"}]}),
    )
}

async fn gateway(referenced: bool) -> Gateway {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let last = Arc::new(Mutex::new(None));
    let (upstream, upstream_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(last.clone()),
    )
    .await;
    let config = Config {
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Chat,
            base_url: format!("{upstream}/v1"),
            api_key: "secret-provider-key".into(),
            api_keys: Vec::new(),
            models: vec!["original".into()],
            fallback: if referenced {
                vec!["group/daily".into()]
            } else {
                Vec::new()
            },
            key_cooldown_ms: None,
            capabilities: Default::default(),
            key_generation: 0,
        }],
        ..Config::default()
    };
    config.save(&path).unwrap();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    Gateway {
        url,
        path,
        last,
        _directory: directory,
        tasks: vec![upstream_task, gateway_task],
    }
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

fn group(id: &str, model: &str) -> Value {
    json!({"id":id, "name":"Daily coding", "routes":[format!("mock/{model}")]})
}

#[tokio::test]
async fn group_crud_persists_and_changes_catalog_and_upstream_without_restart() {
    let gw = gateway(false).await;
    let response = edit(&gw, Method::POST, "/config/groups", group("daily", "first")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let snapshot = response.text().await.unwrap();
    assert!(!snapshot.contains("secret-provider-key"));
    assert!(!snapshot.contains("magpie-lite"));
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].routes,
        ["mock/first"]
    );

    for (expected_model, rename) in [("first", false), ("second", true)] {
        if rename {
            assert_eq!(
                edit(
                    &gw,
                    Method::PUT,
                    "/config/groups/daily",
                    group("daily", "second")
                )
                .await
                .status(),
                StatusCode::OK
            );
        }
        let catalog: Value = Client::new()
            .get(format!("{}/v1/models", gw.url))
            .bearer_auth("magpie-lite")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "group/daily")
        );
        let reply = edit(
            &gw,
            Method::POST,
            "/v1/chat/completions",
            json!({"model":"group/daily", "messages":[{"role":"user", "content":"test"}]}),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(
            gw.last.lock().unwrap().as_ref().unwrap()["model"],
            expected_model
        );
    }
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].routes,
        ["mock/second"]
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/groups/daily", Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    assert!(Config::load(&gw.path).unwrap().group.is_empty());
    let reply = edit(
        &gw,
        Method::POST,
        "/v1/chat/completions",
        json!({"model":"group/daily", "messages":[{"role":"user", "content":"test"}]}),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::NOT_FOUND);
    let catalog: Value = Client::new()
        .get(format!("{}/v1/models", gw.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "group/daily")
    );
}

#[tokio::test]
async fn edits_require_token_and_reject_invalid_input_without_changing_disk() {
    let gw = gateway(false).await;
    let original = std::fs::read(&gw.path).unwrap();
    for (method, path) in [
        (Method::POST, "/config/groups"),
        (Method::PUT, "/config/groups/daily"),
        (Method::DELETE, "/config/groups/daily"),
    ] {
        let response = Client::new()
            .request(method, format!("{}{path}", gw.url))
            .json(&group("daily", "test"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = Client::new()
        .post(format!("{}/config/groups", gw.url))
        .bearer_auth("magpie-lite")
        .body("{broken")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    for body in [
        json!({"id":"bad/id", "routes":["mock/test"]}),
        json!({"id":" ", "routes":["mock/test"]}),
        json!({"id":"daily", "routes":[]}),
        json!({"id":"daily", "routes":["group/nested"]}),
        json!({"id":"daily", "routes":["unknown/model"]}),
        json!({"id":"daily", "routes":["mock/"]}),
        json!({"id":"daily", "routes":["mock"]}),
        json!({"id":"daily", "routes":["mock/bad model"]}),
    ] {
        assert_eq!(
            edit(&gw, Method::POST, "/config/groups", body)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        edit(
            &gw,
            Method::PUT,
            "/config/groups/daily",
            group("other", "test")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        edit(
            &gw,
            Method::PUT,
            "/config/groups/missing",
            group("missing", "test")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/groups/missing", Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(std::fs::read(&gw.path).unwrap(), original);
}

#[tokio::test]
async fn duplicate_and_referenced_group_are_conflicts() {
    let gw = gateway(true).await;
    assert_eq!(
        edit(&gw, Method::POST, "/config/groups", group("daily", "test"))
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        edit(
            &gw,
            Method::POST,
            "/config/groups",
            group("daily", "different")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        edit(&gw, Method::DELETE, "/config/groups/daily", Value::Null)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].routes,
        ["mock/test"]
    );
}

#[tokio::test]
async fn concurrent_edits_are_serialized_and_external_file_changes_are_preserved() {
    let gw = gateway(false).await;
    let (a, b) = tokio::join!(
        edit(&gw, Method::POST, "/config/groups", group("a", "a")),
        edit(&gw, Method::POST, "/config/groups", group("b", "b"))
    );
    assert_eq!(a.status(), StatusCode::CREATED);
    assert_eq!(b.status(), StatusCode::CREATED);
    assert_eq!(Config::load(&gw.path).unwrap().group.len(), 2);
    let mut disk = Config::load(&gw.path).unwrap();
    disk.token = "externally-changed-secret".into();
    disk.save(&gw.path).unwrap();
    let original = std::fs::read(&gw.path).unwrap();
    let response = edit(&gw, Method::PUT, "/config/groups/a", group("a", "changed")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        !response
            .text()
            .await
            .unwrap()
            .contains("externally-changed-secret")
    );
    assert_eq!(std::fs::read(&gw.path).unwrap(), original);
    let snapshot: Value = Client::new()
        .get(format!("{}/config", gw.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        snapshot["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == "a" && g["routes"][0] == "mock/a")
    );
}
