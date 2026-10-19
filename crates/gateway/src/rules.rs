//! A routing group's rules: which member a request goes to first, by what is
//! visible in the request itself — its length, whether it carries an image,
//! the reasoning asked for, which agent sent it, the folder it works in, and
//! when it came. Ports the reference's `grouprule.go` / `rulefolder.go`
//! (internal/provider) and `workdir.go`/`compacting.go` (internal/gateway).
//!
//! The one upstream condition not ported is `intent`: it asks a model of the
//! group to judge the message (the classifier), which is deferred work. A rule
//! with an intent is accepted in config but matches nothing yet — as an
//! unanswerable classifier does upstream — while every other condition of the
//! rule still applies.
//!
//! The classifier is any model lite serves, asked through lite itself (see
//! `classify.rs`); when it cannot say, no intent matches. Lite has no
//! decision providers or decision groups, no per-turn effort judging
//! (`effort: auto`), and no `useEffort` pairing of model and reasoning.

use magpie_core::ir::{CompletionRequest, PartKind, Role};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// The reasoning levels a rule can ask for, lowest first (the reference's
/// `Efforts`).
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// How long an intent may be, in characters (the reference's `MaxIntent`).
pub const MAX_INTENT: usize = 200;

/// How long a turn's rule decision is kept (the reference's `stickKeep`).
const STICK_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// Most conversations remembered at once (the reference caps `turnRules` at
/// 4096).
const TURNS_KEPT: usize = 4096;

/// Rule sends the requests it matches to one of its group's members first.
/// Every condition set must hold; a rule sets at least one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rule {
    /// The member, as the group names it: `provider/model`.
    #[serde(rename = "use")]
    pub use_: String,
    /// The request is at least this long, in tokens — the larger of an
    /// estimate from its size and what the vendor counted for the
    /// conversation's last request.
    #[serde(default)]
    pub tokens: u64,
    /// The request carries an image, in this turn or before.
    #[serde(default)]
    pub images: bool,
    /// The agent asked for reasoning at least this level (one of
    /// [`EFFORTS`]), or `on` for any reasoning at all.
    #[serde(default)]
    pub effort: String,
    /// The request is from one of these agents (UA-derived ids).
    #[serde(default)]
    pub agents: Vec<String>,
    /// The user's message at the turn's start is of this kind, as the
    /// group's classifier judges it. When the classifier cannot say, it
    /// does not match.
    #[serde(default)]
    pub intent: String,
    /// The agent is compacting its conversation — asking a model to
    /// summarize it so it can go on in less room.
    #[serde(default)]
    pub compact: bool,
    /// The turn begins within these hours of the day, local time.
    #[serde(default)]
    pub time: Option<TimeWindow>,
    /// The request's working folder is one of these or inside one.
    #[serde(default)]
    pub folders: Vec<String>,
    /// The request's working folder is none of these nor inside one, as
    /// [`Self::folders`] reads them. A request whose agent says no folder
    /// is not known to be in one, so it holds for it.
    #[serde(default, rename = "exceptFolders")]
    pub except_folders: Vec<String>,
    /// Rather than sending to `use` first, leave it out of the group while
    /// the rule holds. Only `time`, `agents`, `folders` and
    /// `except_folders` may be its conditions: a pause keeps the model out
    /// whatever the request asks for.
    #[serde(default)]
    pub pause: bool,
}

/// Hours of the day, local time: `from` until `to`, `HH:MM` each, past
/// midnight when `to` comes before `from` (22:00–08:00), the whole day when
/// they are the same. `days`, when set, are the days it holds on (`mon` …
/// `sun`); a window past midnight is of the day it begins on, so Friday's
/// 22:00–08:00 takes Saturday's early hours too.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimeWindow {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub days: Vec<String>,
}

/// Days as `time::Weekday` counts them, Sunday first.
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
/// The week as a window shows it, Monday first.
const DAY_ORDER: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Read `HH:MM` (or `H:MM`) as minutes past midnight (the reference's
/// `clockMinutes`).
fn clock_minutes(s: &str) -> Option<u32> {
    let s = s.trim();
    let (h, m) = s.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 || h.starts_with('+') || m.starts_with('+') {
        return None;
    }
    let hh: u32 = h.parse().ok()?;
    let mm: u32 = m.parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(hh * 60 + mm)
}

/// Read a day of the week, `mon` or `Monday`, as [`WEEKDAYS`] has it (the
/// reference's `ParseDay` — a prefix of the English name, at least three
/// letters).
fn parse_day(s: &str) -> Option<&'static str> {
    const FULL: [&str; 7] = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ];
    let s = s.trim().to_ascii_lowercase();
    if s.len() < 3 {
        return None;
    }
    WEEKDAYS
        .iter()
        .zip(FULL)
        .find(|(_, f)| f.starts_with(&s))
        .map(|(d, _)| *d)
}

impl TimeWindow {
    /// Check the window and put it the one way: `09:00`, days known, each
    /// once in the week's order, none when it is every day (`clean`).
    pub fn clean(&mut self) -> Result<(), String> {
        let from = clock_minutes(&self.from)
            .ok_or_else(|| format!("the time {:?} is not HH:MM (09:00)", self.from))?;
        let to = clock_minutes(&self.to)
            .ok_or_else(|| format!("the time {:?} is not HH:MM (18:00)", self.to))?;
        self.from = format!("{:02}:{:02}", from / 60, from % 60);
        self.to = format!("{:02}:{:02}", to / 60, to % 60);
        let mut days: Vec<&str> = Vec::new();
        for d in &self.days {
            let v = parse_day(d)
                .ok_or_else(|| format!("{d:?} is not a day of the week (mon … sun)"))?;
            days.push(v);
        }
        self.days = DAY_ORDER
            .iter()
            .filter(|d| days.contains(d))
            .map(|d| d.to_string())
            .collect();
        if self.days.len() == DAY_ORDER.len() {
            self.days.clear();
        }
        Ok(())
    }

    /// Whether `at` (minutes past midnight, weekday 0 = Sunday) is within
    /// the window. A `None` time is in none.
    pub fn holds(&self, at: Option<(u32, usize)>) -> bool {
        let Some((minutes, mut day)) = at else {
            return false;
        };
        let (Some(from), Some(to)) = (clock_minutes(&self.from), clock_minutes(&self.to)) else {
            return false;
        };
        if from == to {
            // the whole day
        } else if from < to {
            if minutes < from || minutes >= to {
                return false;
            }
        } else {
            // past midnight
            if minutes >= to && minutes < from {
                return false;
            }
            if minutes < to {
                day = (day + 6) % 7; // the window began the day before
            }
        }
        self.days.is_empty() || self.days.iter().any(|d| d == WEEKDAYS[day])
    }
}

impl Rule {
    /// One line for a list: the member, then what it matches.
    pub fn summary(&self) -> String {
        let what = self.conditions().join(", ");
        if what.is_empty() {
            self.use_.clone()
        } else {
            format!("{} when {}", self.use_, what)
        }
    }

    /// What a rule matches, for a list (`Conditions`).
    pub fn conditions(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.tokens > 0 {
            out.push(format!("tokens ≥ {}", self.tokens));
        }
        if self.images {
            out.push("images".to_owned());
        }
        match self.effort.as_str() {
            "" => {}
            "on" => out.push("reasoning".to_owned()),
            e => out.push(format!("effort ≥ {e}")),
        }
        if !self.agents.is_empty() {
            out.push(format!("agent {}", self.agents.join("|")));
        }
        if !self.intent.is_empty() {
            out.push(format!("intent {:?}", self.intent));
        }
        if self.compact {
            out.push("compacting".to_owned());
        }
        if let Some(t) = &self.time {
            let mut s = format!("{}–{}", t.from, t.to);
            if t.from == t.to {
                s = "all day".to_owned();
            }
            if !t.days.is_empty() {
                s.push(' ');
                s.push_str(&t.days.join(","));
            }
            out.push(format!("time {s}"));
        }
        if !self.folders.is_empty() {
            out.push(format!("folder {}", self.folders.join("|")));
        }
        if !self.except_folders.is_empty() {
            out.push(format!("not folder {}", self.except_folders.join("|")));
        }
        out
    }

    /// Normalize the rule's fields; `members` are the group's route ids
    /// (`provider/model`). Port of `cleanRules` per rule, `n` the rule's
    /// number from 1 for messages.
    pub fn clean(&mut self, members: &[String], n: usize) -> Result<(), String> {
        self.use_ = self.use_.trim().to_owned();
        self.effort = self.effort.trim().to_ascii_lowercase();
        self.intent = self.intent.split_whitespace().collect::<Vec<_>>().join(" ");
        self.agents = self
            .agents
            .iter()
            .map(|a| a.trim().to_ascii_lowercase())
            .filter(|a| !a.is_empty())
            .collect();
        self.agents.dedup();
        let mut folders = Vec::new();
        for p in &self.folders {
            if p.trim().is_empty() {
                continue;
            }
            let c = clean_folder(p).map_err(|e| format!("rule {n}: {e}"))?;
            if !folders.contains(&c) {
                folders.push(c);
            }
        }
        self.folders = folders;
        let mut except = Vec::new();
        for p in &self.except_folders {
            if p.trim().is_empty() {
                continue;
            }
            let c = clean_folder(p).map_err(|e| format!("rule {n}: {e}"))?;
            if !except.contains(&c) {
                except.push(c);
            }
        }
        self.except_folders = except;
        if let Some(t) = &mut self.time {
            let mut w = t.clone();
            w.clean().map_err(|e| format!("rule {n}: {e}"))?;
            if w.from == w.to && w.days.is_empty() {
                return Err(format!(
                    "rule {n}: its hours are the whole day, every day: give days or other hours"
                ));
            }
            *t = w;
        }
        if self.use_.is_empty() {
            return Err(format!("rule {n}: which model it sends to is missing"));
        }
        if !members.contains(&self.use_) {
            return Err(format!("rule {n}: {} is not in the group", self.use_));
        }
        if !self.effort.is_empty()
            && self.effort != "on"
            && !EFFORTS.contains(&self.effort.as_str())
        {
            return Err(format!(
                "rule {n}: effort is on or one of {}, not {:?}",
                EFFORTS.join(", "),
                self.effort
            ));
        }
        if self.intent.chars().count() > MAX_INTENT {
            return Err(format!(
                "rule {n}: an intent is at most {MAX_INTENT} characters"
            ));
        }
        if self.conditions().is_empty() {
            return Err(format!(
                "rule {n}: it needs a condition (tokens, images, effort, agents, intent, compacting, time or folders)"
            ));
        }
        if self.pause
            && (self.tokens > 0
                || self.images
                || !self.effort.is_empty()
                || !self.intent.is_empty()
                || self.compact)
        {
            return Err(format!(
                "rule {n}: a pause holds by hours, days, agents or folders only"
            ));
        }
        Ok(())
    }

    /// Whether the request is one the rule sends to its member first
    /// (`Matches`); a pause rule sends none.
    pub fn matches(&self, q: &RuleRequest) -> bool {
        // A rule with an intent waits on the classifier's answer for the
        // turn; when it cannot say (`q.intent` empty), nothing matches.
        if !self.intent.is_empty() && !self.intent.eq_ignore_ascii_case(&q.intent) {
            return false;
        }
        self.matches_besides_intent(q)
    }

    /// `MatchesBesidesIntent`: every condition but the intent.
    pub fn matches_besides_intent(&self, q: &RuleRequest) -> bool {
        if self.pause {
            return false;
        }
        if self.tokens > 0 && q.tokens < self.tokens {
            return false;
        }
        if self.images && !q.images {
            return false;
        }
        if self.compact && !q.compact {
            return false;
        }
        if let Some(t) = &self.time
            && !t.holds(q.at)
        {
            return false;
        }
        if !self.folders.is_empty()
            && !self
                .folders
                .iter()
                .any(|root| folder_under(&q.folder, root))
        {
            return false;
        }
        // Outside every `exceptFolders`: an unknown folder ("") is under
        // none, so it is outside them (`inFolders`).
        if self
            .except_folders
            .iter()
            .any(|root| folder_under(&q.folder, root))
        {
            return false;
        }
        match self.effort.as_str() {
            "" => {}
            "on" => {
                if !q.thinking && q.effort.is_empty() {
                    return false;
                }
            }
            level => {
                let asked = EFFORTS.iter().position(|e| *e == q.effort);
                let want = EFFORTS
                    .iter()
                    .position(|e| *e == level)
                    .unwrap_or(usize::MAX);
                if asked.is_none_or(|a| a < want) {
                    return false;
                }
            }
        }
        if !self.agents.is_empty() && !self.agents.contains(&q.agent.to_ascii_lowercase()) {
            return false;
        }
        !self.conditions().is_empty()
    }

    /// Whether a pause rule holds for a request from `agent` at `at`
    /// (`Pauses`): by the clock, the agent, and the folder it says it is
    /// in — never what the request asks for.
    pub fn pauses(&self, agent: &str, folder: &str, at: Option<(u32, usize)>) -> bool {
        if !self.pause
            || (self.time.is_none()
                && self.agents.is_empty()
                && self.folders.is_empty()
                && self.except_folders.is_empty())
        {
            return false;
        }
        if let Some(t) = &self.time
            && !t.holds(at)
        {
            return false;
        }
        if !self.folders.is_empty() && !self.folders.iter().any(|root| folder_under(folder, root)) {
            return false;
        }
        if self
            .except_folders
            .iter()
            .any(|root| folder_under(folder, root))
        {
            return false;
        }
        self.agents.is_empty() || self.agents.contains(&agent.to_ascii_lowercase())
    }
}

/// The first of `rules` the request matches (`MatchRule`).
pub fn match_rule(rules: &[Rule], q: &RuleRequest) -> Option<usize> {
    rules.iter().position(|r| r.matches(q))
}

/// The members the rules after index `n` (0-based: rules after the one that
/// matched) that also match `q` send to, each once and in the rules' order,
/// leaving out `use_` (`ThenUses`).
pub fn then_uses(rules: &[Rule], q: &RuleRequest, n: usize, use_: &str) -> Vec<String> {
    let mut out = Vec::new();
    for r in rules.iter().skip(n) {
        if r.use_ != use_ && !out.contains(&r.use_) && r.matches(q) {
            out.push(r.use_.clone());
        }
    }
    out
}

/// Leave out of `routes` the members a pause rule holds for now
/// (`PausedOut`, lite's flat form: no groups inside groups).
pub fn paused_out(
    rules: &[Rule],
    routes: Vec<String>,
    agent: &str,
    folder: &str,
    at: Option<(u32, usize)>,
) -> Vec<String> {
    if !rules.iter().any(|r| r.pause) {
        return routes;
    }
    routes
        .into_iter()
        .filter(|route| {
            !rules
                .iter()
                .any(|r| &r.use_ == route && r.pauses(agent, folder, at))
        })
        .collect()
}

/// What a rule looks at in a request.
#[derive(Debug, Clone, Default)]
pub struct RuleRequest {
    pub tokens: u64,
    pub images: bool,
    /// The agent asked for reasoning at all.
    pub thinking: bool,
    /// The level it named, canonical (`low` … `max`), or "".
    pub effort: String,
    /// The agent, as [`agent_of`] names it.
    pub agent: String,
    /// The one of the rules' intents the classifier said the turn's
    /// message is; "" when it could not say (`Intent`).
    pub intent: String,
    /// The agent compacting its conversation.
    pub compact: bool,
    /// (minutes past midnight, weekday 0 = Sunday) in local time; `None` is
    /// in no window.
    pub at: Option<(u32, usize)>,
    /// The working folder the agent says the request is from; "" is in no
    /// rule's folders.
    pub folder: String,
}

/// The intents the classifier is to choose among for `q`: those of the
/// rules that match it but for intent, in order, stopping at the first
/// rule with none — a classifier is asked only when a rule that could be
/// the first to match waits on its intent (`Intents`).
pub fn intents(rules: &[Rule], q: &RuleRequest) -> Vec<String> {
    let mut out = Vec::new();
    for r in rules {
        if !r.matches_besides_intent(q) {
            continue;
        }
        if r.intent.is_empty() {
            break;
        }
        if !out
            .iter()
            .any(|s: &String| s.eq_ignore_ascii_case(&r.intent))
        {
            out.push(r.intent.clone());
        }
    }
    out
}

// ---- working folder ------------------------------------------------------

/// Folders are told apart without case on Windows and macOS, as their file
/// systems do (the reference's `foldCase`).
#[cfg(any(windows, target_os = "macos"))]
const FOLD_CASE: bool = true;
#[cfg(not(any(windows, target_os = "macos")))]
const FOLD_CASE: bool = false;

/// The home folder a rule's `~` stands for; a var so tests set it.
static RULE_HOME: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// Override the home `~` expands to (tests).
#[doc(hidden)]
pub fn set_rule_home(home: &str) {
    *RULE_HOME.lock().unwrap() = Some(home.to_owned());
}

fn rule_home() -> String {
    if let Some(h) = RULE_HOME.lock().unwrap().clone() {
        return h;
    }
    dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Put a folder the one way it is compared (`folderKey`): forward slashes,
/// `~` as the home folder, a Windows drive as WSL mounts it (`C:\x` and
/// `/mnt/c/x` are one), a WSL distro's share as the distro's own path, no
/// trailing slash, and without case where the system's folders have none.
/// `""` when it isn't a whole path.
pub fn folder_key(p: &str) -> String {
    let mut p = p.trim().replace('\\', "/");
    if p == "~" || p.starts_with("~/") {
        let h = rule_home().replace('\\', "/");
        if h.is_empty() {
            return String::new();
        }
        p = format!("{h}{}", &p[1..]);
    }
    let mut fold = FOLD_CASE;
    // \\wsl$\Distro\path or \\wsl.localhost\Distro\path → /path
    let lower = p.to_ascii_lowercase();
    if lower.starts_with("//wsl$") || lower.starts_with("//wsl.localhost") {
        // drop "//wsl$/distro" or "//wsl.localhost/distro" — the distro is
        // the segment after the share's own.
        let rest = &p[2..];
        let skip = rest
            .find('/')
            .map(|i| rest[i + 1..].find('/').map(|j| i + 1 + j));
        p = match skip.flatten() {
            Some(i) => rest[i..].to_owned(),
            None => "/".to_owned(),
        };
    }
    // C:\x → /mnt/c/x
    let bytes = p.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        p = format!(
            "/mnt/{}{}",
            (bytes[0] as char).to_ascii_lowercase(),
            &p[2..]
        );
    }
    // /mnt/C/x → /mnt/c/x, always case-folded
    if let Some(rest) = p.strip_prefix("/mnt/")
        && rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && rest.as_bytes().get(1).is_some_and(|b| *b == b'/')
    {
        p = format!(
            "/mnt/{}{}",
            (rest.as_bytes()[0] as char).to_ascii_lowercase(),
            &rest[1..]
        );
        fold = true; // a Windows drive, from either side
    }
    if !p.starts_with('/') {
        return String::new();
    }
    // clean the path: collapse // and ., resolve ..
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = if parts.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", parts.join("/"))
    };
    if fold {
        out = out.to_ascii_lowercase();
    }
    out
}

/// Whether `folder` is `root` or a folder inside it, at a path boundary
/// (`FolderUnder`). An unknown folder ("") is under none.
pub fn folder_under(folder: &str, root: &str) -> bool {
    let (f, r) = (folder_key(folder), folder_key(root));
    if f.is_empty() || r.is_empty() {
        return false;
    }
    f == r || r == "/" || f.starts_with(&format!("{r}/"))
}

/// Check a rule's folder is a whole path — `~/x`, `/x`, `C:\x` or `\\wsl…` —
/// and keep it as typed but for a trailing slash (`cleanFolder`).
pub fn clean_folder(p: &str) -> Result<String, String> {
    let mut p = p.trim().to_owned();
    let t = p.trim_end_matches(['/', '\\']);
    if !t.is_empty() && !(t.len() == 2 && t.as_bytes()[1] == b':') {
        p = t.to_owned(); // the root, and a drive's, keep theirs
    }
    let fwd = p.replace('\\', "/");
    let fb = fwd.as_bytes();
    let drive = fb.len() >= 2
        && fb[0].is_ascii_alphabetic()
        && fb[1] == b':'
        && (fb.len() == 2 || fb[2] == b'/');
    if p != "~"
        && !p.starts_with("~/")
        && !p.starts_with("~\\")
        && !p.starts_with('/')
        && !p.starts_with("\\\\")
        && !drive
    {
        return Err(format!(
            "the folder {p:?} is not a whole path (/Users/me/work, ~/work, C:\\work)"
        ));
    }
    Ok(p)
}

// ---- the working folder a request states ---------------------------------

/// The folder an agent works in, as it tells its model in the request —
/// for a group's rule by folder. Each agent says it its own way (the
/// reference's `workDir`): Claude Code's "Primary working directory:",
/// Codex's `<cwd>x</cwd>` in `<environment_context>`, Pi's `<cwd>` on its own
/// line, OpenCode's "Working directory:" within `<env>`. Of those it says,
/// the last one goes; assistant messages are skipped.
pub fn work_dir(req: &CompletionRequest) -> String {
    let mut dir = String::new();
    let mut pos = 0usize;
    for part in &req.system {
        work_dir_in(&part.text, &mut dir, &mut pos);
    }
    for m in &req.messages {
        if m.role == Role::Assistant {
            continue;
        }
        for p in &m.parts {
            if p.kind() == PartKind::Text {
                work_dir_in(&p.text, &mut dir, &mut pos);
            }
        }
    }
    dir
}

/// Decode the few entities agents put in a folder (`&lt;`, `&gt;`, `&amp;`
/// and friends — the reference's `html.UnescapeString`; a path only ever
/// carries these).
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The last folder `text` says at a position after `pos`, updating `dir`
/// and `pos` (`workDirIn`). `pos` orders finds across calls so the last in
/// the request wins, across system and user messages.
fn work_dir_in(text: &str, dir: &mut String, pos: &mut usize) {
    static CC: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?m)^(?: - )?Primary working directory: (.+)$").unwrap()
    });
    static CWD: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<cwd>\s*([^<\n]+?)\s*</cwd>").unwrap());
    static ENV_DIR: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^\s*Working directory: (.+)$").unwrap());
    static WAS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r" \(was .*\)$").unwrap());

    if !text.contains("irectory: ") && !text.contains("<cwd>") {
        return;
    }
    for m in CC.captures_iter(text) {
        let whole = m.get(0).unwrap();
        let v = unescape(WAS.replace(m.get(1).unwrap().as_str(), "").trim());
        if !v.is_empty() && whole.start() > *pos {
            *pos = whole.start();
            *dir = v;
        }
    }
    for m in CWD.captures_iter(text) {
        let whole = m.get(0).unwrap();
        let v = unescape(m.get(1).unwrap().as_str().trim());
        if !v.is_empty() && whole.start() > *pos {
            *pos = whole.start();
            *dir = v;
        }
    }
    // OpenCode's, only within its <env>
    let mut rest = text;
    let mut off = 0usize;
    loop {
        let Some(i) = rest.find("<env>") else { break };
        let Some(j) = rest[i..].find("</env>") else {
            break;
        };
        let block = &rest[i..i + j];
        if let Some(m) = ENV_DIR.captures(block) {
            let at = off + i + m.get(0).unwrap().start();
            let v = unescape(m.get(1).unwrap().as_str().trim());
            if !v.is_empty() && at > *pos {
                *pos = at;
                *dir = v;
            }
        }
        off += i + j;
        rest = &rest[i + j..];
    }
}

// ---- the agent compacting ------------------------------------------------

/// The agents' own compaction prompts (the reference's `compactSystems`).
const COMPACT_SYSTEMS: [&str; 5] = [
    "You are a helpful AI assistant tasked with summarizing conversations.",
    "You are a context summarization agent.",
    "You are a context summarization assistant.",
    "You are a specialized system component responsible for distilling chat history into a structured XML <state_snapshot>",
    "You are the component that summarizes a conversation when its context window is about to overflow.",
];

/// In the last user message of a compaction request (`compactAsks`).
const COMPACT_ASKS: [&str; 3] = [
    "Your task is to create a detailed summary of the conversation so far",
    "You are performing a CONTEXT CHECKPOINT COMPACTION.",
    "You are now given a task to compact this conversation context",
];

/// Whether `req` is an agent compacting its conversation (`compacting`).
pub fn compacting(req: &CompletionRequest) -> bool {
    let system: String = req
        .system
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if COMPACT_SYSTEMS.iter().any(|p| system.contains(p)) {
        return true;
    }
    for m in req.messages.iter().rev() {
        if m.role != Role::User {
            continue;
        }
        for p in &m.parts {
            if p.kind() != PartKind::Text {
                continue;
            }
            if COMPACT_ASKS.iter().any(|a| p.text.contains(a)) {
                return true;
            }
        }
        break;
    }
    false
}

// ---- the agent behind a User-Agent ---------------------------------------

/// A known agent's names: `(id, exact-names, UA-prefixes)`. A subset of the
/// reference's builtin agent table — the agents' ids and their UA prefixes,
/// plus magpie and curl. Unknown UAs keep their product name.
const KNOWN_AGENTS: &[(&str, &[&str], &[&str])] = &[
    (
        "claude",
        &["claude", "cc", "claude-code"],
        &["claude-cli", "claude-code"],
    ),
    ("claude-desktop", &["claude-desktop", "claude-app"], &[]),
    ("codex", &["codex"], &["codex"]),
    (
        "gemini",
        &["gemini", "gemini-cli"],
        &["geminicli", "gemini-cli"],
    ),
    ("agy", &["agy", "antigravity-cli"], &[]),
    (
        "opencode",
        &["opencode", "chamber", "openchamber"],
        &["opencode"],
    ),
    ("mimocode", &["mimocode"], &[]),
    ("pi", &["pi"], &["pi"]),
    ("stepcode", &["stepcode"], &["stepcode"]),
    ("aside", &["aside"], &[]),
    ("omo", &["omo"], &[]),
    ("prime-agent", &["prime-agent"], &[]),
    ("goose", &["goose"], &["goose"]),
    ("cursor", &["cursor", "cursor-agent"], &["cursor"]),
    ("zed", &["zed"], &["zed"]),
    (
        "vscode",
        &["vscode", "vs-code", "copilot-chat", "vscode-chat"],
        &["github-copilot-editing", "vscode"],
    ),
    (
        "vscode-insiders",
        &["vscode-insiders", "vs-code-insiders"],
        &[],
    ),
    ("vscodium", &["vscodium", "vscodium-chat"], &["codium"]),
    (
        "copilot",
        &["copilot", "gh-copilot"],
        &["copilot", "github-copilot"],
    ),
    ("crush", &["crush"], &["crush"]),
    ("dsh", &["dsh", "deepseek-harness"], &["deepseek-harness"]),
    ("alma", &["alma"], &["alma"]),
    ("atomcode", &["atomcode"], &["atomcode"]),
    ("cindy", &["cindy"], &["cindy"]),
    ("cline", &["cline", "cline-cli"], &["cline"]),
    (
        "codebuddy",
        &["codebuddy", "codebuddy-code", "cbc"],
        &["codebuddy"],
    ),
    (
        "commandcode",
        &["commandcode", "command-code", "cmd"],
        &["command-code", "commandcode"],
    ),
    (
        "droid",
        &["droid", "factory", "factory-droid"],
        &["factory-cli"],
    ),
    (
        "grok",
        &["grok", "grok-build", "grok-cli"],
        &["grok-shell", "grok-pager", "xai-grok-build"],
    ),
    (
        "hanako",
        &["hanako", "openhanako", "hana", "hanaagent"],
        &["hanaagent"],
    ),
    ("hermes", &["hermes", "hermes-agent"], &["hermes-agent"]),
    ("kimi", &["kimi", "kimi-code", "kimi-cli"], &["kimicli"]),
    ("minimax-code", &["minimax-code", "mcode"], &["mcode"]),
    (
        "morph",
        &["morph", "mistermorph", "mister-morph"],
        &["morph"],
    ),
    (
        "muse",
        &["muse", "muse-code", "musecode"],
        &["muse-build", "muse-code"],
    ),
    ("omp", &["omp", "oh-my-pi"], &["oh-my-pi"]),
    ("qwen", &["qwen", "qwen-code", "qwen-cli"], &["qwencode"]),
    ("reasonix", &["reasonix", "reasonix-studio"], &["reasonix"]),
    (
        "snow",
        &["snow", "snow-cli", "snow-ai", "snow-app"],
        &["snow"],
    ),
    ("t3code", &["t3code", "t3", "t3-code"], &["t3code"]),
    ("zcode", &["zcode", "z-code"], &["zcode"]),
    ("magpie", &["magpie"], &["magpie"]),
    ("curl", &["curl"], &["curl"]),
];

/// The agent behind a client User-Agent (the reference's `AgentOf`). Known
/// agents map to their id; anything else keeps its product name — `""` is
/// "other".
pub fn agent_of(ua: &str) -> String {
    let ua = ua.trim();
    let name = ua
        .split('/')
        .next()
        .unwrap_or("")
        .split(' ')
        .next()
        .unwrap_or("");
    let l = name.to_ascii_lowercase();
    // a name first: cursor-local is its own, though Cursor's UA begins it
    for (id, names, _) in KNOWN_AGENTS {
        if names.iter().any(|n| *n == l) {
            return (*id).to_owned();
        }
    }
    for (id, _, prefixes) in KNOWN_AGENTS {
        if prefixes.iter().any(|p| l.starts_with(p)) {
            return (*id).to_owned();
        }
    }
    if name.is_empty() {
        return "other".to_owned();
    }
    name.to_owned()
}

// ---- a turn's decision ----------------------------------------------------

/// What a group's rules decided for a conversation's turn, kept so the
/// requests within it — the agent handing tool results back — stay with it
/// (the reference's `turnRule`).
#[derive(Debug, Clone)]
struct TurnRule {
    turn: u64,
    /// The member the matching rule puts first; "" when none matched.
    use_: String,
    /// The rule that matched, 0-based, when one did.
    n: Option<usize>,
    /// What the classifier said the turn's message is, for a follow-up
    /// carrying on from it.
    intent: String,
    at: std::time::Instant,
    /// The tokens the vendor counted the conversation's last request as.
    input: u64,
    /// The estimate the request was taken to be.
    size: u64,
    /// When the vendor last answered, for cache-cold checks.
    answered: std::time::Instant,
}

static TURN_RULES: LazyLock<Mutex<HashMap<String, TurnRule>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What `rule_for` reports: which member goes first, which rule put it
/// there, and who comes after it (the reference's `RuleHit`, without the
/// trace-only fields lite doesn't keep).
#[derive(Debug, Clone, Default)]
pub struct RuleHit {
    /// The rule, 0-based; `None` when none matched.
    pub n: Option<usize>,
    /// The member it puts first.
    pub use_: String,
    /// The member's conditions, for the record.
    pub when: Vec<String>,
    /// The request is within the turn the decision was made for.
    pub held: bool,
    /// The turn began before magpie saw it (or its rule changed), so no
    /// rule moves it; the next turn is looked at afresh.
    pub waits: bool,
    /// Within the turn the conversation grew past what the model it was on
    /// can take, so the rules were looked at again — the one time a turn
    /// moves.
    pub grown: bool,
    /// The user's turns so far.
    pub turn: u64,
    /// How long the request was taken to be.
    pub tokens: u64,
    pub images: bool,
    pub effort: String,
    pub folder: String,
    /// The members the rules after it that match too send to (`Then`).
    pub then: Vec<String>,
    /// What the group's classifier was asked as the turn began, and said
    /// (the reference's `Classified`, without effort — lite never judges
    /// it — and without the decision models' confidence).
    pub classified: Option<Classified>,
    /// The request is the agent compacting its conversation.
    pub compact: bool,
    /// The members of rules it matched that take less than it is long,
    /// passed over.
    pub small: Vec<String>,
}

/// What the classifier was asked as a turn began, and said (`Classified`).
/// Told on the request's record; a group whose classifier fails says so
/// on its card through [`crate::classify::failures`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Classified {
    /// The classifier model, as the group names it.
    pub by: String,
    /// What it chose among: the intents of the rules that could match first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<String>,
    /// What it said the message is; "" when it could not say.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub intent: String,
    /// What it said the turn before was, which it was told; "" for none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub after: String,
    /// Said before, for the same message.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cached: bool,
    /// How long asking it took, in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ms: u64,
    /// Why it could not say: no intent matches then, unless `fallback`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// "previous" when it could not say and the turn keeps the intent the
    /// turn before had.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fallback: String,
}

fn is_zero(ms: &u64) -> bool {
    *ms == 0
}

fn is_false(cached: &bool) -> bool {
    !cached
}

/// The conversation a group's rules keep a turn's decision for: the agent's
/// session and the conversation's first words, as the reference's
/// `ruleKey`. Only what the user said is taken (the first user message
/// that has any) — not what the agent put in beside it, which an agent
/// changes as the conversation goes on — so the key stays the same for
/// the conversation's whole life (`firstWords`, #1826). A conversation
/// the user said nothing in yet goes by its first user message's text.
fn rule_key(group_id: &str, conversation: &str, req: &CompletionRequest) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut keyed = false;
    for m in &req.messages {
        if m.role != Role::User {
            continue;
        }
        let said = crate::classify::user_said(&m.parts);
        if !said.is_empty() {
            h.update(said.as_bytes());
            keyed = true;
            break;
        }
    }
    if !keyed {
        for m in &req.messages {
            if m.role != Role::User {
                continue;
            }
            for p in &m.parts {
                if p.kind() == PartKind::Text {
                    h.update(p.text.as_bytes());
                }
            }
            break;
        }
    }
    let sum = h.finalize();
    let hex12: String = sum[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("group/{group_id}|{conversation}|{hex12}")
}

/// The local wall-clock as (minutes past midnight, weekday 0 = Sunday) — a
/// rule's hours are of magpie's clock.
pub fn local_clock() -> Option<(u32, usize)> {
    use time::OffsetDateTime;
    let local = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    Some((
        u32::from(local.hour()) * 60 + u32::from(local.minute()),
        local.weekday().number_days_from_sunday() as usize,
    ))
}

/// What the vendor counted a turn's last request as, a floor for the next
/// one's estimate (`ruleAnswered` kept it); 0 when nothing was answered.
fn turn_input(group_id: &str, conversation: &str, req: &CompletionRequest) -> u64 {
    let key = rule_key(group_id, conversation, req);
    let map = TURN_RULES.lock().unwrap();
    map.get(&key)
        .filter(|tr| tr.at.elapsed() <= STICK_KEEP)
        .map(|tr| tr.input)
        .unwrap_or(0)
}

/// The intent the group's classifier gave the conversation's turn, "" when
/// none was asked or answered: a short follow-up carrying on from it is
/// judged as of its kind (`turnRule.intent`, the reference's `tr.intent`).
pub fn turn_intent(group_id: &str, conversation: &str, req: &CompletionRequest) -> String {
    let key = rule_key(group_id, conversation, req);
    let map = TURN_RULES.lock().unwrap();
    map.get(&key)
        .filter(|tr| tr.at.elapsed() <= STICK_KEEP)
        .map(|tr| tr.intent.clone())
        .unwrap_or_default()
}

/// What the group's rules look at in `req`: the estimate floored by what
/// the vendor counted last, images anywhere in it, the folder when a rule
/// holds by folder, and a compaction as its own match (`q` in ruleFor).
pub fn rule_query(
    rules: &[Rule],
    group_id: &str,
    req: &CompletionRequest,
    agent: &str,
    conversation: &str,
) -> RuleRequest {
    let size = crate::roomy::estimate(req);
    let mut q = RuleRequest {
        tokens: size,
        thinking: req.effort.is_some_and(|e| e.asked()),
        effort: req
            .effort
            .and_then(|e| e.wire_level())
            .unwrap_or("")
            .to_owned(),
        agent: agent.to_owned(),
        at: local_clock(),
        ..Default::default()
    };
    for m in &req.messages {
        if m.parts.iter().any(|p| p.kind() == PartKind::Image) {
            q.images = true;
            break;
        }
    }
    // what the vendor counted last, a floor
    let floor = turn_input(group_id, conversation, req);
    if floor > q.tokens {
        q.tokens = floor;
    }
    if rules
        .iter()
        .any(|r| !r.folders.is_empty() || !r.except_folders.is_empty())
    {
        q.folder = work_dir(req);
    }
    if rules.iter().any(|r| r.compact) && compacting(req) {
        q.compact = true;
    }
    q
}

/// The rule a group request goes by (`ruleFor`, lite's flat form — no
/// groups inside groups). At a turn's start the first matching rule
/// decides; within the turn — tool results going back — what it decided
/// holds, unless the conversation outgrew that model's window and a rule
/// sends it to one with more room.
///
/// `windows` are the context windows of the group's routes by route id, for
/// the outgrowth check and a compaction's too-small pass-over; a member
/// with none (`0` or absent) is never left for it.
///
/// `intent` is what the group's classifier said the turn's message is, ""
/// when it could not say or was not asked; a rule with an intent of its
/// own only matches that. Within the turn the stored intent is gone by —
/// the classifier is only asked as the turn begins — whatever `intent`
/// says.
pub fn rule_for(
    group_id: &str,
    rules: &[Rule],
    req: &CompletionRequest,
    agent: &str,
    conversation: &str,
    windows: &HashMap<String, u64>,
    intent: &str,
) -> Option<RuleHit> {
    if rules.is_empty() {
        return None;
    }
    let (turn, within) = crate::affinity::turn_of(req);
    let now = std::time::Instant::now();
    let size = crate::roomy::estimate(req);
    let mut q = rule_query(rules, group_id, req, agent, conversation);
    q.intent = intent.to_owned();
    let key = rule_key(group_id, conversation, req);
    let mut map = TURN_RULES.lock().unwrap();
    let tr = map
        .get(&key)
        .filter(|tr| now.duration_since(tr.at) <= STICK_KEEP)
        .cloned();
    let mut hit = RuleHit {
        turn,
        tokens: q.tokens,
        images: q.images,
        effort: q.effort.clone(),
        folder: q.folder.clone(),
        ..Default::default()
    };
    if hit.effort.is_empty() && q.thinking {
        hit.effort = "on".to_owned();
    }
    // The agent summarizing its conversation: the first rule it matches, on
    // its own — not the turn's decision, so the requests after it go on as
    // the turn would. A model known to take less than the request is passed
    // over; it would only refuse it.
    if rules.iter().any(|r| r.compact) && compacting(req) {
        q.compact = true;
        hit.compact = true;
        for (i, r) in rules.iter().enumerate() {
            if !r.matches(&q) {
                continue;
            }
            let c = windows.get(&r.use_).copied().unwrap_or(0);
            if c > 0 && c < q.tokens {
                hit.small.push(r.use_.clone());
                continue;
            }
            hit.n = Some(i);
            hit.use_ = r.use_.clone();
            hit.when = r.conditions();
            break;
        }
        if let Some(n) = hit.n {
            hit.then = then_uses(rules, &q, n + 1, &hit.use_);
        }
        return Some(hit);
    }
    if within {
        // the same turn: what was decided as it began, while the group
        // still has that rule; else nothing moves it
        if let Some(tr) = &tr {
            q.intent = tr.intent.clone(); // as the classifier said when the turn began
        }
        match &tr {
            None => hit.waits = true,
            Some(tr) if tr.turn != turn && turn > 0 => hit.waits = true,
            Some(tr) if tr.use_.is_empty() => hit.held = true,
            Some(tr)
                if tr.n.is_some()
                    && tr.n.unwrap() < rules.len()
                    && rules[tr.n.unwrap()].use_ == tr.use_ =>
            {
                hit.held = true;
                hit.n = tr.n;
                hit.use_ = tr.use_.clone();
                hit.when = rules[tr.n.unwrap()].conditions();
            }
            Some(_) => hit.waits = true,
        }
        // grown past the model the turn is on: the one time a turn moves
        let limit = if !hit.use_.is_empty() {
            windows.get(&hit.use_).copied().unwrap_or(0)
        } else {
            windows
                .values()
                .copied()
                .filter(|c| *c > 0)
                .min()
                .unwrap_or(0)
        };
        if limit > 0
            && q.tokens >= limit * 95 / 100
            && let Some(i) = match_rule(rules, &q)
            && rules[i].use_ != hit.use_
            && windows.get(&rules[i].use_).copied().unwrap_or(0) > limit
        {
            hit.held = false;
            hit.waits = false;
            hit.grown = true;
            hit.n = Some(i);
            hit.use_ = rules[i].use_.clone();
            hit.when = rules[i].conditions();
            map.insert(
                key,
                TurnRule {
                    turn,
                    use_: hit.use_.clone(),
                    n: hit.n,
                    intent: q.intent.clone(),
                    at: now,
                    input: tr.as_ref().map(|t| t.input).unwrap_or(0),
                    size,
                    answered: tr.as_ref().map(|t| t.answered).unwrap_or(now),
                },
            );
        } else if let Some(mut cur) = tr.clone() {
            cur.at = now;
            cur.size = size;
            map.insert(key, cur);
        }
        if let Some(n) = hit.n {
            hit.then = then_uses(rules, &q, n + 1, &hit.use_);
        }
        return Some(hit);
    }
    // a new turn
    if let Some(i) = match_rule(rules, &q) {
        hit.n = Some(i);
        hit.use_ = rules[i].use_.clone();
        hit.when = rules[i].conditions();
    }
    map.insert(
        key.clone(),
        TurnRule {
            turn,
            use_: hit.use_.clone(),
            n: hit.n,
            intent: q.intent.clone(),
            at: now,
            input: tr.as_ref().map(|t| t.input).unwrap_or(0),
            size,
            answered: tr.as_ref().map(|t| t.answered).unwrap_or(now),
        },
    );
    if map.len() > TURNS_KEPT {
        map.retain(|_, t| now.duration_since(t.at) <= STICK_KEEP);
    }
    if let Some(n) = hit.n {
        hit.then = then_uses(rules, &q, n + 1, &hit.use_);
    }
    Some(hit)
}

/// Keep what the vendor counted a conversation's last request as, for its
/// next turn's rules (`ruleAnswered`).
pub fn rule_answered(
    group_id: &str,
    conversation: &str,
    req: &CompletionRequest,
    usage: &magpie_core::ir::Usage,
) {
    let n =
        usage.input.unwrap_or(0) + usage.cache_read.unwrap_or(0) + usage.cache_write.unwrap_or(0);
    if n == 0 {
        return;
    }
    let key = rule_key(group_id, conversation, req);
    let mut map = TURN_RULES.lock().unwrap();
    if let Some(tr) = map.get_mut(&key) {
        tr.input = n;
        tr.answered = std::time::Instant::now();
    }
}

/// Forget everything (tests).
#[doc(hidden)]
pub fn reset() {
    TURN_RULES.lock().unwrap().clear();
    *RULE_HOME.lock().unwrap() = None;
}
