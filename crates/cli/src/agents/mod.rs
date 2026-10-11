//! Per-agent wiring: each adapter rewrites the few fields of the agent's
//! own config that say where its model comes from.

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod zed;

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
        AgentInfo {
            id: "gemini",
            name: "Gemini CLI",
            installed: gemini::installed(),
        },
        AgentInfo {
            id: "opencode",
            name: "OpenCode",
            installed: opencode::installed(),
        },
        AgentInfo {
            id: "zed",
            name: "Zed",
            installed: zed::installed(),
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
    fn gemini_wire_writes_only_the_gateway_fields() {
        let dir = tempdir();
        let settings = dir.join("settings.json");
        let env = dir.join(".env");
        std::fs::write(
            &settings,
            r#"{"theme":"dark","security":{"auth":{"selectedType":"oauth-personal"}},"model":{"name":"gemini-2.5-pro"}}"#,
        )
        .unwrap();
        std::fs::write(
            &env,
            "# kept\nGEMINI_API_KEY=real-key\nOTHER=kept\n\nGOOGLE_GEMINI_BASE_URL=https://old\n",
        )
        .unwrap();

        gemini::wire_at(&settings, &env, "deepseek/deepseek-chat", 3426, "tok").expect("wire");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["security"]["auth"]["selectedType"], "gemini-api-key");
        assert_eq!(v["model"]["name"], "deepseek/deepseek-chat");

        let text = std::fs::read_to_string(&env).unwrap();
        assert!(text.contains("# kept"));
        assert!(text.contains("GEMINI_API_KEY=tok"));
        assert!(text.contains("OTHER=kept"));
        assert!(text.contains("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3426"));
        assert!(!text.contains("real-key"));
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
        assert!(text.contains("wire_api = \"responses\""));
        assert!(
            text.contains("[model_providers.custom]"),
            "other providers survive"
        );
        assert!(text.contains("name = \"zhipu\""));
    }

    #[test]
    fn opencode_wire_replaces_only_the_gateway_provider_fields() {
        let dir = tempdir();
        let path = dir.join("opencode.json");
        std::fs::write(
            &path,
            r#"{
  "$schema": "kept",
  "model": "openai/gpt-4o",
  "theme": "dark",
  "provider": {
    "mine": {
      "options": {"baseURL": "https://api.example.com/v1", "apiKey": "keep"},
      "models": {"gpt-4o": {"name": "GPT"}}
    },
    "magpie-lite": {
      "npm": "old-package",
      "custom": true,
      "options": {
        "baseURL": "http://localhost:9999/v1",
        "apiKey": "old",
        "headers": {"X-Kept": "yes"}
      },
      "models": {"old/model": {"name": "Old"}}
    }
  }
}"#,
        )
        .unwrap();

        opencode::wire_at(&path, "deepseek/deepseek-chat", 3426, "tok").expect("wire");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["$schema"], "kept");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["model"], "magpie-lite/deepseek/deepseek-chat");
        assert_eq!(v["provider"]["mine"]["options"]["apiKey"], "keep");
        assert_eq!(
            v["provider"]["magpie-lite"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(v["provider"]["magpie-lite"]["custom"], true);
        assert_eq!(
            v["provider"]["magpie-lite"]["options"]["baseURL"],
            "http://127.0.0.1:3426/v1"
        );
        assert_eq!(v["provider"]["magpie-lite"]["options"]["apiKey"], "tok");
        assert_eq!(
            v["provider"]["magpie-lite"]["options"]["headers"]["X-Kept"],
            "yes"
        );
        assert_eq!(
            v["provider"]["magpie-lite"]["models"],
            serde_json::json!({"deepseek/deepseek-chat": {"name": "deepseek/deepseek-chat"}})
        );
    }

    #[test]
    fn zed_wire_replaces_only_the_gateway_provider_fields() {
        let dir = tempdir();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"theme":"dark","language_models":{"openai":{"api_url":"https://api.openai.com/v1"},"openai_compatible":{"mine":{"api_url":"https://example.com/v1"},"magpie-lite":{"api_url":"http://localhost:9999/v1","custom_headers":{"X-Kept":"yes"},"unrelated":"kept","available_models":[{"name":"old-route","max_tokens":1000}]}}}}"#,
        )
        .unwrap();

        zed::wire_at(&path, "group/daily-coding", 3426, "real-key").expect("wire");

        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(
            v["language_models"]["openai"]["api_url"],
            "https://api.openai.com/v1"
        );
        assert_eq!(
            v["language_models"]["openai_compatible"]["mine"]["api_url"],
            "https://example.com/v1"
        );
        let provider = &v["language_models"]["openai_compatible"]["magpie-lite"];
        assert_eq!(provider["api_url"], "http://127.0.0.1:3426/v1");
        assert_eq!(provider["custom_headers"]["X-Kept"], "yes");
        assert_eq!(provider["unrelated"], "kept");
        assert_eq!(
            provider["available_models"],
            serde_json::json!([{
                "name": "group/daily-coding",
                "display_name": "magpie-lite group/daily-coding",
                "max_tokens": 128000
            }])
        );
        assert!(
            !text.contains("real-key"),
            "gateway token stays out of settings"
        );
    }

    #[test]
    fn opencode_wire_does_not_write_beside_an_existing_jsonc() {
        let dir = tempdir();
        let path = dir.join("opencode.jsonc");
        std::fs::write(&path, "{\n  // mine\n}\n").unwrap();
        let err = opencode::wire_at(&path, "deepseek/deepseek-chat", 3426, "tok")
            .expect_err("JSONC is not rewritten");
        assert!(err.to_string().contains("JSONC"));
        assert!(!dir.join("opencode.json").exists());
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
