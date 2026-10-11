//! Authenticated HTTP agent management uses only an explicit temporary home.
use magpie_agents::lifecycle::Manager;
use magpie_gateway::agents::AgentService;
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, Protocol, ProviderConfig, build_router,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
const TOKEN: &str = "private-agent-http-token";
const UPSTREAM_KEY: &str = "never-expose-provider-secret";
struct Server {
    root: tempfile::TempDir,
    path: PathBuf,
    url: String,
    port: u16,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(enabled: bool) -> Server {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let config = Config {
        token: TOKEN.into(),
        port: 12345,
        default_provider: "mock".into(),
        provider: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock".into(),
            protocol: Protocol::Chat,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: UPSTREAM_KEY.into(),
            api_keys: Vec::new(),
            models: vec!["model".into()],
            fallback: Vec::new(),
            key_cooldown_ms: None,
            key_generation: 0,
        }],
        group: vec![GroupConfig {
            id: "daily".into(),
            name: "Daily".into(),
            routes: vec!["mock/model".into()],
        }],
        ..Config::default()
    };
    config.save(&path).unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let port = address.port();
    let agents = enabled.then(|| AgentService {
        manager: Arc::new(
            Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap(),
        ),
        port,
    });
    let state = Arc::new(AppState {
        agents,
        config: ConfigStore::new(config, Some(path.clone())),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, build_router(state)).await.unwrap();
    });
    Server {
        root,
        path,
        url: format!("http://{address}"),
        port,
        task,
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
    }
}
impl Server {
    async fn request(&self, method: Method, path: &str, body: Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn ok(&self, method: Method, path: &str, body: Value) -> Value {
        let r = self.request(method, path, body).await;
        assert_eq!(r.status(), StatusCode::OK);
        let text = r.text().await.unwrap();
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(UPSTREAM_KEY));
        serde_json::from_str(&text).unwrap()
    }
    async fn preview(&self, agent: &str, action: &str, model: &str) -> Value {
        self.ok(
            Method::POST,
            &format!("/agents/{agent}/preview"),
            json!({"action":action,"model":model}),
        )
        .await
    }
    async fn apply(&self, agent: &str, preview: &Value) -> Value {
        self.ok(Method::POST, &format!("/agents/{agent}/apply"), json!({"action":preview["action"],"model":preview["model"],"revision":preview["revision"]})).await
    }
}
#[tokio::test]
async fn agent_routes_require_auth_and_missing_service_stays_sanitized() {
    let s = server(false).await;
    for path in [
        "/agents",
        "/agents/claude/preview",
        "/agents/claude/apply",
        "/agents/claude/recover",
    ] {
        let method = if path == "/agents" {
            Method::GET
        } else {
            Method::POST
        };
        let r = s
            .client
            .request(method, format!("{}{path}", s.url))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let text = r.text().await.unwrap();
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(UPSTREAM_KEY));
    }
    let r = s.request(Method::GET, "/agents", json!(null)).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        s.client
            .get(format!("{}/health", s.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
#[tokio::test]
async fn all_agents_preview_apply_restore_without_mutating_config_or_usage() {
    let s = server(true).await;
    let config_before = std::fs::read(&s.path).unwrap();
    let stats_before = s.ok(Method::GET, "/stats", json!(null)).await;
    let list = s.ok(Method::GET, "/agents", json!(null)).await;
    assert_eq!(list["agents"].as_array().unwrap().len(), 7);
    for (index, agent) in [
        "claude", "codex", "crush", "gemini", "goose", "opencode", "zed",
    ]
    .into_iter()
    .enumerate()
    {
        let model = match index % 3 {
            0 => "mock/model",
            1 => "model",
            _ => "group/daily",
        };
        let p = s.preview(agent, "connect", model).await;
        for file in p["files"].as_array().unwrap() {
            assert!(!PathBuf::from(file["path"].as_str().unwrap()).exists());
        }
        let status = s.apply(agent, &p).await;
        assert_eq!(status["connected"], true);
        assert_eq!(status["model"], model);
        for file in p["files"].as_array().unwrap() {
            let bytes = std::fs::read(file["path"].as_str().unwrap()).unwrap();
            assert_eq!(bytes.len() as u64, file["after_bytes"].as_u64().unwrap());
            if agent != "zed" && String::from_utf8_lossy(&bytes).contains("127.0.0.1") {
                assert!(String::from_utf8_lossy(&bytes).contains(&format!("127.0.0.1:{}", s.port)));
            }
        }
        let p = s.preview(agent, "disconnect", "").await;
        assert_eq!(s.apply(agent, &p).await["connected"], false);
        for file in p["files"].as_array().unwrap() {
            assert!(!PathBuf::from(file["path"].as_str().unwrap()).exists());
        }
    }
    assert_eq!(std::fs::read(&s.path).unwrap(), config_before);
    assert_eq!(s.ok(Method::GET, "/stats", json!(null)).await, stats_before);
}
#[tokio::test]
async fn invalid_actions_models_revisions_and_payloads_have_static_errors() {
    let s = server(true).await;
    for (path, body, status) in [
        (
            "/agents/unknown/preview",
            json!({"action":"connect","model":"mock/model"}),
            StatusCode::NOT_FOUND,
        ),
        (
            "/agents/claude/preview",
            json!({"action":"overwrite","model":UPSTREAM_KEY}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/agents/claude/preview",
            json!({"action":"connect","model":"absent/model"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/agents/claude/preview",
            json!({"action":"connect"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/agents/claude/preview",
            json!({"action":"connect","model":"mock/model","path":UPSTREAM_KEY}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/agents/claude/apply",
            json!({"action":"connect","model":"mock/model"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/agents/claude/apply",
            json!({"action":"connect","model":"mock/model","revision":"invalid"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/agents/claude/apply",
            json!({"action":"connect","model":"mock/model","revision":"0".repeat(64)}),
            StatusCode::CONFLICT,
        ),
        (
            "/agents/claude/preview",
            json!({"action":"disconnect"}),
            StatusCode::CONFLICT,
        ),
    ] {
        let r = s.request(Method::POST, path, body).await;
        assert_eq!(r.status(), status, "{path}");
        let text = r.text().await.unwrap();
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(UPSTREAM_KEY));
    }
    assert!(!s.root.path().join("home/.claude/settings.json").exists());
}
#[tokio::test]
async fn stale_preview_and_drift_preserve_external_edits() {
    let s = server(true).await;
    let p = s.preview("claude", "connect", "mock/model").await;
    let path = PathBuf::from(p["files"][0]["path"].as_str().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"external\":true}").unwrap();
    let edit = json!({"action":"connect","model":"mock/model","revision":p["revision"]});
    assert_eq!(
        s.request(Method::POST, "/agents/claude/apply", edit)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let p = s.preview("claude", "connect", "mock/model").await;
    s.apply("claude", &p).await;
    std::fs::write(&path, b"{\"external\":\"keep\"}").unwrap();
    let list = s.ok(Method::GET, "/agents", json!(null)).await;
    assert_eq!(list["agents"][0]["drifted"], true);
    assert_eq!(
        s.request(
            Method::POST,
            "/agents/claude/preview",
            json!({"action":"disconnect"})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(std::fs::read(path).unwrap(), b"{\"external\":\"keep\"}");
}
#[tokio::test]
async fn recovery_endpoint_rolls_back_a_durable_pending_connection() {
    let s = server(true).await;
    let p = s.preview("gemini", "connect", "mock/model").await;
    s.apply("gemini", &p).await;
    let journal_path = s.root.path().join("agent-state/gemini.json");
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    journal["phase"] = json!("connecting");
    journal["rollback"] = json!(
        journal["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["baseline"].clone())
            .collect::<Vec<_>>()
    );
    std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let list = s.ok(Method::GET, "/agents", json!(null)).await;
    assert_eq!(list["agents"][3]["recovery_needed"], true);
    let r = s
        .ok(Method::POST, "/agents/gemini/recover", json!(null))
        .await;
    assert_eq!(r["connected"], false);
    assert_eq!(r["recovery_needed"], false);
    assert!(!s.root.path().join("home/.gemini/settings.json").exists());
    assert!(!s.root.path().join("home/.gemini/.env").exists());
}
