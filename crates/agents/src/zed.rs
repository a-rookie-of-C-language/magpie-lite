//! Zed: register the gateway as an OpenAI-compatible provider.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The OpenAI-compatible provider id used in Zed's settings.
pub const PROVIDER_ID: &str = "magpie-lite";

/// Zed derives this from the provider id and reads it from its own process
/// environment. The key itself belongs in Zed's provider UI, never in
/// `settings.json`.
pub const API_KEY_ENV_VAR: &str = "MAGPIE_LITE_API_KEY";

/// Zed requires a context-window size for every custom model. The gateway
/// cannot know the real window behind a route, so start with a conservative
/// common value and let users adjust it in Zed's settings.
const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 128_000;

/// Where Zed's user settings live.
pub fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("settings.json"))
}

/// The Zed config directory, with `MAGPIE_LITE_HOME` taking precedence in
/// tests and wrappers.
fn config_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("MAGPIE_LITE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".config").join("zed"));
    }

    if cfg!(target_os = "windows") {
        let dir = dirs::config_dir().context("cannot find the Windows config directory")?;
        return Ok(dir.join("Zed"));
    }
    if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        if let Some(dir) = std::env::var_os("FLATPAK_XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let dir = dirs::config_dir().context("cannot find the XDG config directory")?;
        return Ok(dir.join("zed"));
    }

    let home = super::agent_home().context("cannot find the home directory")?;
    Ok(home.join(".config").join("zed"))
}

/// Whether Zed has a local configuration directory.
pub fn installed() -> bool {
    config_dir().map(|dir| dir.exists()).unwrap_or(false)
}

/// Wire Zed onto the gateway on `model`. Returns the settings path.
pub fn wire(model: &str, port: u16, _token: &str) -> Result<PathBuf> {
    let path = settings_path()?;
    wire_at(&path, model, port, _token)?;
    Ok(path)
}

/// The wiring itself, against an explicit settings path.
pub fn wire_at(path: &Path, model: &str, port: u16, _token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut settings: Value = if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    } else {
        json!({})
    };
    if !settings.is_object() {
        anyhow::bail!("{} must contain a JSON object", path.display());
    }

    if !settings
        .get("language_models")
        .is_some_and(Value::is_object)
    {
        settings["language_models"] = json!({});
    }
    if !settings
        .pointer("/language_models/openai_compatible")
        .is_some_and(Value::is_object)
    {
        settings["language_models"]["openai_compatible"] = json!({});
    }

    let current = settings["language_models"]["openai_compatible"][PROVIDER_ID].take();
    let mut provider = match current {
        Value::Object(provider) => provider,
        Value::Null => Map::new(),
        value => anyhow::bail!(
            "{} must contain an object at language_models.openai_compatible.{PROVIDER_ID}; found {value}",
            path.display()
        ),
    };
    provider.insert(
        "api_url".into(),
        json!(format!("http://127.0.0.1:{port}/v1")),
    );
    // Wiring selects one route. Replacing the gateway-owned list prevents
    // stale models from remaining after a user switches routes.
    provider.insert(
        "available_models".into(),
        json!([{
            "name": model,
            "display_name": format!("magpie-lite {model}"),
            "max_tokens": DEFAULT_CONTEXT_WINDOW_TOKENS,
        }]),
    );
    settings["language_models"]["openai_compatible"][PROVIDER_ID] = Value::Object(provider);

    std::fs::write(path, serde_json::to_vec_pretty(&settings)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
