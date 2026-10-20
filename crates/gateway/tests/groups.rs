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
