//! All commands run in children with an explicit sandbox home. Never mutate the user's HOME.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
const TOKEN: &str = "isolated-cli-gateway-token";
struct Sandbox {
    root: tempfile::TempDir,
    config: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, format!("port=45554\ntoken='{TOKEN}'\ndefault_provider='mock'\n[[provider]]\nid='mock'\nprotocol='chat'\nbase_url='http://127.0.0.1:9/v1'\nmodels=['model']\n")).unwrap();
        Self { root, config }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_magpie-lite"));
        c.arg("--config")
            .arg(&self.config)
            .env("MAGPIE_LITE_HOME", self.root.path().join("home"));
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn success(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(!String::from_utf8_lossy(&o.stdout).contains(TOKEN));
        serde_json::from_slice(&o.stdout).unwrap()
    }
}
#[test]
fn all_shorthand_commands_share_backups_and_agent_disconnect() {
    let s = Sandbox::new();
    for agent in [
        "claude", "codex", "crush", "gemini", "goose", "opencode", "zed",
    ] {
        let preview = s.success(&["agent", "preview", agent, "mock/model"]);
        for file in preview["files"].as_array().unwrap() {
            assert!(!PathBuf::from(file["path"].as_str().unwrap()).exists());
        }
        assert_eq!(s.success(&[agent, "mock/model"])["connected"], true);
        let list = s.success(&["agent", "list"]);
        assert_eq!(list.as_array().unwrap().len(), 7);
        assert_eq!(
            s.success(&["agent", "preview", agent, "--disconnect"])["action"],
            "disconnect"
        );
        assert_eq!(
            s.success(&["agent", "disconnect", agent])["connected"],
            false
        );
        for file in preview["files"].as_array().unwrap() {
            assert!(!PathBuf::from(file["path"].as_str().unwrap()).exists());
        }
    }
}
#[test]
fn reviewed_revisions_detect_edits_and_restore_works_without_gateway_config() {
    let s = Sandbox::new();
    let p = s.success(&["agent", "preview", "claude", "mock/model"]);
    let path = PathBuf::from(p["files"][0]["path"].as_str().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"user\":true}").unwrap();
    assert!(
        !s.run(&[
            "agent",
            "connect",
            "claude",
            "mock/model",
            "--revision",
            p["revision"].as_str().unwrap()
        ])
        .status
        .success()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"user\":true}");
    s.success(&["agent", "connect", "claude", "mock/model"]);
    std::fs::remove_file(&s.config).unwrap();
    s.success(&["agent", "disconnect", "claude"]);
    assert_eq!(std::fs::read(path).unwrap(), b"{\"user\":true}");
}
#[test]
fn cli_rejects_unknown_models_agents_and_drift() {
    let s = Sandbox::new();
    for args in [
        vec!["agent", "connect", "claude", "missing/model"],
        vec!["agent", "preview", "unknown", "mock/model"],
        vec!["agent", "preview", "claude"],
    ] {
        let o = s.run(&args);
        assert!(!o.status.success());
        assert!(!String::from_utf8_lossy(&o.stderr).contains(TOKEN));
    }
    let status = s.success(&["claude", "mock/model"]);
    let path = PathBuf::from(status["files"][0].as_str().unwrap());
    std::fs::write(&path, b"{\"user\":true}").unwrap();
    let o = s.run(&["agent", "disconnect", "claude"]);
    assert!(!o.status.success());
    assert_eq!(std::fs::read(path).unwrap(), b"{\"user\":true}");
}
struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn gateway_uses_actual_port_and_shares_cli_journals() {
    use std::io::BufRead;
    let s = Sandbox::new();
    let mut server = Server(
        s.command()
            .args(["serve", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = server.0.stdout.take().unwrap();
    let line = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .unwrap();
            line
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let url = line
        .trim()
        .strip_prefix("magpie-lite gateway listening on ")
        .expect("server did not announce readiness");
    let port = url.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    assert_ne!(port, 0);
    assert_ne!(port, 45554);
    let client = magpie_gateway::upstream::http_client().unwrap();
    let p: Value = client
        .post(format!("{url}/agents/claude/preview"))
        .bearer_auth(TOKEN)
        .json(&json!({"action":"connect","model":"mock/model"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = client
        .post(format!("{url}/agents/claude/apply"))
        .bearer_auth(TOKEN)
        .json(&json!({"action":"connect","model":"mock/model","revision":p["revision"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let settings: Value = serde_json::from_slice(
        &std::fs::read(s.root.path().join("home/.claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], url);
    let list = s.success(&["agent", "list"]);
    assert_eq!(list[0]["connected"], true);
    s.success(&["agent", "disconnect", "claude"]);
    let statuses: Value = client
        .get(format!("{url}/agents"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(statuses["agents"][0]["connected"], false);
}

#[cfg(windows)]
#[test]
fn windows_junction_cannot_redirect_agent_writes() {
    use std::os::windows::process::CommandExt;
    let s = Sandbox::new();
    let home = s.root.path().join("home");
    let elsewhere = s.root.path().join("elsewhere");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let result = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(home.join(".claude"))
        .arg(&elsewhere)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(result.status.success());
    let output = s.run(&["agent", "preview", "claude", "mock/model"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reparse-point"));
    assert!(!elsewhere.join("settings.json").exists());
}
