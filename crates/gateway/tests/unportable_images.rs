//! A Responses `file_id` image names an image only its own vendor API can
//! open. The reference answers 400 on a cross-protocol route
//! (`hasUnportableCurrentImage`); lite cannot carry it on a native route
//! either, so it is refused before any upstream call instead of being
//! silently dropped. Native client request, strict loopback upstream.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use magpie_gateway::{
    AffinityStore, AppState, Config, ConfigStore, GroupConfig, KeyRotator, Protocol,
    ProviderConfig, RoutingPolicy, build_router, usage::RequestLog,
};
use serde_json::{Value, json};

const TOKEN: &str = "PRIVATE_UNPORTABLE_GATEWAY_TOKEN";
const KEY: &str = "PRIVATE_UNPORTABLE_PROVIDER_KEY";

struct Server {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { base, task }
}

fn body(model: &str) -> Value {
    json!({"model":model,"input":[
        {"role":"user","content":[
            {"type":"input_text","text":"read this"},
            {"type":"input_image","file_id":"file_private"}
        ]}
    ]})
}

async fn upstream(State(seen): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>) -> Response {
    seen.lock().unwrap().push(body);
    Json(json!({"unexpected": "upstream must not be called"})).into_response()
}

struct Fixture {
    gateway: Server,
    _upstream: Server,
    _home: tempfile::TempDir,
    seen: Arc<Mutex<Vec<Value>>>,
}
async fn fixture(protocol: Protocol) -> Fixture {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let endpoint = match protocol {
        Protocol::Chat => "/v1/chat/completions",
        Protocol::Responses => "/v1/responses",
        _ => unreachable!(),
    };
    let upstream =
        serve(Router::new().route(endpoint, post(upstream).with_state(seen.clone()))).await;
    let home = tempfile::tempdir().unwrap();
    let base = format!("{}/v1", upstream.base);
    let config = Config {
        token: TOKEN.into(),
        default_provider: "mock".into(),
        hidden_auto_groups: Vec::new(),
        no_auto_groups: false,
        history_path: Some(home.path().join("history.jsonl")),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol,
            capabilities: Default::default(),
            base_url: base,
            api_key: KEY.into(),
            api_keys: vec![],
            models: vec!["mock-model".into()],
            contexts: Default::default(),
            outputs: Default::default(),
            fallback: vec![],
            key_cooldown_ms: None,
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            affinity: Default::default(),
            max_concurrency: None,
            max_rpm: None,
            queue_limit: None,
            queue_wait_s: None,
            key_generation: 0,
            key_limits: Vec::new(),
        }],
        group: vec![GroupConfig {
            id: "daily".into(),
            name: "Daily".into(),
            routes: vec!["mock/mock-model".into()],
            rules: Vec::new(),
            classifier: String::new(),
            effort: String::new(),
            levels: Vec::new(),
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Rotate,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s: None,
        }],
        ..Config::default()
    };
    let log = Arc::new(Mutex::new(
        RequestLog::open(&home.path().join("history.jsonl")).unwrap(),
    ));
    let gateway = serve(build_router(Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: log,
        key_rotation: KeyRotator::default(),
        affinity: AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    })))
    .await;
    Fixture {
        gateway,
        _upstream: upstream,
        _home: home,
        seen,
    }
}

#[tokio::test]
async fn file_id_images_are_refused_before_any_upstream_call() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    // Cross-protocol: a Responses client image a Chat upstream could never
    // open is refused, as the reference refuses it.
    let cross = fixture(Protocol::Chat).await;
    let response = client
        .post(format!("{}/v1/responses", cross.gateway.base))
        .bearer_auth(TOKEN)
        .json(&body("mock/mock-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let wire = response.text().await.unwrap();
    assert!(wire.contains("image_url"), "{wire}");
    assert!(
        cross.seen.lock().unwrap().is_empty(),
        "unportable image reached the upstream"
    );
    // Native: lite has no file registry, so the same refusal applies even
    // where the reference would pass the file_id through.
    let native = fixture(Protocol::Responses).await;
    let response = client
        .post(format!("{}/v1/responses", native.gateway.base))
        .bearer_auth(TOKEN)
        .json(&body("mock/mock-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let wire = response.text().await.unwrap();
    assert!(wire.contains("image_url"), "{wire}");
    assert!(
        native.seen.lock().unwrap().is_empty(),
        "unportable image reached the native upstream"
    );
}
