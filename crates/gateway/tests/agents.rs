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

fn agent_paths(server: &Server, preview: &Value) -> Vec<PathBuf> {
    preview["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            let path = PathBuf::from(file["path"].as_str().unwrap());
            assert!(
                path.starts_with(server.root.path()),
                "Agent path escaped sandbox"
            );
            path
        })
        .collect()
}
fn agent_baseline(agent: &str, index: usize) -> &'static str {
    match (agent, index) {
        ("codex", _) => "# original comment\r\nmodel = 'original'\r\n",
        ("goose", 0) => {
            "# original comment\r\nGOOSE_PROVIDER: original\r\nGOOSE_MODEL: original\r\n"
        }
        ("gemini", 1) => {
            "# original comment\r\nGEMINI_API_KEY=original-secret\r\nUNRELATED=original\r\n"
        }
        _ => r#"{"custom":"original","remove_me":true}"#,
    }
}
fn edit_agent_note(agent: &str, index: usize, path: &std::path::Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let output = match (agent, index) {
        ("codex", _) => format!("user_note = 'HTTP_USER_BEFORE'\r\n{text}"),
        ("goose", 0) => format!("user_note: HTTP_USER_BEFORE\r\n{text}"),
        ("gemini", 1) => format!("{text}# user comment\r\nUSER_NOTE=HTTP_USER_BEFORE\r\n"),
        _ => {
            let mut value: Value = serde_json::from_str(&text).unwrap();
            value["user_note"] = json!("HTTP_USER_BEFORE");
            value["custom"] = json!("user-updated");
            value.as_object_mut().unwrap().remove("remove_me");
            serde_json::to_string_pretty(&value).unwrap()
        }
    };
    std::fs::write(path, output).unwrap();
}
#[tokio::test]
async fn selective_http_restore_preserves_foreign_edits_for_all_seven_agents() {
    let s = server(true).await;
    let config_before = std::fs::read(&s.path).unwrap();
    let stats_before = s.ok(Method::GET, "/stats", json!(null)).await;
    for agent in [
        "claude", "codex", "crush", "gemini", "goose", "opencode", "zed",
    ] {
        let paths = agent_paths(&s, &s.preview(agent, "connect", "mock/model").await);
        for (i, path) in paths.iter().enumerate() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, agent_baseline(agent, i)).unwrap();
        }
        let p = s.preview(agent, "connect", "mock/model").await;
        s.apply(agent, &p).await;
        for (i, path) in paths.iter().enumerate() {
            edit_agent_note(agent, i, path);
        }
        let status = s.ok(Method::GET, "/agents", json!(null)).await;
        let status = status["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == agent)
            .unwrap();
        assert_eq!(status["drifted"], false);
        assert!(!status.to_string().contains("HTTP_USER_BEFORE"));
        let stale = s.preview(agent, "connect", "mock/rewired").await;
        for path in &paths {
            let text = std::fs::read_to_string(path)
                .unwrap()
                .replace("HTTP_USER_BEFORE", "HTTP_USER_AFTER");
            std::fs::write(path, text).unwrap();
        }
        let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let response = s
            .request(
                Method::POST,
                &format!("/agents/{agent}/apply"),
                json!({"action":"connect","model":"mock/rewired","revision":stale["revision"]}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            paths
                .iter()
                .map(|p| std::fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            before
        );
        let p = s.preview(agent, "connect", "mock/rewired").await;
        assert_eq!(s.apply(agent, &p).await["model"], "mock/rewired");
        // No more edits: this must restore the latest retained image, not the first file.
        let p = s.preview(agent, "disconnect", "").await;
        assert_eq!(s.apply(agent, &p).await["connected"], false);
        for (i, path) in paths.iter().enumerate() {
            let text = std::fs::read_to_string(path).unwrap();
            assert!(
                text.contains("HTTP_USER_AFTER"),
                "lost edits for {agent} file {i}"
            );
            assert!(!text.contains(TOKEN));
            if !(agent == "codex" || (agent == "goose" && i == 0) || (agent == "gemini" && i == 1))
            {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["custom"], "user-updated");
                assert!(value.get("remove_me").is_none());
            }
        }
    }
    assert_eq!(std::fs::read(&s.path).unwrap(), config_before);
    assert_eq!(s.ok(Method::GET, "/stats", json!(null)).await, stats_before);
}
#[tokio::test]
async fn ambiguous_json_and_managed_conflicts_have_sanitized_http_errors() {
    let s = server(true).await;
    let p = s.preview("claude", "connect", "mock/model").await;
    let path = agent_paths(&s, &p).remove(0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let duplicate = format!(r#"{{"{UPSTREAM_KEY}":1,"{UPSTREAM_KEY}":2}}"#);
    std::fs::write(&path, &duplicate).unwrap();
    let response = s
        .request(
            Method::POST,
            "/agents/claude/preview",
            json!({"action":"connect","model":"mock/model"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.text().await.unwrap().contains(UPSTREAM_KEY));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), duplicate);
    std::fs::write(&path, "{}").unwrap();
    s.apply(
        "claude",
        &s.preview("claude", "connect", "mock/model").await,
    )
    .await;
    let good: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut v = good.clone();
    v["user_note"] = json!("keep this");
    v["model"] = json!("user-owned-model");
    let edited = serde_json::to_vec(&v).unwrap();
    std::fs::write(&path, &edited).unwrap();
    let response = s
        .request(
            Method::POST,
            "/agents/claude/preview",
            json!({"action":"disconnect"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!response.text().await.unwrap().contains(TOKEN));
    assert_eq!(std::fs::read(&path).unwrap(), edited);
    let mut duplicate = serde_json::to_string(&good).unwrap();
    duplicate.insert_str(
        duplicate.rfind('}').unwrap(),
        ",\"foreign\":1,\"foreign\":2",
    );
    std::fs::write(&path, &duplicate).unwrap();
    let response = s
        .request(
            Method::POST,
            "/agents/claude/preview",
            json!({"action":"disconnect"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!response.text().await.unwrap().contains(TOKEN));
    assert_eq!(std::fs::read_to_string(path).unwrap(), duplicate);
}
#[tokio::test]
async fn selective_http_pending_disconnect_recovers_merged_multifile_targets() {
    let s = server(true).await;
    let p = s.preview("gemini", "connect", "mock/model").await;
    let paths = agent_paths(&s, &p);
    s.apply("gemini", &p).await;
    for (i, path) in paths.iter().enumerate() {
        edit_agent_note("gemini", i, path);
    }
    let p = s.preview("gemini", "connect", "mock/rewired").await;
    s.apply("gemini", &p).await;
    let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let journal_path = s.root.path().join("agent-state/gemini.json");
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    let p = s.preview("gemini", "disconnect", "").await;
    s.apply("gemini", &p).await;
    let targets: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    // Reconstruct an interrupted two-file disconnect from actual connection images.
    // First file is already restored, second still has the pre-disconnect bytes.
    journal["phase"] = json!("disconnecting");
    journal["rollback"] = json!(
        journal["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["applied"].clone())
            .collect::<Vec<_>>()
    );
    journal["restore"] = json!(
        journal["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["restore_base"].clone())
            .collect::<Vec<_>>()
    );
    std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    std::fs::write(&paths[1], &before[1]).unwrap();
    let list = s.ok(Method::GET, "/agents", json!(null)).await;
    let status = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "gemini")
        .unwrap();
    assert_eq!(status["recovery_needed"], true);
    let later = format!(
        "{}LATER_USER_EDIT=keep\n",
        String::from_utf8(before[1].clone()).unwrap()
    );
    std::fs::write(&paths[1], &later).unwrap();
    let response = s
        .request(Method::POST, "/agents/gemini/recover", json!(null))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), targets[0]);
    assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), later);
    std::fs::write(&paths[1], &before[1]).unwrap();
    let result = s
        .ok(Method::POST, "/agents/gemini/recover", json!(null))
        .await;
    assert_eq!(result["connected"], false);
    assert_eq!(result["recovery_needed"], false);
    assert_eq!(
        paths
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect::<Vec<_>>(),
        targets
    );
    for path in paths {
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("HTTP_USER_BEFORE"));
        assert!(!text.contains(TOKEN));
    }
}

#[tokio::test]
async fn selected_opencode_jsonc_is_lossless_through_http_lifecycle_and_refusal() {
    let s = server(true).await;
    let first = s.preview("opencode", "connect", "mock/model").await;
    let json = agent_paths(&s, &first).remove(0);
    let path = json.with_extension("jsonc");
    let original = "\u{feff}// HTTP header {\r\n{\r\n\t\"theme\": \"dark\",\r\n\t\"model\": \"native/old\",\r\n}\r\n";
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, original).unwrap();
    let p = s.preview("opencode", "connect", "mock/model").await;
    assert_eq!(agent_paths(&s, &p), vec![path.clone()]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(s.apply("opencode", &p).await["connected"], true);
    let applied = std::fs::read_to_string(&path).unwrap();
    assert!(applied.starts_with("\u{feff}// HTTP header {\r\n"));
    assert!(applied.contains("\t\"theme\": \"dark\",\r\n"));
    let stale = s.preview("opencode", "connect", "group/daily").await;
    let edited = format!(
        "{}// HTTP user note\r\n",
        applied.replace("\"dark\"", "\"light\"")
    );
    std::fs::write(&path, &edited).unwrap();
    let response = s
        .request(
            Method::POST,
            "/agents/opencode/apply",
            json!({"action":"connect","model":stale["model"],"revision":stale["revision"]}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    let p = s.preview("opencode", "connect", "group/daily").await;
    s.apply("opencode", &p).await;
    let rewired = std::fs::read_to_string(&path).unwrap();
    assert!(rewired.contains("// HTTP user note"));
    assert!(rewired.contains("magpie-lite/group/daily"));
    let p = s.preview("opencode", "disconnect", "").await;
    s.apply("opencode", &p).await;
    let restored = std::fs::read_to_string(&path).unwrap();
    assert!(restored.starts_with("\u{feff}// HTTP header {\r\n"));
    assert!(restored.contains("\t\"theme\": \"light\",\r\n"));
    assert!(restored.contains("\"model\": \"native/old\""));
    assert!(restored.contains("// HTTP user note\r\n"));
    assert!(!restored.contains(TOKEN));
    assert!(!json.exists());
    let invalid = "{ /* private-note */ \"secret-private\":1,\"\\u0073ecret-private\":2,}";
    std::fs::write(&path, invalid).unwrap();
    let response = s
        .request(
            Method::POST,
            "/agents/opencode/preview",
            json!({"action":"connect","model":"mock/model"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = response.text().await.unwrap();
    assert!(!error.contains("secret-private"));
    assert!(!error.contains("private-note"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    assert!(!json.exists());
}
