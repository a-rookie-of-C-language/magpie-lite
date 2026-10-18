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

/// Optional, user-supplied capability metadata for an upstream provider.
///
/// A missing boolean means that the capability is unknown; it is deliberately
/// not inferred from the wire protocol. This keeps old configuration files
/// honest while still allowing a UI or routing policy to distinguish an
/// explicitly unsupported feature from one that has not been checked yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Protocols the provider is known to accept. An empty list means only
    /// the configured `ProviderConfig::protocol` is known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_protocols: Vec<Protocol>,
    /// Whether the provider can return streamed responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    /// Whether the provider accepts tool/function definitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    /// Whether the provider accepts image or other multimodal input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
}

impl ProviderCapabilities {
    /// Return the protocols that are safe to display as supported.
    pub fn effective_protocols(&self, current: Protocol) -> Vec<Protocol> {
        if self.supported_protocols.is_empty() {
            vec![current]
        } else {
            self.supported_protocols.clone()
        }
    }

    fn validate(&self, current: Protocol) -> anyhow::Result<()> {
        if !self.supported_protocols.is_empty() && !self.supported_protocols.contains(&current) {
            anyhow::bail!("capabilities.supported_protocols must include the configured protocol");
        }
        for (index, protocol) in self.supported_protocols.iter().enumerate() {
            if self.supported_protocols[..index].contains(protocol) {
                anyhow::bail!("capabilities.supported_protocols cannot contain duplicates");
            }
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.supported_protocols.is_empty()
            && self.streaming.is_none()
            && self.tools.is_none()
            && self.vision.is_none()
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
    /// Optional provider feature metadata. It does not change request
    /// translation; it describes what the configured upstream is known to
    /// support for management clients and future routing policies.
    #[serde(default, skip_serializing_if = "ProviderCapabilities::is_empty")]
    pub capabilities: ProviderCapabilities,
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
    /// How requests spread over this provider's key pool. `rotate`, the
    /// default, is the round-robin the pool has always used.
    #[serde(default = "default_provider_routing")]
    pub routing: RoutingPolicy,
    /// Per-key weights for the `weight` policy, parallel to
    /// `api_key_pool()`: the slot at index `i` weighs `weights[i]`. A
    /// missing or zero entry weighs 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<u32>,
    /// Process-local credential/endpoint version; never saved or disclosed.
    #[serde(skip)]
    pub key_generation: u64,
}

/// The largest weight a key may carry; zero is taken as the default 1.
pub const MAX_KEY_WEIGHT: u32 = 1000;

/// A provider rotates its key pool unless it says otherwise.
pub(crate) fn default_provider_routing() -> RoutingPolicy {
    RoutingPolicy::Rotate
}

impl ProviderConfig {
    /// Shared validation for console and CLI writes. Errors never quote secrets.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.id.is_empty()
            || self.id == "group"
            || self
                .id
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            anyhow::bail!(
                "provider id must contain only ASCII letters, digits, '-', or '_'; 'group' is reserved"
            );
        }
        if self.base_url.chars().any(char::is_whitespace) {
            anyhow::bail!("base URL cannot contain whitespace");
        }
        self.capabilities.validate(self.protocol)?;
        let url = reqwest::Url::parse(&self.base_url)
            .map_err(|_| anyhow::anyhow!("base URL must be an absolute HTTP or HTTPS URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            anyhow::bail!(
                "base URL must use HTTP or HTTPS without credentials, query, or fragment"
            );
        }
        let keys = if self.api_keys.is_empty() {
            vec![&self.api_key]
        } else {
            self.api_keys.iter().collect()
        };
        if keys
            .iter()
            .any(|key| key.bytes().any(|c| c.is_ascii_control() || c == b' '))
        {
            anyhow::bail!("API keys cannot contain whitespace or control characters");
        }
        if self
            .models
            .iter()
            .any(|m| m.is_empty() || m.chars().any(char::is_whitespace))
        {
            anyhow::bail!("model ids must be non-empty and contain no whitespace");
        }
        if self.weights.iter().any(|weight| *weight > MAX_KEY_WEIGHT) {
            anyhow::bail!("key weights cannot be above {MAX_KEY_WEIGHT}");
        }
        if self.weights.len() > self.api_key_pool().len() {
            anyhow::bail!("there are more key weights than keys in the pool");
        }
        Ok(())
    }
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

    /// One weight per effective key slot, in pool order. Absent and zero
    /// entries weigh 1 so an unweighted pool keeps the rotation it had.
    pub fn key_weights(&self) -> Vec<u32> {
        let slots = self.api_key_pool().len();
        (0..slots)
            .map(|slot| self.weights.get(slot).copied().unwrap_or(0))
            .collect()
    }
}

/// Round-robin and cooldown selection for one gateway process.
///
/// The cursor is provider-local and never persisted. Rotation spreads normal
/// load across a pool; a quota/rate-limit rejection makes the next key serve
/// that request and cools the rejected key for later requests.
#[derive(Debug, Default)]
struct KeyRotationState {
    generation: u64,
    cursor: usize,
    cooldowns: BTreeMap<usize, Instant>,
    /// Smooth weighted round robin: each key slot's current weight, and the
    /// total the last pick charged. Kept between requests so the ratio holds
    /// over every few requests rather than only on average.
    current: BTreeMap<usize, i64>,
    /// What each key slot answered lately, for the `usage` policy.
    served: BTreeMap<usize, TokenUse>,
}

/// Tokens a key served, decaying so a key that has rested catches up.
#[derive(Debug, Clone, Copy)]
struct TokenUse {
    tokens: f64,
    at: Instant,
}

/// How fast served tokens stop counting against a key: half of them after an
/// hour, as the reference `usageHalfLife` does.
const USAGE_HALF_LIFE: Duration = Duration::from_secs(60 * 60);

impl TokenUse {
    /// The tokens still counted against the key now.
    fn now(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens * 2f64.powf(-elapsed / USAGE_HALF_LIFE.as_secs_f64())
    }
}

#[derive(Debug)]
struct GroupRoutingState {
    policy: RoutingPolicy,
    routes: Vec<String>,
    cursor: usize,
    attempts: Vec<u64>,
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
    group_states: Mutex<BTreeMap<String, GroupRoutingState>>,
}

impl KeyRotator {
    /// Return `(configured slot, key)` pairs, starting at the next key that is
    /// not cooling. If every key is cooling, all are returned as a best effort
    /// rather than leaving the provider with no route.
    pub fn rotate(
        &self,
        provider_id: &str,
        generation: u64,
        keys: &[String],
    ) -> Vec<(usize, String)> {
        self.rotate_routed(provider_id, generation, RoutingPolicy::Rotate, keys, &[])
    }

    /// Return `(configured slot, key)` pairs in the order the provider's
    /// routing policy picks, so the first pair is tried first and the rest are
    /// its failover. `weights` is parallel to `keys`; a missing or zero entry
    /// weighs 1. A cooling key is left out while another key is healthy, and
    /// kept as a last resort when every key is cooling.
    ///
    /// Slots always stay the configured ones: a policy only decides the order,
    /// so a request can still re-ask the exact key that answered it.
    pub fn rotate_routed(
        &self,
        provider_id: &str,
        generation: u64,
        routing: RoutingPolicy,
        keys: &[String],
        weights: &[u32],
    ) -> Vec<(usize, String)> {
        if keys.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states.entry(provider_id.to_owned()).or_default();
        // A request pinned before an edit must never reset a newer schedule.
        if generation < state.generation {
            return keys.iter().cloned().enumerate().collect();
        }
        if generation > state.generation {
            *state = KeyRotationState {
                generation,
                ..Default::default()
            };
        }
        state.cooldowns.retain(|_, until| *until > now);
        // A pool that shrank inside one generation must not keep the weights
        // and usage counts of the slots it no longer has.
        state.current.retain(|slot, _| *slot < keys.len());
        state.served.retain(|slot, _| *slot < keys.len());

        let ordered = match routing {
            RoutingPolicy::Order => (0..keys.len()).collect::<Vec<_>>(),
            RoutingPolicy::Rotate => {
                let start = state.cursor % keys.len();
                state.cursor = (start + 1) % keys.len();
                (0..keys.len())
                    .map(|offset| (start + offset) % keys.len())
                    .collect()
            }
            RoutingPolicy::Usage => {
                // Least used first, by the tokens still counted against each
                // key after an hour's half-life; ties keep their slot order.
                let served = |slot: &usize| {
                    state
                        .served
                        .get(slot)
                        .map(|use_| use_.now(now))
                        .unwrap_or(0.0)
                };
                let mut slots = (0..keys.len()).collect::<Vec<_>>();
                slots.sort_by(|a, b| {
                    served(a)
                        .partial_cmp(&served(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.cmp(b))
                });
                slots
            }
            RoutingPolicy::Weighted => {
                // Smooth weighted round robin: every key that isn't cooling
                // gains its weight, and the heaviest takes the request and is
                // charged the total, which holds the ratio over every few
                // requests. A cooling key neither gains nor is charged, so it
                // takes up its share where it left off once it is back.
                let mut best: Option<usize> = None;
                let mut total: i64 = 0;
                for slot in 0..keys.len() {
                    if state.cooldowns.contains_key(&slot) {
                        continue;
                    }
                    let weight = i64::from(weights.get(slot).copied().unwrap_or(0).max(1));
                    *state.current.entry(slot).or_insert(0) += weight;
                    total += weight;
                    let current = *state.current.get(&slot).unwrap_or(&0);
                    if best.is_none_or(|best| current > *state.current.get(&best).unwrap_or(&0)) {
                        best = Some(slot);
                    }
                }
                match best {
                    Some(best) => {
                        *state.current.entry(best).or_insert(0) -= total;
                        let mut slots = Vec::with_capacity(keys.len());
                        slots.push(best);
                        slots.extend((0..keys.len()).filter(|slot| *slot != best));
                        slots
                    }
                    // Every key is cooling: leave them in their order, as the
                    // reference's weighted round does.
                    None => (0..keys.len()).collect(),
                }
            }
        };

        let available = ordered
            .iter()
            .copied()
            .filter(|slot| !state.cooldowns.contains_key(slot))
            .collect::<Vec<_>>();
        let ordered = if available.is_empty() { ordered } else { available };
        ordered
            .into_iter()
            .map(|slot| (slot, keys[slot].clone()))
            .collect()
    }

    /// Count what a key just answered against it for the `usage` policy, and
    /// wake it: a key that answers is not resting any more. `tokens` of zero
    /// still counts as one request served.
    pub fn record_served(
        &self,
        provider_id: &str,
        generation: u64,
        key_slot: usize,
        tokens: u64,
    ) {
        let now = Instant::now();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states.entry(provider_id.to_owned()).or_default();
        if generation < state.generation {
            return;
        }
        if generation > state.generation {
            *state = KeyRotationState {
                generation,
                ..Default::default()
            };
        }
        let served = state
            .served
            .get(&key_slot)
            .copied()
            .unwrap_or(TokenUse {
                tokens: 0.0,
                at: now,
            });
        state.served.insert(
            key_slot,
            TokenUse {
                tokens: served.now(now) + tokens.max(1) as f64,
                at: now,
            },
        );
        state.cooldowns.remove(&key_slot);
    }

    /// Return a group route order and reset its process-local schedule when its
    /// policy or configured routes change.
    pub fn group_order(
        &self,
        group_id: &str,
        policy: RoutingPolicy,
        routes: &[String],
    ) -> Vec<(usize, String)> {
        if routes.is_empty() {
            return Vec::new();
        }
        let mut states = self
            .group_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states
            .entry(group_id.to_owned())
            .or_insert_with(|| GroupRoutingState {
                policy,
                routes: routes.to_vec(),
                cursor: 0,
                attempts: vec![0; routes.len()],
            });
        if state.policy != policy || state.routes != routes {
            *state = GroupRoutingState {
                policy,
                routes: routes.to_vec(),
                cursor: 0,
                attempts: vec![0; routes.len()],
            };
        }

        let indices = match state.policy {
            // A group has no per-route weights, so a weighted provider
            // policy takes its configured order here.
            RoutingPolicy::Order | RoutingPolicy::Weighted => {
                (0..routes.len()).collect::<Vec<_>>()
            }
            RoutingPolicy::Rotate => {
                let start = state.cursor % routes.len();
                state.cursor = (start + 1) % routes.len();
                (0..routes.len())
                    .map(|offset| (start + offset) % routes.len())
                    .collect()
            }
            RoutingPolicy::Usage => {
                let mut indices = (0..routes.len()).collect::<Vec<_>>();
                indices.sort_by_key(|&index| (state.attempts[index], index));
                indices
            }
        };
        indices
            .into_iter()
            .map(|index| (index, routes[index].clone()))
            .collect()
    }

    /// Record one request attempt against a group route. Counts are process-local
    /// and intentionally reset when the group route configuration changes.
    pub fn record_group_attempt(
        &self,
        group_id: &str,
        policy: RoutingPolicy,
        routes: &[String],
        route_index: usize,
    ) {
        if route_index >= routes.len() {
            return;
        }
        let mut states = self
            .group_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states
            .entry(group_id.to_owned())
            .or_insert_with(|| GroupRoutingState {
                policy,
                routes: routes.to_vec(),
                cursor: 0,
                attempts: vec![0; routes.len()],
            });
        if state.policy != policy || state.routes != routes {
            *state = GroupRoutingState {
                policy,
                routes: routes.to_vec(),
                cursor: 0,
                attempts: vec![0; routes.len()],
            };
        }
        if let Some(attempts) = state.attempts.get_mut(route_index) {
            *attempts = attempts.saturating_add(1);
        }
    }

    /// Copy the currently active cooldowns, dropping expired entries.
    pub fn cooldown_snapshot(&self, config: &Config) -> KeyCooldownSnapshotMap {
        let now = Instant::now();
        let wall_now = SystemTime::now();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut out = KeyCooldownSnapshotMap::new();
        states.retain(|provider_id, _| config.provider(provider_id).is_some());
        for (provider_id, state) in states.iter_mut() {
            if config
                .provider(provider_id)
                .is_none_or(|p| p.key_generation != state.generation)
            {
                continue;
            }
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
    pub fn cool_down(
        &self,
        provider_id: &str,
        generation: u64,
        key_slot: usize,
        cooldown: Duration,
    ) {
        if cooldown.is_zero() {
            return;
        }
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states.entry(provider_id.to_owned()).or_default();
        if generation < state.generation {
            return;
        }
        if generation > state.generation {
            *state = KeyRotationState {
                generation,
                ..Default::default()
            };
        }
        if let Some(until) = Instant::now().checked_add(cooldown) {
            state.cooldowns.insert(key_slot, until);
        }
    }
}

/// How a routing group chooses its next route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicy {
    /// Try routes in their configured order for every request.
    #[default]
    Order,
    /// Start each request at the next route, then fall back through the rest.
    Rotate,
    /// Prefer routes with fewer attempts in this gateway process.
    Usage,
    /// Spread a provider's requests over its keys by each key's weight,
    /// smoothly, as NGINX's smooth weighted round robin does: keys weighing
    /// 3 and 1 take three requests to one, holding the ratio over every few
    /// requests rather than only on average. A routing group has no weights,
    /// so it takes this for `order`.
    #[serde(rename = "weight")]
    Weighted,
}

impl RoutingPolicy {
    /// The canonical spelling used by config files, APIs, and the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Order => "order",
            Self::Rotate => "rotate",
            Self::Usage => "usage",
            Self::Weighted => "weight",
        }
    }
}

impl std::str::FromStr for RoutingPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "order" => Ok(Self::Order),
            "rotate" | "round_robin" => Ok(Self::Rotate),
            "usage" | "least_used" => Ok(Self::Usage),
            "weight" | "weighted" => Ok(Self::Weighted),
            _ => Err(format!(
                "unknown routing policy `{value}`; expected order, rotate, usage or weight"
            )),
        }
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
    /// Process-local route selection policy.
    #[serde(default)]
    pub routing: RoutingPolicy,
    /// Per-attempt first-content budget for non-final streaming group routes.
    /// Zero disables this group limit; gateway-wide stream limits still apply.
    #[serde(default)]
    pub first_token_timeout_ms: u64,
}

impl GroupConfig {
    /// The same rules the CLI and console API enforce.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.id.is_empty()
            || self
                .id
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            anyhow::bail!("group id must contain only ASCII letters, digits, `-`, or `_`");
        }
        if self.routes.is_empty() {
            anyhow::bail!("a group needs at least one route");
        }
        if self.routes.iter().any(|route| route.starts_with("group/")) {
            anyhow::bail!("group routes must name providers directly, not other groups");
        }
        for route in &self.routes {
            let Some((provider, model)) = route.split_once('/') else {
                anyhow::bail!("group routes must use provider/model");
            };
            if provider.is_empty() || model.is_empty() || route.chars().any(char::is_whitespace) {
                anyhow::bail!("group routes must use non-empty provider/model without whitespace");
            }
        }
        Ok(())
    }

    /// Display name: the given one, or the id.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

/// One provider as the console may show it: routing metadata only, never an
/// upstream API key or the gateway token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfigSnapshot {
    /// Short id models route by.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Upstream wire protocol.
    pub protocol: Protocol,
    /// Capability metadata; null feature values mean unknown.
    pub capabilities: ProviderCapabilitiesSnapshot,
    /// Upstream base URL.
    pub base_url: String,
    /// Models explicitly listed for bare-name routing and `/v1/models`.
    pub models: Vec<String>,
    /// Number of effective key slots, not the keys themselves.
    pub key_count: usize,
    /// Number of configured credentials (zero means no upstream authentication).
    pub credential_count: usize,
    /// Explicit override; null means inherit the gateway default.
    pub key_cooldown_override_ms: Option<u64>,
    /// Ordered fallback references.
    pub fallback: Vec<String>,
    /// Effective cooldown after quota or rate-limit rejection.
    pub key_cooldown_ms: u64,
    /// How requests spread over this provider's key pool.
    pub routing: RoutingPolicy,
    /// Per-key weights, parallel to the key pool, for the `weight` policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<u32>,
}

/// Capability metadata exposed by the secret-free configuration snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilitiesSnapshot {
    /// Protocols the provider is known to accept, including the configured
    /// protocol when no broader list was supplied.
    pub supported_protocols: Vec<Protocol>,
    /// Whether streamed responses are supported, or null when unknown.
    pub streaming: Option<bool>,
    /// Whether tools/functions are supported, or null when unknown.
    pub tools: Option<bool>,
    /// Whether multimodal input is supported, or null when unknown.
    pub vision: Option<bool>,
}

/// One routing group as the console may show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupConfigSnapshot {
    /// Short id; agents use `group/<id>`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Ordered route chain.
    pub routes: Vec<String>,
    /// Route selection policy.
    pub routing: RoutingPolicy,
    /// First-content budget for non-final streaming group routes (0 disables).
    #[serde(default)]
    pub first_token_timeout_ms: u64,
}

/// Secret-free routing configuration for the token-protected console.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// Durable opaque revision used for optimistic management updates.
    /// It is opaque to clients and never contains configuration or secret data.
    pub revision: u64,
    /// Provider used when a model names none.
    pub default_provider: String,
    /// Gateway-wide cooldown default, in milliseconds.
    pub key_cooldown_ms: u64,
    /// Per-attempt streaming first-content timeout; zero disables it.
    pub stream_first_content_timeout_ms: u64,
    /// Streaming upstream read-idle timeout; zero disables it.
    pub stream_idle_timeout_ms: u64,
    /// Downstream SSE comment interval; zero disables keepalive.
    pub stream_keepalive_interval_ms: u64,
    /// Stop comments after this much upstream silence; zero disables keepalive.
    pub stream_keepalive_max_silence_ms: u64,
    /// Responses opening-event hold budget across all attempts; zero disables it.
    pub stream_responses_hold_timeout_ms: u64,
    /// Whether long text/reasoning repetition loops are terminated.
    #[serde(default = "default_stream_loop_guard")]
    pub stream_loop_guard: bool,
    /// Upstream routing metadata.
    pub providers: Vec<ProviderConfigSnapshot>,
    /// Named route chains.
    pub groups: Vec<GroupConfigSnapshot>,
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
    /// Per-attempt streaming first-content timeout, including response headers.
    /// Zero disables it. Metadata, comments and empty deltas do not satisfy it.
    #[serde(default = "default_stream_first_content_timeout_ms")]
    pub stream_first_content_timeout_ms: u64,
    /// Maximum wait for the next upstream body chunk while streaming.
    /// Zero disables it; downstream backpressure is not upstream read-idle time.
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    /// Interval between downstream SSE comments (except Gemini/nonstream).
    /// Zero disables keepalive without changing upstream timeout policies.
    #[serde(default = "default_stream_keepalive_interval_ms")]
    pub stream_keepalive_interval_ms: u64,
    /// Maximum upstream silence during which to send downstream comments.
    /// Zero disables keepalive; transport activity or a new attempt restarts it.
    #[serde(default = "default_stream_keepalive_max_silence_ms")]
    pub stream_keepalive_max_silence_ms: u64,
    /// Request-wide hold budget for Responses opening events, across attempts.
    /// Zero disables it. Releasing actual metadata commits the current attempt.
    #[serde(default = "default_stream_responses_hold_timeout_ms")]
    pub stream_responses_hold_timeout_ms: u64,
    /// Stop streamed text/reasoning that is stuck in a long repetition loop.
    /// Enabled by default; tool arguments and nonstreaming replies are excluded.
    #[serde(default = "default_stream_loop_guard")]
    pub stream_loop_guard: bool,
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

fn default_stream_first_content_timeout_ms() -> u64 {
    600_000
}

fn default_stream_idle_timeout_ms() -> u64 {
    300_000
}

fn default_stream_keepalive_interval_ms() -> u64 {
    15_000
}

fn default_stream_keepalive_max_silence_ms() -> u64 {
    300_000
}

fn default_stream_responses_hold_timeout_ms() -> u64 {
    240_000
}

fn default_stream_loop_guard() -> bool {
    true
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
            stream_first_content_timeout_ms: default_stream_first_content_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            stream_keepalive_interval_ms: default_stream_keepalive_interval_ms(),
            stream_keepalive_max_silence_ms: default_stream_keepalive_max_silence_ms(),
            stream_responses_hold_timeout_ms: default_stream_responses_hold_timeout_ms(),
            stream_loop_guard: default_stream_loop_guard(),
            history_path: None,
            provider: Vec::new(),
            group: Vec::new(),
        }
    }
}

impl Config {
    /// Add or replace a provider after checking routes against the next config.
    pub fn set_provider(
        &mut self,
        provider: ProviderConfig,
        make_default: bool,
    ) -> anyhow::Result<()> {
        provider.validate()?;
        let mut next = self.clone();
        let id = provider.id.clone();
        match next.provider.iter().position(|p| p.id == id) {
            Some(index) => next.provider[index] = provider,
            None => next.provider.push(provider),
        }
        for route in &next.provider(&id).expect("just inserted").fallback {
            if route.is_empty()
                || route.chars().any(char::is_whitespace)
                || next.resolve(route).is_none()
                || (!route.contains('/') && !next.provider.iter().any(|p| p.models.contains(route)))
            {
                anyhow::bail!(
                    "fallback must name an existing provider/model, group/id, or known bare model"
                );
            }
            if let Some((_, model)) = route.split_once('/')
                && model.is_empty()
            {
                anyhow::bail!("fallback must include a model or group id");
            }
        }
        if make_default || next.default_provider.is_empty() {
            next.default_provider = id;
        }
        *self = next;
        Ok(())
    }

    /// Remove a provider only if no other provider or group still uses it.
    pub fn remove_provider(&mut self, id: &str) -> anyhow::Result<()> {
        let index = self
            .provider
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| anyhow::anyhow!("provider not found"))?;
        if self.group.iter().any(|g| {
            g.routes
                .iter()
                .any(|r| r.split_once('/').is_some_and(|(p, _)| p == id))
        }) {
            anyhow::bail!("provider is referenced by a routing group");
        }
        if self.provider.iter().filter(|p| p.id != id).any(|p| {
            p.fallback.iter().any(|r| {
                !r.starts_with("group/")
                    && self.resolve(r).is_some_and(|(target, _)| target.id == id)
            })
        }) {
            anyhow::bail!("provider is referenced by a provider fallback");
        }
        self.provider.remove(index);
        if self.default_provider == id {
            self.default_provider = self
                .provider
                .first()
                .map(|p| p.id.clone())
                .unwrap_or_default();
        }
        Ok(())
    }

    /// Load from a path; a missing file is a fresh config, not an error.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    const PERSISTENCE_METADATA_KEY: &str = "__magpie_lite";

    /// Read the durable optimistic-concurrency revision embedded in the TOML.
    ///
    /// The metadata table is intentionally ignored by `Config` deserialization,
    /// so older configuration files and hand-edited files remain compatible.
    pub fn persisted_revision(path: &std::path::Path) -> anyhow::Result<u64> {
        if !path.exists() {
            return Ok(1);
        }
        let text = fs::read_to_string(path)?;
        let document: toml::Value = toml::from_str(&text)?;
        let revision = document
            .get(Self::PERSISTENCE_METADATA_KEY)
            .and_then(|value| value.get("revision"))
            .and_then(toml::Value::as_integer)
            .map(|value| value.max(1) as u64)
            .unwrap_or(1);
        Ok(revision)
    }

    /// Save back to the same path, preserving the durable revision if present.
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let revision = Self::persisted_revision(path).unwrap_or(1);
        self.save_with_revision(path, revision)
    }

    /// Save an atomically replaced configuration and its durable revision.
    ///
    /// Keeping the revision in the same TOML document avoids the two-file
    /// failure window that a sidecar would introduce during a crash.
    pub fn save_with_revision(&self, path: &std::path::Path, revision: u64) -> anyhow::Result<()> {
        use std::io::Write;

        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        fs::create_dir_all(parent)?;
        let mut text = toml::to_string_pretty(self)?;
        text.push_str("\n[");
        text.push_str(Self::PERSISTENCE_METADATA_KEY);
        text.push_str("]\nrevision = ");
        text.push_str(&revision.max(1).to_string());
        text.push('\n');
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }

    /// A secret-free view for `/config` and the console.
    pub fn snapshot(&self) -> ConfigSnapshot {
        self.snapshot_with_revision(0)
    }

    /// Build a secret-free view with the store-owned optimistic-concurrency revision.
    pub fn snapshot_with_revision(&self, revision: u64) -> ConfigSnapshot {
        ConfigSnapshot {
            revision,
            default_provider: self.default_provider.clone(),
            key_cooldown_ms: self.key_cooldown_ms,
            stream_first_content_timeout_ms: self.stream_first_content_timeout_ms,
            stream_idle_timeout_ms: self.stream_idle_timeout_ms,
            stream_keepalive_interval_ms: self.stream_keepalive_interval_ms,
            stream_keepalive_max_silence_ms: self.stream_keepalive_max_silence_ms,
            stream_responses_hold_timeout_ms: self.stream_responses_hold_timeout_ms,
            stream_loop_guard: self.stream_loop_guard,
            providers: self
                .provider
                .iter()
                .map(|p| ProviderConfigSnapshot {
                    id: p.id.clone(),
                    name: p.label().to_owned(),
                    protocol: p.protocol,
                    capabilities: ProviderCapabilitiesSnapshot {
                        supported_protocols: p.capabilities.effective_protocols(p.protocol),
                        streaming: p.capabilities.streaming,
                        tools: p.capabilities.tools,
                        vision: p.capabilities.vision,
                    },
                    base_url: p.base_url.clone(),
                    models: p.models.clone(),
                    key_count: p.api_key_pool().len(),
                    credential_count: p
                        .api_key_pool()
                        .iter()
                        .filter(|key| !key.is_empty())
                        .count(),
                    key_cooldown_override_ms: p.key_cooldown_ms,
                    fallback: p.fallback.clone(),
                    key_cooldown_ms: self.key_cooldown_for(p).as_millis().min(u64::MAX as u128)
                        as u64,
                    routing: p.routing,
                    weights: p.key_weights(),
                })
                .collect(),
            groups: self
                .group
                .iter()
                .map(|g| GroupConfigSnapshot {
                    id: g.id.clone(),
                    name: g.label().to_owned(),
                    routes: g.routes.clone(),
                    routing: g.routing,
                    first_token_timeout_ms: g.first_token_timeout_ms,
                })
                .collect(),
        }
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
                // A model its vendor answered with a retirement leaves the
                // lists for that provider only, until it answers again or
                // the retirement week passes (crates/gateway/src/retired.rs).
                if crate::retired::is_retired(&p.id, m, std::time::SystemTime::now()) {
                    continue;
                }
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
    fn streaming_deadlines_default_round_trip_and_appear_in_secret_free_snapshot() {
        let mut config: Config = toml::from_str("port = 3426").unwrap();
        assert_eq!(config.stream_first_content_timeout_ms, 600_000);
        assert_eq!(config.stream_idle_timeout_ms, 300_000);
        assert_eq!(config.stream_keepalive_interval_ms, 15_000);
        assert_eq!(config.stream_keepalive_max_silence_ms, 300_000);
        assert_eq!(config.stream_responses_hold_timeout_ms, 240_000);
        config.stream_first_content_timeout_ms = 0;
        config.stream_idle_timeout_ms = 1234;
        config.stream_keepalive_interval_ms = 0;
        config.stream_keepalive_max_silence_ms = 5678;
        config.stream_responses_hold_timeout_ms = 890;
        let encoded = toml::to_string(&config).unwrap();
        let restored: Config = toml::from_str(&encoded).unwrap();
        let snapshot = restored.snapshot();
        assert_eq!(snapshot.stream_first_content_timeout_ms, 0);
        assert_eq!(snapshot.stream_idle_timeout_ms, 1234);
        assert_eq!(snapshot.stream_keepalive_interval_ms, 0);
        assert_eq!(snapshot.stream_keepalive_max_silence_ms, 5678);
        assert_eq!(snapshot.stream_responses_hold_timeout_ms, 890);
    }

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
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            key_generation: 0,
        };

        assert_eq!(provider.api_key_pool(), vec!["legacy-key".to_owned()]);
    }

    #[test]
    fn provider_capabilities_are_backward_compatible_and_round_trip() {
        let legacy: ProviderConfig = toml::from_str(
            r#"id = "legacy"
protocol = "chat"
base_url = "http://127.0.0.1:1/v1"
"#,
        )
        .unwrap();
        assert_eq!(legacy.capabilities, ProviderCapabilities::default());

        let configured = ProviderCapabilities {
            supported_protocols: vec![Protocol::Chat, Protocol::Responses],
            streaming: Some(true),
            tools: Some(false),
            vision: Some(true),
        };
        let provider = ProviderConfig {
            id: "multi".into(),
            name: String::new(),
            protocol: Protocol::Chat,
            capabilities: configured.clone(),
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: String::new(),
            api_keys: Vec::new(),
            models: Vec::new(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            key_generation: 0,
        };
        let encoded = toml::to_string(&provider).unwrap();
        assert!(encoded.contains("supported_protocols = [\"chat\", \"responses\"]"));
        assert_eq!(
            toml::from_str::<ProviderConfig>(&encoded)
                .unwrap()
                .capabilities,
            configured
        );
    }

    #[test]
    fn provider_capabilities_validate_protocol_membership_and_duplicates() {
        let mut provider = ProviderConfig {
            id: "mock".into(),
            name: String::new(),
            protocol: Protocol::Chat,
            capabilities: ProviderCapabilities {
                supported_protocols: vec![Protocol::Responses],
                ..Default::default()
            },
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: String::new(),
            api_keys: Vec::new(),
            models: Vec::new(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            key_generation: 0,
        };
        assert!(provider.validate().is_err());
        provider.capabilities.supported_protocols = vec![Protocol::Chat, Protocol::Chat];
        assert!(provider.validate().is_err());
        provider.capabilities.supported_protocols = vec![Protocol::Chat, Protocol::Responses];
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn provider_snapshot_defaults_empty_protocol_metadata_and_stays_secret_free() {
        let mut config = Config::default();
        config.provider.push(ProviderConfig {
            id: "mock".into(),
            name: String::new(),
            protocol: Protocol::Anthropic,
            capabilities: ProviderCapabilities::default(),
            base_url: "http://127.0.0.1:1".into(),
            api_key: "super-secret".into(),
            api_keys: Vec::new(),
            models: Vec::new(),
            fallback: Vec::new(),
            key_cooldown_ms: None,
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            key_generation: 0,
        });
        let snapshot = config.snapshot();
        assert_eq!(
            snapshot.providers[0].capabilities.supported_protocols,
            vec![Protocol::Anthropic]
        );
        assert_eq!(snapshot.providers[0].capabilities.streaming, None);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("super-secret"));
    }

    #[test]
    fn key_rotation_preserves_configured_slots() {
        let rotator = KeyRotator::default();
        let keys = vec!["one".to_owned(), "two".to_owned()];

        assert_eq!(
            rotator.rotate("mock", 0, &keys),
            vec![(0, "one".to_owned()), (1, "two".to_owned())]
        );
        assert_eq!(
            rotator.rotate("mock", 0, &keys),
            vec![(1, "two".to_owned()), (0, "one".to_owned())]
        );
    }

    #[test]
    fn newer_key_generation_is_not_reset_by_a_stale_request() {
        let rotator = KeyRotator::default();
        let old = vec![
            "old-one".to_owned(),
            "old-two".to_owned(),
            "old-three".to_owned(),
        ];
        let new = vec!["new-one".to_owned(), "new-two".to_owned()];
        rotator.rotate("mock", 1, &old);
        rotator.rotate("mock", 1, &old);
        rotator.cool_down("mock", 1, 0, Duration::from_secs(60));
        assert_eq!(
            rotator.rotate("mock", 2, &new)[0],
            (0, "new-one".to_owned())
        );
        rotator.cool_down("mock", 2, 0, Duration::from_secs(60));
        rotator.rotate("mock", 1, &old);
        rotator.cool_down("mock", 1, 1, Duration::from_secs(60));
        assert_eq!(
            rotator.rotate("mock", 2, &new),
            vec![(1, "new-two".to_owned())]
        );
        let mut config = cooldown_config();
        config.provider[0].key_generation = 2;
        assert_eq!(rotator.cooldown_snapshot(&config)["mock"].len(), 1);
        assert!(rotator.cooldown_snapshot(&Config::default()).is_empty());
    }

    #[test]
    fn key_cooldown_hides_a_slot_until_it_expires() {
        let rotator = KeyRotator::default();
        let keys = vec!["one".to_owned(), "two".to_owned()];
        rotator.rotate("mock", 0, &keys);
        rotator.rotate("mock", 0, &keys);
        rotator.cool_down("mock", 0, 0, Duration::from_millis(20));

        assert_eq!(
            rotator.rotate("mock", 0, &keys),
            vec![(1, "two".to_owned())]
        );

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rotator.rotate("mock", 0, &keys),
            vec![(1, "two".to_owned()), (0, "one".to_owned())]
        );
    }

    #[test]
    fn cooldown_snapshot_reports_slots_and_expires_them() {
        let rotator = KeyRotator::default();
        rotator.cool_down("mock", 0, 0, Duration::from_millis(80));

        let snapshot = rotator.cooldown_snapshot(&cooldown_config());
        let slot = &snapshot["mock"]["0"];
        assert!(slot.remaining_ms > 0 && slot.remaining_ms <= 80);
        assert!(slot.until_unix_ms >= unix_now_ms());

        std::thread::sleep(Duration::from_millis(100));
        assert!(rotator.cooldown_snapshot(&cooldown_config()).is_empty());
    }

    #[test]
    fn weighting_spreads_a_provider_pool_by_each_keys_share() {
        let rotator = KeyRotator::default();
        let keys = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let weights = [3u32, 1, 4];
        let picks = || {
            rotator
                .rotate_routed("mock", 0, RoutingPolicy::Weighted, &keys, &weights)
                .into_iter()
                .map(|(slot, _)| slot)
                .collect::<Vec<_>>()
        };

        let mut firsts = Vec::new();
        for _ in 0..8 {
            firsts.push(picks()[0]);
        }
        // Every round of 8 keeps the 3 : 1 : 4 ratio exactly, not just on
        // average, and the heaviest key never takes three in a row while a
        // lighter one is ready.
        assert_eq!(firsts.iter().filter(|slot| **slot == 0).count(), 3);
        assert_eq!(firsts.iter().filter(|slot| **slot == 1).count(), 1);
        assert_eq!(firsts.iter().filter(|slot| **slot == 2).count(), 4);
        assert!(
            !firsts.windows(3).any(|window| window == [2, 2, 2]),
            "the heavy key ran three times: {firsts:?}"
        );
    }

    #[test]
    fn a_cooling_key_is_hidden_and_the_rest_share_its_weight() {
        let rotator = KeyRotator::default();
        let keys = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let weights = [3u32, 1, 4];
        rotator.cool_down("mock", 0, 2, Duration::from_secs(60));

        let mut firsts = Vec::new();
        for _ in 0..4 {
            let order =
                rotator.rotate_routed("mock", 0, RoutingPolicy::Weighted, &keys, &weights);
            // The cooling key is left out entirely while others are healthy,
            // matching how the pool already hides a key in cooldown.
            assert!(order.iter().all(|(slot, _)| *slot != 2));
            firsts.push(order[0].0);
        }
        assert_eq!(firsts.iter().filter(|slot| **slot == 0).count(), 3);
        assert_eq!(firsts.iter().filter(|slot| **slot == 1).count(), 1);
    }

    #[test]
    fn least_used_puts_the_lightest_key_first_and_a_wake_clears_its_rest() {
        let rotator = KeyRotator::default();
        let keys = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let order = || {
            rotator
                .rotate_routed("mock", 0, RoutingPolicy::Usage, &keys, &[])
                .into_iter()
                .map(|(slot, _)| slot)
                .collect::<Vec<_>>()
        };

        rotator.record_served("mock", 0, 0, 5000);
        rotator.record_served("mock", 0, 2, 10);
        assert_eq!(order(), vec![1, 2, 0]);

        // A key that answered with no reported usage still counts as serving
        // one request, so it keeps its place ahead of a key that never ran.
        rotator.cool_down("mock", 0, 1, Duration::from_secs(60));
        rotator.record_served("mock", 0, 1, 0);
        assert_eq!(order(), vec![1, 2, 0]);
    }

    #[test]
    fn provider_routing_policy_parses_weight_and_roundtrips_serde() {
        assert_eq!("weight".parse(), Ok(RoutingPolicy::Weighted));
        assert_eq!("weighted".parse(), Ok(RoutingPolicy::Weighted));
        assert_eq!(RoutingPolicy::Weighted.as_str(), "weight");
        assert_eq!(
            serde_json::to_string(&RoutingPolicy::Weighted).unwrap(),
            "\"weight\""
        );
        assert_eq!(
            serde_json::from_str::<RoutingPolicy>("\"weight\"").unwrap(),
            RoutingPolicy::Weighted
        );
        assert!("pace".parse::<RoutingPolicy>().is_err());
    }

    #[test]
    fn provider_weights_are_bounded_and_parallel_to_the_pool() {
        let mut provider = cooldown_config().provider.remove(0);
        provider.api_keys = vec!["one".to_owned(), "two".to_owned()];
        // A missing tail weighs nothing here and is treated as 1 by callers.
        provider.weights = vec![3];
        assert!(provider.validate().is_ok());
        assert_eq!(provider.key_weights(), vec![3, 0]);

        provider.weights = vec![0, MAX_KEY_WEIGHT + 1];
        assert!(provider.validate().is_err());

        provider.weights = vec![1, 1, 1];
        assert!(provider.validate().is_err());
    }

    fn cooldown_config() -> Config {
        Config {
            provider: vec![ProviderConfig {
                id: "mock".into(),
                name: String::new(),
                protocol: Protocol::Chat,
                base_url: "http://localhost/v1".into(),
                api_key: String::new(),
                api_keys: vec![],
                models: vec![],
                fallback: vec![],
                key_cooldown_ms: None,
                capabilities: Default::default(),
                routing: RoutingPolicy::Rotate,
                weights: Vec::new(),
                key_generation: 0,
            }],
            ..Default::default()
        }
    }

    fn unix_now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn config_snapshot_is_route_metadata_without_secrets() {
        let mut config = Config {
            default_provider: "mock".into(),
            key_cooldown_ms: 60_000,
            ..Config::default()
        };
        config.provider.push(ProviderConfig {
            id: "mock".into(),
            name: "Mock Provider".into(),
            protocol: Protocol::Chat,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: String::new(),
            api_keys: vec!["secret-one".into(), "secret-two".into()],
            models: vec!["mock-model".into()],
            fallback: vec!["backup/other".into()],
            key_cooldown_ms: Some(1_200),
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            key_generation: 0,
        });
        config.group.push(GroupConfig {
            id: "daily".into(),
            name: "Daily coding".into(),
            routes: vec!["mock/mock-model".into(), "backup/other".into()],
            routing: RoutingPolicy::default(),
            first_token_timeout_ms: 0,
        });

        let snapshot = config.snapshot();
        assert_eq!(snapshot.default_provider, "mock");
        assert_eq!(snapshot.providers.len(), 1);
        let provider = &snapshot.providers[0];
        assert_eq!(provider.id, "mock");
        assert_eq!(provider.name, "Mock Provider");
        assert_eq!(provider.protocol, Protocol::Chat);
        assert_eq!(provider.key_count, 2);
        assert_eq!(provider.key_cooldown_ms, 1_200);
        assert_eq!(snapshot.groups[0].id, "daily");
        assert_eq!(snapshot.groups[0].routes.len(), 2);

        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("secret-one"));
        assert!(!json.contains("secret-two"));
        assert!(!json.contains("magpie-lite"));
    }
    #[test]
    fn group_validation_allows_distinct_models_from_one_provider() {
        let group = GroupConfig {
            id: "same-provider".into(),
            name: String::new(),
            routes: vec!["mock/model-a".into(), "mock/model-b".into()],
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
        };

        assert!(group.validate().is_ok());
    }

    #[test]
    fn group_routing_policy_parses_aliases_and_defaults_to_order() {
        assert_eq!(RoutingPolicy::default(), RoutingPolicy::Order);
        assert_eq!("round_robin".parse(), Ok(RoutingPolicy::Rotate));
        assert_eq!("least_used".parse(), Ok(RoutingPolicy::Usage));
        assert_eq!(RoutingPolicy::Rotate.as_str(), "rotate");
        let group: GroupConfig = toml::from_str(
            r#"id = "daily"
routes = ["mock/model"]
"#,
        )
        .unwrap();
        assert_eq!(group.routing, RoutingPolicy::Order);
        assert_eq!(group.first_token_timeout_ms, 0);
        let mut configured = group;
        configured.first_token_timeout_ms = 1234;
        let encoded = toml::to_string(&configured).unwrap();
        let decoded: GroupConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.first_token_timeout_ms, 1234);
    }

    #[test]
    fn group_route_scheduler_rotates_and_tracks_usage() {
        let scheduler = KeyRotator::default();
        let routes = vec!["a/one".to_owned(), "b/two".to_owned(), "c/three".to_owned()];
        assert_eq!(
            scheduler.group_order("daily", RoutingPolicy::Rotate, &routes),
            vec![
                (0, "a/one".to_owned()),
                (1, "b/two".to_owned()),
                (2, "c/three".to_owned())
            ]
        );
        assert_eq!(
            scheduler.group_order("daily", RoutingPolicy::Rotate, &routes)[0],
            (1, "b/two".to_owned())
        );
        assert_eq!(
            scheduler.group_order("daily", RoutingPolicy::Rotate, &routes)[0],
            (2, "c/three".to_owned())
        );

        let usage = KeyRotator::default();
        assert_eq!(
            usage.group_order("daily", RoutingPolicy::Usage, &routes)[0].0,
            0
        );
        usage.record_group_attempt("daily", RoutingPolicy::Usage, &routes, 0);
        assert_eq!(
            usage.group_order("daily", RoutingPolicy::Usage, &routes)[0].0,
            1
        );
        usage.record_group_attempt("daily", RoutingPolicy::Usage, &routes, 1);
        assert_eq!(
            usage.group_order("daily", RoutingPolicy::Usage, &routes)[0].0,
            2
        );

        let changed = vec!["new/route".to_owned(), "other/route".to_owned()];
        assert_eq!(
            usage.group_order("daily", RoutingPolicy::Usage, &changed)[0],
            (0, "new/route".to_owned())
        );
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
            capabilities: Default::default(),
            routing: RoutingPolicy::Rotate,
            weights: Vec::new(),
            key_generation: 0,
        };

        assert_eq!(config.key_cooldown_for(&provider), Duration::from_secs(60));
        provider.key_cooldown_ms = Some(0);
        assert_eq!(config.key_cooldown_for(&provider), Duration::ZERO);
    }
}

#[cfg(test)]
mod loop_guard_config_tests {
    use super::*;

    #[test]
    fn loop_guard_defaults_enabled_and_roundtrips_explicit_disable_and_snapshot() {
        let legacy: Config = toml::from_str("").unwrap();
        assert!(legacy.stream_loop_guard);
        assert!(legacy.snapshot().stream_loop_guard);
        assert!(Config::default().stream_loop_guard);
        let disabled: Config = toml::from_str("stream_loop_guard = false").unwrap();
        assert!(!disabled.stream_loop_guard);
        assert!(!disabled.snapshot().stream_loop_guard);
        let encoded = toml::to_string(&disabled).unwrap();
        assert!(encoded.contains("stream_loop_guard = false"));
        assert!(
            !toml::from_str::<Config>(&encoded)
                .unwrap()
                .stream_loop_guard
        );
        assert!(toml::from_str::<Config>("stream_loop_guard = 0").is_err());
        assert!(toml::from_str::<Config>("stream_loop_guard = \"false\"").is_err());
    }
}
