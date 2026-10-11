//! OpenCode: add the gateway as an OpenAI-compatible provider.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Where OpenCode reads its user configuration from.
pub fn config_path() -> Result<PathBuf> {
    let dir = config_dir()?;
    Ok(preferred_path(&dir))
}

pub(crate) fn preferred_path(dir: &Path) -> PathBuf {
    // OpenCode itself prefers the JSONC file, so do the same. Wiring rejects
    // JSONC rather than writing an ignored opencode.json beside it.
    for name in ["opencode.jsonc", "opencode.json", "config.json"] {
        let path = dir.join(name);
        if path.exists() {
            return path;
        }
    }
    dir.join("opencode.json")
}

/// The OpenCode config directory, with `MAGPIE_LITE_HOME` taking precedence
/// over machine-specific environment variables in tests and wrappers.
fn config_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("MAGPIE_LITE_HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(home).join(".config").join("opencode"));
    }
    if let Some(dir) = std::env::var_os("OPENCODE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute()
            && let Ok(cwd) = std::env::current_dir()
        {
            return Ok(cwd.join(dir));
        }
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("opencode"));
        }
    }
    let home = super::agent_home().context("cannot find the home directory")?;
    Ok(home.join(".config").join("opencode"))
}

/// Whether OpenCode has a local configuration directory.
pub fn installed() -> bool {
    config_dir().map(|dir| dir.exists()).unwrap_or(false)
}

/// Wire OpenCode onto the gateway on `model`. Returns the config path.
pub fn wire(model: &str, port: u16, token: &str) -> Result<PathBuf> {
    let path = config_path()?;
    wire_at(&path, model, port, token)?;
    Ok(path)
}

/// The wiring itself, against an explicit config path.
pub fn wire_at(path: &Path, model: &str, port: u16, token: &str) -> Result<()> {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonc"))
    {
        anyhow::bail!(
            "{} uses JSONC; preserve its comments and copy the gateway provider into it manually",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut config: Value = if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        super::json_config::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        json!({})
    };
    if !config.is_object() {
        anyhow::bail!("{} must contain a JSON object", path.display());
    }

    let wanted = json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "magpie-lite",
        "options": {
            "baseURL": format!("http://127.0.0.1:{port}/v1"),
            "apiKey": token,
        },
        // This command wires one selected model. Replacing the list avoids
        // stale entries when a user changes models.
        "models": {
            model: {
                "name": model,
            },
        },
    });
    let current = config.pointer("/provider/magpie-lite").cloned();
    let provider = match current {
        Some(current) => merge_owned(current, wanted, &["models"]),
        None => wanted,
    };

    if !config
        .get("provider")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        config["provider"] = json!({});
    }
    config["provider"]["magpie-lite"] = provider;
    config["model"] = json!(format!("magpie-lite/{model}"));

    std::fs::write(path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Keep every foreign key from an existing block while applying the fields
/// owned by magpie-lite. Whole-key fields (such as the model list) are
/// replaced because stale gateway models should disappear.
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
