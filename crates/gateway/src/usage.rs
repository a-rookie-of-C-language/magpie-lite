//! Request accounting: enough history to see what the gateway has been doing,
//! without persisting conversation content or credentials.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use magpie_core::ir::{CompletionRequest, Usage};
use serde::{Deserialize, Serialize};

/// How many recent request records are kept for `/stats`.
const RECENT_LIMIT: usize = 1_000;

/// Token accounting as reported by the provider that served a request.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Per-key totals inside a provider. Slot names are stable, secret-free ids.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct KeyStats {
    /// Upstream attempts made with this key, including fallback attempts.
    pub attempts: u64,
    /// Attempts accepted for handoff (after opening prefetch for SSE). A later
    /// streaming error still makes the final request record unsuccessful.
    pub successes: u64,
    /// Attempts that were rejected, failed, or could not connect.
    pub errors: u64,
}

/// Request and token totals over an arbitrary history window.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecentUsageStats {
    /// Completed or failed records in the window.
    pub requests: u64,
    /// Records whose final status was not `ok`.
    pub errors: u64,
    /// Records for which the provider reported at least one usage field.
    pub requests_with_usage: u64,
    /// Summed reported input tokens.
    pub input_tokens: u64,
    /// Summed reported output tokens.
    pub output_tokens: u64,
    /// Summed reported prompt-cache read tokens.
    pub cache_read_tokens: u64,
    /// Summed reported prompt-cache write tokens.
    pub cache_write_tokens: u64,
    /// Summed gateway latency.
    pub total_latency_ms: u64,
    /// Mean gateway latency, rounded down.
    pub average_latency_ms: Option<u64>,
}

impl RecentUsageStats {
    fn absorb(&mut self, record: &RequestRecord) {
        self.requests = self.requests.saturating_add(1);
        if record.status != "ok" {
            self.errors = self.errors.saturating_add(1);
        }
        if record.usage.input.is_some()
            || record.usage.output.is_some()
            || record.usage.cache_read.is_some()
            || record.usage.cache_write.is_some()
        {
            self.requests_with_usage = self.requests_with_usage.saturating_add(1);
        }
        self.input_tokens = self
            .input_tokens
            .saturating_add(record.usage.input.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.usage.output.unwrap_or(0));
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(record.usage.cache_read.unwrap_or(0));
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(record.usage.cache_write.unwrap_or(0));
        self.total_latency_ms = self.total_latency_ms.saturating_add(record.latency_ms);
    }

    fn finish(&mut self) {
        self.average_latency_ms = self.total_latency_ms.checked_div(self.requests);
    }
}

/// Aggregate usage over the recent-history window loaded in this process.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecentUsageSummary {
    /// Totals for the whole window.
    pub totals: RecentUsageStats,
    /// Totals by provider id.
    pub by_provider: BTreeMap<String, RecentUsageStats>,
    /// Totals by the model name the agent selected.
    pub by_model: BTreeMap<String, RecentUsageStats>,
}

/// Per-provider totals. Missing usage fields count as zero.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProviderStats {
    /// Requests routed to this provider.
    pub requests: u64,
    /// Requests that did not finish with status `ok`.
    pub errors: u64,
    /// Summed reported input tokens.
    pub input_tokens: u64,
    /// Summed reported output tokens.
    pub output_tokens: u64,
    /// Attempt totals by zero-based key slot.
    pub keys: BTreeMap<String, KeyStats>,
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
    /// Active key cooldowns by provider id and stable slot number.
    pub key_cooldowns: crate::config::KeyCooldownSnapshotMap,
    /// Usage totals over the recent-history window.
    pub recent_usage: RecentUsageSummary,
    /// The most recent records, oldest first.
    pub recent_requests: Vec<RequestRecord>,
}

/// Request totals and recent history for one gateway process.
///
/// When a history file is configured, completed records are appended as JSONL
/// and the most recent bounded window is loaded on startup. Aggregate totals
/// intentionally remain process-local: reloading only the recent window cannot
/// reconstruct trustworthy lifetime totals or every historical key attempt.
#[derive(Debug, Default)]
pub struct RequestLog {
    total_requests: u64,
    errors: u64,
    providers: BTreeMap<String, ProviderStats>,
    recent_requests: Vec<RequestRecord>,
    sink: Option<File>,
}

impl RequestLog {
    /// Open (and create when needed) a JSONL history file.
    ///
    /// Invalid or truncated lines are ignored so a crash during an append or a
    /// manually damaged file cannot make the gateway refuse to start.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let existing = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let file = OpenOptions::new().append(true).create(true).open(path)?;

        let mut log = Self {
            sink: Some(file),
            ..Self::default()
        };
        for line in existing.lines() {
            let Ok(record) = serde_json::from_str::<RequestRecord>(line) else {
                continue;
            };
            log.recent_requests.push(record);
            if log.recent_requests.len() > RECENT_LIMIT {
                let overflow = log.recent_requests.len() - RECENT_LIMIT;
                log.recent_requests.drain(0..overflow);
            }
        }
        Ok(log)
    }

    /// Record one concrete upstream key attempt. Finished request totals are
    /// counted separately by [`RequestLog::record`].
    pub fn record_attempt(&mut self, provider: &str, key_slot: usize, status: &str) {
        let stats = self.providers.entry(provider.to_owned()).or_default();
        let key = stats.keys.entry(key_slot.to_string()).or_default();
        key.attempts = key.attempts.saturating_add(1);
        if status == "ok" {
            key.successes = key.successes.saturating_add(1);
        } else {
            key.errors = key.errors.saturating_add(1);
        }
    }

    /// Add one finished record and update every aggregate.
    pub fn record(&mut self, record: RequestRecord) {
        self.persist(&record);
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
        self.snapshot_filtered(None, None)
    }

    /// Return a consistent copy with the recent-history portion bounded by a
    /// Unix-millisecond range. `from` is inclusive and `to` is exclusive;
    /// process lifetime totals and key state remain unfiltered.
    pub fn snapshot_filtered(&self, from: Option<u64>, to: Option<u64>) -> StatsSnapshot {
        let recent_requests = self
            .recent_requests
            .iter()
            .filter(|record| {
                from.is_none_or(|from| record.started_unix_ms >= from)
                    && to.is_none_or(|to| record.started_unix_ms < to)
            })
            .cloned()
            .collect::<Vec<_>>();
        StatsSnapshot {
            total_requests: self.total_requests,
            errors: self.errors,
            providers: self.providers.clone(),
            key_cooldowns: BTreeMap::new(),
            recent_usage: summarize_recent(&recent_requests),
            recent_requests,
        }
    }

    /// Append one sanitized record. Statistics still update if the disk write
    /// fails; history is useful, but never more important than serving.
    fn persist(&mut self, record: &RequestRecord) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        let result: anyhow::Result<()> = (|| {
            let line = serde_json::to_string(record)?;
            writeln!(sink, "{line}")?;
            sink.flush()?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!("could not append request history: {error}");
        }
    }
}

fn summarize_recent(records: &[RequestRecord]) -> RecentUsageSummary {
    let mut summary = RecentUsageSummary::default();
    for record in records {
        summary.totals.absorb(record);
        summary
            .by_provider
            .entry(record.provider.clone())
            .or_default()
            .absorb(record);
        summary
            .by_model
            .entry(record.requested_model.clone())
            .or_default()
            .absorb(record);
    }
    summary.totals.finish();
    for stats in summary.by_provider.values_mut() {
        stats.finish();
    }
    for stats in summary.by_model.values_mut() {
        stats.finish();
    }
    summary
}

/// Tracks one request from routing until its reply or stream finishes.
pub struct RequestTracker {
    log: Arc<Mutex<RequestLog>>,
    started_at: Instant,
    record: RequestRecord,
    usage: Usage,
    reask_usage: Usage,
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
            usage: Usage::default(),
            reask_usage: Usage::default(),
            finished: false,
        }
    }

    /// Record a concrete upstream attempt without waiting for the full reply.
    pub fn record_attempt(&self, provider: &str, key_slot: usize, status: &str) {
        lock(&self.log).record_attempt(provider, key_slot, status);
    }

    /// Replace the route after fallback chooses a different provider.
    pub fn set_route(&mut self, provider: &str, upstream_model: &str, key_slot: usize) {
        self.record.provider = provider.to_owned();
        self.record.upstream_model = upstream_model.to_owned();
        self.record.key_slot = key_slot;
    }

    /// Switch accounting before awaiting a concrete attempt, so cancellation
    /// records the actual route and never combines different attempts' usage.
    pub fn begin_attempt(&mut self, provider: &str, upstream_model: &str, key_slot: usize) {
        self.set_route(provider, upstream_model, key_slot);
        self.usage = Usage::default();
        self.reask_usage = Usage::default();
    }

    /// Begin a same-candidate reask without mixing different routes or keys.
    pub fn begin_reask(&mut self) {
        self.reask_usage = add_usage(&self.reask_usage, &self.usage);
        self.usage = Usage::default();
    }

    /// Usage across the initial attempt and its same-candidate reasks.
    pub fn total_usage(&self) -> Usage {
        add_usage(&self.reask_usage, &self.usage)
    }

    /// Keep observed usage even if the response body is dropped mid-stream.
    pub fn observe_usage(&mut self, usage: &Usage) {
        self.usage.merge(usage);
    }

    /// Whether the request already recorded its final status. A reply replayed
    /// from a whole upstream answer finishes before its stream is drained, so
    /// its terminal frame must not account for the same reply twice.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Record the final status. Later calls (including Drop) are no-ops.
    pub fn finish(&mut self, status: &str, usage: &Usage) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.record.status = status.to_owned();
        self.record.latency_ms = elapsed_ms(self.started_at.elapsed());
        self.usage.merge(usage);
        let total = self.total_usage();
        self.record.usage = UsageSnapshot::from(&total);
        let mut log = lock(&self.log);
        log.record(self.record.clone());
    }
}

impl Drop for RequestTracker {
    fn drop(&mut self) {
        self.finish("client_disconnected", &Usage::default());
    }
}

fn add_counter(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn add_usage(left: &Usage, right: &Usage) -> Usage {
    Usage {
        input: add_counter(left.input, right.input),
        output: add_counter(left.output, right.output),
        cache_read: add_counter(left.cache_read, right.cache_read),
        cache_write: add_counter(left.cache_write, right.cache_write),
        reasoning: add_counter(left.reasoning, right.reasoning),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn history_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        std::env::temp_dir().join(format!(
            "magpie-lite-{label}-{}-{unique}.jsonl",
            std::process::id()
        ))
    }

    fn record(index: u64, status: &str) -> RequestRecord {
        RequestRecord {
            started_unix_ms: index,
            client_protocol: "chat".into(),
            requested_model: format!("mock/model-{index}"),
            provider: "mock".into(),
            upstream_model: format!("model-{index}"),
            key_slot: index as usize,
            stream: index.is_multiple_of(2),
            status: status.into(),
            latency_ms: index,
            usage: UsageSnapshot {
                input: Some(index),
                output: Some(index + 1),
                cache_read: None,
                cache_write: None,
            },
        }
    }

    #[test]
    fn recent_usage_summarizes_the_loaded_window() {
        let mut log = RequestLog::default();
        log.record(record(1, "ok"));
        log.record(record(2, "upstream_error"));
        let mut fallback = record(20, "ok");
        fallback.provider = "backup".into();
        fallback.requested_model = "group/daily-coding".into();
        fallback.usage.cache_read = Some(5);
        log.record(fallback);

        let summary = log.snapshot().recent_usage;
        assert_eq!(summary.totals.requests, 3);
        assert_eq!(summary.totals.errors, 1);
        assert_eq!(summary.totals.requests_with_usage, 3);
        assert_eq!(summary.totals.input_tokens, 23);
        assert_eq!(summary.totals.output_tokens, 26);
        assert_eq!(summary.totals.cache_read_tokens, 5);
        assert_eq!(summary.totals.total_latency_ms, 23);
        assert_eq!(summary.totals.average_latency_ms, Some(7));

        let mock = &summary.by_provider["mock"];
        assert_eq!(mock.requests, 2);
        assert_eq!(mock.errors, 1);
        assert_eq!(mock.input_tokens, 3);
        assert_eq!(mock.output_tokens, 5);

        let backup = &summary.by_provider["backup"];
        assert_eq!(backup.requests, 1);
        assert_eq!(backup.input_tokens, 20);
        assert_eq!(backup.cache_read_tokens, 5);
        assert_eq!(summary.by_model["group/daily-coding"].requests, 1);
    }
    #[test]
    fn history_round_trips_recent_metadata_only() {
        let path = history_path("round-trip");
        let mut log = RequestLog::open(&path).unwrap();
        log.record(record(7, "ok"));
        drop(log);

        let reloaded = RequestLog::open(&path).unwrap();
        let snapshot = reloaded.snapshot();
        assert_eq!(snapshot.total_requests, 0);
        assert!(snapshot.providers.is_empty());
        assert_eq!(snapshot.recent_requests.len(), 1);
        assert_eq!(snapshot.recent_requests[0].requested_model, "mock/model-7");

        let text = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "started_unix_ms",
                "client_protocol",
                "requested_model",
                "provider",
                "upstream_model",
                "key_slot",
                "stream",
                "status",
                "latency_ms",
                "usage"
            ]
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_history_lines_are_ignored_and_appending_continues() {
        let path = history_path("invalid");
        fs::write(&path, "{not json\n").unwrap();

        let mut log = RequestLog::open(&path).unwrap();
        assert!(log.snapshot().recent_requests.is_empty());
        log.record(record(1, "upstream_error"));
        drop(log);

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{not json");
        let reloaded = RequestLog::open(&path).unwrap();
        assert_eq!(reloaded.snapshot().recent_requests.len(), 1);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn history_keeps_only_the_recent_window() {
        let path = history_path("window");
        let mut log = RequestLog::open(&path).unwrap();
        for index in 0..(RECENT_LIMIT as u64 + 2) {
            log.record(record(index, "ok"));
        }
        drop(log);

        let reloaded = RequestLog::open(&path).unwrap();
        let snapshot = reloaded.snapshot();
        assert_eq!(snapshot.recent_requests.len(), RECENT_LIMIT);
        assert_eq!(snapshot.recent_requests[0].requested_model, "mock/model-2");
        assert_eq!(
            snapshot.recent_requests.last().unwrap().requested_model,
            format!("mock/model-{}", RECENT_LIMIT + 1)
        );
        fs::remove_file(&path).ok();
    }
    #[test]
    fn filtered_snapshot_uses_inclusive_from_and_exclusive_to() {
        let mut log = RequestLog::default();
        log.record(record(10, "ok"));
        log.record(record(20, "upstream_error"));
        log.record(record(30, "ok"));

        let snapshot = log.snapshot_filtered(Some(10), Some(30));
        assert_eq!(snapshot.total_requests, 3);
        assert_eq!(snapshot.recent_requests.len(), 2);
        assert_eq!(snapshot.recent_requests[0].started_unix_ms, 10);
        assert_eq!(snapshot.recent_requests[1].started_unix_ms, 20);
        assert_eq!(snapshot.recent_usage.totals.requests, 2);
        assert_eq!(snapshot.recent_usage.totals.errors, 1);
        assert_eq!(snapshot.recent_usage.totals.input_tokens, 30);
        assert_eq!(snapshot.recent_usage.totals.output_tokens, 32);
    }
    fn tracker(log: Arc<Mutex<RequestLog>>) -> RequestTracker {
        RequestTracker::new(
            log,
            "chat",
            &CompletionRequest {
                model: "mock/requested".into(),
                stream: true,
                ..CompletionRequest::default()
            },
            "mock",
            "model",
            0,
        )
    }

    #[test]
    fn usage_merge_keeps_known_fields_and_does_not_add_cumulative_frames() {
        let log = Arc::new(Mutex::new(RequestLog::default()));
        let mut tracker = tracker(log.clone());
        tracker.observe_usage(&Usage {
            input: Some(10),
            output: Some(3),
            cache_read: None,
            cache_write: None,
            reasoning: None,
        });
        tracker.observe_usage(&Usage {
            input: None,
            output: Some(7),
            cache_read: Some(2),
            cache_write: None,
            reasoning: None,
        });
        tracker.finish("ok", &Usage::default());

        let record = &log.lock().unwrap().snapshot().recent_requests[0];
        assert_eq!(record.usage.input, Some(10));
        assert_eq!(record.usage.output, Some(7));
        assert_eq!(record.usage.cache_read, Some(2));
    }

    #[test]
    fn reask_usage_adds_attempts_but_preserves_unknown_fields() {
        let log = Arc::new(Mutex::new(RequestLog::default()));
        let mut tracker = tracker(log.clone());
        tracker.observe_usage(&Usage {
            input: Some(10),
            output: Some(3),
            cache_read: Some(4),
            cache_write: None,
            reasoning: None,
        });
        tracker.begin_reask();
        tracker.observe_usage(&Usage {
            input: None,
            output: Some(7),
            cache_read: None,
            cache_write: Some(5),
            reasoning: None,
        });
        tracker.finish("ok", &Usage::default());

        let record = &log.lock().unwrap().snapshot().recent_requests[0];
        assert_eq!(record.usage.input, Some(10));
        assert_eq!(record.usage.output, Some(10));
        assert_eq!(record.usage.cache_read, Some(4));
        assert_eq!(record.usage.cache_write, Some(5));
    }

    #[test]
    fn fallback_attempt_reset_discards_previous_attempt_usage() {
        let log = Arc::new(Mutex::new(RequestLog::default()));
        let mut tracker = tracker(log.clone());
        tracker.observe_usage(&Usage {
            input: Some(10),
            output: Some(3),
            cache_read: Some(4),
            cache_write: Some(5),
            reasoning: None,
        });
        tracker.begin_attempt("backup", "backup-model", 1);
        tracker.observe_usage(&Usage {
            input: Some(20),
            output: Some(6),
            cache_read: None,
            cache_write: None,
            reasoning: None,
        });
        tracker.finish("ok", &Usage::default());

        let record = &log.lock().unwrap().snapshot().recent_requests[0];
        assert_eq!(record.provider, "backup");
        assert_eq!(record.usage.input, Some(20));
        assert_eq!(record.usage.output, Some(6));
        assert_eq!(record.usage.cache_read, None);
        assert_eq!(record.usage.cache_write, None);
    }

    #[test]
    fn usage_addition_saturates_and_drop_records_once() {
        let max = u64::MAX;
        assert_eq!(add_counter(Some(max), Some(1)), Some(max));
        assert_eq!(
            add_usage(
                &Usage {
                    input: Some(max),
                    ..Usage::default()
                },
                &Usage {
                    input: Some(1),
                    ..Usage::default()
                }
            )
            .input,
            Some(max)
        );

        let log = Arc::new(Mutex::new(RequestLog::default()));
        {
            let mut tracker = tracker(log.clone());
            tracker.observe_usage(&Usage {
                input: Some(1),
                output: None,
                cache_read: None,
                cache_write: None,
                reasoning: None,
            });
            tracker.finish("ok", &Usage::default());
            tracker.finish("upstream_error", &Usage::default());
        }
        let snapshot = log.lock().unwrap().snapshot();
        assert_eq!(snapshot.total_requests, 1);
        assert_eq!(snapshot.recent_requests[0].status, "ok");
        assert_eq!(snapshot.recent_requests[0].usage.input, Some(1));
    }
}
