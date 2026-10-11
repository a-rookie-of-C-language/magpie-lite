//! Preview and apply use the same adapters. Journals contain secrets and never leave this module.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const JOURNAL_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentId {
    Claude,
    Codex,
    Crush,
    Gemini,
    Goose,
    Opencode,
    Zed,
}
impl AgentId {
    pub const ALL: [Self; 7] = [
        Self::Claude,
        Self::Codex,
        Self::Crush,
        Self::Gemini,
        Self::Goose,
        Self::Opencode,
        Self::Zed,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Crush => "crush",
            Self::Gemini => "gemini",
            Self::Goose => "goose",
            Self::Opencode => "opencode",
            Self::Zed => "zed",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Crush => "Crush",
            Self::Gemini => "Gemini CLI",
            Self::Goose => "Goose",
            Self::Opencode => "OpenCode",
            Self::Zed => "Zed",
        }
    }
}
impl std::str::FromStr for AgentId {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::ALL.into_iter().find(|id| id.id() == value)
            .ok_or_else(|| invalid("unsupported agent; choose claude, codex, crush, gemini, goose, opencode or zed"))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Connect,
    Disconnect,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Invalid,
    Conflict,
    Unavailable,
}
#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: &'static str,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for Error {}
pub(super) type Result<T> = std::result::Result<T, Error>;
fn invalid(message: &'static str) -> Error {
    Error {
        kind: ErrorKind::Invalid,
        message,
    }
}
pub(super) fn conflict(message: &'static str) -> Error {
    Error {
        kind: ErrorKind::Conflict,
        message,
    }
}
fn unavailable(message: &'static str) -> Error {
    Error {
        kind: ErrorKind::Unavailable,
        message,
    }
}

/// A borrowed gateway snapshot. Never serialized or included in diagnostics.
pub struct Gateway<'a> {
    pub port: u16,
    pub token: &'a str,
}
#[derive(Debug, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub operation: &'static str,
    pub before_bytes: usize,
    pub after_bytes: usize,
}
#[derive(Debug, Serialize)]
pub struct Preview {
    pub agent: AgentId,
    pub action: Action,
    pub model: Option<String>,
    pub revision: String,
    pub files: Vec<FileChange>,
    pub notice: &'static str,
}
#[derive(Debug, Serialize)]
pub struct Status {
    pub id: AgentId,
    pub name: &'static str,
    pub detected: bool,
    pub connected: bool,
    pub model: Option<String>,
    pub drifted: bool,
    pub recovery_needed: bool,
    pub files: Vec<PathBuf>,
    pub notice: &'static str,
}
fn notice(id: AgentId) -> &'static str {
    if id == AgentId::Zed {
        "Restart Zed; set MAGPIE_LITE_API_KEY in Zed's environment or enter the gateway token in its provider settings. No key is written to settings.json."
    } else {
        "Restart the agent after connecting or restoring. User-level files only; project settings and environment variables may override them."
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Image {
    pub(super) data: Option<String>,
    pub(super) mode: Option<u32>,
}
impl Image {
    fn capture(path: &Path) -> Result<Self> {
        check_path(path)?;
        let data = read_optional(path, FILE_LIMIT)?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(path).ok().map(|m| m.permissions().mode())
        };
        #[cfg(not(unix))]
        let mode = None;
        Ok(Self {
            data: data.map(|bytes| STANDARD.encode(bytes)),
            mode,
        })
    }
    pub(super) fn bytes(&self) -> Result<Option<Vec<u8>>> {
        self.data
            .as_ref()
            .map(|data| {
                STANDARD
                    .decode(data)
                    .map_err(|_| conflict("agent journal is invalid"))
            })
            .transpose()
    }
    fn len(&self) -> usize {
        self.bytes().ok().flatten().map_or(0, |bytes| bytes.len())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Connecting,
    Connected,
    Disconnecting,
    Disconnected,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    baseline: Image,
    applied: Image,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owned: Option<Vec<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_base: Option<Image>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    schema: u32,
    agent: AgentId,
    model: String,
    phase: Phase,
    files: Vec<Entry>,
    rollback: Option<Vec<Image>>,
    prior_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore: Option<Vec<Image>>,
}
struct Plan {
    paths: Vec<PathBuf>,
    preview: Preview,
    before: Vec<Image>,
    after: Vec<Image>,
    journal: Option<Journal>,
    restored: Option<Vec<Image>>,
}

/// A manager owns only enumerated agent paths; callers cannot supply arbitrary filenames.
/// The OS lock serializes CLI and HTTP operations using the same state directory.
pub struct Manager {
    state_dir: PathBuf,
    targets: BTreeMap<AgentId, Vec<PathBuf>>,
    local: Mutex<()>,
}
impl Manager {
    pub fn native(state_dir: PathBuf) -> Result<Self> {
        let crush =
            super::crush::paths().map_err(|_| unavailable("cannot locate Crush configuration"))?;
        let mut targets = BTreeMap::new();
        targets.insert(
            AgentId::Claude,
            vec![
                super::claude::settings_path()
                    .map_err(|_| unavailable("cannot locate Claude configuration"))?,
            ],
        );
        targets.insert(
            AgentId::Codex,
            vec![
                super::codex::config_path()
                    .map_err(|_| unavailable("cannot locate Codex configuration"))?,
            ],
        );
        targets.insert(AgentId::Crush, vec![crush.0, crush.1]);
        targets.insert(
            AgentId::Gemini,
            vec![
                super::gemini::settings_path()
                    .map_err(|_| unavailable("cannot locate Gemini configuration"))?,
                super::gemini::env_path()
                    .map_err(|_| unavailable("cannot locate Gemini credentials"))?,
            ],
        );
        let goose = super::goose::config_path()
            .map_err(|_| unavailable("cannot locate Goose configuration"))?;
        let custom = goose
            .parent()
            .ok_or_else(|| unavailable("cannot locate Goose provider directory"))?
            .join("custom_providers/magpie-lite.json");
        targets.insert(AgentId::Goose, vec![goose, custom]);
        targets.insert(
            AgentId::Opencode,
            vec![
                super::opencode::config_path()
                    .map_err(|_| unavailable("cannot locate OpenCode configuration"))?,
            ],
        );
        targets.insert(
            AgentId::Zed,
            vec![
                super::zed::settings_path()
                    .map_err(|_| unavailable("cannot locate Zed configuration"))?,
            ],
        );
        Self::new(state_dir, targets)
    }
    /// Isolated layout, without changing process-wide HOME variables.
    pub fn at_home(home: &Path, state_dir: PathBuf) -> Result<Self> {
        let mut targets = BTreeMap::new();
        targets.insert(AgentId::Claude, vec![home.join(".claude/settings.json")]);
        targets.insert(AgentId::Codex, vec![home.join(".codex/config.toml")]);
        targets.insert(
            AgentId::Crush,
            vec![
                home.join(".config/crush/crush.json"),
                home.join(".local/share/crush/crush.json"),
            ],
        );
        targets.insert(
            AgentId::Gemini,
            vec![
                home.join(".gemini/settings.json"),
                home.join(".gemini/.env"),
            ],
        );
        targets.insert(
            AgentId::Goose,
            vec![
                home.join(".config/goose/config.yaml"),
                home.join(".config/goose/custom_providers/magpie-lite.json"),
            ],
        );
        let opencode = ["opencode.jsonc", "opencode.json", "config.json"]
            .into_iter()
            .map(|name| home.join(".config/opencode").join(name))
            .find(|path| path.exists())
            .unwrap_or_else(|| home.join(".config/opencode/opencode.json"));
        targets.insert(AgentId::Opencode, vec![opencode]);
        targets.insert(AgentId::Zed, vec![home.join(".config/zed/settings.json")]);
        Self::new(state_dir, targets)
    }
    fn new(state_dir: PathBuf, mut targets: BTreeMap<AgentId, Vec<PathBuf>>) -> Result<Self> {
        for paths in targets.values_mut() {
            paths.dedup();
        }
        if !state_dir.is_absolute() || targets.values().flatten().any(|p| !p.is_absolute()) {
            return Err(invalid(
                "agent configuration and state paths must be absolute",
            ));
        }
        check_path(&state_dir)?;
        fs::create_dir_all(&state_dir)
            .map_err(|_| unavailable("cannot create agent state directory"))?;
        private_directory(&state_dir)?;
        Ok(Self {
            state_dir,
            targets,
            local: Mutex::new(()),
        })
    }
    fn lock(&self) -> Result<(std::sync::MutexGuard<'_, ()>, File)> {
        let guard = self
            .local
            .lock()
            .map_err(|_| unavailable("agent manager lock failed"))?;
        let path = self.state_dir.join("operations.lock");
        check_path(&path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| unavailable("cannot open agent operation lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| conflict("another agent operation is in progress; retry shortly"))?;
        Ok((guard, lock))
    }
    fn paths(&self, id: AgentId) -> Result<Vec<PathBuf>> {
        if id != AgentId::Opencode {
            return Ok(self.targets[&id].clone());
        }
        let dir = self.targets[&id][0]
            .parent()
            .ok_or_else(|| invalid("agent path has no parent"))?;
        // A connected/pending journal owns its original path even if OpenCode's
        // precedence changed. Disconnect/recovery can still restore that file.
        if let Some(journal) = self.read_journal(id)?
            && journal.phase != Phase::Disconnected
        {
            if journal.files.len() != 1
                || !["opencode.jsonc", "opencode.json", "config.json"]
                    .into_iter()
                    .any(|name| journal.files[0].path == dir.join(name))
            {
                return Err(conflict(
                    "agent journal does not match the current adapter paths",
                ));
            }
            return Ok(vec![journal.files[0].path.clone()]);
        }
        Ok(vec![super::opencode::preferred_path(dir)])
    }
    fn path_overridden(&self, id: AgentId, paths: &[PathBuf]) -> bool {
        id == AgentId::Opencode
            && paths[0]
                .parent()
                .is_some_and(|dir| super::opencode::preferred_path(dir) != paths[0])
    }
    fn journal_path(&self, id: AgentId) -> PathBuf {
        self.state_dir.join(format!("{}.json", id.id()))
    }
    fn read_journal(&self, id: AgentId) -> Result<Option<Journal>> {
        let Some(bytes) = read_optional(&self.journal_path(id), JOURNAL_LIMIT)? else {
            return Ok(None);
        };
        let journal: Journal = serde_json::from_slice(&bytes)
            .map_err(|_| conflict("agent journal is invalid; do not delete its backups"))?;
        Ok(Some(journal))
    }
    fn load(&self, id: AgentId) -> Result<Option<Journal>> {
        let Some(mut journal) = self.read_journal(id)? else {
            return Ok(None);
        };
        if ![1, 2].contains(&journal.schema)
            || journal.agent != id
            || journal
                .files
                .iter()
                .map(|f| &f.path)
                .ne(self.paths(id)?.iter())
                && !(id == AgentId::Opencode
                    && journal.phase == Phase::Disconnected
                    && journal.files.len() == 1
                    && ["opencode.jsonc", "opencode.json", "config.json"]
                        .into_iter()
                        .any(|name| {
                            self.targets[&id][0]
                                .parent()
                                .is_some_and(|dir| journal.files[0].path == dir.join(name))
                        }))
            || journal
                .rollback
                .as_ref()
                .is_some_and(|r| r.len() != journal.files.len())
        {
            return Err(conflict(
                "agent journal does not match the current adapter paths",
            ));
        }
        let pending_connect = journal.phase == Phase::Connecting;
        let pending_disconnect = journal.phase == Phase::Disconnecting;
        let old = journal.schema == 1;
        if (if old {
            pending_connect
        } else {
            pending_connect || pending_disconnect
        }) != journal.rollback.is_some()
            || (!pending_connect && journal.prior_model.is_some())
            || (if old { false } else { pending_disconnect }) != journal.restore.is_some()
            || journal
                .restore
                .as_ref()
                .is_some_and(|r| r.len() != journal.files.len())
        {
            return Err(conflict("agent recovery images are inconsistent"));
        }
        for image in journal
            .files
            .iter()
            .flat_map(|entry| [&entry.baseline, &entry.applied])
            .chain(
                journal
                    .files
                    .iter()
                    .filter_map(|entry| entry.restore_base.as_ref()),
            )
            .chain(journal.rollback.iter().flatten())
            .chain(journal.restore.iter().flatten())
        {
            if image
                .bytes()?
                .is_some_and(|bytes| bytes.len() as u64 > FILE_LIMIT)
                || (image.data.is_none() && image.mode.is_some())
            {
                return Err(conflict(
                    "agent journal exceeds file limits or has invalid images",
                ));
            }
        }
        for (index, entry) in journal.files.iter_mut().enumerate() {
            if old {
                if entry.owned.is_some() || entry.restore_base.is_some() {
                    return Err(conflict(
                        "legacy agent journal contains unexpected ownership",
                    ));
                }
                entry.restore_base = Some(entry.baseline.clone());
                entry.owned = Some(super::selective::ownership(
                    id,
                    index,
                    &entry.baseline,
                    &entry.applied,
                    true,
                )?);
            }
            if entry.restore_base.is_none() {
                return Err(conflict(
                    "agent journal is missing its retained restoration image",
                ));
            }
            let owned = entry
                .owned
                .as_ref()
                .ok_or_else(|| conflict("agent journal is missing managed-field ownership"))?;
            if owned.is_empty()
                || owned.len() > 128
                || owned.iter().any(|p| {
                    p.is_empty() || p.len() > 16 || p.iter().any(|k| k.is_empty() || k.len() > 4096)
                })
            {
                return Err(conflict(
                    "agent journal has invalid managed-field ownership",
                ));
            }
        }
        if old && pending_disconnect {
            journal.rollback = Some(journal.files.iter().map(|e| e.applied.clone()).collect());
            journal.restore = Some(journal.files.iter().map(|e| e.baseline.clone()).collect());
        }
        journal.schema = 2;
        Ok(Some(journal))
    }
    fn save(&self, journal: &Journal) -> Result<()> {
        #[cfg(windows)]
        {
            let path = self.journal_path(journal.agent);
            check_path(&path)?;
            if path
                .try_exists()
                .map_err(|_| unavailable("cannot inspect agent journal"))?
            {
                super::windows::make_private(&path, false)
                    .map_err(|_| unavailable("cannot restrict agent journal"))?;
            }
        }
        let bytes =
            serde_json::to_vec(journal).map_err(|_| unavailable("cannot encode agent journal"))?;
        atomic(
            &self.journal_path(journal.agent),
            &Image {
                data: Some(STANDARD.encode(bytes)),
                mode: Some(0o600),
            },
        )
    }
    pub fn statuses(&self) -> Result<Vec<Status>> {
        let _lock = self.lock()?;
        AgentId::ALL.into_iter().map(|id| self.status(id)).collect()
    }
    fn status(&self, id: AgentId) -> Result<Status> {
        let journal = self.load(id)?;
        let connected = journal
            .as_ref()
            .is_some_and(|j| j.phase == Phase::Connected);
        let recovery_needed = journal
            .as_ref()
            .is_some_and(|j| matches!(j.phase, Phase::Connecting | Phase::Disconnecting));
        let paths = self.paths(id)?;
        let drifted = connected
            && (self.path_overridden(id, &paths)
                || journal.as_ref().is_some_and(|j| {
                    j.files.iter().enumerate().any(|(index, entry)| {
                        Image::capture(&entry.path)
                            .and_then(|current| {
                                super::selective::restore(
                                    id,
                                    index,
                                    &entry.baseline,
                                    &entry.applied,
                                    &current,
                                    entry.owned.as_ref().unwrap(),
                                )
                            })
                            .is_err()
                    })
                }));
        Ok(Status {
            id,
            name: id.name(),
            detected: self
                .paths(id)?
                .iter()
                .any(|p| p.exists() || p.parent().is_some_and(Path::is_dir))
                || binary_detected(id),
            connected,
            model: journal
                .filter(|j| j.phase != Phase::Disconnected)
                .map(|j| j.model),
            drifted,
            recovery_needed,
            files: self.paths(id)?,
            notice: notice(id),
        })
    }
    pub fn preview(
        &self,
        id: AgentId,
        action: Action,
        model: Option<&str>,
        gateway: Gateway<'_>,
    ) -> Result<Preview> {
        let _lock = self.lock()?;
        Ok(self.plan(id, action, model, gateway)?.preview)
    }
    fn plan(
        &self,
        id: AgentId,
        action: Action,
        model: Option<&str>,
        gateway: Gateway<'_>,
    ) -> Result<Plan> {
        let journal = self.load(id)?;
        if action == Action::Connect && self.path_overridden(id, &self.paths(id)?) {
            return Err(conflict(
                "OpenCode selects another configuration file; disconnect the previous managed connection first",
            ));
        }
        if journal
            .as_ref()
            .is_some_and(|j| matches!(j.phase, Phase::Connecting | Phase::Disconnecting))
        {
            return Err(conflict(
                "unfinished agent operation requires recovery before another change",
            ));
        }
        let paths = self.paths(id)?;
        let before = paths
            .iter()
            .map(|p| Image::capture(p))
            .collect::<Result<Vec<_>>>()?;
        let restored = if let Some(j) = journal.as_ref().filter(|j| j.phase == Phase::Connected) {
            Some(
                j.files
                    .iter()
                    .zip(&before)
                    .enumerate()
                    .map(|(index, (entry, current))| restore_entry(id, index, entry, current))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            None
        };
        let (after, model) = match action {
            Action::Connect => {
                let model = model
                    .filter(|m| {
                        !m.is_empty()
                            && m.len() <= 1024
                            && !m.chars().any(char::is_control)
                            && !m.chars().any(char::is_whitespace)
                    })
                    .ok_or_else(|| invalid("choose a non-empty agent model without whitespace"))?;
                if gateway.port == 0
                    || gateway.token.is_empty()
                    || gateway
                        .token
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace())
                {
                    return Err(invalid(
                        "gateway port or token cannot be used in agent configuration",
                    ));
                }
                if model.contains(gateway.token) {
                    return Err(invalid("model cannot contain gateway credentials"));
                }
                (
                    self.render(id, &paths, &before, model, &gateway)?,
                    Some(model.to_owned()),
                )
            }
            Action::Disconnect => {
                let j = journal
                    .as_ref()
                    .filter(|j| j.phase == Phase::Connected)
                    .ok_or_else(|| conflict("agent has no managed connection to restore"))?;
                (restored.clone().unwrap(), Some(j.model.clone()))
            }
        };
        if after.iter().any(|image| image.len() as u64 > FILE_LIMIT) {
            return Err(invalid("generated agent file exceeds its size limit"));
        }
        let mut hash = Sha256::new();
        hash.update(b"magpie-lite-agent-preview-v1");
        hash.update(
            serde_json::to_vec(&(id, action, &model, &paths, &before, &after, &journal))
                .map_err(|_| unavailable("cannot build preview revision"))?,
        );
        let files = paths
            .iter()
            .zip(&before)
            .zip(&after)
            .map(|((path, before), after)| FileChange {
                path: path.clone(),
                operation: if before == after {
                    "unchanged"
                } else if after.data.is_none() {
                    "delete"
                } else if before.data.is_none() {
                    "create"
                } else {
                    "update"
                },
                before_bytes: before.len(),
                after_bytes: after.len(),
            })
            .collect();
        Ok(Plan {
            paths,
            preview: Preview {
                agent: id,
                action,
                model,
                revision: format!("{:x}", hash.finalize()),
                files,
                notice: notice(id),
            },
            before,
            after,
            journal,
            restored,
        })
    }
    fn render(
        &self,
        id: AgentId,
        source_paths: &[PathBuf],
        before: &[Image],
        model: &str,
        gateway: &Gateway<'_>,
    ) -> Result<Vec<Image>> {
        let staging = tempfile::tempdir_in(&self.state_dir)
            .map_err(|_| unavailable("cannot create private preview directory"))?;
        let paths: Vec<_> = source_paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                if id == AgentId::Goose && index == 1 {
                    staging.path().join("custom_providers/magpie-lite.json")
                } else {
                    staging
                        .path()
                        .join(path.file_name().unwrap_or_default())
                        .with_file_name(format!(
                            "{index}-{}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ))
                }
            })
            .collect();
        for (path, image) in paths.iter().zip(before) {
            if image.data.is_some() {
                atomic(path, image)?;
            }
        }
        let result = match id {
            AgentId::Claude => {
                super::claude::wire_at(&paths[0], model, gateway.port, gateway.token)
            }
            AgentId::Codex => super::codex::wire_at(&paths[0], model, gateway.port, gateway.token),
            AgentId::Crush => super::crush::wire_at(
                &paths[0],
                paths.get(1).unwrap_or(&paths[0]),
                model,
                gateway.port,
                gateway.token,
            ),
            AgentId::Gemini => {
                super::gemini::wire_at(&paths[0], &paths[1], model, gateway.port, gateway.token)
            }
            AgentId::Goose => super::goose::wire_at(&paths[0], model, gateway.port, gateway.token),
            AgentId::Opencode => {
                super::opencode::wire_at(&paths[0], model, gateway.port, gateway.token)
            }
            AgentId::Zed => super::zed::wire_at(&paths[0], model, gateway.port, gateway.token),
        };
        result.map_err(|_| {
            invalid("agent adapter could not parse or edit the existing configuration")
        })?;
        paths
            .iter()
            .zip(before)
            .map(|(path, before)| {
                let mut image = Image::capture(path)?;
                image.mode = before.mode;
                #[cfg(unix)]
                {
                    image.mode = Some(0o600);
                }
                Ok(image)
            })
            .collect()
    }
    pub fn apply(
        &self,
        id: AgentId,
        action: Action,
        model: Option<&str>,
        revision: &str,
        gateway: Gateway<'_>,
    ) -> Result<Status> {
        let _lock = self.lock()?;
        let plan = self.plan(id, action, model, gateway)?;
        if revision != plan.preview.revision {
            return Err(conflict(
                "agent preview is stale; preview the operation again",
            ));
        }
        let mut journal = match action {
            Action::Connect => Journal {
                schema: 2,
                agent: id,
                model: plan.preview.model.clone().unwrap(),
                phase: Phase::Connecting,
                files: self
                    .paths(id)?
                    .iter()
                    .zip(&plan.before)
                    .zip(&plan.after)
                    .enumerate()
                    .map(|(index, ((path, before), after))| {
                        let prior = plan
                            .journal
                            .as_ref()
                            .filter(|j| j.phase == Phase::Connected)
                            .map(|j| &j.files[index]);
                        Ok(Entry {
                            path: path.clone(),
                            baseline: prior.map_or_else(|| before.clone(), |e| e.baseline.clone()),
                            applied: after.clone(),
                            restore_base: Some(
                                plan.restored
                                    .as_ref()
                                    .map_or_else(|| before.clone(), |images| images[index].clone()),
                            ),
                            owned: Some(match prior {
                                Some(e) => e.owned.clone().unwrap(),
                                None => {
                                    super::selective::ownership(id, index, before, after, false)?
                                }
                            }),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                rollback: Some(plan.before.clone()),
                prior_model: plan
                    .journal
                    .as_ref()
                    .filter(|j| j.phase == Phase::Connected)
                    .map(|j| j.model.clone()),
                restore: None,
            },
            Action::Disconnect => {
                let mut j = plan.journal.unwrap();
                j.phase = Phase::Disconnecting;
                j.rollback = Some(plan.before.clone());
                j.restore = Some(plan.after.clone());
                j
            }
        };
        self.save(&journal)?; // The recovery image is durable before any agent file changes.
        for ((path, before), after) in plan.paths.iter().zip(&plan.before).zip(&plan.after) {
            if Image::capture(path)? != *before {
                return Err(conflict(
                    "agent file changed during apply; recovery is required",
                ));
            }
            if before != after {
                atomic(path, after)?;
            }
        }
        journal.phase = match action {
            Action::Connect => Phase::Connected,
            Action::Disconnect => Phase::Disconnected,
        };
        journal.rollback = None;
        journal.restore = None;
        journal.prior_model = None;
        self.save(&journal)?;
        self.status(id)
    }
    /// Connecting rolls back to the prior connection; disconnecting finishes restoration.
    /// If a third party changed either image, recovery refuses to overwrite it.
    pub fn recover(&self, id: AgentId) -> Result<Status> {
        let _lock = self.lock()?;
        let mut j = self
            .load(id)?
            .ok_or_else(|| conflict("agent has no operation journal"))?;
        let targets = match j.phase {
            Phase::Connecting => j
                .rollback
                .clone()
                .filter(|r| r.len() == j.files.len())
                .ok_or_else(|| conflict("agent recovery images are missing"))?,
            Phase::Disconnecting => j
                .restore
                .clone()
                .ok_or_else(|| conflict("agent restoration targets are missing"))?,
            _ => return self.status(id),
        };
        let before = if j.phase == Phase::Disconnecting {
            j.rollback
                .clone()
                .ok_or_else(|| conflict("agent restoration inputs are missing"))?
        } else {
            j.files.iter().map(|e| e.applied.clone()).collect()
        };
        // Validate every file before changing any: no partial recovery of third-party drift.
        for ((entry, prior), target) in j.files.iter().zip(&before).zip(&targets) {
            let current = Image::capture(&entry.path)?;
            if current != *prior && current != *target {
                return Err(conflict(
                    "agent files changed since the interrupted operation; recovery cannot overwrite them",
                ));
            }
        }
        for ((entry, prior), target) in j.files.iter().zip(&before).zip(&targets) {
            let current = Image::capture(&entry.path)?;
            if current != *prior && current != *target {
                return Err(conflict(
                    "agent file changed during recovery; recovery cannot overwrite it",
                ));
            }
            if current != *target {
                atomic(&entry.path, target)?;
            }
        }
        if j.phase == Phase::Connecting && j.prior_model.is_some() {
            j.phase = Phase::Connected;
            j.model = j.prior_model.take().unwrap();
            for (entry, target) in j.files.iter_mut().zip(targets) {
                entry.applied = target;
            }
        } else {
            j.phase = Phase::Disconnected;
        }
        j.rollback = None;
        j.restore = None;
        j.prior_model = None;
        self.save(&j)?;
        self.status(id)
    }
}

// A rewire adopts unrelated user edits without adopting their fields as owned.
// Keep the first baseline for managed values and an exact restoration image for
// the latest connection, otherwise current==applied would erase adopted edits.
fn restore_entry(id: AgentId, index: usize, entry: &Entry, current: &Image) -> Result<Image> {
    if *current == entry.applied {
        return entry
            .restore_base
            .clone()
            .ok_or_else(|| conflict("agent restoration image is missing"));
    }
    super::selective::restore(
        id,
        index,
        &entry.baseline,
        &entry.applied,
        current,
        entry.owned.as_ref().unwrap(),
    )
}

fn read_optional(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    check_path(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unavailable("cannot read agent configuration or journal")),
    };
    if !file
        .metadata()
        .map_err(|_| unavailable("cannot inspect agent file"))?
        .is_file()
    {
        return Err(invalid("agent path must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable("cannot read agent file"))?;
    if bytes.len() as u64 > limit {
        return Err(invalid("agent file exceeds its size limit"));
    }
    Ok(Some(bytes))
}
fn check_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(invalid(
            "agent paths must be absolute without parent traversal",
        ));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                #[cfg(windows)]
                let reparse = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let reparse = false;
                if metadata.file_type().is_symlink() || reparse {
                    return Err(conflict(
                        "symlink or reparse-point agent paths are not supported",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(unavailable("cannot inspect agent path")),
        }
    }
    Ok(())
}
fn atomic(path: &Path, image: &Image) -> Result<()> {
    check_path(path)?;
    let Some(bytes) = image.bytes()? else {
        return match fs::remove_file(path) {
            Ok(()) => sync_parent(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(unavailable("cannot remove restored agent file")),
        };
    };
    let parent = path
        .parent()
        .ok_or_else(|| invalid("agent file has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|_| unavailable("cannot create agent configuration directory"))?;
    check_path(path)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| unavailable("cannot create atomic agent file"))?;
    #[cfg(windows)]
    super::windows::make_private(temp.path(), false)
        .map_err(|_| unavailable("cannot restrict temporary agent file"))?;
    temp.write_all(&bytes)
        .map_err(|_| unavailable("cannot write agent file"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = image.mode {
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|_| unavailable("cannot preserve agent file permissions"))?;
        }
    }
    temp.as_file()
        .sync_all()
        .map_err(|_| unavailable("cannot sync agent file"))?;
    #[cfg(windows)]
    let publish = super::windows::publish(temp.into_temp_path(), path);
    #[cfg(not(windows))]
    let publish = temp.persist(path).map(|_| ()).map_err(|e| e.error);
    publish.map_err(|_| {
        unavailable(
            "cannot replace agent file; close applications holding it and recover the operation",
        )
    })?;
    sync_parent(path)
}
fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(
        path.parent()
            .ok_or_else(|| invalid("agent file has no parent"))?,
    )
    .and_then(|file| file.sync_all())
    .map_err(|_| unavailable("cannot sync agent directory"))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| unavailable("cannot restrict agent backup directory"))?;
    }
    #[cfg(windows)]
    super::windows::make_private(path, true)
        .map_err(|_| unavailable("cannot restrict agent backup directory"))?;
    Ok(())
}
fn binary_detected(id: AgentId) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let binary = if id == AgentId::Zed { "zed" } else { id.id() };
    std::env::split_paths(&paths).any(|dir| {
        #[cfg(windows)]
        let names = [
            format!("{binary}.exe"),
            format!("{binary}.cmd"),
            format!("{binary}.bat"),
        ];
        #[cfg(not(windows))]
        let names = [binary.to_owned()];
        names.into_iter().any(|name| dir.join(name).is_file())
    })
}

/// Keep journals shared between local CLI and gateway processes for this config location.
pub fn state_directory(config_path: &Path) -> Result<PathBuf> {
    let absolute = if config_path.is_absolute() {
        config_path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| unavailable("cannot locate agent state"))?
            .join(config_path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| invalid("configuration file has no parent directory"))?;
    Ok(parent.join("agent-state"))
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
