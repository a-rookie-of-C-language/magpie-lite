//! Provider and gateway configuration, one TOML file.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Override the gateway-wide cooldown applied after this provider marks
    /// a key out of quota or rate limited.
    #[serde(default)]
    pub key_cooldown_ms: Option<u64>,
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

/// Round-robin and cooldown selection for one gateway process.
///
/// The cursor is provider-local and never persisted. Rotation spreads normal
/// load across a pool; a quota/rate-limit rejection makes the next key serve
/// that request and cools the rejected key for later requests.
#[derive(Debug, Default)]
struct KeyRotationState {
    cursor: usize,
    cooldowns: BTreeMap<usize, Instant>,
}

/// One active key cooldown. It names only the key's stable slot and when the
/// slot becomes available again — never the configured key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KeyCooldownSnapshot {
    /// Unix time in milliseconds when the cooldown ends.
    pub until_unix_ms: u64,
    /// Milliseconds left when the snapshot was taken.
    pub remaining_ms: u64,
}

/// Active cooldowns by provider id and zero-based key slot.
pub type KeyCooldownSnapshotMap = BTreeMap<String, BTreeMap<String, KeyCooldownSnapshot>>;

/// Thread-safe key pool scheduler used by the gateway state.
#[derive(Debug, Default)]
pub struct KeyRotator {
    states: Mutex<BTreeMap<String, KeyRotationState>>,
}

impl KeyRotator {
    /// Return `(configured slot, key)` pairs, starting at the next key that is
    /// not cooling. If every key is cooling, all are returned as a best effort
    /// rather than leaving the provider with no route.
    pub fn rotate(&self, provider_id: &str, keys: &[String]) -> Vec<(usize, String)> {
        if keys.len() < 2 {
            return keys.iter().cloned().map(|key| (0, key)).collect();
        }
        let now = Instant::now();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states.entry(provider_id.to_owned()).or_default();
        state.cooldowns.retain(|_, until| *until > now);

        let start = state.cursor;
        let rotated = keys[start..]
            .iter()
            .chain(keys[..start].iter())
            .enumerate()
            .map(|(rotated_slot, key)| ((start + rotated_slot) % keys.len(), key.clone()))
            .collect::<Vec<_>>();
        state.cursor = (start + 1) % keys.len();

        let available = rotated
            .iter()
            .filter(|(slot, _)| !state.cooldowns.contains_key(slot))
            .cloned()
            .collect::<Vec<_>>();
        if available.is_empty() {
            rotated
        } else {
            available
        }
    }

    /// Copy the currently active cooldowns, dropping expired entries.
    pub fn cooldown_snapshot(&self) -> KeyCooldownSnapshotMap {
        let now = Instant::now();
        let wall_now = SystemTime::now();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut out = KeyCooldownSnapshotMap::new();
        for (provider_id, state) in states.iter_mut() {
            state.cooldowns.retain(|_, until| *until > now);
            for (slot, until) in &state.cooldowns {
                let remaining = until.duration_since(now);
                let Some(unix) = wall_now
                    .checked_add(remaining)
                    .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                else {
                    continue;
                };
                let slots = out.entry(provider_id.clone()).or_default();
                slots.insert(
                    slot.to_string(),
                    KeyCooldownSnapshot {
                        until_unix_ms: unix.as_millis().min(u64::MAX as u128) as u64,
                        remaining_ms: remaining.as_millis().min(u64::MAX as u128) as u64,
                    },
                );
            }
        }
        out
    }

    /// Avoid a quota-limited or rate-limited key when another key is healthy.
    pub fn cool_down(&self, provider_id: &str, key_slot: usize, cooldown: Duration) {
        if cooldown.is_zero() {
            return;
        }
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        states
            .entry(provider_id.to_owned())
            .or_default()
            .cooldowns
            .insert(key_slot, Instant::now() + cooldown);
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
    /// Default time to avoid a provider key after quota or rate-limit rejection.
    #[serde(default = "default_key_cooldown_ms")]
    pub key_cooldown_ms: u64,
    /// Optional JSONL file for sanitized request-history metadata. Setting this
    /// makes recent `/stats` records survive gateway restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_path: Option<std::path::PathBuf>,
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

fn default_key_cooldown_ms() -> u64 {
    60_000
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
            key_cooldown_ms: default_key_cooldown_ms(),
            history_path: None,
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

    /// The cooldown to apply after a provider key is quota-limited or rate
    /// limited. A provider override wins over the gateway-wide default.
    pub fn key_cooldown_for(&self, provider: &ProviderConfig) -> Duration {
        Duration::from_millis(provider.key_cooldown_ms.unwrap_or(self.key_cooldown_ms))
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
            key_cooldown_ms: None,
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

    #[test]
    fn key_cooldown_hides_a_slot_until_it_expires() {
        let rotator = KeyRotator::default();
        let keys = vec!["one".to_owned(), "two".to_owned()];
        rotator.rotate("mock", &keys);
        rotator.rotate("mock", &keys);
        rotator.cool_down("mock", 0, Duration::from_millis(20));

        assert_eq!(rotator.rotate("mock", &keys), vec![(1, "two".to_owned())]);

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rotator.rotate("mock", &keys),
            vec![(1, "two".to_owned()), (0, "one".to_owned())]
        );
    }

    #[test]
    fn cooldown_snapshot_reports_slots_and_expires_them() {
        let rotator = KeyRotator::default();
        rotator.cool_down("mock", 0, Duration::from_millis(80));

        let snapshot = rotator.cooldown_snapshot();
        let slot = &snapshot["mock"]["0"];
        assert!(slot.remaining_ms > 0 && slot.remaining_ms <= 80);
        assert!(slot.until_unix_ms >= unix_now_ms());

        std::thread::sleep(Duration::from_millis(100));
        assert!(rotator.cooldown_snapshot().is_empty());
    }

    fn unix_now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn provider_cooldown_overrides_the_gateway_default() {
        let config = Config::default();
        let mut provider = ProviderConfig {
            id: "mock".into(),
            name: String::new(),
            protocol: Protocol::Chat,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: String::new(),
            api_keys: Vec::new(),
            models: Vec::new(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
        };

        assert_eq!(config.key_cooldown_for(&provider), Duration::from_secs(60));
        provider.key_cooldown_ms = Some(0);
        assert_eq!(config.key_cooldown_for(&provider), Duration::ZERO);
    }
}
