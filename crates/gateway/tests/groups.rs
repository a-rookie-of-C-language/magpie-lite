//! Authenticated group editing persists before publishing and immediately changes routing.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::post};
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, Protocol, ProviderConfig, RoutingPolicy,
    build_router,
};
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

/// A vendor whose member is down: records the ask, answers 500.
async fn failing(
    State(last): State<Arc<Mutex<Option<Value>>>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    *last.lock().unwrap() = Some(body);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": {"message": "mate is down", "type": "server_error"}})),
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
            contexts: Default::default(),
            fallback: if referenced {
                vec!["group/daily".into()]
            } else {
                Vec::new()
            },
            key_cooldown_ms: None,
            capabilities: Default::default(),
            routing: magpie_gateway::RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
            key_limits: Vec::new(),
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
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
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

async fn edit_with_key(
    gw: &Gateway,
    method: Method,
    path: &str,
    body: Value,
    key: &str,
) -> reqwest::Response {
    Client::new()
        .request(method, format!("{}{path}", gw.url))
        .bearer_auth("magpie-lite")
        .header("Idempotency-Key", key)
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn group(id: &str, model: &str) -> Value {
    json!({"id":id, "name":"Daily coding", "routes":[format!("mock/{model}")]})
}

fn weighted_group() -> Value {
    json!({
        "id": "weighted",
        "name": "Weighted routes",
        "routes": ["mock/first", "mock/second", "mock/third"],
        "weights": [3, 1, 4],
        "routing": "weight"
    })
}

#[tokio::test]
async fn idempotency_replays_group_create_without_advancing_revision() {
    let gw = gateway(false).await;
    let body = group("durable", "first");
    let first = edit_with_key(
        &gw,
        Method::POST,
        "/config/groups",
        body.clone(),
        "group-create-1",
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_snapshot: Value = first.json().await.unwrap();
    assert_eq!(first_snapshot["revision"], 2);

    let replay = edit_with_key(&gw, Method::POST, "/config/groups", body, "group-create-1").await;
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(replay.json::<Value>().await.unwrap(), first_snapshot);
    assert_eq!(Config::load(&gw.path).unwrap().group.len(), 1);

    let conflict = edit_with_key(
        &gw,
        Method::POST,
        "/config/groups",
        group("durable", "second"),
        "group-create-1",
    )
    .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].routes,
        ["mock/first"]
    );
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

#[tokio::test]
async fn group_first_token_budget_is_persisted_exposed_editable_and_defaults_to_zero() {
    let gw = gateway(false).await;
    let mut body = group("timed", "original");
    body["first_token_timeout_ms"] = json!(1234);
    let created = edit(&gw, Method::POST, "/config/groups", body.clone()).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        created.json::<Value>().await.unwrap()["groups"][0]["first_token_timeout_ms"],
        1234
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].first_token_timeout_ms,
        1234
    );
    body["first_token_timeout_ms"] = json!(5678);
    let updated = edit(&gw, Method::PUT, "/config/groups/timed", body).await;
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        updated.json::<Value>().await.unwrap()["groups"][0]["first_token_timeout_ms"],
        5678
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].first_token_timeout_ms,
        5678
    );
    for invalid in [json!(-1), json!(1.5), json!("100"), json!(null)] {
        let mut body = group("timed", "original");
        body["first_token_timeout_ms"] = invalid;
        assert_eq!(
            edit(&gw, Method::PUT, "/config/groups/timed", body)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            Config::load(&gw.path).unwrap().group[0].first_token_timeout_ms,
            5678
        );
    }
    // PUT replaces the group, including defaulting an omitted budget to zero.
    let reset = edit(
        &gw,
        Method::PUT,
        "/config/groups/timed",
        group("timed", "original"),
    )
    .await;
    assert_eq!(reset.status(), StatusCode::OK);
    assert_eq!(
        reset.json::<Value>().await.unwrap()["groups"][0]["first_token_timeout_ms"],
        0
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].first_token_timeout_ms,
        0
    );
}

#[tokio::test]
async fn group_affinity_round_trips_and_defaults_on_replace() {
    let gw = gateway(false).await;
    let mut body = group("sticky", "original");
    body["affinity"] = json!("session");
    let created = edit(&gw, Method::POST, "/config/groups", body.clone()).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        created.json::<Value>().await.unwrap()["groups"][0]["affinity"],
        "session"
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].affinity,
        magpie_gateway::Affinity::Session
    );

    body["affinity"] = json!("turn");
    let updated = edit(&gw, Method::PUT, "/config/groups/sticky", body).await;
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        updated.json::<Value>().await.unwrap()["groups"][0]["affinity"],
        "turn"
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].affinity,
        magpie_gateway::Affinity::Turn
    );

    // PUT replaces the group, so an omitted affinity returns to auto.
    let reset = edit(
        &gw,
        Method::PUT,
        "/config/groups/sticky",
        group("sticky", "original"),
    )
    .await;
    assert_eq!(reset.status(), StatusCode::OK);
    assert_eq!(
        reset.json::<Value>().await.unwrap()["groups"][0]["affinity"],
        "auto"
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].affinity,
        magpie_gateway::Affinity::Auto
    );
}

#[tokio::test]
async fn group_weights_round_trip_are_exposed_and_reject_invalid_shapes() {
    let gw = gateway(false).await;
    let created = edit(&gw, Method::POST, "/config/groups", weighted_group()).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let snapshot: Value = created.json().await.unwrap();
    assert_eq!(snapshot["groups"][0]["routing"], "weight");
    assert_eq!(snapshot["groups"][0]["weights"], json!([3, 1, 4]));
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].route_weights(),
        vec![3, 1, 4]
    );

    // A short vector is padded to the route count by the secret-free snapshot.
    let mut partial = weighted_group();
    partial["weights"] = json!([2]);
    let updated = edit(&gw, Method::PUT, "/config/groups/weighted", partial).await;
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        updated.json::<Value>().await.unwrap()["groups"][0]["weights"],
        json!([2, 1, 1])
    );
    assert_eq!(
        Config::load(&gw.path).unwrap().group[0].route_weights(),
        vec![2, 1, 1]
    );

    // An explicit empty list clears configured overrides; the snapshot still
    // materializes the effective default weight for every route.
    let mut cleared = weighted_group();
    cleared["weights"] = json!([]);
    let cleared = edit(&gw, Method::PUT, "/config/groups/weighted", cleared).await;
    assert_eq!(cleared.status(), StatusCode::OK);
    assert_eq!(
        cleared.json::<Value>().await.unwrap()["groups"][0]["weights"],
        json!([1, 1, 1])
    );
    assert!(Config::load(&gw.path).unwrap().group[0].weights.is_empty());

    for weights in [
        json!([1, 1, 1, 1]),
        json!([0, 1, 1001]),
        json!([0, -1]),
        json!("3"),
    ] {
        let mut body = weighted_group();
        body["weights"] = weights;
        let response = edit(&gw, Method::PUT, "/config/groups/weighted", body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            Config::load(&gw.path).unwrap().group[0].route_weights(),
            vec![1, 1, 1]
        );
    }
}

#[tokio::test]
async fn group_rules_round_trip_and_reject_bad_shapes() {
    let gw = gateway(false).await;
    let mut body = group("ruled", "first");
    body["routes"] = json!(["mock/first", "mock/second"]);
    body["rules"] = json!([
        {"use": "mock/second", "images": true},
        {"use": "mock/first", "pause": true, "agents": ["claude"]},
    ]);
    let created = edit(&gw, Method::POST, "/config/groups", body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let snapshot: Value = created.json().await.unwrap();
    assert_eq!(snapshot["groups"][0]["rules"][0]["use"], "mock/second");
    assert_eq!(snapshot["groups"][0]["rules"][1]["pause"], true);
    let saved = &Config::load(&gw.path).unwrap().group[0].rules;
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0].summary(), "mock/second when images");

    // A rule naming a route outside the group is refused, naming the rule.
    let mut foreign = group("foreign", "first");
    foreign["routes"] = json!(["mock/first"]);
    foreign["rules"] = json!([{"use": "mock/second", "images": true}]);
    let response = edit(&gw, Method::POST, "/config/groups", foreign).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("is not in the group")
    );

    // A rule with no condition, and a pause reaching past hours and agents,
    // are both refused with what to change.
    for (name, rules, hint) in [
        ("bare", json!([{"use": "mock/first"}]), "needs a condition"),
        (
            "greedy-pause",
            json!([{"use": "mock/first", "pause": true, "images": true}]),
            "agents or folders only",
        ),
    ] {
        let mut body = group(name, "first");
        body["rules"] = rules;
        let response = edit(&gw, Method::POST, "/config/groups", body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(response.text().await.unwrap().contains(hint), "{name}");
    }

    // PUT replaces the rule list, so omitting rules clears them.
    let cleared = edit(
        &gw,
        Method::PUT,
        "/config/groups/ruled",
        group("ruled", "first"),
    )
    .await;
    assert_eq!(cleared.status(), StatusCode::OK);
    assert!(Config::load(&gw.path).unwrap().group[0].rules.is_empty());
}

async fn gateway_with_shared_model() -> Gateway {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let last = Arc::new(Mutex::new(None));
    let (upstream, upstream_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(last.clone()),
    )
    .await;
    let peer = |id: &str| ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "secret-provider-key".into(),
        api_keys: Vec::new(),
        models: vec!["shared".into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: magpie_gateway::RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    };
    let config = Config {
        provider: vec![peer("mock-a"), peer("mock-b")],
        ..Config::default()
    };
    config.save(&path).unwrap();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
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

#[tokio::test]
async fn found_groups_serve_hide_and_come_back() {
    async fn models(gw: &Gateway) -> Value {
        Client::new()
            .get(format!("{}/v1/models", gw.url))
            .bearer_auth("magpie-lite")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn ask(gw: &Gateway, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{}/v1/chat/completions", gw.url))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hi"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    fn listed(models: &Value) -> bool {
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "auto/shared")
    }
    fn unlisted_old(models: &Value) -> bool {
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != "group/auto-shared")
    }
    let gw = gateway_with_shared_model().await;

    // Found, served and listed without writing anything.
    let snapshot: Value = Client::new()
        .get(format!("{}/config", gw.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let found = snapshot["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "auto-shared")
        .expect("found group in snapshot");
    assert_eq!(found["routes"], json!(["mock-a/shared", "mock-b/shared"]));
    assert_eq!(found["auto"], true);
    assert!(listed(&models(&gw).await));
    assert!(unlisted_old(&models(&gw).await));
    assert_eq!(ask(&gw, "auto/shared").await, StatusCode::OK);
    assert_eq!(ask(&gw, "group/auto-shared").await, StatusCode::OK);
    assert!(Config::load(&gw.path).unwrap().group.is_empty());

    // Deleting a found group hides it: unlisted, unrouted, kept for
    // bringing back.
    let removed = edit(
        &gw,
        Method::DELETE,
        "/config/groups/auto-shared",
        Value::Null,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::OK);
    let snapshot: Value = removed.json().await.unwrap();
    let hidden = snapshot["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "auto-shared")
        .expect("hidden group in snapshot");
    assert_eq!(hidden["hidden"], true);
    assert!(!listed(&models(&gw).await));
    assert_eq!(ask(&gw, "auto/shared").await, StatusCode::NOT_FOUND);
    assert_eq!(ask(&gw, "group/auto-shared").await, StatusCode::NOT_FOUND);
    assert_eq!(
        Config::load(&gw.path).unwrap().hidden_auto_groups,
        vec!["auto-shared"]
    );

    // Saving it again brings it back as the user's own.
    let mut back = group("auto-shared", "shared");
    back["routes"] = json!(["mock-a/shared", "mock-b/shared"]);
    let restored = edit(&gw, Method::POST, "/config/groups", back).await;
    assert_eq!(restored.status(), StatusCode::CREATED);
    let snapshot: Value = restored.json().await.unwrap();
    let owned = snapshot["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "auto-shared")
        .expect("restored group in snapshot");
    assert_eq!(owned["auto"], Value::Null);
    assert_eq!(ask(&gw, "group/auto-shared").await, StatusCode::OK);
    assert!(
        Config::load(&gw.path)
            .unwrap()
            .hidden_auto_groups
            .is_empty()
    );

    // Removing the user's own with a found id shows the found one again;
    // changing that found one makes it theirs, over PUT too.
    let removed = edit(
        &gw,
        Method::DELETE,
        "/config/groups/auto-shared",
        Value::Null,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::OK);
    let snapshot: Value = removed.json().await.unwrap();
    let found = snapshot["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "auto-shared")
        .expect("found group back in snapshot");
    assert_eq!(found["auto"], true);
    let mut changed = group("auto-shared", "shared");
    changed["routes"] = json!(["mock-b/shared"]);
    let updated = edit(&gw, Method::PUT, "/config/groups/auto-shared", changed).await;
    assert_eq!(updated.status(), StatusCode::OK);
    let snapshot: Value = updated.json().await.unwrap();
    let owned = snapshot["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "auto-shared")
        .expect("changed group in snapshot");
    assert_eq!(owned["auto"], Value::Null);
    assert_eq!(owned["routes"], json!(["mock-b/shared"]));
    assert_eq!(ask(&gw, "group/auto-shared").await, StatusCode::OK);
}

#[tokio::test]
async fn found_groups_off_serves_the_first_seat_but_lists_nothing() {
    async fn ask(url: &str, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hi"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    // Two vendors serving one model, on separate mocks so the stand-in's
    // seat is visible: while found groups are off, either spelling is
    // served by the first provider alone.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let first = Arc::new(Mutex::new(None));
    let second = Arc::new(Mutex::new(None));
    let (upstream_a, upstream_a_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(first.clone()),
    )
    .await;
    let (upstream_b, upstream_b_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(second.clone()),
    )
    .await;
    let peer = |id: &str, upstream: &str| ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "secret-provider-key".into(),
        api_keys: Vec::new(),
        models: vec!["shared".into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: magpie_gateway::RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    };
    let config = Config {
        provider: vec![peer("mock-a", &upstream_a), peer("mock-b", &upstream_b)],
        no_auto_groups: true,
        ..Config::default()
    };
    config.save(&path).unwrap();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    let _tasks = vec![upstream_a_task, upstream_b_task, gateway_task];
    // Listed nowhere: neither the public ref nor the old id.
    let models: Value = Client::new()
        .get(format!("{url}/v1/models"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"auto/shared"), "{ids:?}");
    assert!(!ids.contains(&"group/auto-shared"), "{ids:?}");
    assert!(ids.contains(&"mock-a/shared"), "{ids:?}");
    // Either spelling is served, from the first provider's seat.
    assert_eq!(ask(&url, "auto/shared").await, StatusCode::OK);
    assert_eq!(first.lock().unwrap().as_ref().unwrap()["model"], "shared");
    assert!(second.lock().unwrap().is_none());
    assert_eq!(ask(&url, "group/auto-shared").await, StatusCode::OK);
    assert_eq!(first.lock().unwrap().as_ref().unwrap()["model"], "shared");
    assert!(second.lock().unwrap().is_none());
}

#[tokio::test]
async fn a_bare_model_id_reaches_its_found_group() {
    async fn ask(url: &str, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hi"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    // Two vendors serve one model; the first is down. A bare id names
    // the found group (the reference's `GroupFor`), so the ask fails
    // over to the second member; sent to the first provider alone it
    // would come back 500.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let down = Arc::new(Mutex::new(None));
    let up = Arc::new(Mutex::new(None));
    let (down_url, down_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(failing))
            .with_state(down.clone()),
    )
    .await;
    let (up_url, up_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(up.clone()),
    )
    .await;
    let peer = |id: &str, upstream: &str| ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "secret-provider-key".into(),
        api_keys: Vec::new(),
        models: vec!["shared".into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: magpie_gateway::RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    };
    let config = Config {
        provider: vec![peer("mock-a", &down_url), peer("mock-b", &up_url)],
        ..Config::default()
    };
    config.save(&path).unwrap();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    let _tasks = vec![down_task, up_task, gateway_task];
    assert_eq!(ask(&url, "shared").await, StatusCode::OK);
    // The down member was tried first, in provider order, and the
    // second member served it.
    assert!(down.lock().unwrap().is_some());
    assert_eq!(up.lock().unwrap().as_ref().unwrap()["model"], "shared");
}

#[tokio::test]
async fn vendors_spelling_one_model_apart_share_one_served_group() {
    async fn ask(url: &str, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hi"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    // Two vendors list one model apart: one group, each asked for the
    // id it lists, by whatever spelling the agent uses.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let first = Arc::new(Mutex::new(None));
    let second = Arc::new(Mutex::new(None));
    let (first_url, first_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(first.clone()),
    )
    .await;
    let (second_url, second_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(second.clone()),
    )
    .await;
    let peer = |id: &str, upstream: &str, model: &str| ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "secret-provider-key".into(),
        api_keys: Vec::new(),
        models: vec![model.into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: magpie_gateway::RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    };
    let config = Config {
        provider: vec![
            peer("mock-a", &first_url, "Claude Opus 5.5"),
            peer("mock-b", &second_url, "claude-opus-5-5"),
        ],
        ..Config::default()
    };
    config.save(&path).unwrap();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    let _tasks = vec![first_task, second_task, gateway_task];
    // Listed once, under the first spelling; either spelling and the
    // bare one are served, the first vendor asked for its own id.
    let models: Value = Client::new()
        .get(format!("{url}/v1/models"))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"auto/Claude Opus 5.5"), "{ids:?}");
    assert_eq!(ask(&url, "auto/claude-opus-5-5").await, StatusCode::OK);
    assert_eq!(
        first.lock().unwrap().as_ref().unwrap()["model"],
        "Claude Opus 5.5"
    );
    assert_eq!(ask(&url, "claude-opus-5-5").await, StatusCode::OK);
    assert_eq!(
        first.lock().unwrap().as_ref().unwrap()["model"],
        "Claude Opus 5.5"
    );
}

#[tokio::test]
async fn found_groups_switch_turns_finding_off_and_on() {
    async fn snapshot(url: &str) -> Value {
        Client::new()
            .get(format!("{url}/config"))
            .bearer_auth("magpie-lite")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn switch(url: &str, revision: Option<u64>, body: Value) -> (StatusCode, Value) {
        let mut request = Client::new()
            .put(format!("{url}/config/found-groups"))
            .bearer_auth("magpie-lite");
        if let Some(revision) = revision {
            request = request.header("If-Match", revision.to_string());
        }
        let reply = request.json(&body).send().await.unwrap();
        let status = reply.status();
        let snapshot: Value = reply.json().await.unwrap_or(Value::Null);
        (status, snapshot)
    }
    async fn listed(url: &str, id: &str) -> bool {
        let models: Value = Client::new()
            .get(format!("{url}/v1/models"))
            .bearer_auth("magpie-lite")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id)
    }
    async fn ask(url: &str, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hi"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    let gw = gateway_with_shared_model().await;
    let url = &gw.url;
    let first = snapshot(url).await;
    assert_eq!(first["no_auto_groups"], false);
    let revision = first["revision"].as_u64().unwrap();
    assert!(listed(url, "auto/shared").await);
    // Not a boolean: refused, and the file is unchanged.
    let (status, _) = switch(url, Some(revision), json!({"on": "x"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!Config::load(&gw.path).unwrap().no_auto_groups);
    // Off: the snapshot says so, the found ref leaves the catalog, and
    // a request for it still serves from the first provider's seat.
    let (status, off) = switch(url, Some(revision), json!({"on": false})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(off["no_auto_groups"], true);
    assert!(off["revision"].as_u64().unwrap() > revision);
    assert!(Config::load(&gw.path).unwrap().no_auto_groups);
    assert!(!listed(url, "auto/shared").await);
    assert_eq!(ask(url, "auto/shared").await, StatusCode::OK);
    assert_eq!(gw.last.lock().unwrap().as_ref().unwrap()["model"], "shared");
    // A stale revision loses to the flip above.
    let (status, _) = switch(url, Some(revision), json!({"on": true})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // On again, at the fresh revision: listed and served as a group.
    let fresh = off["revision"].as_u64().unwrap();
    let (status, on) = switch(url, Some(fresh), json!({"on": true})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(on["no_auto_groups"], false);
    assert!(listed(url, "auto/shared").await);
    assert_eq!(ask(url, "auto/shared").await, StatusCode::OK);
}

#[tokio::test]
async fn manual_group_sends_every_request_to_its_pick_and_rules_wait() {
    async fn ask(gw: &Gateway, content: &str) -> StatusCode {
        Client::new()
            .post(format!("{}/v1/chat/completions", gw.url))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "group/manual",
                "messages": [{"role": "user", "content": content}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    fn upstream_model(gw: &Gateway) -> String {
        gw.last
            .lock()
            .unwrap()
            .as_ref()
            .expect("upstream was called")["model"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    let gw = gateway(false).await;
    let mut body = group("manual", "first");
    body["routes"] = json!(["mock/first", "mock/second"]);
    body["routing"] = json!("manual");
    body["pick"] = json!("mock/second");
    // Names the other member, and matches every message: with the group
    // routed otherwise it would send to mock/first first.
    body["rules"] = json!([{"use": "mock/first", "tokens": 1}]);
    let created = edit(&gw, Method::POST, "/config/groups", body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let snapshot: Value = created.json().await.unwrap();
    assert_eq!(snapshot["groups"][0]["routing"], "manual");
    assert_eq!(snapshot["groups"][0]["pick"], "mock/second");

    // Twice, so rotate/order state cannot explain it: the pick alone is
    // asked, and the matching rule never moves its member first.
    for _ in 0..2 {
        assert_eq!(
            ask(&gw, "Hello there, how are you doing today?").await,
            StatusCode::OK
        );
    }
    assert_eq!(upstream_model(&gw), "second");

    // One click picks the other member: the next request follows it, and
    // the pick is kept on disk.
    let mut again = group("manual", "first");
    again["routes"] = json!(["mock/first", "mock/second"]);
    again["routing"] = json!("manual");
    again["pick"] = json!("mock/first");
    again["rules"] = json!([{"use": "mock/first", "tokens": 1}]);
    let updated = edit(&gw, Method::PUT, "/config/groups/manual", again).await;
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        ask(&gw, "Hello there, how are you doing today?").await,
        StatusCode::OK
    );
    assert_eq!(upstream_model(&gw), "first");
    assert_eq!(Config::load(&gw.path).unwrap().group[0].pick, "mock/first");
}

#[tokio::test]
async fn a_manual_pick_can_name_a_bare_model_id() {
    async fn ask(gw: &Gateway, content: &str) -> StatusCode {
        Client::new()
            .post(format!("{}/v1/chat/completions", gw.url))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": "group/bare",
                "messages": [{"role": "user", "content": content}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    fn upstream_model(gw: &Gateway) -> String {
        gw.last
            .lock()
            .unwrap()
            .as_ref()
            .expect("upstream was called")["model"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    let gw = gateway(false).await;
    // A bare model id one member has names it, stored as its route.
    let mut body = group("bare", "first");
    body["routes"] = json!(["mock/first", "mock/second"]);
    body["routing"] = json!("manual");
    body["pick"] = json!("second");
    let created = edit(&gw, Method::POST, "/config/groups", body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let snapshot: Value = created.json().await.unwrap();
    assert_eq!(snapshot["groups"][0]["pick"], "mock/second");
    assert_eq!(
        ask(&gw, "Hello there, how are you doing today?").await,
        StatusCode::OK
    );
    assert_eq!(upstream_model(&gw), "second");
    // A pick naming nothing is no pick: the manual group falls back to
    // its first route.
    let mut bad = group("bare", "first");
    bad["routes"] = json!(["mock/first", "mock/second"]);
    bad["routing"] = json!("manual");
    bad["pick"] = json!("ghost");
    let updated = edit(&gw, Method::PUT, "/config/groups/bare", bad).await;
    assert_eq!(updated.status(), StatusCode::OK);
    let snapshot: Value = updated.json().await.unwrap();
    assert_eq!(snapshot["groups"][0]["pick"], "mock/first");
    assert_eq!(
        ask(&gw, "Hello there, how are you doing today?").await,
        StatusCode::OK
    );
    assert_eq!(upstream_model(&gw), "first");
}

#[tokio::test]
async fn recent_requests_name_the_rule_that_matched() {
    async fn ask(gw: &Gateway, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{}/v1/chat/completions", gw.url))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hello there, how are you doing today?"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    async fn stats(gw: &Gateway) -> Value {
        Client::new()
            .get(format!("{}/stats", gw.url))
            .bearer_auth("magpie-lite")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    let gw = gateway(false).await;
    // A rule on one token fires for any real message; a second group
    // names a threshold no message reaches.
    let mut matched = group("traced", "first");
    matched["routes"] = json!(["mock/first", "mock/second"]);
    matched["rules"] = json!([{"use": "mock/second", "tokens": 1}]);
    assert_eq!(
        edit(&gw, Method::POST, "/config/groups", matched)
            .await
            .status(),
        StatusCode::CREATED
    );
    let mut quiet = group("quiet", "first");
    quiet["routes"] = json!(["mock/first", "mock/second"]);
    quiet["rules"] = json!([{"use": "mock/second", "tokens": 100000000}]);
    assert_eq!(
        edit(&gw, Method::POST, "/config/groups", quiet)
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(ask(&gw, "group/traced").await, StatusCode::OK);
    assert_eq!(ask(&gw, "group/quiet").await, StatusCode::OK);
    let recent = stats(&gw).await["recent_requests"].clone();
    let recent = recent.as_array().expect("recent requests");
    let traced = recent
        .iter()
        .find(|record| record["requested_model"] == "group/traced")
        .expect("traced record");
    assert_eq!(traced["provider"], "mock");
    assert_eq!(traced["upstream_model"], "second");
    assert_eq!(
        traced["rule"],
        json!({"rule": 1, "use": "mock/second", "when": ["tokens ≥ 1"]})
    );
    let quiet = recent
        .iter()
        .find(|record| record["requested_model"] == "group/quiet")
        .expect("quiet record");
    assert_eq!(quiet["upstream_model"], "first");
    assert!(
        quiet.get("rule").is_none_or(|rule| rule.is_null()),
        "no rule matched: {}",
        quiet["rule"]
    );
}

#[tokio::test]
async fn found_group_request_level_round_trip() {
    async fn ask(url: &str, model: &str) -> StatusCode {
        Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("magpie-lite")
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Hi"}],
            }))
            .send()
            .await
            .unwrap()
            .status()
    }
    // A found ref may ask for an effort on itself (`auto/<model>:<level>`):
    // the group serves it at that effort.
    let gw = gateway_with_shared_model().await;
    assert_eq!(ask(&gw.url, "auto/shared:high").await, StatusCode::OK);
    {
        let body = gw.last.lock().unwrap().clone().expect("upstream was asked");
        assert_eq!(body["model"], "shared");
        assert_eq!(body["reasoning_effort"], "high");
    }
    assert_eq!(ask(&gw.url, "auto/shared:low").await, StatusCode::OK);
    {
        let body = gw.last.lock().unwrap().clone().expect("upstream was asked");
        assert_eq!(body["reasoning_effort"], "low");
    }
    // An unknown level is no found ref: 404, and nothing is written.
    // (`ultra` is known — it fits to `max` — so the probe uses a level
    // no dialect names.)
    assert_eq!(
        ask(&gw.url, "auto/shared:turbo").await,
        StatusCode::NOT_FOUND
    );
    assert!(Config::load(&gw.path).unwrap().group.is_empty());
    // A provider's own colon-model stays whole: `openrouter/x:free` is
    // its model, not a level ask. A provider id'd `auto` owning such a
    // model keeps `auto/<model>:<level>` for itself the same way.
    let guard = |id: &str, models: Vec<&str>| ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key: "secret-provider-key".into(),
        api_keys: Vec::new(),
        models: models.into_iter().map(|m| m.to_owned()).collect(),
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: magpie_gateway::RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    };
    let colon = Config {
        provider: vec![guard("openrouter", vec!["x:free"])],
        ..Config::default()
    };
    assert_eq!(
        colon.member_effort("openrouter/x:free"),
        ("openrouter/x:free".to_owned(), String::new())
    );
    let owned = Config {
        provider: vec![guard("auto", vec!["shared:high"])],
        ..Config::default()
    };
    assert_eq!(
        owned.member_effort("auto/shared:high"),
        ("auto/shared:high".to_owned(), String::new())
    );
    // A request-level `:max` does not leak onto failover: the down seat
    // is tried at `:max`, the second seat serves with no fixed effort.
    // (Members fixed at their own `:level` keep it mid-chain: covered
    // below with a written group.)
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let down = Arc::new(Mutex::new(None));
    let up = Arc::new(Mutex::new(None));
    let (down_url, down_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(failing))
            .with_state(down.clone()),
    )
    .await;
    let (up_url, up_task) = bind(
        Router::new()
            .route("/v1/chat/completions", post(mock))
            .with_state(up.clone()),
    )
    .await;
    let peer = |id: &str, upstream: &str| ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "secret-provider-key".into(),
        api_keys: Vec::new(),
        models: vec!["shared".into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: magpie_gateway::RoutingPolicy::Rotate,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    };
    let config = Config {
        provider: vec![peer("mock-a", &down_url), peer("mock-b", &up_url)],
        ..Config::default()
    };
    config.save(&path).unwrap();
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, gateway_task) = bind(build_router(state)).await;
    let _tasks = vec![down_task, up_task, gateway_task];
    assert_eq!(ask(&url, "auto/shared:max").await, StatusCode::OK);
    assert_eq!(
        down.lock().unwrap().as_ref().unwrap()["reasoning_effort"],
        "max"
    );
    {
        let served = up.lock().unwrap().clone().expect("failover served");
        assert_eq!(served["model"], "shared");
        assert!(
            served.get("reasoning_effort").is_none(),
            "request :max must not leak onto failover: {served}"
        );
    }
    // Members fixed at their own `:level` keep it mid-chain: the down
    // seat at `:low` fails, the next seat serves at its own `:high`.
    let created = Client::new()
        .request(Method::POST, format!("{url}/config/groups"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "id": "chain",
            "name": "chain",
            "routes": ["mock-a/shared:low", "mock-b/shared:high"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    *down.lock().unwrap() = None;
    *up.lock().unwrap() = None;
    assert_eq!(ask(&url, "group/chain").await, StatusCode::OK);
    assert_eq!(
        down.lock().unwrap().as_ref().unwrap()["reasoning_effort"],
        "low"
    );
    assert_eq!(
        up.lock().unwrap().as_ref().unwrap()["reasoning_effort"],
        "high"
    );
}

/// A fake vendor that counts asks, holds `hold`, then answers whole.
#[derive(Clone)]
struct PatientMember {
    asked: Arc<AtomicUsize>,
    hold: Duration,
}

impl PatientMember {
    fn new(hold: Duration) -> Self {
        Self {
            asked: Arc::new(AtomicUsize::new(0)),
            hold,
        }
    }
}

async fn patient_chat(
    State(member): State<PatientMember>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    member.asked.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(member.hold).await;
    Json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

fn patient_provider(url: String) -> ProviderConfig {
    ProviderConfig {
        id: "solo".into(),
        name: "Solo".into(),
        protocol: Protocol::Chat,
        base_url: url,
        api_key: "fixture-key".into(),
        api_keys: Vec::new(),
        models: vec!["mock-model".into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: RoutingPolicy::Order,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: Some(1),
        max_rpm: None,
        queue_limit: Some(1),
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    }
}

/// A gateway whose only group `g` holds one slow member, with the
/// patience given.
async fn patient_gateway(patience_s: Option<i64>, hold: Duration) -> (String, Arc<AtomicUsize>) {
    let member = PatientMember::new(hold);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let member = member.clone();
        async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(patient_chat))
                    .with_state(member),
            )
            .await
            .unwrap()
        }
    });
    let config = Config {
        provider: vec![patient_provider(format!("http://{addr}/v1"))],
        group: vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes: vec!["solo/mock-model".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s,
        }],
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let state = state.clone();
        async move { axum::serve(listener, build_router(state)).await.unwrap() }
    });
    (format!("http://{addr}"), member.asked)
}

async fn patient_ask(gateway: String) -> (StatusCode, Value) {
    let resp = Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": "group/g",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

/// A group whose only member is busy asks it again as its slot frees,
/// within its patience, instead of answering 429 (the reference's
/// replan, #1418).
#[tokio::test]
async fn a_busy_group_is_asked_again_and_served_within_patience() {
    let (gw, asked) = patient_gateway(None, Duration::from_millis(300)).await;
    // Occupy the slot and its one queue place.
    let first = tokio::spawn(patient_ask(gw.clone()));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let queued = tokio::spawn(patient_ask(gw.clone()));
    tokio::time::sleep(Duration::from_millis(60)).await;
    // The slot and its queue place are taken: without patience this
    // third request is a 429 at once; with it the round is asked
    // again and the request is served.
    let (status, body) = patient_ask(gw.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drop(first.await.unwrap());
    drop(queued.await.unwrap());
    assert_eq!(
        asked.load(Ordering::SeqCst),
        3,
        "the refused round reached the vendor again"
    );
}

/// Patience spent while every member stays busy answers 429: the wait
/// does not outlast what the group allows (the reference's replan,
/// #1418).
#[tokio::test]
async fn a_group_out_of_patience_answers_429_while_busy() {
    let (gw, asked) = patient_gateway(Some(1), Duration::from_millis(700)).await;
    // Occupy the slot and its one queue place past the one second.
    let first = tokio::spawn(patient_ask(gw.clone()));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let queued = tokio::spawn(patient_ask(gw.clone()));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let (status, body) = patient_ask(gw.clone()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    drop(first.await.unwrap());
    drop(queued.await.unwrap());
    assert_eq!(
        asked.load(Ordering::SeqCst),
        2,
        "the refused request never went upstream"
    );
}

/// A vendor that is overloaded once, then serves: records the ask,
/// answers 503 (a held SSE overload for stream asks when told to, or a
/// credit 429), then hello.
#[derive(Clone)]
struct FlakyMember {
    asked: Arc<AtomicUsize>,
    fail_first_stream: bool,
    credit_first: bool,
    rate_limited_first: bool,
}

async fn flaky_chat(State(member): State<FlakyMember>, Json(body): Json<Value>) -> Response {
    let n = member.asked.fetch_add(1, Ordering::SeqCst);
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if n == 0 && member.credit_first {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": {"message": "insufficient credit balance, please recharge"}})),
        )
            .into_response();
    }
    if n == 0 && member.rate_limited_first {
        // A short rate limit names when the key is back: the replan
        // waits out the Retry-After (floored at the round's pause)
        // instead of ending on it.
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "1"), ("content-type", "application/json")],
            Json(json!({"error": {"type": "rate_limit_error", "message": "slow down"}})),
        )
            .into_response();
    }
    if n == 0 {
        if streaming && member.fail_first_stream {
            // A held stream failure: nothing visible went out, so the
            // round may be asked again within patience.
            return Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(
                    "data: {\"error\":{\"code\":503,\"message\":\"overloaded\"}}\n\n",
                ))
                .unwrap();
        }
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "overloaded"}})),
        )
            .into_response();
    }
    if streaming {
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            )))
            .unwrap();
    }
    Json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

/// A gateway whose only group `g` holds one flaky member, with the
/// patience given.
async fn flaky_gateway(
    patience_s: Option<i64>,
    fail_first_stream: bool,
    credit_first: bool,
    rate_limited_first: bool,
) -> (String, Arc<AtomicUsize>) {
    let member = FlakyMember {
        asked: Arc::new(AtomicUsize::new(0)),
        fail_first_stream,
        credit_first,
        rate_limited_first,
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let member = member.clone();
        async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(flaky_chat))
                    .with_state(member),
            )
            .await
            .unwrap()
        }
    });
    let config = Config {
        provider: vec![patient_provider(format!("http://{addr}/v1"))],
        group: vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes: vec!["solo/mock-model".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s,
        }],
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let state = state.clone();
        async move { axum::serve(listener, build_router(state)).await.unwrap() }
    });
    (format!("http://{addr}"), member.asked)
}

async fn flaky_ask(gateway: String, model: &str, stream: bool) -> (StatusCode, String) {
    let resp = Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth("magpie-lite")
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": stream,
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    (status, body)
}

/// A group whose only member answers 503 once serves on the ask again
/// within its patience, instead of ending on the overload (the
/// reference's replan, #1418).
#[tokio::test]
async fn a_transient_overload_is_asked_again_and_served() {
    let (gw, asked) = flaky_gateway(None, false, false, false).await;
    let (status, body) = flaky_ask(gw, "group/g", false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("hello"), "{body}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        2,
        "the overloaded round reached the vendor again"
    );
}

/// A held stream overload asks the round again too: the prefetch held
/// nothing visible, so nothing was lost (the reference's replan, #1418).
#[tokio::test]
async fn a_held_stream_overload_is_asked_again_and_served() {
    let (gw, asked) = flaky_gateway(None, true, false, false).await;
    let (status, body) = flaky_ask(gw, "group/g", true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("hello"), "{body}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        2,
        "the overloaded stream reached the vendor again"
    );
}

/// The money gone is final: a credit 429 stands without the round being
/// asked again (the reference's `busy`, #1418).
#[tokio::test]
async fn a_credit_429_stands_without_being_asked_again() {
    let (gw, asked) = flaky_gateway(None, false, true, false).await;
    let (status, body) = flaky_ask(gw, "group/g", false).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the credit refusal never reached the vendor again"
    );
}

/// A pause past the group's patience stands on the first answer: the
/// wait never outlasts what the group allows (the reference's replan,
/// #1418).
#[tokio::test]
async fn an_overload_beyond_patience_stands_on_the_first_answer() {
    let (gw, asked) = flaky_gateway(Some(1), false, false, false).await;
    let (status, body) = flaky_ask(gw, "group/g", false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the out-of-patience overload never reached the vendor again"
    );
}

/// A direct provider keeps today's fail-fast on a transient overload:
/// no group, no patience, no ask again (the reference's replan is a
/// group behavior, #1418).
#[tokio::test]
async fn a_direct_provider_keeps_fail_fast_on_transient_overload() {
    let (gw, asked) = flaky_gateway(None, false, false, false).await;
    let (status, body) = flaky_ask(gw, "solo/mock-model", false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the direct ask never reached the vendor again"
    );
}

/// A short 429 names its Retry-After: the group waits it out (floored
/// at the round's pause) and is served on the ask again, instead of
/// ending on the limit (the reference's replan, #1418).
#[tokio::test]
async fn a_rate_limit_retry_after_is_honored_within_patience() {
    let (gw, asked) = flaky_gateway(None, false, false, true).await;
    let (status, body) = flaky_ask(gw, "group/g", false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("hello"), "{body}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        2,
        "the rate-limited round reached the vendor again"
    );
}

fn named_provider(id: &str, url: String, model: &str) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: url,
        api_key: "fixture-key".into(),
        api_keys: Vec::new(),
        models: vec![model.into()],
        contexts: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: RoutingPolicy::Order,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    }
}

/// Two members behind one mock: `model-a` refuses the request outright,
/// `model-b` is overloaded. Each records its own asks.
#[derive(Clone)]
struct MixedMember {
    asked_a: Arc<AtomicUsize>,
    asked_b: Arc<AtomicUsize>,
}

async fn mixed_chat(State(member): State<MixedMember>, Json(body): Json<Value>) -> Response {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if model == "model-a" {
        member.asked_a.fetch_add(1, Ordering::SeqCst);
        // Final: no such thing served here, however often it is asked.
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"message": "not found"}})),
        )
            .into_response();
    }
    member.asked_b.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {"message": "overloaded"}})),
    )
        .into_response()
}

/// A group whose first member refuses outright and whose second is
/// overloaded: the round had a final failure, so nothing is asked again
/// and the overload stands (the reference's replan replans only
/// all-passing rounds, #1418).
#[tokio::test]
async fn a_mixed_final_and_transient_round_stands_without_replan() {
    let member = MixedMember {
        asked_a: Arc::new(AtomicUsize::new(0)),
        asked_b: Arc::new(AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let member = member.clone();
        async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(mixed_chat))
                    .with_state(member),
            )
            .await
            .unwrap()
        }
    });
    let url = format!("http://{addr}/v1");
    let config = Config {
        provider: vec![
            named_provider("pa", url.clone(), "model-a"),
            named_provider("pb", url, "model-b"),
        ],
        group: vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes: vec!["pa/model-a".into(), "pb/model-b".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s: None,
        }],
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let state = state.clone();
        async move { axum::serve(listener, build_router(state)).await.unwrap() }
    });
    let (status, body) = flaky_ask(format!("http://{addr}"), "group/g", false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        member.asked_a.load(Ordering::SeqCst),
        1,
        "the refusing member was asked once"
    );
    assert_eq!(
        member.asked_b.load(Ordering::SeqCst),
        1,
        "the overloaded member was never asked again"
    );
}
