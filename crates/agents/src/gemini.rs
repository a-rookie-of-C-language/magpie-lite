//! Gemini CLI: point its Google-compatible endpoint at the gateway.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where Gemini CLI's settings live.
pub fn settings_path() -> Result<PathBuf> {
    let home = super::agent_home().context("cannot find the home directory")?;
    Ok(home.join(".gemini").join("settings.json"))
}

/// Where Gemini CLI loads endpoint credentials from.
pub fn env_path() -> Result<PathBuf> {
    let home = super::agent_home().context("cannot find the home directory")?;
    Ok(home.join(".gemini").join(".env"))
}

/// Whether Gemini CLI has a local configuration directory.
pub fn installed() -> bool {
    super::agent_home()
        .map(|home| home.join(".gemini").exists())
        .unwrap_or(false)
}

/// Wire Gemini CLI onto the gateway with its Gemini API-key mode.
/// Returns the settings path.
pub fn wire(model: &str, port: u16, token: &str) -> Result<PathBuf> {
    let settings = settings_path()?;
    let env = env_path()?;
    wire_at(&settings, &env, model, port, token)?;
    Ok(settings)
}

/// The wiring itself, against explicit paths.
pub fn wire_at(
    settings_path: &Path,
    env_path: &Path,
    model: &str,
    port: u16,
    token: &str,
) -> Result<()> {
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut settings: Value = if settings_path.exists() {
        let text = std::fs::read_to_string(settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", settings_path.display()))?
    } else {
        json!({})
    };
    if !settings.is_object() {
        anyhow::bail!("{} must contain a JSON object", settings_path.display());
    }

    if !settings.get("security").is_some_and(Value::is_object) {
        settings["security"] = json!({});
    }
    if !settings
        .pointer("/security/auth")
        .is_some_and(Value::is_object)
    {
        settings["security"]["auth"] = json!({});
    }
    settings["security"]["auth"]["selectedType"] = json!("gemini-api-key");

    if !settings.get("model").is_some_and(Value::is_object) {
        settings["model"] = json!({});
    }
    settings["model"]["name"] = json!(model);

    std::fs::write(settings_path, serde_json::to_vec_pretty(&settings)?)
        .with_context(|| format!("writing {}", settings_path.display()))?;

    set_env_values(
        env_path,
        &[
            ("GOOGLE_GEMINI_BASE_URL", format!("http://127.0.0.1:{port}")),
            ("GEMINI_API_KEY", token.to_owned()),
        ],
    )?;
    Ok(())
}

/// Replace only the named dotenv keys, preserving comments, blank lines,
/// and unrelated variables.
fn set_env_values(path: &Path, values: &[(&str, String)]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let wanted: HashSet<&str> = values.iter().map(|(key, _)| *key).collect();
    let mut seen = HashSet::new();
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let Some((key, _)) = line.trim_start().split_once('=') else {
                return line.to_owned();
            };
            let key = key.trim();
            if !wanted.contains(key) {
                return line.to_owned();
            }
            seen.insert(key.to_owned());
            let (_, value) = values
                .iter()
                .find(|(wanted_key, _)| wanted_key == &key)
                .expect("wanted key was checked above");
            format!("{key}={}", quote_env_value(value))
        })
        .collect();

    for (key, value) in values {
        if !seen.contains(*key) {
            lines.push(format!("{key}={}", quote_env_value(value)));
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn quote_env_value(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '#' | '$' | '"' | '\'' | '\\'));
    if needs_quotes {
        format!("{value:?}")
    } else {
        value.to_owned()
    }
}
