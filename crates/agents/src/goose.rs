//! Goose: register the gateway as a custom OpenAI-compatible provider.

use anyhow::{Context, Result};
use serde_json::{Map, Value as JsonValue, json};
use serde_yaml::{Mapping, Value as YamlValue};
use std::path::{Path, PathBuf};

/// Goose's id for the gateway provider.
pub const PROVIDER_ID: &str = "magpie-lite";

/// Goose requires a context limit for a custom model. The gateway cannot know
/// the window behind every route, so start conservative and let users adjust
/// Goose's provider file when needed.
const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 128_000;

/// Where Goose's user configuration lives.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

/// Goose's config directory, with `MAGPIE_LITE_HOME` taking precedence in
/// tests and wrappers.
fn config_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("MAGPIE_LITE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".config").join("goose"));
    }

    if cfg!(target_os = "windows") {
        let dir = dirs::config_dir().context("cannot find the Windows config directory")?;
        return Ok(dir.join("Block").join("goose").join("config"));
    }

    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("goose"));
        }
    }
    let home = super::agent_home().context("cannot find the home directory")?;
    Ok(home.join(".config").join("goose"))
}

/// Whether Goose has a local configuration directory.
pub fn installed() -> bool {
    config_dir().map(|dir| dir.exists()).unwrap_or(false)
}

/// Wire Goose onto the gateway on `model`. Returns `config.yaml`'s path.
pub fn wire(model: &str, port: u16, token: &str) -> Result<PathBuf> {
    let path = config_path()?;
    wire_at(&path, model, port, token)?;
    Ok(path)
}

/// The wiring itself, against an explicit config path.
pub fn wire_at(path: &Path, model: &str, port: u16, token: &str) -> Result<()> {
    let parent = path.parent().context("Goose config has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let mut config = read_config(path)?;
    let new_layout = config.get("providers").is_some() || config.get("active_provider").is_some();
    if new_layout {
        set_active_provider(&mut config, model)?;
    } else {
        set_top(&mut config, "GOOSE_PROVIDER", PROVIDER_ID);
        set_top(&mut config, "GOOSE_MODEL", model);
    }

    let original = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    std::fs::write(path, super::selective::edit_yaml(&original, &config)?)
        .with_context(|| format!("writing {}", path.display()))?;
    write_custom_provider(parent, model, port, token)?;
    Ok(())
}

/// Read `config.yaml`, treating an absent or empty file as an empty mapping.
fn read_config(path: &Path) -> Result<YamlValue> {
    if !path.exists() {
        return Ok(YamlValue::Mapping(Mapping::new()));
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(YamlValue::Mapping(Mapping::new()));
    }
    let config: YamlValue =
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if config.is_null() {
        return Ok(YamlValue::Mapping(Mapping::new()));
    }
    if !config.is_mapping() {
        anyhow::bail!("Goose configuration must contain a YAML mapping");
    }
    Ok(config)
}

/// Write Goose's newer `active_provider` plus `providers` layout, preserving
/// every other provider entry and every other field in our own entry.
fn set_active_provider(config: &mut YamlValue, model: &str) -> Result<()> {
    set_top(config, "active_provider", PROVIDER_ID);

    if config
        .get("providers")
        .is_some_and(|providers| !providers.is_mapping())
    {
        anyhow::bail!("Goose providers must contain a YAML mapping");
    }
    if !config.get("providers").is_some_and(YamlValue::is_mapping) {
        config["providers"] = YamlValue::Mapping(Mapping::new());
    }
    let mut provider = match config.get("providers").and_then(|providers| {
        providers
            .as_mapping()
            .and_then(|providers| providers.get(PROVIDER_ID))
    }) {
        Some(value @ YamlValue::Mapping(_)) => value.clone(),
        Some(other) => {
            anyhow::bail!("Goose provider `{PROVIDER_ID}` must be a mapping; found {other:?}");
        }
        None => YamlValue::Mapping(Mapping::new()),
    };
    set_top(&mut provider, "enabled", true);
    set_top(&mut provider, "model", model);
    set_top(&mut provider, "configured", true);
    if let YamlValue::Mapping(providers) = &mut config["providers"] {
        providers.insert(PROVIDER_ID.into(), provider);
    }

    // A migrated Goose ignores and later deletes these flat keys. Removing
    // them now prevents a selected model from silently going stale.
    if let YamlValue::Mapping(config) = config {
        config.remove("GOOSE_PROVIDER");
        config.remove("GOOSE_MODEL");
    }
    Ok(())
}

fn set_top(value: &mut YamlValue, key: &str, replacement: impl Into<YamlValue>) {
    if !value.is_mapping() {
        *value = YamlValue::Mapping(Mapping::new());
    }
    if let YamlValue::Mapping(mapping) = value {
        mapping.insert(key.into(), replacement.into());
    }
}

/// Write the gateway-owned declarative provider Goose loads at startup.
fn write_custom_provider(config_dir: &Path, model: &str, port: u16, token: &str) -> Result<()> {
    let dir = config_dir.join("custom_providers");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{PROVIDER_ID}.json"));

    let mut provider = if path.exists() {
        let current: JsonValue = super::json_config::from_slice(&std::fs::read(&path)?)?;
        current
            .as_object()
            .context("Goose custom provider must be a JSON object")?
            .clone()
    } else {
        Map::new()
    };
    provider.insert("name".into(), json!(PROVIDER_ID));
    provider.insert("engine".into(), json!("openai"));
    provider.insert("display_name".into(), json!("magpie-lite"));
    provider.insert(
        "description".into(),
        json!("Models through the local magpie-lite gateway"),
    );
    provider.insert("api_key_env".into(), json!(""));
    provider.insert(
        "base_url".into(),
        json!(format!("http://127.0.0.1:{port}/v1")),
    );
    let mut headers = match provider.remove("headers") {
        None => Map::new(),
        Some(JsonValue::Object(map)) => map,
        Some(_) => anyhow::bail!("Goose custom provider headers must be a JSON object"),
    };
    headers.insert("Authorization".into(), json!(format!("Bearer {token}")));
    provider.insert("headers".into(), JsonValue::Object(headers));
    provider.insert("requires_auth".into(), json!(false));
    provider.insert("skip_canonical_filtering".into(), json!(true));
    provider.insert(
        "models".into(),
        json!([{
            "name": model,
            "context_limit": DEFAULT_CONTEXT_WINDOW_TOKENS,
            "reasoning": false,
        }]),
    );
    provider.insert("dynamic_models".into(), json!(false));

    super::json_config::write(
        &path,
        &JsonValue::Object(provider),
        super::json_config::Syntax::Json,
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
