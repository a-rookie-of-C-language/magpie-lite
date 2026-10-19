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
