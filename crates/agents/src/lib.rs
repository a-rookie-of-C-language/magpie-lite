//! Per-agent wiring: each adapter rewrites the few fields of the agent's
//! own config that say where its model comes from.

pub mod claude;
pub mod codex;
pub mod crush;
pub mod gemini;
pub mod goose;
pub mod opencode;
pub mod zed;

/// One supported agent, as `magpie-lite ls` shows it.
pub struct AgentInfo {
    /// Short id.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Configuration file/directory or a CLI launcher was found (not a health check).
    pub installed: bool,
}

/// Every agent this build can wire, using the same passive detection as HTTP/CLI.
pub fn list() -> Vec<AgentInfo> {
    let search = discovery::Discovery::native().search();
    lifecycle::AgentId::ALL
        .into_iter()
        .map(|id| {
            let files = native_files(id).unwrap_or_default();
            AgentInfo {
                id: id.id(),
                name: id.name(),
                installed: search.detect(id, &files).detected(),
            }
        })
        .collect()
}

pub(crate) fn native_files(id: lifecycle::AgentId) -> anyhow::Result<Vec<std::path::PathBuf>> {
    use lifecycle::AgentId;
    Ok(match id {
        AgentId::Claude => vec![claude::settings_path()?],
        AgentId::Codex => vec![codex::config_path()?],
        AgentId::Crush => {
            let (config, data) = crush::paths()?;
            vec![config, data]
        }
        AgentId::Gemini => vec![gemini::settings_path()?, gemini::env_path()?],
        AgentId::Goose => {
            let config = goose::config_path()?;
            let provider = config
                .parent()
                .ok_or_else(|| anyhow::anyhow!("cannot locate Goose providers"))?
                .join("custom_providers/magpie-lite.json");
            vec![config, provider]
        }
        AgentId::Opencode => vec![opencode::config_path()?],
        AgentId::Zed => vec![zed::settings_path()?],
    })
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
    fn goose_wire_handles_both_config_layouts() {
        let dir = tempdir();
        let old = dir.join("old-config.yaml");
        std::fs::write(
            &old,
            "GOOSE_PROVIDER: openai\nGOOSE_MODEL: gpt-5\nGOOSE_MODE: auto\nother: kept\n",
        )
        .unwrap();
        goose::wire_at(&old, "deepseek/deepseek-chat", 3426, "tok").expect("wire old layout");

        let config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&old).unwrap()).unwrap();
        assert_eq!(config["GOOSE_PROVIDER"], "magpie-lite");
        assert_eq!(config["GOOSE_MODEL"], "deepseek/deepseek-chat");
        assert_eq!(config["GOOSE_MODE"], "auto");
        assert_eq!(config["other"], "kept");
        assert!(config.get("active_provider").is_none());

        let new = dir.join("new-config.yaml");
        std::fs::write(
            &new,
            "GOOSE_PROVIDER: openai\nGOOSE_MODEL: gpt-5\nGOOSE_MODE: auto\nactive_provider: openai\nproviders:\n  openai:\n    enabled: true\n    model: gpt-5\n    configured: true\n    extra: keep\nother: kept\n",
        )
        .unwrap();
        goose::wire_at(&new, "group/daily-coding", 3426, "tok").expect("wire new layout");

        let config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&new).unwrap()).unwrap();
        assert_eq!(config["active_provider"], "magpie-lite");
        assert_eq!(config["GOOSE_MODE"], "auto");
        assert_eq!(config["other"], "kept");
        assert!(config.get("GOOSE_PROVIDER").is_none());
        assert!(config.get("GOOSE_MODEL").is_none());
        assert_eq!(config["providers"]["openai"]["extra"], "keep");
        let provider = &config["providers"]["magpie-lite"];
        assert_eq!(provider["enabled"], true);
        assert_eq!(provider["configured"], true);
        assert_eq!(provider["model"], "group/daily-coding");

        let provider_path = new
            .parent()
            .unwrap()
            .join("custom_providers")
            .join("magpie-lite.json");
        let provider: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&provider_path).unwrap()).unwrap();
        assert_eq!(provider["engine"], "openai");
        assert_eq!(provider["base_url"], "http://127.0.0.1:3426/v1");
        assert_eq!(provider["headers"]["Authorization"], "Bearer tok");
        assert_eq!(provider["dynamic_models"], false);
        assert_eq!(
            provider["models"],
            serde_json::json!([{
                "name": "group/daily-coding",
                "context_limit": 128000,
                "reasoning": false
            }])
        );
    }

    #[test]
    fn crush_wire_preserves_unrelated_config_and_replaces_the_large_pick() {
        let dir = tempdir();
        let config = dir.join("crush-config.json");
        let data = dir.join("crush-data.json");
        std::fs::write(
            &config,
            r#"{
  "theme": "dark",
  "providers": {
    "mine": {"type": "openai", "api_key": "keep-me"},
    "magpie-lite": {
      "custom": true,
      "models": [{"id": "old-route", "name": "Old"}]
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            &data,
            r#"{
  "models": {
    "large": {
      "provider": "openai",
      "model": "gpt-5",
      "reasoning_effort": "medium",
      "max_tokens": 128000,
      "temperature": 0.2,
      "provider_options": {"x": 1}
    },
    "small": {"provider": "openai", "model": "gpt-5-mini"}
  },
  "providers": {"openai": {"api_key": "sk-keep"}},
  "recent_models": {
    "large": [{"provider": "openai", "model": "gpt-5"}]
  }
}"#,
        )
        .unwrap();

        crush::wire_at(&config, &data, "deepseek/deepseek-chat", 3426, "tok").expect("wire");

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["providers"]["mine"]["api_key"], "keep-me");
        let provider = &config["providers"]["magpie-lite"];
        assert_eq!(provider["type"], "openai");
        assert_eq!(provider["base_url"], "http://127.0.0.1:3426/v1");
        assert_eq!(provider["api_key"], "tok");
        assert_eq!(provider["custom"], true);
        assert_eq!(
            provider["models"],
            serde_json::json!([{
                "id": "deepseek/deepseek-chat",
                "name": "deepseek/deepseek-chat",
                "context_window": 200000,
                "default_max_tokens": 16384,
                "can_reason": false
            }])
        );

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&data).unwrap()).unwrap();
        assert_eq!(
            data["models"]["large"],
            serde_json::json!({
                "provider": "magpie-lite",
                "model": "deepseek/deepseek-chat"
            })
        );
        assert_eq!(data["models"]["small"]["model"], "gpt-5-mini");
        assert_eq!(data["providers"]["openai"]["api_key"], "sk-keep");
        assert_eq!(data["recent_models"]["large"][0]["model"], "gpt-5");
    }

    #[test]
    fn crush_windows_layout_writes_provider_and_pick_in_one_file() {
        let dir = tempdir();
        let path = dir.join("crush.json");
        std::fs::write(
            &path,
            r#"{"theme":"dark","models":{"large":{"provider":"openai","model":"gpt-5"},"small":{"provider":"openai","model":"gpt-5-mini"}},"providers":{"openai":{"api_key":"sk-keep"},"magpie-lite":{"custom":true,"models":[{"id":"old-route"}]}}}"#,
        )
        .unwrap();

        crush::wire_at(&path, &path, "group/daily-coding", 3426, "tok").expect("wire");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(
            v["models"]["large"],
            serde_json::json!({
                "provider": "magpie-lite",
                "model": "group/daily-coding"
            })
        );
        assert_eq!(v["models"]["small"]["model"], "gpt-5-mini");
        assert_eq!(v["providers"]["openai"]["api_key"], "sk-keep");
        assert_eq!(
            v["providers"]["magpie-lite"]["base_url"],
            "http://127.0.0.1:3426/v1"
        );
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
        opencode::wire_at(&path, "deepseek/deepseek-chat", 3426, "tok").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("// mine"));
        let value = super::json_config::parse(&text, super::json_config::Syntax::Jsonc).unwrap();
        assert_eq!(value["model"], "magpie-lite/deepseek/deepseek-chat");
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

pub mod lifecycle;

pub mod discovery;

#[cfg(windows)]
mod windows;

mod dotenv;
mod json_config;
mod selective;
