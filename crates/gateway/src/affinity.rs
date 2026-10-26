//! Conversation affinity: a conversation stays with the key that answered it,
//! so that what the vendor cached of it - the whole conversation so far, on
//! every request - is read again rather than sent afresh to someone else and
//! paid for in full.
//!
//! Ports `internal/gateway/affinity.go`. The unit kept here is a provider key
//! slot; the reference keeps a subscription account, which this gateway has
//! none of. The reference also remembers the effort a group's member is fixed
//! at; this gateway's group members have none, so that match level collapses.
//! The reference's allowance-driven reasons a conversation is let go - an
//! account resting or spent - have no counterpart here: a seat routing left
//! out is simply absent from the seats handed to [`AffinityStore::affine`].
//!
//! Who answered is kept on disk as well (`affinity.json`, next to the config),
//! so a restart - an update's included - does not hand a conversation to
//! whoever routing puts first while the vendor still has it cached at the one
//! before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use magpie_core::ir::{CompletionRequest, PartKind, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Affinity;

/// How many tokens read from the vendor's cache make a conversation worth
/// keeping where it is across turns.
pub const CACHE_WORTH: u64 = 1024;
/// How long a vendor keeps a prompt cached without it being read: five
/// minutes at Anthropic and at OpenAI, the shortest there is.
pub const CACHE_COLD: Duration = Duration::from_secs(5 * 60);
/// How long a conversation's last answerer is remembered.
pub const STICK_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// How many conversations, the latest, are kept on disk.
pub const STICKS_KEPT: usize = 512;
/// How long after its last answer a conversation still counts as holding the
/// key that answered it: a session whose user is reading or typing is going.
pub const HELD_FOR: Duration = Duration::from_secs(30 * 60);
/// How long an answer waits before `affinity.json` is written: what is
/// answered meanwhile goes in the same write, so a gateway answering many
/// requests a second writes it once in a while, not once a request (up to
/// [`STICKS_KEPT`] conversations marshalled, written and renamed each
/// time). A crash loses the last moment's; [`AffinityStore::flush`] as the
/// gateway ends loses nothing.
pub const WRITE_BEHIND: Duration = Duration::from_secs(2);

/// Headers where agents already name their conversation. OpenCode sends the
/// first; the rest are what other agents and proxies use.
const SESSION_HEADERS: [&str; 6] = [
    "x-opencode-session",
    "x-session-affinity",
    "x-session-id",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
];

/// Claude Code subagents reuse the lead session's id; this header alone says
/// a request belongs to a subagent, so its answer must not replace the lead's
/// affinity stick.
const CLAUDE_AGENT_HEADER: &str = "x-claude-code-agent-id";

/// Milliseconds since the Unix epoch, saturating at zero before it.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Lowercase hex, one byte at a time.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A duration in whole milliseconds, saturating.
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

/// What a key of a provider is called in a stick: the provider alone for its
/// empty key, else the provider and a short fingerprint of the key. The
/// fingerprint is not the key itself, so the file never holds a secret.
pub fn who_of(provider_id: &str, key: &str) -> String {
    if key.is_empty() {
        return provider_id.to_owned();
    }
    let sum = Sha256::digest(key.as_bytes());
    format!("{provider_id}#{}", hex(&sum[..5]))
}

/// A stable id for the conversation a request belongs to. It is the agent's
/// own session id when it sends one; otherwise it is derived from the
/// conversation's first user message, which every later turn repeats.
pub fn conversation_id(headers: &HeaderMap, body: &[u8]) -> String {
    // An undecodable body still gets an id: the hash of the whole body.
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    conversation_id_from_value(headers, &value, body)
}

/// The affinity stick key: the conversation, with a Claude Code subagent's
/// conversation kept apart from its lead's.
pub(crate) fn stick_key(headers: &HeaderMap, conversation: &str) -> String {
    let subagent = headers
        .get(CLAUDE_AGENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match subagent {
        Some(agent) => format!("{conversation}|agent:{agent}"),
        None => conversation.to_owned(),
    }
}

/// The same conversation id, using a body its caller already decoded.
///
/// `body` is only the fallback hash when the request names no user turn.
pub(crate) fn conversation_id_from_value(
    headers: &HeaderMap,
    value: &serde_json::Value,
    body: &[u8],
) -> String {
    for name in SESSION_HEADERS {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    let items_of = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let input = value.get("input").cloned();
    let mut items = items_of("messages");
    // Gemini generateContent repeats the first user turn under `contents`.
    let gemini = items.is_empty() && !items_of("contents").is_empty();
    if gemini {
        items = items_of("contents");
    }
    if items.is_empty() && input.as_ref().is_some_and(|v| v.is_array()) {
        items = input
            .as_ref()
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
    }
    let mut first: Option<Vec<u8>> = input.as_ref().and_then(|v| serde_json::to_vec(v).ok());
    if let Some(item) = items.first() {
        first = serde_json::to_vec(item).ok();
    }
    for item in &items {
        // Gemini treats an omitted role as user; Chat and Responses do not.
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "user" || (gemini && role.is_empty()) {
            first = serde_json::to_vec(item).ok();
            break;
        }
    }
    let first = match first.filter(|f| !f.is_empty()) {
        Some(first) => first,
        None => body.to_vec(),
    };
    let sum = Sha256::digest(&first);
    format!("magpie-{}", hex(&sum[..12]))
}

/// Count the user's turns in a request's conversation, and say whether it is
/// the agent handing tool results back within one. The last of the user's
/// messages tells whether the turn goes on: an agent may put its own notes
/// after it (Claude Code a system message after the tool results).
pub fn turn_of(request: &CompletionRequest) -> (u64, bool) {
    let mut turn = 0u64;
    let mut within = false;
    for message in &request.messages {
        if message.role != Role::User {
            continue;
        }
        let mut text = false;
        let mut result = false;
        for part in &message.parts {
            match part.kind() {
                PartKind::Text | PartKind::Image => text = true,
                PartKind::ToolResult => result = true,
                _ => {}
            }
        }
        if text && !result {
            turn += 1;
        }
        within = result;
    }
    (turn, within)
}

/// One key that answered a conversation, and what it read from the vendor's
/// cache doing so.
#[derive(Debug, Clone)]
struct Stick {
    who: String,
    model: String,
    turn: u64,
    at_ms: u64,
    cache_read: u64,
}

/// A stick as the file has it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedStick {
    #[serde(default)]
    who: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    turn: u64,
    /// When it answered, in milliseconds since the Unix epoch.
    at: u64,
    #[serde(rename = "cacheRead", default)]
    cache_read: u64,
}

#[derive(Default)]
struct State {
    answered: HashMap<String, Stick>,
    /// `scope|conversation` to who a conversation nobody has answered yet was
    /// sent to, while its first request is out: in turn, the fewest-held count
    /// includes them, so a burst of new conversations spreads rather than all
    /// going to the one holding the fewest a moment before.
    opening: HashMap<String, Stick>,
    /// The file as last read or written: another magpie's writes (the one
    /// handing over to this one) are read again when a conversation is missed.
    from: Option<PathBuf>,
    read_ms: Option<u64>,
}

/// Who answered which conversation, kept in process and on disk.
pub struct AffinityStore {
    path: Option<PathBuf>,
    state: Arc<Mutex<State>>,
    /// The write-behind signal: an answer marks the file dirty and wakes
    /// the writer, which takes a copy within [`WRITE_BEHIND`].
    writer: Option<Arc<(Mutex<Writer>, Condvar)>>,
}

/// The write-behind state: whether an answer is waiting, and whether the
/// store is ending (so the writer writes what waits and exits).
struct Writer {
    dirty: bool,
    stopping: bool,
    /// Set once the writer has written what waited and ended, so
    /// [`AffinityStore::flush`] can wait for the file.
    finished: bool,
}

impl Default for AffinityStore {
    fn default() -> Self {
        Self::new(None)
    }
}

impl AffinityStore {
    /// A store persisting to `path`, or in memory only when it is `None`.
    pub fn new(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self {
                path: None,
                state: Arc::new(Mutex::new(State::default())),
                writer: None,
            };
        };
        let state = Arc::new(Mutex::new(State::default()));
        let writer = Arc::new((
            Mutex::new(Writer {
                dirty: false,
                stopping: false,
                finished: false,
            }),
            Condvar::new(),
        ));
        let worker_state = Arc::clone(&state);
        let worker_path = path.clone();
        let worker_writer = Arc::clone(&writer);
        std::thread::spawn(move || writer_loop(worker_path, worker_state, worker_writer));
        Self {
            path: Some(path),
            state,
            writer: Some(writer),
        }
    }

    /// The store beside a config file (`affinity.json` in the same directory).
    pub fn beside(config_path: Option<&Path>) -> Self {
        Self::new(config_path.map(|path| path.with_file_name("affinity.json")))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read the file again when it changed since: after a restart, or written
    /// by the magpie that handed over.
    fn reread(&self, state: &mut State) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let Ok(modified) = meta.modified() else {
            return;
        };
        let read_ms = modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis().min(u64::MAX as u128) as u64);
        if state.from.as_deref() == Some(path.as_path()) && state.read_ms == read_ms {
            return;
        }
        state.from = Some(path.clone());
        state.read_ms = read_ms;
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(saved) = serde_json::from_slice::<HashMap<String, SavedStick>>(&bytes) else {
            return;
        };
        let now = now_ms();
        for (key, value) in saved {
            if value.who.is_empty() || now.saturating_sub(value.at) > millis(STICK_KEEP) {
                continue;
            }
            if let Some(existing) = state.answered.get(&key)
                && existing.at_ms >= value.at
            {
                continue;
            }
            state.answered.insert(
                key,
                Stick {
                    who: value.who,
                    model: value.model,
                    turn: value.turn,
                    at_ms: value.at,
                    cache_read: value.cache_read,
                },
            );
        }
    }

    /// Who answered this conversation last, read from the file again when it
    /// isn't known here and the file changed since.
    fn stick_of(&self, state: &mut State, key: &str) -> Option<Stick> {
        if let Some(stick) = state.answered.get(key) {
            return Some(stick.clone());
        }
        self.reread(state);
        state.answered.get(key).cloned()
    }

    /// Remember who answered a conversation, and what it read from the
    /// vendor's cache doing so. A conversation's entry in flight is ended.
    pub fn answered(&self, key: &str, who: &str, model: &str, turn: u64, cache_read: u64) {
        let now = now_ms();
        {
            let mut state = self.lock();
            state.answered.insert(
                key.to_owned(),
                Stick {
                    who: who.to_owned(),
                    model: model.to_owned(),
                    turn,
                    at_ms: now,
                    cache_read,
                },
            );
            state.opening.remove(key);
            if state.answered.len() > 4096 {
                state
                    .answered
                    .retain(|_, stick| now.saturating_sub(stick.at_ms) <= millis(STICK_KEEP));
            }
        }
        self.note_save();
    }

    /// Forget that a key answered a conversation, when its reply to it broke
    /// off: the agent's retry goes by routing again, not back to the one that
    /// just failed it. The key alone is enough: a wider match keeps a
    /// conversation on a key whose answerer's model has since left, so a stick
    /// that only matched by its model would leave the next request kept on the
    /// key that just broke off.
    pub fn unanswered(&self, key: &str, who: &str) {
        let removed = {
            let mut state = self.lock();
            let matches = state
                .answered
                .get(key)
                .is_some_and(|stick| stick.who == who);
            if matches {
                state.answered.remove(key);
            }
            matches
        };
        if removed {
            self.note_save();
        }
    }

    /// End a request's part in the count of conversations being opened,
    /// however it ended.
    pub fn opened(&self, key: &str) {
        self.lock().opening.remove(key);
    }

    /// How many conversations in `scope` are held on each seat right now,
    /// counting those whose first request is still out.
    fn held(&self, scope: &str, now: u64) -> HashMap<String, usize> {
        let state = self.lock();
        let mut held = HashMap::new();
        let prefix = format!("{scope}|");
        for (key, stick) in state.answered.iter().chain(state.opening.iter()) {
            if !key.starts_with(&prefix) || now.saturating_sub(stick.at_ms) > millis(HELD_FOR) {
                continue;
            }
            *held.entry(seat_of(&stick.who, &stick.model)).or_insert(0) += 1;
        }
        held
    }

    /// Have the latest answerers written within [`WRITE_BEHIND`]: what is
    /// answered meanwhile goes in the same write.
    fn note_save(&self) {
        let Some(writer) = &self.writer else {
            return;
        };
        let (lock, wake) = &**writer;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.dirty {
            state.dirty = true;
            wake.notify_one();
        }
    }

    /// Write now what [`Self::note_save`] has waiting, then stop the
    /// writer: the gateway calls it as it ends, so the magpie it hands over
    /// to, or the next start, reads every answer.
    pub fn flush(&self) {
        let Some(writer) = &self.writer else {
            return;
        };
        let (lock, wake) = &**writer;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stopping = true;
        wake.notify_all();
        while !state.finished {
            state = wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Write a snapshot atomically, through a temporary file beside it.
fn write_sticks(path: &Path, out: &HashMap<String, SavedStick>) {
    let Ok(bytes) = serde_json::to_vec(out) else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let Ok(mut file) =
        tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))
    else {
        return;
    };
    use std::io::Write;
    if file.write_all(&bytes).is_err() || file.flush().is_err() {
        return;
    }
    let _ = file.persist(path);
}

/// Wait for answers to write, write them once [`WRITE_BEHIND`] has passed,
/// and write the last of them as the store ends.
fn writer_loop(path: PathBuf, answers: Arc<Mutex<State>>, writer: Arc<(Mutex<Writer>, Condvar)>) {
    let (lock, wake) = &*writer;
    loop {
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while !state.dirty && !state.stopping {
            state = wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let stopping = state.stopping;
        state.dirty = false;
        drop(state);
        if stopping {
            save_sticks(&path, &answers);
            finish(writer);
            return;
        }
        let deadline = std::time::Instant::now() + WRITE_BEHIND;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if state.stopping || left.is_zero() {
                break;
            }
            state = wake
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        let stopping = state.stopping;
        state.dirty = false;
        drop(state);
        save_sticks(&path, &answers);
        if stopping {
            finish(writer);
            return;
        }
    }
}

/// Take the latest answerers out of `answers` and write them to `path`,
/// noting in `answers` that this store wrote it so it isn't read back as
/// another's.
fn save_sticks(path: &Path, answers: &Mutex<State>) {
    let out = {
        let state = answers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot(&state)
    };
    write_sticks(path, &out);
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(modified) = meta.modified()
    {
        let mut state = answers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.from = Some(path.to_owned());
        state.read_ms = modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis().min(u64::MAX as u128) as u64);
    }
}

/// The latest conversations' answerers, as the file keeps them.
fn snapshot(state: &State) -> HashMap<String, SavedStick> {
    let now = now_ms();
    let mut kept: Vec<(String, SavedStick)> = state
        .answered
        .iter()
        .filter(|(_, stick)| now.saturating_sub(stick.at_ms) <= millis(STICK_KEEP))
        .map(|(key, stick)| {
            (
                key.clone(),
                SavedStick {
                    who: stick.who.clone(),
                    model: stick.model.clone(),
                    turn: stick.turn,
                    at: stick.at_ms,
                    cache_read: stick.cache_read,
                },
            )
        })
        .collect();
    kept.sort_by_key(|a| std::cmp::Reverse(a.1.at));
    kept.truncate(STICKS_KEPT);
    kept.into_iter().collect()
}
/// Mark the write-behind thread ended, waking [`AffinityStore::flush`].
fn finish(writer: Arc<(Mutex<Writer>, Condvar)>) {
    let (lock, wake) = &*writer;
    let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.finished = true;
    wake.notify_all();
}
/// A candidate as one of a route's keys: its key, the model it would answer
/// as, and its place in the routing order and the key pool.
#[derive(Debug, Clone)]
pub struct Seat {
    /// The key or account, as [`who_of`] names it.
    pub who: String,
    /// The model it would answer as.
    pub model: String,
    /// Its index in the fallback route chain, not the configured route: a
    /// conversation kept on a fallback route moves that route forward.
    pub route_index: usize,
    /// Its index in the route list.
    pub route: usize,
    /// Its index in that route's key pool.
    pub key: usize,
    /// Whether routing left it out (cooling): it is not put first.
    pub resting: bool,
}

/// The seat a stick is held on: the key, and the model it answered as.
fn seat_of(who: &str, model: &str) -> String {
    format!("{who}/{model}")
}

/// What a request's conversation came to: the identity kept, and why.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    /// The provider's or group's affinity as configured.
    pub mode: Affinity,
    /// The user's turns in the conversation so far.
    pub turn: u64,
    /// The agent sending tool results back: mid-turn.
    pub within: bool,
    /// Who answered the conversation last, as it goes by now.
    pub last: String,
    /// Whether the answerer was put first for it.
    pub kept: bool,
    /// Why it was kept - `session`, `turn`, `cache` - or not: `off`, `first`,
    /// `new-turn`, `no-cache`, `cold`, `gone`.
    pub why: &'static str,
}

/// The outcome of [`AffinityStore::affine`]: what to put first, if anything,
/// and the trace of the decision.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    /// The conversation identity the decision was made under.
    pub key: String,
    /// What the request's conversation came to.
    pub trace: Trace,
    /// The seat to put first, as `(route, key)`, when the conversation is kept
    /// or a new one is spread. `None` leaves routing's order as it is.
    pub first: Option<(usize, usize)>,
    /// The seat's index in the route chain, when a route moves forward too.
    pub first_route: Option<usize>,
}

impl AffinityStore {
    /// Put first whoever answered the conversation last, while its affinity
    /// says to keep it there, or spread a brand-new conversation over the
    /// seat holding the fewest.
    // Eight independent affinity inputs; the unit tests call this
    // wrapper directly, and bundling would churn them for no gain.
    #[allow(clippy::too_many_arguments)]
    pub fn affine(
        &self,
        scope: &str,
        mode: Affinity,
        rotate: bool,
        headers: &HeaderMap,
        body: &[u8],
        request: &CompletionRequest,
        seats: &[Seat],
    ) -> Decision {
        let conversation = conversation_id(headers, body);
        let conversation = stick_key(headers, &conversation);
        self.affine_conversation(scope, mode, rotate, &conversation, request, seats)
    }

    /// Affinity for a caller that already derived the conversation id from a
    /// decoded request body.
    pub(crate) fn affine_conversation(
        &self,
        scope: &str,
        mode: Affinity,
        rotate: bool,
        conversation: &str,
        request: &CompletionRequest,
        seats: &[Seat],
    ) -> Decision {
        let key = format!("{scope}|{conversation}");
        let (turn, within) = turn_of(request);
        let now = now_ms();
        let stick = {
            let mut state = self.lock();
            self.stick_of(&mut state, &key)
        };
        let had = stick
            .as_ref()
            .is_some_and(|stick| now.saturating_sub(stick.at_ms) <= millis(STICK_KEEP));
        let mut trace = Trace {
            mode,
            turn,
            within,
            last: String::new(),
            kept: false,
            why: "off",
        };
        // The account and the model that answered: a group with Opus and
        // Sonnet both on one Claude account, a rule sending the turn to Opus,
        // had the turn's next request kept to the account and so to Sonnet,
        // its first member there; the account alone when the model is gone.
        let mut at: Option<usize> = None;
        if had && let Some(stick) = stick.as_ref() {
            trace.last = stick.who.clone();
            let matches: [fn(&Seat, &Stick) -> bool; 2] = [
                |seat, stick| seat.who == stick.who && seat.model == stick.model,
                |seat, stick| seat.who == stick.who,
            ];
            for same in matches {
                for (index, seat) in seats.iter().enumerate() {
                    if at.is_none() && same(seat, stick) {
                        at = Some(index);
                        trace.last = seat.who.clone();
                    }
                }
                if at.is_some() {
                    break;
                }
            }
        }
        trace.why = match mode {
            Affinity::Off => "off",
            _ if !had => "first",
            _ if at.is_none() => "gone",
            _ if at.is_some_and(|at| seats[at].resting) => "resting",
            Affinity::Session => "session",
            _ if within => "turn",
            Affinity::Turn => "new-turn",
            _ if rotate => "new-turn",
            _ if stick.as_ref().is_some_and(|s| s.cache_read < CACHE_WORTH) => "no-cache",
            _ if stick
                .as_ref()
                .is_some_and(|s| now.saturating_sub(s.at_ms) > millis(CACHE_COLD)) =>
            {
                "cold"
            }
            _ => "cache",
        };
        let mut first = None;
        let mut first_route = None;
        match trace.why {
            "session" | "turn" | "cache" => {
                trace.kept = true;
                if let Some(at) = at.filter(|at| *at > 0) {
                    first = Some((seats[at].route, seats[at].key));
                    first_route = Some(seats[at].route_index);
                }
            }
            "new-turn" if rotate => {
                if let Some(at) = at
                    && let Some(index) = after(seats, at)
                {
                    first = Some((seats[index].route, seats[index].key));
                    first_route = Some(seats[index].route_index);
                }
            }
            "first" if rotate => {
                if let Some(index) = self.least_held(scope, &key, seats, now) {
                    first = Some((seats[index].route, seats[index].key));
                    first_route = Some(seats[index].route_index);
                }
            }
            _ => {}
        }
        Decision {
            key,
            trace,
            first,
            first_route,
        }
    }

    /// Put first, for a conversation nobody has answered yet, the one routing
    /// that the fewest other conversations in scope are held on: in turn
    /// counts requests, not conversations, so a second session started after
    /// the first had sent an even number of them went to the key the first was
    /// kept on. A conversation whose first request is still out counts where
    /// it was sent.
    fn least_held(&self, scope: &str, key: &str, seats: &[Seat], now: u64) -> Option<usize> {
        if seats.len() < 2 {
            return None;
        }
        let held = self.held(scope, now);
        let seat = |seat: &Seat| seat_of(&seat.who, &seat.model);
        let mut best: Option<usize> = None;
        for (index, candidate) in seats.iter().enumerate() {
            if candidate.resting {
                continue;
            }
            if best.is_none_or(|best| {
                held.get(&seat(candidate)).copied().unwrap_or(0)
                    < held.get(&seat(&seats[best])).copied().unwrap_or(0)
            }) {
                best = Some(index);
            }
        }
        let best = best?;
        // Counted from now until it is answered or ends, so a burst of new
        // conversations spreads instead of all going to the one that held the
        // fewest a moment before.
        self.lock().opening.insert(
            key.to_owned(),
            Stick {
                who: seats[best].who.clone(),
                model: seats[best].model.clone(),
                turn: 0,
                at_ms: now,
                cache_read: 0,
            },
        );
        (best > 0).then_some(best)
    }
}

/// Put first the one after `at`, round from the end, that isn't resting.
fn after(seats: &[Seat], at: usize) -> Option<usize> {
    for distance in 1..seats.len() {
        let index = (at + distance) % seats.len();
        if seats[index].resting {
            continue; // a turn goes round the keys routed over only
        }
        if index > 0 {
            return Some(index);
        }
        break;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use magpie_core::ir::{Message, Part};

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            model: "mock/model".into(),
            messages,
            ..CompletionRequest::default()
        }
    }

    fn seat(who: &str, model: &str, route: usize, key: usize) -> Seat {
        Seat {
            who: who.to_owned(),
            model: model.to_owned(),
            route_index: 0,
            route,
            key,
            resting: false,
        }
    }

    #[test]
    fn a_session_header_names_the_conversation() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "abc".parse().unwrap());
        assert_eq!(conversation_id(&headers, b"{}"), "abc");
    }

    #[test]
    fn a_claude_code_subagent_does_not_take_its_leads_stick() {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-session-id", "s-1".parse().unwrap());
        let lead = conversation_id(&headers, b"{}");
        assert_eq!(stick_key(&headers, &lead), lead);

        headers.insert("x-claude-code-agent-id", "agent-1".parse().unwrap());
        assert_eq!(stick_key(&headers, &lead), format!("{lead}|agent:agent-1"));

        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-session-id", "s-1".parse().unwrap());
        headers.insert("x-claude-code-agent-id", " ".parse().unwrap());
        assert_eq!(stick_key(&headers, &lead), lead);
    }

    #[test]
    fn a_conversation_without_a_header_is_named_by_its_first_user_turn() {
        let first = br#"{"messages":[{"role":"assistant","content":"hi"},{"role":"user","content":"one"},{"role":"user","content":"two"}]}"#;
        let later = br#"{"messages":[{"role":"assistant","content":"hi"},{"role":"user","content":"one"},{"role":"user","content":"two"},{"role":"assistant","content":"x"},{"role":"user","content":"three"}]}"#;
        let headers = HeaderMap::new();
        let id = conversation_id(&headers, first);
        assert!(id.starts_with("magpie-"));
        assert_eq!(id, conversation_id(&headers, later));
    }

    #[test]
    fn gemini_contents_name_the_conversation_and_an_omitted_role_is_user() {
        let first = br#"{"contents":[{"parts":[{"text":"one"}]}]}"#;
        let later = br#"{"contents":[{"parts":[{"text":"one"}]},{"role":"model","parts":[{"text":"x"}]},{"role":"user","parts":[{"text":"two"}]}]}"#;
        let headers = HeaderMap::new();
        assert_eq!(
            conversation_id(&headers, first),
            conversation_id(&headers, later)
        );
    }

    #[test]
    fn turn_counts_users_and_says_whether_tool_results_are_going_back() {
        let ask = request(vec![Message {
            role: Role::User,
            parts: vec![Part::text("do it")],
        }]);
        assert_eq!(turn_of(&ask), (1, false));

        let tool = request(vec![
            Message {
                role: Role::User,
                parts: vec![Part::text("do it")],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::tool_call("a", "run", serde_json::json!({}))],
            },
            Message {
                role: Role::User,
                parts: vec![Part::tool_result("a", "done", false)],
            },
        ]);
        assert_eq!(turn_of(&tool), (1, true));
    }

    fn affine(mode: Affinity, rotate: bool, seats: &[Seat], store: &AffinityStore) -> Decision {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "conversation".parse().unwrap());
        store.affine(
            "mock/model",
            mode,
            rotate,
            &headers,
            b"{}",
            &request(vec![Message {
                role: Role::User,
                parts: vec![Part::text("hello")],
            }]),
            seats,
        )
    }

    #[test]
    fn a_fresh_conversation_is_first_and_kept_on_the_seat_that_answers() {
        let store = AffinityStore::default();
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = affine(Affinity::Auto, false, &seats, &store);
        assert_eq!(first.trace.why, "first");
        assert_eq!(first.trace.turn, 1);
        store.answered(&first.key, "mock#b", "model", 1, 4096);
        // A rotating provider starts a new turn somewhere else, as the
        // reference does; cache affinity only keeps a conversation when
        // routing is not rotating.
        let second = affine(Affinity::Auto, true, &seats, &store);
        assert_eq!(second.trace.why, "new-turn");
        assert!(!second.trace.kept);
        let kept = affine(Affinity::Auto, false, &seats, &store);
        assert_eq!(kept.trace.why, "cache");
        assert!(kept.trace.kept);
        assert_eq!(kept.first, Some((1, 1)));
    }

    #[test]
    fn session_keeps_a_conversation_even_without_cache_and_off_never_does() {
        let store = AffinityStore::default();
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = affine(Affinity::Off, true, &seats, &store);
        store.answered(&first.key, "mock#b", "model", 1, 0);
        assert_eq!(affine(Affinity::Off, true, &seats, &store).trace.why, "off");
        let kept = affine(Affinity::Session, true, &seats, &store);
        assert_eq!(kept.trace.why, "session");
        assert_eq!(kept.first, Some((1, 1)));
    }

    #[test]
    fn a_turn_keeps_a_conversation_while_tool_results_come_back() {
        let store = AffinityStore::default();
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = affine(Affinity::Turn, true, &seats, &store);
        store.answered(&first.key, "mock#b", "model", 1, 0);
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "conversation".parse().unwrap());
        let mid_turn = request(vec![
            Message {
                role: Role::User,
                parts: vec![Part::text("hello")],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::tool_call("a", "run", serde_json::json!({}))],
            },
            Message {
                role: Role::User,
                parts: vec![Part::tool_result("a", "done", false)],
            },
        ]);
        let kept = store.affine(
            "mock/model",
            Affinity::Turn,
            true,
            &headers,
            b"{}",
            &mid_turn,
            &seats,
        );
        assert_eq!(kept.trace.why, "turn");
        assert_eq!(kept.first, Some((1, 1)));
    }

    #[test]
    fn a_new_turn_with_rotate_moves_on_and_turn_affinity_sticks() {
        let store = AffinityStore::default();
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = affine(Affinity::Auto, false, &seats, &store);
        store.answered(&first.key, "mock#a", "model", 1, 0);
        let rotated = affine(Affinity::Auto, true, &seats, &store);
        assert_eq!(rotated.trace.why, "new-turn");
        assert_eq!(rotated.first, Some((1, 1)));
        let sticky = affine(Affinity::Turn, true, &seats, &store);
        assert_eq!(sticky.trace.why, "new-turn");
    }

    #[test]
    fn answers_write_behind_and_flush_writes_what_waits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("affinity.json");
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let store = AffinityStore::new(Some(path.clone()));
        let decision = affine(Affinity::Auto, true, &seats, &store);
        for _ in 0..200 {
            store.answered(&decision.key, "mock#b", "model", 1, 4096);
        }
        // Nothing is written on the answers themselves.
        assert!(!path.exists());
        // The answer is known in memory at once.
        assert_eq!(
            affine(Affinity::Auto, false, &seats, &store).trace.why,
            "cache"
        );
        // Ending the store writes everything that waited.
        store.flush();
        assert!(path.exists());
        let reopened = AffinityStore::new(Some(path));
        let kept = affine(Affinity::Auto, false, &seats, &reopened);
        assert_eq!(kept.key, decision.key);
        assert_eq!(kept.trace.why, "cache");
    }
    #[test]
    fn a_stick_survives_a_restart_from_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("affinity.json");
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = {
            let store = AffinityStore::new(Some(path.clone()));
            let decision = affine(Affinity::Auto, true, &seats, &store);
            store.answered(&decision.key, "mock#b", "model", 1, 4096);
            store.flush();
            decision.key
        };
        assert!(path.exists());
        let reopened = AffinityStore::new(Some(path));
        let decision = affine(Affinity::Auto, false, &seats, &reopened);
        assert_eq!(decision.key, first);
        assert_eq!(decision.trace.why, "cache");
        assert_eq!(decision.first, Some((1, 1)));
    }

    #[test]
    fn a_broken_off_reply_forgets_the_key_that_answered() {
        let store = AffinityStore::default();
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = affine(Affinity::Session, true, &seats, &store);
        store.answered(&first.key, "mock#b", "model", 1, 0);
        store.unanswered(&first.key, "mock#a");
        assert_eq!(
            affine(Affinity::Session, true, &seats, &store).trace.why,
            "session"
        );
        store.unanswered(&first.key, "mock#b");
        assert_eq!(
            affine(Affinity::Session, true, &seats, &store).trace.why,
            "first"
        );
    }

    #[test]
    fn opening_a_conversation_counts_until_it_is_answered_or_ends() {
        let store = AffinityStore::default();
        let seats = vec![seat("mock#a", "model", 0, 0), seat("mock#b", "model", 1, 1)];
        let first = affine(Affinity::Auto, true, &seats, &store);
        assert_eq!(first.trace.why, "first");

        // The first is in flight on the front seat. While it is out, a second
        // new conversation must spread to the other seat rather than pick the
        // same zero-held seat from the stale count.
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "another".parse().unwrap());
        let body = br#"{"messages":[{"role":"user","content":"another"}]}"#;
        let second = store.affine(
            "mock/model",
            Affinity::Auto,
            true,
            &headers,
            body,
            &request(vec![Message {
                role: Role::User,
                parts: vec![Part::text("another")],
            }]),
            &seats,
        );
        assert_eq!(second.first, Some((1, 1)));

        // After the first ends, only the second is still in flight; a third
        // conversation goes to the least-held remaining seat (seat 0).
        store.opened(&first.key);
        let mut third_headers = HeaderMap::new();
        third_headers.insert("x-session-id", "third".parse().unwrap());
        let third = store.affine(
            "mock/model",
            Affinity::Auto,
            true,
            &third_headers,
            br#"{"messages":[{"role":"user","content":"third"}]}"#,
            &request(vec![Message {
                role: Role::User,
                parts: vec![Part::text("third")],
            }]),
            &seats,
        );
        assert_eq!(third.trace.why, "first");
        assert_eq!(third.first, None);

        // Releasing the last opening leaves no phantom hold behind.
        store.opened(&second.key);
        store.opened(&third.key);
        let fourth = store.affine(
            "mock/model",
            Affinity::Auto,
            true,
            &HeaderMap::new(),
            br#"{"messages":[{"role":"user","content":"fourth"}]}"#,
            &request(vec![Message {
                role: Role::User,
                parts: vec![Part::text("fourth")],
            }]),
            &seats,
        );
        assert_eq!(fourth.first, None);
    }
}
