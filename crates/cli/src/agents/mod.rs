//! Per-agent wiring: each adapter rewrites the few fields of the agent's
//! own config that say where its model comes from.

pub mod claude;
pub mod codex;

/// One supported agent, as `magpie-lite ls` shows it.
pub struct AgentInfo {
    /// Short id.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Whether its config directory exists.
    pub installed: bool,
}

/// Every agent this build can wire.
pub fn list() -> Vec<AgentInfo> {
    vec![
        AgentInfo {
            id: "claude",
            name: "Claude Code",
            installed: claude::installed(),
        },
        AgentInfo {
            id: "codex",
            name: "Codex",
            installed: codex::installed(),
        },
    ]
}

/// The home directory agent configs live under. `MAGPIE_LITE_HOME`
/// overrides it, so sandboxes and wrappers never touch the real one — the
/// `dirs` crate ignores `USERPROFILE`/`HOME` on Windows.
pub fn agent_home() -> Option<std::path::PathBuf> {
    std::env::var_os("MAGPIE_LITE_HOME")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_wire_writes_only_the_gateway_fields() {
        let dir = tempdir();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"effortLevel": "xhigh", "env": {"OTHER": "kept"}}"#,
        )
        .unwrap();
        claude::wire_at(&path, "deepseek/deepseek-chat", 3426, "tok").expect("wire");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:3426");
        assert_eq!(v["env"]["ANTHROPIC_AUTH_TOKEN"], "tok");
        assert_eq!(v["model"], "deepseek/deepseek-chat");
        assert_eq!(v["effortLevel"], "xhigh", "other settings survive");
        assert_eq!(v["env"]["OTHER"], "kept", "other env survives");
    }

    #[test]
    fn codex_wire_writes_the_provider_table_and_keeps_the_rest() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"glm-5.3\"\nmodel_provider = \"custom\"\n\n[model_providers.custom]\nname = \"zhipu\"\n").unwrap();
        codex::wire_at(&path, "deepseek/deepseek-chat", 3426, "tok").expect("wire");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"deepseek/deepseek-chat\""));
        assert!(text.contains("model_provider = \"magpie-lite\""));
        assert!(text.contains("base_url = \"http://127.0.0.1:3426/v1\""));
        assert!(text.contains("wire_api = \"chat\""));
        assert!(
            text.contains("[model_providers.custom]"),
            "other providers survive"
        );
        assert!(text.contains("name = \"zhipu\""));
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "magpie-lite-agents-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
