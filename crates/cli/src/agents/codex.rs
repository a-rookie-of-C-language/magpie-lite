//! Codex: name the gateway as a model provider in its `config.toml`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, value};

/// Where Codex's config lives.
pub fn config_path() -> Result<PathBuf> {
    let home = super::agent_home().context("cannot find the home directory")?;
    Ok(home.join(".codex").join("config.toml"))
}

/// Whether Codex looks installed.
pub fn installed() -> bool {
    config_path().map(|p| p.exists()).unwrap_or(false)
}

/// Wire Codex onto the gateway as a provider that speaks Chat Completions
/// (`wire_api = "chat"`), keeping the rest of the file as it is.
/// Returns the config path.
pub fn wire(model: &str, port: u16, token: &str) -> Result<PathBuf> {
    let path = config_path()?;
    wire_at(&path, model, port, token)?;
    Ok(path)
}

/// The wiring itself, against an explicit config path.
pub fn wire_at(path: &Path, model: &str, port: u16, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let text = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    doc["model"] = value(model);
    doc["model_provider"] = value("magpie-lite");

    if doc.get("model_providers").is_none() {
        let mut providers = Table::new();
        providers.set_implicit(true);
        doc["model_providers"] = Item::Table(providers);
    }
    let providers = doc["model_providers"]
        .as_table_mut()
        .context("model_providers is not a table")?;
    if providers.get("magpie-lite").is_none() {
        providers["magpie-lite"] = Item::Table(Table::new());
    }
    let table = providers["magpie-lite"]
        .as_table_mut()
        .context("magpie-lite is not a table")?;
    table["name"] = value("magpie-lite");
    table["base_url"] = value(format!("http://127.0.0.1:{port}/v1"));
    table["wire_api"] = value("chat");
    table["experimental_bearer_token"] = value(token);

    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
