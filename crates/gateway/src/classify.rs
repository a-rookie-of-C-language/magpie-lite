//! A rule with an intent is for a kind of message: as a user's turn begins,
//! the group's classifier model is asked which of the intents the user's
//! message is — once, before the turn goes anywhere, and never again within
//! it. The call goes through lite itself, so the classifier is any model
//! lite serves, with its failover and usage; it shows in the usage as
//! lite's own call. When it cannot say (it fails, is slow, or answers
//! something else) no intent matches. Ports the reference's
//! `internal/gateway/classify.go` without its decision providers: lite has
//! no Jev/System One models, so there is no per-turn effort judging, only
//! the intent question through the gateway itself.
//!
//! The cache, failure rest, and "said once in the log" bookkeeping are the
//! reference's (`classifyKeep`, `classifyRest`); concurrent asks for the
//! same question are not coalesced the way its inflight calls are — each
//! asks, and the cache dedups after — which is only ever a duplicate
//! classifier call, never a wrong answer.
use magpie_core::ir::{CompletionRequest, Message, Part, PartKind, Role};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
/// The User-Agent lite's own classifier calls carry (the reference's
/// `RouterAgent`).
pub const ROUTER_AGENT: &str = "magpie-router/1";
/// How long the classifier may take to answer (the reference's
/// `classifyTimeout`).
pub const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(8);
/// How long an answer holds for the same message and intents (the
/// reference's `classifyKeep`).
const CLASSIFY_KEEP: Duration = Duration::from_secs(10 * 60);
/// How long a failed classifier rests before it is asked again (the
/// reference's `classifyRest`).
const CLASSIFY_REST: Duration = Duration::from_secs(30);
/// Most answers remembered at once (the reference caps `classified.m` at
/// 4096).
const CLASSIFIED_KEPT: usize = 4096;
/// How much of each of the turn before's two texts the classifier is told,
/// in characters (the reference's `priorCap`).
const PRIOR_CAP: usize = 1000;
/// What the turn before was said to be, and what was said in it: a short
/// follow-up (go on, fix that) says little of its own, and is judged with
/// what it follows (the reference's `before`, without `Effort` — lite
/// never judges effort).
#[derive(Debug, Clone, Default)]
pub struct Before {
    /// One of the intents asked about now, or "".
    pub intent: String,
    /// The user's message before this one, cut short; "" for none.
    pub user: String,
    /// The assistant's last reply before this message, its text alone,
    /// its end kept; "" for none.
    pub reply: String,
}
/// What the classifier said (the reference's `verdict`, without effort).
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    /// "" for none of the intents.
    pub intent: String,
}
/// Why asking failed: the classifier is down (`Failed`, rested before the
/// next ask) or answered but not with one of the numbers (`Answered`, up,
/// so not rested — the reference's `answerError`).
#[derive(Debug, Clone)]
pub enum AskError {
    Failed(String),
    Answered(String),
}
struct ClassifiedAs {
    v: Verdict,
    at: Instant,
}
struct ClassifyFailure {
    err: String,
    at: Instant,
    /// Wall-clock milliseconds, for the console: an `Instant` never leaves
    /// the process.
    at_ms: u64,
}
#[derive(Default)]
struct Classified {
    m: HashMap<String, ClassifiedAs>,
    failed: HashMap<String, ClassifyFailure>,
    last: HashMap<String, ClassifyFailure>,
}
static CLASSIFIED: LazyLock<Mutex<Classified>> =
    LazyLock::new(|| Mutex::new(Classified::default()));
/// Forget every answer and failure (tests).
#[doc(hidden)]
pub fn reset() {
    *CLASSIFIED.lock().unwrap() = Classified::default();
}
/// Why `model`, a routing group's classifier, could not say which intent a
/// turn is the last time it was asked, and when; `None` once it answers,
/// and before it is asked (the reference's `ClassifierFailure`).
pub fn classifier_failure(model: &str) -> Option<(String, Instant)> {
    let map = CLASSIFIED.lock().unwrap();
    map.last.get(model).map(|f| (f.err.clone(), f.at))
}
/// A classifier that could not say when last asked, for the console's
/// group cards (the reference's `ClassifierFailure` on the Routing page,
/// #1929): a classifier that fails at every turn otherwise only shows in
/// each request's own details.
#[derive(Debug, Clone, Serialize)]
pub struct ClassifierStatus {
    /// The classifier model, as the group names it.
    pub model: String,
    /// Why it could not say.
    pub error: String,
    /// Wall-clock milliseconds of the failed ask.
    pub at_unix_ms: u64,
}
/// Every classifier that could not say when last asked, oldest first.
pub fn failures() -> Vec<ClassifierStatus> {
    let map = CLASSIFIED.lock().unwrap();
    let mut out: Vec<ClassifierStatus> = map
        .last
        .iter()
        .map(|(model, f)| ClassifierStatus {
            model: model.clone(),
            error: f.err.clone(),
            at_unix_ms: f.at_ms,
        })
        .collect();
    out.sort_by_key(|s| s.at_unix_ms);
    out
}
/// Wall-clock milliseconds now, for [`ClassifierStatus`].
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
/// Ask which of `intents` the message is, from what was asked before when
/// it can be: the same message and intents within `CLASSIFY_KEEP`. A
/// classifier that failed rests for `CLASSIFY_REST` rather than making
/// every turn wait out its timeout. Returns the intent ("" for none),
/// whether it came from the cache, and why not when it could not say.
/// `ask` is boxed so the gateway asking itself never names the caller's
/// future type: an in-process classifier call through `proxy_parsed`
/// reaches `send_with_fallback`, which reaches this call — naming the
/// concrete future on both sides is an unprovable `Send` cycle (E0391).
pub type AskFuture = Pin<Box<dyn Future<Output = Result<Verdict, AskError>> + Send>>;
/// How the gateway answers: any model through itself, boxed with its
/// future. It takes an owned `Arc` so the closure is `'static`.
pub type AskFn = Box<dyn Fn(String, Vec<String>, Before, String) -> AskFuture + Send>;
pub async fn classify(
    ask: AskFn,
    model: &str,
    intents: &[String],
    prev: &Before,
    text: &str,
) -> (String, bool, Option<String>) {
    let key = cache_key(model, intents, prev, text);
    let now = Instant::now();
    {
        let map = CLASSIFIED.lock().unwrap();
        if let Some(c) = map.m.get(&key)
            && now.duration_since(c.at) < CLASSIFY_KEEP
        {
            return (c.v.intent.clone(), true, None);
        }
        if let Some(f) = map.failed.get(model)
            && now.duration_since(f.at) < CLASSIFY_REST
        {
            return (
                String::new(),
                false,
                Some(format!(
                    "{} failed {} ago ({}); not asked again for now",
                    model,
                    humane_age(now.duration_since(f.at)),
                    f.err
                )),
            );
        }
    }
    let out = ask(
        model.to_owned(),
        intents.to_vec(),
        prev.clone(),
        text.to_owned(),
    )
    .await;
    let mut map = CLASSIFIED.lock().unwrap();
    match out {
        Ok(v) => {
            let intent = v.intent.clone();
            map.m.insert(
                key,
                ClassifiedAs {
                    v,
                    at: Instant::now(),
                },
            );
            map.failed.remove(model);
            map.last.remove(model);
            if map.m.len() > CLASSIFIED_KEPT {
                map.m
                    .retain(|_, c| now.duration_since(c.at) < CLASSIFY_KEEP);
            }
            (intent, false, None)
        }
        Err(AskError::Answered(err)) => {
            // Up, but its answer was no number: said once in the log, not
            // at every turn it answers the same way, and never rested.
            if map.last.get(model).is_none_or(|was| was.err != err) {
                tracing::info!("routing: the classifier {model} couldn't say: {err}");
            }
            map.last.insert(
                model.to_owned(),
                ClassifyFailure {
                    err: err.clone(),
                    at: Instant::now(),
                    at_ms: wall_ms(),
                },
            );
            (String::new(), false, Some(err))
        }
        Err(AskError::Failed(err)) => {
            if map.last.get(model).is_none_or(|was| was.err != err) {
                tracing::info!("routing: the classifier {model} couldn't say: {err}");
            }
            let at = Instant::now();
            map.failed.insert(
                model.to_owned(),
                ClassifyFailure {
                    err: err.clone(),
                    at,
                    at_ms: wall_ms(),
                },
            );
            map.last.insert(
                model.to_owned(),
                ClassifyFailure {
                    err: err.clone(),
                    at,
                    at_ms: wall_ms(),
                },
            );
            (String::new(), false, Some(err))
        }
    }
}
fn humane_age(d: Duration) -> String {
    if d.as_secs() >= 60 {
        format!("{}m", d.as_secs() / 60)
    } else {
        format!("{}s", d.as_secs())
    }
}
fn cache_key(model: &str, intents: &[String], prev: &Before, text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update(b"\x00");
    for intent in intents {
        h.update(intent.to_ascii_lowercase().as_bytes());
        h.update(b"\x00");
    }
    h.update(b"\x00");
    h.update(prev.intent.as_bytes());
    h.update(b"\x00");
    h.update(prev.user.as_bytes());
    h.update(b"\x00");
    h.update(prev.reply.as_bytes());
    h.update(b"\x00");
    h.update(text.as_bytes());
    hex12(&h.finalize())
}
fn hex12(sum: &[u8]) -> String {
    sum[..12].iter().map(|b| format!("{b:02x}")).collect()
}
/// What the user said to begin the turn: the words of the last user
/// message that has any — not what agents add to it, or put in after it,
/// for the model (a system reminder, a task notice, an agent list, the
/// Codex app's context tail) — its middle left out when long (`userText`,
/// #1826).
pub fn user_text(req: &CompletionRequest) -> String {
    let Some((_, text)) = last_said(req) else {
        return String::new();
    };
    cut_middle(&text, 3000, 1000)
}
/// The last user message that has words of the user's, and those words.
fn last_said(req: &CompletionRequest) -> Option<(usize, String)> {
    for (i, m) in req.messages.iter().enumerate().rev() {
        if m.role == Role::User {
            let said = user_said(&m.parts);
            if !said.is_empty() {
                return Some((i, said));
            }
        }
    }
    None
}
/// What the agents put in as the user, as they send it (`reminders`,
/// `myRequest`, `injected`, `agentsFor` in the reference's prompt.go,
/// #1826): Claude Code's and ZCode's reminders, notices and agent lists,
/// ZCode's plugin metadata, Codex's context and the Codex app's.
fn reminders() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?s)<system-reminder(?:\s[^>]*)?>.*?</system-reminder>")
            .expect("reminders pattern")
    })
}
/// What the user typed in the Codex app, after the context it puts first
/// (`myRequest`).
fn my_request() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?m)^## My request(?: for Codex)?:[ \t]*\n?")
            .expect("myRequest pattern")
    })
}
/// An agent list heading (`agentsFor`).
fn agents_for() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^# (\S+\.md) instructions for (\S.*?)[ \t]*\n")
            .expect("agentsFor pattern")
    })
}
/// How a text the agent put in as the user begins, beside `agentsFor` and
/// the reminders (`injected`).
const INJECTED: &[&str] = &[
    "<system-reminder",
    "<environment_context>",
    "<user_instructions>",
    "<turn_aborted>",
    "<user_shell_command>",
    "<subagent_notification>",
    "<in-app-browser-context",
    "<task-notification>",
    "<subagent-notification>",
    "<plugin_reference>",
    "Available agent types for the Agent tool:",
    "Available agent types and the tools they have access to:",
    "New agent types are now available for the Agent tool:",
    "The TodoWrite tool hasn't been used recently.",
    "The task tools haven't been used recently.",
    "Conversation rewind applied.",
    "Workspace rewind applied.",
];
/// What the user said in one of their texts: "" for a text the agent put
/// in (an instruction file, a reminder, a notice), else the text without
/// the reminders in it, and after the Codex app's context (`userWords`).
/// Routing (turn keys, `user_text`, turn counts) goes by it, as the
/// prompt's turns do.
pub fn user_words(text: &str) -> String {
    let mut t = reminders().replace_all(text, "").trim().to_owned();
    let mut cut = None;
    for m in my_request().find_iter(&t) {
        cut = Some(m.end());
    }
    if let Some(at) = cut {
        t = t[at..].trim().to_owned();
    }
    if t.is_empty() || agents_for().is_match(&t) {
        return String::new();
    }
    for lead in INJECTED {
        if t.starts_with(lead) {
            return String::new();
        }
    }
    t
}
/// Whether a text holds words of the user's, not something the agent put
/// in for them (`userSays`).
pub fn user_says(text: &str) -> bool {
    !user_words(text).is_empty()
}
/// What the user said in a message's parts: the words of its texts
/// (`userSaid`). Tool results ride user-side messages in lite's IR, but
/// they are their own part kind, never text, so they are not the user's
/// words.
pub fn user_said(parts: &[Part]) -> String {
    let mut said = Vec::new();
    for p in parts {
        if p.kind() == PartKind::Text {
            let w = user_words(&p.text);
            if !w.is_empty() {
                said.push(w);
            }
        }
    }
    said.join("\n")
}
/// The message's words with the middle left out when longer than
/// head+tail characters (`cutMiddle`).
pub fn cut_middle(text: &str, head: usize, tail: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > head + tail {
        format!(
            "{}\n…\n{}",
            chars[..head].iter().collect::<String>(),
            chars[chars.len() - tail..].iter().collect::<String>()
        )
    } else {
        text.to_owned()
    }
}
/// What was said in the conversation before the user's message (the
/// message's own [`user_text`]): the user's message before it and the
/// assistant's last reply between the two — its text alone, its end kept
/// past `PRIOR_CAP` — for a follow-up that says little of its own
/// (`turnBefore`). A reply of only tool calls is none.
pub fn turn_before(req: &CompletionRequest) -> (String, String) {
    let Some((at, _)) = last_said(req) else {
        return (String::new(), String::new());
    };
    let from = last_said_before(req, at);
    // The reply between the message before and this one.
    let start = from.as_ref().map(|(i, _)| i + 1).unwrap_or(0);
    let mut reply = String::new();
    for m in req.messages[start..at].iter().rev() {
        if m.role == Role::Assistant {
            let said = assistant_said(m);
            if !said.is_empty() {
                reply = said;
                break;
            }
        }
    }
    let user = from
        .as_ref()
        .map(|(_, u)| cut_middle(&u, PRIOR_CAP * 3 / 4, PRIOR_CAP / 4))
        .unwrap_or_default();
    let kept: Vec<char> = reply.chars().collect();
    let reply = if kept.len() > PRIOR_CAP {
        format!(
            "…{}",
            kept[kept.len() - PRIOR_CAP..].iter().collect::<String>()
        )
    } else {
        reply
    };
    (user, reply)
}
fn last_said_before(req: &CompletionRequest, end: usize) -> Option<(usize, String)> {
    for (i, m) in req.messages[..end].iter().enumerate().rev() {
        if m.role == Role::User {
            let said = user_said(&m.parts);
            if !said.is_empty() {
                return Some((i, said));
            }
        }
    }
    None
}
/// The text an assistant message holds: no tool call, thinking, or search
/// in it (`assistantSaid`).
fn assistant_said(m: &Message) -> String {
    let mut said = Vec::new();
    for p in &m.parts {
        if p.kind() == PartKind::Text {
            let t = p.text.trim();
            if !t.is_empty() {
                said.push(t.to_owned());
            }
        }
    }
    said.join("\n")
}
/// The words telling the classifier what was said before the message, when
/// anything was (`writePrior`).
fn write_prior(b: &mut String, prev: &Before) {
    if prev.user.is_empty() && prev.reply.is_empty() {
        return;
    }
    b.push_str("\nWhat came before the user's message, in the same conversation, to judge a short follow-up (go on, yes, fix that) by:\n");
    if !prev.user.is_empty() {
        b.push_str("<previous_user_message>\n");
        b.push_str(&prev.user);
        b.push_str("\n</previous_user_message>\n");
    }
    if !prev.reply.is_empty() {
        b.push_str("<previous_assistant_reply>\n");
        b.push_str(&prev.reply);
        b.push_str("\n</previous_assistant_reply>\n");
    }
}
const CLASSIFY_PROMPT: &str = "You route a user's message to a coding assistant. Given numbered kinds and the user's message, answer with the number of the kind that fits the message best. The kinds may be topics, or levels such as how hard or how big a request is; when they are levels, every message has one, a greeting or a question about the assistant included. Answer 0 only when the message is plainly none of the kinds. Answer with the number only.";
/// The Chat request asking `model` which of `intents` the message is
/// (`classifyBody`); a model that reasons does so before it answers, and
/// 2048 leaves room for that and the number.
pub fn classify_body(model: &str, intents: &[String], prev: &Before, text: &str) -> Value {
    let mut b = String::from("Kinds:\n");
    for (i, intent) in intents.iter().enumerate() {
        b.push_str(&format!("{}. {}\n", i + 1, intent));
    }
    write_prior(&mut b, prev);
    if let Some(i) = intents.iter().position(|s| s == &prev.intent) {
        b.push_str(&format!(
            "\nThe user's message before this one, in the same conversation, was of kind {}. A message that only carries on from it — go on, yes, do it, fix that — is of kind {} too; one that asks for something of its own is of the kind that fits it.\n",
            i + 1,
            i + 1
        ));
    }
    b.push_str("\nThe user's message:\n<message>\n");
    b.push_str(text);
    b.push_str("\n</message>\n\nThe number of the kind that fits it best (0 only if none does):");
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": CLASSIFY_PROMPT},
            {"role": "user", "content": b},
        ],
        "stream": false,
        "temperature": 0,
        "max_tokens": 2048,
    })
}
/// The intent a classifier's answer names: its first number, 0 for none
/// (`readIntent`).
pub fn read_intent(answer: &str, intents: &[String]) -> Result<String, String> {
    let n = first_number(answer).and_then(|n| n.parse::<usize>().ok());
    match n {
        Some(n) if n <= intents.len() => {
            if n == 0 {
                Ok(String::new())
            } else {
                Ok(intents[n - 1].clone())
            }
        }
        _ => {
            let trimmed = answer.trim();
            let mut short: String = trimmed.chars().take(80).collect();
            if trimmed.chars().count() > 80 {
                short.push('…');
            }
            Err(format!(
                "it answered {short:?}, not a number from 0 to {}",
                intents.len()
            ))
        }
    }
}
/// The first run of digits in the answer (the reference's `firstNumber`).
fn first_number(answer: &str) -> Option<String> {
    let bytes = answer.as_bytes();
    let start = bytes.iter().position(|b| b.is_ascii_digit())?;
    let len = bytes[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    Some(answer[start..start + len].to_owned())
}
