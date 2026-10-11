//! Provider and gateway configuration, one TOML file.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

impl std::str::FromStr for Protocol {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "chat" => Ok(Protocol::Chat),
            "anthropic" => Ok(Protocol::Anthropic),
            "gemini" => Ok(Protocol::Gemini),
            "responses" => Ok(Protocol::Responses),
            _ => Err(format!(
                "unknown protocol `{s}`; expected chat, anthropic, responses or gemini"
            )),
        }
    }
}

/// The wire protocol an upstream speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// OpenAI Chat Completions (most vendors).
    Chat,
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI Responses (the newer endpoint Codex speaks).
    Responses,
    /// Google Gemini generateContent.
    Gemini,
}

impl Protocol {
    /// The spelling used in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Anthropic => "anthropic",
            Protocol::Responses => "responses",
            Protocol::Gemini => "gemini",
        }
    }
}

/// One configured upstream vendor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Short id models are routed by: `deepseek/deepseek-chat`.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Base URL: chat/responses bases end in `/v1`, Anthropic bases are the root, Gemini bases end in `/v1beta`.
    pub base_url: String,
    /// Compatibility key for configs written before key pools existed.
    /// It is used only when `api_keys` is empty.
    #[serde(default)]
    pub api_key: String,
    /// API key pool. Requests start at a rotating key and retry another key
    /// when this one is rejected, out of quota, or rate limited.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Known model ids, for bare-name routing and the catalog.
    #[serde(default)]
    pub models: Vec<String>,
    /// Where a request goes when this provider cannot take it — out of
    /// quota, rate limited, overloaded, or down — tried in order, before
    /// any of the reply has been sent: models as agents pick them.
    #[serde(default)]
    pub fallback: Vec<String>,
}

impl ProviderConfig {
    /// Display name: the given one, or the id.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// The effective key pool, in configured order. The legacy singular key
    /// keeps old TOML files working; an explicit pool wins.
    pub fn api_key_pool(&self) -> Vec<String> {
        let configured = if self.api_keys.is_empty() {
            vec![self.api_key.clone()]
        } else {
            self.api_keys.clone()
        };
        let mut keys = Vec::<String>::new();
        for key in configured {
            if !keys.iter().any(|k| k.as_str() == key) {
                keys.push(key.clone());
            }
        }
        if keys.is_empty() {
            keys.push(String::new());
        }
        keys
    }
}

/// Round-robin key selection for one gateway process.
///
/// The cursor is provider-local and never persisted. Rotation spreads normal
/// load across a pool; a key-level rejection makes the next key serve that
/// same request.
#[derive(Debug, Default)]
pub struct KeyRotator {
    cursors: Mutex<BTreeMap<String, usize>>,
}

impl KeyRotator {
    /// Return (configured slot, key) pairs rotated to the next starting key.
    pub fn rotate(&self, provider_id: &str, keys: &[String]) -> Vec<(usize, String)> {
        if keys.len() < 2 {
            return keys.iter().cloned().map(|key| (0, key)).collect();
        }
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = cursors.entry(provider_id.to_owned()).or_insert(0);
        let rotated = keys[*start..]
            .iter()
            .chain(keys[..*start].iter())
            .enumerate()
            .map(|(rotated_slot, key)| ((*start + rotated_slot) % keys.len(), key.clone()))
            .collect::<Vec<_>>();
        *start = (*start + 1) % keys.len();
        rotated
    }
}

/// A named route chain agents can pick as one model id: `group/daily-coding`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupConfig {
    /// Short id used after the reserved `group/` prefix.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Ordered model routes. The first available one serves the request;
    /// the rest are fallbacks tried before any reply bytes are streamed.
    pub routes: Vec<String>,
}

impl GroupConfig {
    /// Display name: the given one, or the id.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

/// Everything the gateway needs to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Port to listen on, loopback only.
    #[serde(default = "default_port")]
    pub port: u16,
    /// The exact token agents must send as Bearer, `x-api-key`, or `x-goog-api-key`.
    #[serde(default = "default_token")]
    pub token: String,
    /// Provider id used when a model names none.
    #[serde(default)]
    pub default_provider: String,
    /// The upstreams.
    #[serde(default)]
    pub provider: Vec<ProviderConfig>,
    /// Named route chains.
    #[serde(default)]
    pub group: Vec<GroupConfig>,
}

fn default_port() -> u16 {
    3426
}

fn default_token() -> String {
    "magpie-lite".to_owned()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            token: default_token(),
            default_provider: String::new(),
            provider: Vec::new(),
            group: Vec::new(),
        }
    }
}

impl Config {
    /// Load from a path; a missing file is a fresh config, not an error.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    /// Save back to the same path, creating parents as needed.
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Find a provider by id.
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.provider.iter().find(|p| p.id == id)
    }

    /// Find a routing group by id.
    pub fn group(&self, id: &str) -> Option<&GroupConfig> {
        self.group.iter().find(|g| g.id == id)
    }

    /// Resolve a model name to (upstream, upstream model name).
    ///
    /// `group/<id>` resolves the first usable route in that group.
    /// `deepseek/deepseek-chat` routes by provider id; a bare name matches
    /// a provider's `models` list, else the default provider, else the
    /// first provider there is.
    pub fn resolve(&self, model: &str) -> Option<(&ProviderConfig, String)> {
        if let Some(id) = model.strip_prefix("group/") {
            let group = self.group(id)?;
            return group
                .routes
                .iter()
                .find_map(|route| self.resolve_provider(route));
        }
        self.resolve_provider(model)
    }

    /// Resolve an ordinary provider/model or bare model reference.
    fn resolve_provider(&self, model: &str) -> Option<(&ProviderConfig, String)> {
        if let Some((pid, rest)) = model.split_once('/') {
            return self.provider(pid).map(|p| (p, rest.to_owned()));
        }
        for p in &self.provider {
            if p.models.iter().any(|m| m == model) {
                return Some((p, model.to_owned()));
            }
        }
        if let Some(p) = self.provider(&self.default_provider) {
            return Some((p, model.to_owned()));
        }
        self.provider.first().map(|p| (p, model.to_owned()))
    }

    /// The fallback references that follow the provider that first resolved
    /// `model`. A group owns its whole ordered chain; the provider fallbacks
    /// inside that chain are deliberately not combined with it.
    pub fn fallback_references(&self, model: &str, resolved: &ProviderConfig) -> Vec<String> {
        if let Some(id) = model.strip_prefix("group/")
            && let Some(group) = self.group(id)
        {
            return group.routes.clone();
        }
        resolved.fallback.clone()
    }

    /// Every model the catalog lists, as agents pick them (`provider/model`
    /// or `group/<id>`).
    pub fn catalog(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for g in &self.group {
            out.push((format!("group/{}", g.id), g.label().to_owned()));
        }
        for p in &self.provider {
            for m in &p.models {
                out.push((format!("{}/{}", p.id, m), p.label().to_owned()));
            }
        }
        out
    }
}

/// The config file's platform path (`%APPDATA%\magpie-lite\config.toml` on
/// Windows, `~/.config/magpie-lite/config.toml` elsewhere).
pub fn config_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot find the user config directory"))?
        .join("magpie-lite");
    Ok(dir.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_pool_keeps_the_legacy_singular_key() {
        let provider = ProviderConfig {
            id: "mock".into(),
            name: String::new(),
            protocol: Protocol::Chat,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "legacy-key".into(),
            api_keys: Vec::new(),
            models: Vec::new(),
            fallback: Vec::new(),
        };

        assert_eq!(provider.api_key_pool(), vec!["legacy-key".to_owned()]);
    }

    #[test]
    fn key_rotation_preserves_configured_slots() {
        let rotator = KeyRotator::default();
        let keys = vec!["one".to_owned(), "two".to_owned()];

        assert_eq!(
            rotator.rotate("mock", &keys),
            vec![(0, "one".to_owned()), (1, "two".to_owned())]
        );
        assert_eq!(
            rotator.rotate("mock", &keys),
            vec![(1, "two".to_owned()), (0, "one".to_owned())]
        );
    }
}
