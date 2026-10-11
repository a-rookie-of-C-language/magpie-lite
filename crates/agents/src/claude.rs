//! Claude Code: point its `settings.json` env block at the gateway.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Where Claude Code's settings live. `CLAUDE_CONFIG_DIR` is honored only
/// when absolute, matching Claude's native desktop/config path behavior.
pub fn settings_path() -> Result<PathBuf> {
    let home = super::agent_home().context("cannot find the home directory")?;
    let dir = if std::env::var_os("MAGPIE_LITE_HOME").is_some_and(|value| !value.is_empty()) {
        home.join(".claude")
    } else {
        std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".claude"))
    };
    Ok(dir.join("settings.json"))
}

/// Whether Claude Code looks installed.
pub fn installed() -> bool {
    settings_path().map(|p| p.exists()).unwrap_or(false)
}

/// Wire Claude Code onto the gateway: `ANTHROPIC_BASE_URL`,
/// `ANTHROPIC_AUTH_TOKEN`, and the model. Returns the settings path.
pub fn wire(model: &str, port: u16, token: &str) -> Result<PathBuf> {
    let path = settings_path()?;
    wire_at(&path, model, port, token)?;
    Ok(path)
}

/// The wiring itself, against an explicit settings path.
pub fn wire_at(path: &Path, model: &str, port: u16, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the settings directory")?;
    }
    let mut settings: Value = if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        super::json_config::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        json!({})
    };
    if !settings.is_object() {
        anyhow::bail!("Claude settings must contain a JSON object");
    }
    if settings.get("env").is_some_and(|env| !env.is_object()) {
        anyhow::bail!("Claude env must contain a JSON object");
    }
    if !settings.get("env").map(Value::is_object).unwrap_or(false) {
        settings["env"] = json!({});
    }
    settings["env"]["ANTHROPIC_BASE_URL"] = json!(format!("http://127.0.0.1:{port}"));
    settings["env"]["ANTHROPIC_AUTH_TOKEN"] = json!(token);
    settings["model"] = json!(model);
    super::json_config::write(path, &settings, super::json_config::Syntax::Json)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
