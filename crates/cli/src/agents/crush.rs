//! Crush: register the gateway as an OpenAI-compatible provider.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Crush's id for the gateway provider.
pub const PROVIDER_ID: &str = "magpie-lite";

/// Conservative defaults Crush needs for a custom model.
const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16_384;

/// Crush keeps providers and picker data in different files on Unix, but in
/// one `%LOCALAPPDATA%\crush\crush.json` on Windows.
fn paths() -> Result<(PathBuf, PathBuf)> {
    if let Some(home) = std::env::var_os("MAGPIE_LITE_HOME").filter(|value| !value.is_empty()) {
        let home = PathBuf::from(home);
        return Ok((
            home.join(".config").join("crush").join("crush.json"),
            home.join(".local")
                .join("share")
                .join("crush")
                .join("crush.json"),
        ));
    }

    if cfg!(target_os = "windows") {
        let dir = dirs::data_local_dir().context("cannot find the Windows local data directory")?;
        let path = dir.join("crush").join("crush.json");
        return Ok((path.clone(), path));
    }

    let config_dir = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(dir) if PathBuf::from(&dir).is_absolute() => PathBuf::from(dir),
        _ => super::agent_home()
            .context("cannot find the home directory")?
            .join(".config"),
    };
    let data_dir = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(dir) if PathBuf::from(&dir).is_absolute() => PathBuf::from(dir),
        _ => super::agent_home()
            .context("cannot find the home directory")?
            .join(".local")
            .join("share"),
    };
    Ok((
        config_dir.join("crush").join("crush.json"),
        data_dir.join("crush").join("crush.json"),
    ))
}

/// Whether Crush has a local configuration directory.
pub fn installed() -> bool {
    paths()
        .map(|(config, _)| config.parent().is_some_and(Path::exists))
        .unwrap_or(false)
}

/// Wire Crush's large model onto the gateway. Returns the provider config path.
pub fn wire(model: &str, port: u16, token: &str) -> Result<PathBuf> {
    let (config, data) = paths()?;
    wire_at(&config, &data, model, port, token)?;
    Ok(config)
}

/// The wiring itself, against explicit provider-config and picker-data paths.
pub fn wire_at(
    config_path: &Path,
    data_path: &Path,
    model: &str,
    port: u16,
    token: &str,
) -> Result<()> {
    let same_file = config_path == data_path;
    let mut config = read_object(config_path)?;
    set_provider(&mut config, model, port, token);
    set_large_pick(&mut config, model);

    if same_file {
        write_json(config_path, &config)?;
        return Ok(());
    }

    write_json(config_path, &config)?;
    let data_existed = data_path.exists();
    let mut data = read_object(data_path)?;
    set_large_pick(&mut data, model);
    write_json(data_path, &data)?;
    if !data_existed {
        restrict_to_owner(data_path)?;
    }
    Ok(())
}

fn read_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn set_provider(config: &mut Value, model: &str, port: u16, token: &str) {
    let wanted = json!({
        "type": "openai",
        "name": PROVIDER_ID,
        "base_url": format!("http://127.0.0.1:{port}/v1"),
        "api_key": token,
        "models": [{
            "id": model,
            "name": model,
            "context_window": DEFAULT_CONTEXT_WINDOW_TOKENS,
            "default_max_tokens": DEFAULT_MAX_OUTPUT_TOKENS,
            "can_reason": false,
        }],
    });
    let current = config.pointer("/providers/magpie-lite").cloned();
    let provider = match current {
        Some(current) => merge_owned(current, wanted, &["models"]),
        None => wanted,
    };

    if !config.get("providers").is_some_and(Value::is_object) {
        config["providers"] = json!({});
    }
    config["providers"][PROVIDER_ID] = provider;
}

/// Replace the whole large-model pick. Crush stores per-model output and
/// sampling settings in this object; keeping them when changing models can
/// apply an old model's limits to the new one.
fn set_large_pick(data: &mut Value, model: &str) {
    if !data.get("models").is_some_and(Value::is_object) {
        data["models"] = json!({});
    }
    data["models"]["large"] = json!({
        "provider": PROVIDER_ID,
        "model": model,
    });
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

/// Keep foreign keys while applying gateway-owned fields. Whole keys such as
/// the model list are replaced so stale routes disappear.
fn merge_owned(mut current: Value, wanted: Value, whole: &[&str]) -> Value {
    let Some(current) = current.as_object_mut() else {
        return wanted;
    };
    let Some(wanted) = wanted.as_object() else {
        return Value::Object(current.clone());
    };
    let mut out = Map::new();
    for (key, value) in wanted {
        out.insert(key.clone(), value.clone());
    }
    for (key, theirs) in current {
        if !out.contains_key(key) {
            out.insert(key.clone(), theirs.clone());
        } else if !whole.contains(&key.as_str()) {
            let ours = out.get(key).cloned().unwrap_or(Value::Null);
            out.insert(key.clone(), merge_owned(theirs.clone(), ours, &[]));
        }
    }
    Value::Object(out)
}
