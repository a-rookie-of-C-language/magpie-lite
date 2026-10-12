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
    fn native_command(&self) -> Command {
        let home = self.root.path().join("native-home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_magpie-lite"));
        command
            .arg("--config")
            .arg(&self.config)
            .env_remove("MAGPIE_LITE_HOME")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("APPDATA", home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", home.join("AppData/Local"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude-config"))
            .env("CODEX_HOME", home.join("codex-home"))
            .env("MAGPIE_ZED_CONFIG_DIR", home.join("zed-config"))
            .env("OPENCODE_CONFIG_DIR", home.join("opencode-config"));
        command
    }
    fn success(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(!String::from_utf8_lossy(&o.stdout).contains(TOKEN));
        serde_json::from_slice(&o.stdout).unwrap()
    }
}
#[test]
fn native_agent_config_directory_overrides_are_used() {
    let s = Sandbox::new();
    for (agent, component) in [
        ("claude", "claude-config/settings.json"),
        ("codex", "codex-home/config.toml"),
        ("opencode", "opencode-config/opencode.json"),
        ("zed", "zed-config/settings.json"),
    ] {
        let output = s
            .native_command()
            .args(["agent", "preview", agent, "mock/model"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{agent}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
        let path = PathBuf::from(preview["files"][0]["path"].as_str().unwrap());
        assert!(
            path.ends_with(component),
            "{agent} used unexpected native path: {}",
            path.display()
        );
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

fn managed_paths(sandbox: &Sandbox, preview: &Value) -> Vec<PathBuf> {
    preview["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let path = PathBuf::from(f["path"].as_str().unwrap());
            assert!(
                path.starts_with(sandbox.root.path()),
                "Agent path escaped sandbox"
            );
            path
        })
        .collect()
}
fn add_cli_user_note(agent: &str, index: usize, path: &std::path::Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let text = match (agent, index) {
        ("codex", _) => format!("user_note = 'CLI_USER_BEFORE'\r\n{text}"),
        ("goose", 0) => format!("user_note: CLI_USER_BEFORE\r\n{text}"),
        ("gemini", 1) => format!("{text}# user note\r\nUSER_NOTE=CLI_USER_BEFORE\r\n"),
        _ => {
            let mut v: Value = serde_json::from_str(&text).unwrap();
            v["user_note"] = json!("CLI_USER_BEFORE");
            v["custom"] = json!("user-updated");
            v.as_object_mut().unwrap().remove("remove_me");
            serde_json::to_string_pretty(&v).unwrap()
        }
    };
    std::fs::write(path, text).unwrap();
}
#[test]
fn all_cli_agents_preserve_unrelated_edits_across_rewire_and_configless_disconnect() {
    let s = Sandbox::new();
    let config = std::fs::read(&s.config).unwrap();
    for agent in [
        "claude", "codex", "crush", "gemini", "goose", "opencode", "zed",
    ] {
        let p = s.success(&["agent", "preview", agent, "mock/model"]);
        let paths = managed_paths(&s, &p);
        for (i, path) in paths.iter().enumerate() {
            let original = match (agent, i) {
                ("codex", _) => "# prior\r\nmodel = 'original'\r\n",
                ("goose", 0) => "# prior\r\nGOOSE_PROVIDER: original\r\nGOOSE_MODEL: original\r\n",
                ("gemini", 1) => "# prior\r\nGEMINI_API_KEY=original-secret\r\n",
                _ => r#"{"custom":"original","remove_me":true}"#,
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, original).unwrap();
        }
        s.success(&["agent", "connect", agent, "mock/model"]);
        for (i, path) in paths.iter().enumerate() {
            add_cli_user_note(agent, i, path);
        }
        let list = s.success(&["agent", "list"]);
        let status = list
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == agent)
            .unwrap();
        assert_eq!(status["drifted"], false);
        let stale = s.success(&["agent", "preview", agent, "mock/rewired"]);
        for path in &paths {
            let text = std::fs::read_to_string(path)
                .unwrap()
                .replace("CLI_USER_BEFORE", "CLI_USER_AFTER");
            std::fs::write(path, text).unwrap();
        }
        let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let failed = s.run(&[
            "agent",
            "connect",
            agent,
            "mock/rewired",
            "--revision",
            stale["revision"].as_str().unwrap(),
        ]);
        assert!(!failed.status.success());
        assert!(!String::from_utf8_lossy(&failed.stderr).contains(TOKEN));
        assert_eq!(
            paths
                .iter()
                .map(|p| std::fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            before
        );
        s.success(&["agent", "connect", agent, "mock/rewired"]);
        std::fs::remove_file(&s.config).unwrap();
        let p = s.success(&["agent", "preview", agent, "--disconnect"]);
        assert_eq!(
            s.success(&[
                "agent",
                "disconnect",
                agent,
                "--revision",
                p["revision"].as_str().unwrap()
            ])["connected"],
            false
        );
        for (i, path) in paths.iter().enumerate() {
            let text = std::fs::read_to_string(path).unwrap();
            assert!(text.contains("CLI_USER_AFTER"), "lost {agent} file {i}");
            assert!(!text.contains(TOKEN));
            if !(agent == "codex" || (agent == "goose" && i == 0) || (agent == "gemini" && i == 1))
            {
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["custom"], "user-updated");
                assert!(v.get("remove_me").is_none());
            }
        }
        std::fs::write(&s.config, &config).unwrap();
    }
}
#[test]
fn cli_duplicate_json_is_rejected_without_writes_or_private_error_content() {
    let s = Sandbox::new();
    let p = s.success(&["agent", "preview", "claude", "mock/model"]);
    let path = managed_paths(&s, &p).remove(0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let duplicate = r#"{"private-key-name":1,"private-key-name":2}"#;
    std::fs::write(&path, duplicate).unwrap();
    let failed = s.run(&["agent", "connect", "claude", "mock/model"]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("private-key-name"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), duplicate);
    assert!(!s.root.path().join("agent-state/claude.json").exists());
    std::fs::write(&path, "{}").unwrap();
    s.success(&["agent", "connect", "claude", "mock/model"]);
    let mut duplicate = std::fs::read_to_string(&path).unwrap();
    duplicate.insert_str(
        duplicate.rfind('}').unwrap(),
        ",\"foreign\":1,\"foreign\":2",
    );
    std::fs::write(&path, &duplicate).unwrap();
    let failed = s.run(&["agent", "disconnect", "claude"]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains(TOKEN));
    assert_eq!(std::fs::read_to_string(path).unwrap(), duplicate);
}
#[test]
fn cli_recovers_selective_multifile_disconnect_after_restart_without_config() {
    let s = Sandbox::new();
    let p = s.success(&["agent", "preview", "gemini", "mock/model"]);
    let paths = managed_paths(&s, &p);
    s.success(&["agent", "connect", "gemini", "mock/model"]);
    for (i, path) in paths.iter().enumerate() {
        add_cli_user_note("gemini", i, path);
    }
    s.success(&["agent", "connect", "gemini", "mock/rewired"]);
    let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let journal_path = s.root.path().join("agent-state/gemini.json");
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    s.success(&["agent", "disconnect", "gemini"]);
    let targets: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
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
    std::fs::remove_file(&s.config).unwrap();
    let result = s.success(&["agent", "recover", "gemini"]);
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
        assert!(text.contains("CLI_USER_BEFORE"));
        assert!(!text.contains(TOKEN));
    }
}

#[test]
fn cli_selected_opencode_jsonc_preserves_format_and_configless_restore() {
    let s = Sandbox::new();
    let config = std::fs::read_to_string(&s.config)
        .unwrap()
        .replace("models=['model']", "models=['model','other']");
    std::fs::write(&s.config, config).unwrap();
    let p = s.success(&["agent", "preview", "opencode", "mock/model"]);
    let json = managed_paths(&s, &p).remove(0);
    let path = json.with_extension("jsonc");
    let original = "\u{feff}// CLI header {\r\n{\r\n\t\"theme\": \"dark\",\r\n\t\"model\": \"native/old\",\r\n}\r\n";
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ambiguous = r#"{ /* private-comment */ "private-key":1, "\u0070rivate-key":2,}"#;
    std::fs::write(&path, ambiguous).unwrap();
    let refusal = s.run(&["agent", "preview", "opencode", "mock/model"]);
    assert!(!refusal.status.success());
    assert!(!String::from_utf8_lossy(&refusal.stderr).contains("private-key"));
    assert!(!String::from_utf8_lossy(&refusal.stderr).contains("private-comment"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), ambiguous);
    assert!(!json.exists());
    std::fs::write(&path, original).unwrap();
    let p = s.success(&["agent", "preview", "opencode", "mock/model"]);
    assert_eq!(managed_paths(&s, &p), vec![path.clone()]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    s.success(&[
        "agent",
        "connect",
        "opencode",
        "mock/model",
        "--revision",
        p["revision"].as_str().unwrap(),
    ]);
    let applied = std::fs::read_to_string(&path).unwrap();
    assert!(applied.starts_with("\u{feff}// CLI header {\r\n"));
    let edited = format!(
        "{}// CLI user note\r\n",
        applied.replace("\"dark\"", "\"light\"")
    );
    std::fs::write(&path, &edited).unwrap();
    s.success(&["agent", "connect", "opencode", "mock/other"]);
    let rewired = std::fs::read_to_string(&path).unwrap();
    assert!(rewired.contains("// CLI user note"));
    assert!(rewired.contains("magpie-lite/mock/other"));
    let duplicate = rewired.replacen("\r\n{", "\r\n{\"private-key\":1,\"private-key\":2,", 1);
    std::fs::write(&path, &duplicate).unwrap();
    let refusal = s.run(&["agent", "disconnect", "opencode"]);
    assert!(!refusal.status.success());
    assert!(!String::from_utf8_lossy(&refusal.stderr).contains("private-key"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), duplicate);
    std::fs::write(&path, rewired).unwrap();
    std::fs::remove_file(&s.config).unwrap();
    s.success(&["agent", "disconnect", "opencode"]);
    let restored = std::fs::read_to_string(&path).unwrap();
    for exact in [
        "\u{feff}// CLI header {\r\n",
        "\t\"theme\": \"light\",\r\n",
        "\"model\": \"native/old\"",
        "// CLI user note\r\n",
    ] {
        assert!(restored.contains(exact), "{restored}");
    }
    assert!(!restored.contains(TOKEN));
    assert!(!json.exists());
}

#[test]
fn sandbox_discovery_ignores_machine_paths_and_reports_passive_user_launchers() {
    let s = Sandbox::new();
    let external = s.root.path().join("external-machine-bin");
    std::fs::create_dir_all(&external).unwrap();
    let name = if cfg!(windows) { "codex.cmd" } else { "codex" };
    let outside = external.join(name);
    std::fs::write(&outside, "must not count as a sandbox installation").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let isolated = s
        .command()
        .env("PATH", &external)
        .env("BUN_INSTALL", &external)
        .env("APPDATA", &external)
        .env("NVM_HOME", &external)
        .args(["agent", "list"])
        .output()
        .unwrap();
    assert!(
        isolated.status.success(),
        "{}",
        String::from_utf8_lossy(&isolated.stderr)
    );
    let list: Value = serde_json::from_slice(&isolated.stdout).unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|a| a["detected"] == false && a["detection"]["cli"].is_null())
    );
    let bin = s.root.path().join("home/.local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join(name);
    let marker = s.root.path().join("must-not-run-codex");
    let script = if cfg!(windows) {
        format!(
            "@echo off\r\necho executed>\"{}\"\r\nREM {TOKEN}\r\n",
            marker.display()
        )
    } else {
        format!(
            "#!/bin/sh\nprintf executed > '{}'\n# {TOKEN}\n",
            marker.display()
        )
    };
    std::fs::write(&path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let list = s.success(&["agent", "list"]);
    let codex = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "codex")
        .unwrap();
    assert_eq!(codex["detected"], true);
    assert_eq!(codex["detection"]["config_file"], false);
    assert_eq!(
        PathBuf::from(codex["detection"]["cli"]["path"].as_str().unwrap()),
        path
    );
    assert_eq!(codex["detection"]["cli"]["on_path"], false);
    let output = s.run(&["ls"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.replace('\\', "/")
            .contains(&path.to_str().unwrap().replace('\\', "/"))
    );
    assert!(text.contains("not on launcher's PATH"));
    assert!(!text.contains(TOKEN));
    assert!(!marker.exists());
    assert_eq!(std::fs::read(path).unwrap(), script.as_bytes());
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "must not count as a sandbox installation"
    );
}

#[cfg(unix)]
#[test]
fn unix_cli_checks_permission_revisions_and_restores_legacy_modes_without_config() {
    use std::os::unix::fs::PermissionsExt;
    for schema in [1, 2] {
        for existing in [false, true] {
            let s = Sandbox::new();
            let preview = s.success(&["agent", "preview", "gemini", "mock/model"]);
            let paths = managed_paths(&s, &preview);
            let original = [
                b"{\"user\":true}\n".as_slice(),
                b"# retained\nUNRELATED=kept\n",
            ];
            if existing {
                for (path, bytes) in paths.iter().zip(original) {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, bytes).unwrap();
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
                }
            }
            s.success(&["agent", "connect", "gemini", "mock/model"]);
            for path in &paths {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                    0o600
                );
            }
            let preview = s.success(&["agent", "preview", "gemini", "--disconnect"]);
            let connected: Vec<_> = paths
                .iter()
                .map(|path| std::fs::read(path).unwrap())
                .collect();
            std::fs::set_permissions(&paths[1], std::fs::Permissions::from_mode(0o660)).unwrap();
            let failed = s.run(&[
                "agent",
                "disconnect",
                "gemini",
                "--revision",
                preview["revision"].as_str().unwrap(),
            ]);
            assert!(!failed.status.success());
            assert!(!String::from_utf8_lossy(&failed.stderr).contains(TOKEN));
            assert_eq!(
                paths
                    .iter()
                    .map(|path| std::fs::read(path).unwrap())
                    .collect::<Vec<_>>(),
                connected
            );
            assert_eq!(
                std::fs::metadata(&paths[1]).unwrap().permissions().mode() & 0o7777,
                0o660
            );
            std::fs::set_permissions(&paths[1], std::fs::Permissions::from_mode(0o600)).unwrap();

            // A separate process must accept old stat modes and restore exactly
            // without loading the gateway configuration or rewriting on preview.
            let journal_path = s.root.path().join("agent-state/gemini.json");
            let mut journal: Value =
                serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
            journal["schema"] = json!(schema);
            for entry in journal["files"].as_array_mut().unwrap() {
                for key in ["baseline", "applied", "restore_base"] {
                    if let Some(mode) = entry[key]["mode"].as_u64() {
                        entry[key]["mode"] = json!(mode | 0o100000);
                    }
                }
                if schema == 1 {
                    entry.as_object_mut().unwrap().remove("owned");
                    entry.as_object_mut().unwrap().remove("restore_base");
                }
            }
            let bytes = serde_json::to_vec(&journal).unwrap();
            std::fs::write(&journal_path, &bytes).unwrap();
            std::fs::remove_file(&s.config).unwrap();
            let preview = s.success(&["agent", "preview", "gemini", "--disconnect"]);
            assert_eq!(std::fs::read(&journal_path).unwrap(), bytes);
            assert_eq!(
                s.success(&[
                    "agent",
                    "disconnect",
                    "gemini",
                    "--revision",
                    preview["revision"].as_str().unwrap()
                ])["connected"],
                false
            );
            for (path, bytes) in paths.iter().zip(original) {
                if existing {
                    assert_eq!(std::fs::read(path).unwrap(), bytes);
                    assert_eq!(
                        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                        0o640
                    );
                } else {
                    assert!(!path.exists());
                }
            }
        }
    }
}
