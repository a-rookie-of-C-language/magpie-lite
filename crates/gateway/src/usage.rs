//! In-memory request accounting: enough history to see what the gateway has
//! been doing, without persisting conversation content or credentials.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use magpie_core::ir::{CompletionRequest, Usage};
use serde::Serialize;

/// How many recent request records are kept for `/stats`.
const RECENT_LIMIT: usize = 1_000;

/// Token accounting as reported by the provider that served a request.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UsageSnapshot {
    /// Input (prompt) tokens, when reported.
    pub input: Option<u64>,
    /// Output (completion) tokens, when reported.
    pub output: Option<u64>,
    /// Tokens read from a prompt cache, when reported.
    pub cache_read: Option<u64>,
    /// Tokens written to a prompt cache, when reported.
    pub cache_write: Option<u64>,
}

impl From<&Usage> for UsageSnapshot {
    fn from(value: &Usage) -> Self {
        Self {
            input: value.input,
            output: value.output,
            cache_read: value.cache_read,
            cache_write: value.cache_write,
        }
    }
}

/// One completed (or failed) model request. No prompt or reply text is kept.
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecord {
    /// Unix time in milliseconds when the gateway received the request.
    pub started_unix_ms: u64,
    /// The protocol the agent used.
    pub client_protocol: String,
    /// The model as the agent named it.
    pub requested_model: String,
    /// The provider id that served (or last tried) the request.
    pub provider: String,
    /// The provider's spelling of that model.
    pub upstream_model: String,
    /// Zero-based position in the provider's API-key pool. The key itself is
    /// deliberately not recorded.
    pub key_slot: usize,
    /// Whether the agent asked for SSE.
    pub stream: bool,
    /// `ok`, or a short failure kind.
    pub status: String,
    /// Gateway processing time.
    pub latency_ms: u64,
    /// Provider usage, when reported.
    pub usage: UsageSnapshot,
}

/// Per-provider totals. Missing usage fields count as zero.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ProviderStats {
    /// Requests routed to this provider.
    pub requests: u64,
    /// Requests that did not finish with status `ok`.
    pub errors: u64,
    /// Summed reported input tokens.
    pub input_tokens: u64,
    /// Summed reported output tokens.
    pub output_tokens: u64,
}

/// The complete `/stats` response.
#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    /// Requests routed by this gateway process.
    pub total_requests: u64,
    /// Requests whose final status was not `ok`.
    pub errors: u64,
    /// Totals by provider id.
    pub providers: BTreeMap<String, ProviderStats>,
    /// The most recent records, oldest first.
    pub recent_requests: Vec<RequestRecord>,
}

/// Shared, in-memory request history for one gateway process.
#[derive(Debug, Default)]
pub struct RequestLog {
    total_requests: u64,
    errors: u64,
    providers: BTreeMap<String, ProviderStats>,
    recent_requests: Vec<RequestRecord>,
}

impl RequestLog {
    /// Add one finished record and update every aggregate.
    pub fn record(&mut self, record: RequestRecord) {
        self.total_requests = self.total_requests.saturating_add(1);
        let ok = record.status == "ok";
        if !ok {
            self.errors = self.errors.saturating_add(1);
        }

        let stats = self.providers.entry(record.provider.clone()).or_default();
        stats.requests = stats.requests.saturating_add(1);
        if !ok {
            stats.errors = stats.errors.saturating_add(1);
        }
        stats.input_tokens = stats
            .input_tokens
            .saturating_add(record.usage.input.unwrap_or(0));
        stats.output_tokens = stats
            .output_tokens
            .saturating_add(record.usage.output.unwrap_or(0));

        self.recent_requests.push(record);
        if self.recent_requests.len() > RECENT_LIMIT {
            let overflow = self.recent_requests.len() - RECENT_LIMIT;
            self.recent_requests.drain(0..overflow);
        }
    }

    /// A consistent copy for JSON rendering.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            total_requests: self.total_requests,
            errors: self.errors,
            providers: self.providers.clone(),
            recent_requests: self.recent_requests.clone(),
        }
    }
}

/// Tracks one request from routing until its reply or stream finishes.
pub struct RequestTracker {
    log: Arc<Mutex<RequestLog>>,
    started_at: Instant,
    record: RequestRecord,
    finished: bool,
}

impl RequestTracker {
    /// Start tracking after the request has been decoded and routed.
    pub fn new(
        log: Arc<Mutex<RequestLog>>,
        client_protocol: &str,
        request: &CompletionRequest,
        provider: &str,
        upstream_model: &str,
        key_slot: usize,
    ) -> Self {
        Self {
            log,
            started_at: Instant::now(),
            record: RequestRecord {
                started_unix_ms: unix_ms(),
                client_protocol: client_protocol.to_owned(),
                requested_model: request.model.clone(),
                provider: provider.to_owned(),
                upstream_model: upstream_model.to_owned(),
                key_slot,
                stream: request.stream,
                status: "incomplete".to_owned(),
                latency_ms: 0,
                usage: UsageSnapshot::default(),
            },
            finished: false,
        }
    }

    /// Replace the route after fallback chooses a different provider.
    pub fn set_route(&mut self, provider: &str, upstream_model: &str, key_slot: usize) {
        self.record.provider = provider.to_owned();
        self.record.upstream_model = upstream_model.to_owned();
        self.record.key_slot = key_slot;
    }

    /// Record the final status. Later calls (including Drop) are no-ops.
    pub fn finish(&mut self, status: &str, usage: &Usage) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.record.status = status.to_owned();
        self.record.latency_ms = elapsed_ms(self.started_at.elapsed());
        self.record.usage = UsageSnapshot::from(usage);
        let mut log = lock(&self.log);
        log.record(self.record.clone());
    }
}

impl Drop for RequestTracker {
    fn drop(&mut self) {
        self.finish("client_disconnected", &Usage::default());
    }
}

fn lock(log: &Mutex<RequestLog>) -> std::sync::MutexGuard<'_, RequestLog> {
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}
