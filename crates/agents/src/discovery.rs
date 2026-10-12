//! Passive, bounded CLI discovery. Never execute a shell, an agent or an installer.
use crate::lifecycle::AgentId;
use serde::Serialize;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const PATH_LIMIT: usize = 128;
const DIRECTORY_LIMIT: usize = 256;
const VERSION_LIMIT: usize = 64;
const ALIAS_FILE_LIMIT: u64 = 8 * 1024;
const ALIAS_ENTRY_LIMIT: usize = 128;
const ALIAS_DEPTH_LIMIT: usize = 16;

#[derive(Debug, Serialize)]
pub struct Cli {
    pub path: PathBuf,
    /// In the launcher's captured PATH, not a new terminal's PATH; unknown if that search was truncated.
    pub on_path: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Detection {
    pub config_file: bool,
    pub config_directory: bool,
    pub cli: Option<Cli>,
    /// A bounded search omitted entries. Absence is not proof of no installation.
    pub search_limited: bool,
}
impl Detection {
    pub(crate) fn detected(&self) -> bool {
        self.config_file || self.config_directory || self.cli.is_some()
    }
}

pub(crate) struct Discovery {
    path: Vec<PathBuf>,
    extra: Vec<PathBuf>,
    versions: Vec<(PathBuf, &'static str)>,
    /// nvm roots whose default alias should precede numerically newer versions.
    nvm: Vec<PathBuf>,
    npm_home: Option<PathBuf>,
    npm_env_prefix: Option<PathBuf>,
    confined_to: Option<PathBuf>,
    limited: bool,
    path_limited: bool,
}
pub(crate) struct Search {
    directories: Vec<(PathBuf, bool)>,
    confined_to: Option<PathBuf>,
    limited: bool,
    path_limited: bool,
}
impl Discovery {
    pub(crate) fn native() -> Self {
        Self::from_environment(
            super::agent_home().as_deref(),
            std::env::current_dir().ok().as_deref(),
            |key| std::env::var_os(key),
        )
    }
    fn from_environment(
        home: Option<&Path>,
        cwd: Option<&Path>,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        let absolute_env = |key| env(key).map(PathBuf::from).filter(|p| p.is_absolute());
        // A sandbox must not identify an agent from the real machine's PATH,
        // npm prefix, APPDATA, version managers or global Homebrew install.
        if let Some(home) = env("MAGPIE_LITE_HOME").filter(|v| !v.is_empty()) {
            return Self::at_home(&PathBuf::from(home));
        }
        let mut discovery = home
            .map(|home| Self::at_home_with_options(home, false))
            .unwrap_or_else(Self::empty);
        if let Some(path) = env("PATH") {
            for (index, dir) in std::env::split_paths(&path).enumerate() {
                if index == PATH_LIMIT {
                    discovery.limited = true;
                    discovery.path_limited = true;
                    break;
                }
                // Explicit PATH entries can be relative, including the empty
                // entry. Resolve once against the launcher's working directory.
                if dir.is_absolute() {
                    discovery.path.push(dir);
                } else if let Some(cwd) = &cwd {
                    discovery.path.push(cwd.join(dir));
                } else {
                    discovery.limited = true;
                    discovery.path_limited = true;
                }
            }
        }
        for (key, suffix) in [
            ("BUN_INSTALL", "bin"),
            ("VOLTA_HOME", "bin"),
            ("PNPM_HOME", ""),
            ("MISE_DATA_DIR", "shims"),
            ("ASDF_DATA_DIR", "shims"),
            ("XDG_DATA_HOME", "mise/shims"),
        ] {
            if let Some(dir) = absolute_env(key) {
                discovery.extra.push(dir.join(suffix));
            }
        }
        // Prefer the lowercase spelling when both are present (Unix).
        discovery.npm_env_prefix = absolute_env("npm_config_prefix")
            .or_else(|| absolute_env("NPM_CONFIG_PREFIX"))
            .map(npm_bin);
        if let Some(dir) = absolute_env("NVM_DIR") {
            // An explicit nvm root replaces ~/.nvm, it is not another fallback.
            discovery.nvm = vec![dir];
        }
        if cfg!(windows) {
            if let Some(dir) = absolute_env("APPDATA") {
                discovery.extra.push(dir.join("npm"));
            }
            if let Some(dir) = absolute_env("LOCALAPPDATA") {
                for suffix in ["Volta/bin", "pnpm", "mise/shims"] {
                    discovery.extra.push(dir.join(suffix));
                }
                discovery
                    .versions
                    .push((dir.join("fnm/node-versions"), "installation"));
            }
            if let Some(dir) = absolute_env("NVM_HOME") {
                discovery.versions.push((dir, ""));
            }
        } else {
            discovery.extra.extend([
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ]);
        }
        for (key, suffix, bin) in [
            (
                "FNM_DIR",
                "node-versions",
                if cfg!(windows) {
                    "installation"
                } else {
                    "installation/bin"
                },
            ),
            (
                "MISE_DATA_DIR",
                "installs/node",
                if cfg!(windows) { "" } else { "bin" },
            ),
            (
                "ASDF_DATA_DIR",
                "installs/nodejs",
                if cfg!(windows) { "" } else { "bin" },
            ),
        ] {
            if let Some(dir) = absolute_env(key) {
                discovery.versions.push((dir.join(suffix), bin));
            }
        }
        discovery
    }
    pub(crate) fn empty() -> Self {
        Self {
            path: Vec::new(),
            extra: Vec::new(),
            versions: Vec::new(),
            nvm: Vec::new(),
            npm_home: None,
            npm_env_prefix: None,
            confined_to: None,
            limited: false,
            path_limited: false,
        }
    }
    pub(crate) fn at_home(home: &Path) -> Self {
        Self::at_home_with_options(home, true)
    }
    fn at_home_with_options(home: &Path, restrict_to_home: bool) -> Self {
        let mut discovery = Self::empty();
        discovery.npm_home = Some(home.to_path_buf());
        discovery.confined_to = restrict_to_home.then(|| home.to_path_buf());
        for suffix in [
            ".local/bin",
            ".cargo/bin",
            ".bun/bin",
            ".volta/bin",
            ".asdf/shims",
            ".local/share/mise/shims",
            ".local/share/pnpm",
            ".npm-global/bin",
            ".npm/bin",
        ] {
            discovery.extra.push(home.join(suffix));
        }
        let bin = if cfg!(windows) { "" } else { "bin" };
        let nvm_dir = home.join(".nvm");
        discovery.nvm.push(nvm_dir);
        discovery.versions.extend([
            (
                home.join(".local/share/fnm/node-versions"),
                if cfg!(windows) {
                    "installation"
                } else {
                    "installation/bin"
                },
            ),
            (
                home.join("Library/Application Support/fnm/node-versions"),
                if cfg!(windows) {
                    "installation"
                } else {
                    "installation/bin"
                },
            ),
            (home.join(".local/share/mise/installs/node"), bin),
            (home.join(".asdf/installs/nodejs"), bin),
        ]);
        if cfg!(windows) {
            for suffix in [
                "AppData/Roaming/npm",
                "AppData/Local/Volta/bin",
                "AppData/Local/pnpm",
                "AppData/Local/mise/shims",
            ] {
                discovery.extra.push(home.join(suffix));
            }
            discovery
                .versions
                .push((home.join("AppData/Local/fnm/node-versions"), "installation"));
        }
        discovery
    }
    /// Refresh files and version directories, but never change the process environment.
    pub(crate) fn search(&self) -> Search {
        let mut search = Search {
            directories: Vec::new(),
            confined_to: self.confined_to.clone(),
            limited: self.limited,
            path_limited: self.path_limited,
        };
        for dir in &self.path {
            search.add(dir.clone(), true);
        }
        if let Some(prefix) = &self.npm_env_prefix {
            search.add(prefix.clone(), false);
        } else if let Some(home) = &self.npm_home {
            let (prefix, bounded) = npm_prefix(home, self.confined_to.is_some());
            search.limited |= bounded;
            if let Some(prefix) = prefix {
                search.add(prefix, false);
            }
        }
        for dir in &self.extra {
            search.add(dir.clone(), false);
        }
        // A fresh terminal gets nvm's resolved default Node before the other
        // installed versions. `alias/default` is a text file (possibly an
        // alias chain), never a directory.
        for nvm in &self.nvm {
            let root = nvm.join("versions/node");
            if !search.permits_existing(&root) {
                continue;
            }
            let (entries, bounded) = version_entries(&root, "bin");
            search.limited |= bounded;
            let names: Vec<_> = entries.iter().map(|(name, _)| name.as_str()).collect();
            let (default, bounded) = if search.permits_existing(&nvm.join("alias")) {
                nvm_default(nvm, &names)
            } else {
                (None, false)
            };
            search.limited |= bounded;
            if let Some(default) = default {
                search.add(root.join(default).join("bin"), false);
            }
            for (_, dir) in entries {
                search.add(dir, false);
            }
        }
        let mut visited = Vec::new();
        for (root, suffix) in &self.versions {
            if visited.contains(&(root, suffix)) {
                continue;
            }
            visited.push((root, suffix));
            if !search.permits_existing(root) {
                continue;
            }
            let (versions, bounded) = version_entries(root, suffix);
            search.limited |= bounded;
            for (_, dir) in versions {
                search.add(dir, false);
            }
        }
        search
    }
}
impl Search {
    fn permits_existing(&self, path: &Path) -> bool {
        self.confined_to
            .as_deref()
            .is_none_or(|home| confined(path, home))
    }
    fn add(&mut self, path: PathBuf, on_path: bool) {
        let path: PathBuf = path.components().collect();
        if self.confined_to.as_deref().is_some_and(|home| {
            !path.starts_with(home)
                || path.components().any(|part| part == Component::ParentDir)
                || (path.exists() && !confined(&path, home))
        }) {
            return;
        }
        if self.directories.iter().any(|(dir, _)| *dir == path) {
            return;
        }
        if self.directories.len() == DIRECTORY_LIMIT {
            self.limited = true;
        } else if path.is_absolute() {
            self.directories.push((path, on_path));
        } else {
            self.limited = true;
        }
    }
    pub(crate) fn detect(&self, id: AgentId, files: &[PathBuf]) -> Detection {
        let commands: &[&str] = if id == AgentId::Zed {
            &["zed", "zeditor"]
        } else {
            &[id.id()]
        };
        let mut cli = None;
        let mut limited = self.limited;
        'directories: for (dir, on_path) in &self.directories {
            for command in commands {
                #[cfg(windows)]
                let names = [
                    format!("{command}.exe"),
                    format!("{command}.com"),
                    format!("{command}.cmd"),
                    format!("{command}.bat"),
                ];
                #[cfg(not(windows))]
                let names = [(*command).to_owned()];
                for name in names {
                    let path = dir.join(name);
                    if executable(&path) && self.permits_existing(&path) {
                        // Never let an unrepresentable PATH entry break the whole
                        // authenticated JSON status response for unrelated Agents.
                        if path.to_str().is_none() {
                            limited = true;
                            continue;
                        }
                        cli = Some(Cli {
                            path,
                            on_path: if *on_path {
                                Some(true)
                            } else if self.path_limited {
                                None
                            } else {
                                Some(false)
                            },
                        });
                        break 'directories;
                    }
                }
            }
        }
        Detection {
            config_file: files.iter().any(|p| p.is_file()),
            config_directory: files.iter().any(|p| p.parent().is_some_and(Path::is_dir)),
            cli,
            search_limited: limited,
        }
    }
}
#[derive(Debug)]
enum BoundedRead {
    Missing,
    Limited,
    Bytes(Vec<u8>),
}

fn read_bounded_file(path: &Path, limit: u64, confined_to: Option<&Path>) -> BoundedRead {
    let Ok(metadata) = std::fs::metadata(path) else {
        return BoundedRead::Missing;
    };
    if !metadata.is_file() {
        return BoundedRead::Missing;
    }
    if metadata.len() > limit {
        return BoundedRead::Limited;
    }
    if let Some(root) = confined_to {
        let (Ok(root), Ok(target)) = (std::fs::canonicalize(root), std::fs::canonicalize(path))
        else {
            return BoundedRead::Missing;
        };
        if !target.starts_with(root) {
            return BoundedRead::Missing;
        }
    }
    let Ok(file) = std::fs::File::open(path) else {
        return BoundedRead::Missing;
    };
    let mut bytes = Vec::new();
    if file
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return BoundedRead::Missing;
    }
    if bytes.len() as u64 > limit {
        BoundedRead::Limited
    } else {
        BoundedRead::Bytes(bytes)
    }
}

fn version_entries(root: &Path, suffix: &str) -> (Vec<(String, PathBuf)>, bool) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (Vec::new(), false);
    };
    let mut versions = Vec::new();
    let mut limited = false;
    for (index, entry) in entries.enumerate() {
        if index == VERSION_LIMIT {
            limited = true;
            break;
        }
        let Ok(entry) = entry else {
            limited = true;
            continue;
        };
        if let Some(raw) = entry.file_name().to_str()
            && let Some(parsed) = version(raw)
        {
            let path = entry.path().join(suffix);
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                versions.push((raw.to_owned(), path, parsed));
            }
        }
    }
    versions.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    (
        versions
            .into_iter()
            .map(|(name, path, _)| (name, path))
            .collect(),
        limited,
    )
}

fn nvm_default(dir: &Path, names: &[&str]) -> (Option<String>, bool) {
    let alias = dir.join("alias");
    let mut current = String::from("default");
    let mut seen = std::collections::HashSet::from([current.clone()]);
    let mut limited = false;
    let mut resolved = false;
    for _ in 0..ALIAS_DEPTH_LIMIT {
        let (next, bounded) = nvm_alias(&alias, &current);
        limited |= bounded;
        if bounded {
            return (None, true);
        }
        let Some(next) = next else {
            resolved = true;
            break;
        };
        if !seen.insert(next.clone()) {
            return (None, limited);
        }
        current = next;
    }
    if !resolved {
        return (None, true);
    }
    if current == "default" {
        return (None, limited);
    }
    match current.as_str() {
        "system" => return (None, limited),
        "node" | "stable" => {
            return (
                names
                    .iter()
                    .copied()
                    .find(|name| name.starts_with('v'))
                    .map(str::to_owned),
                limited,
            );
        }
        _ => {}
    }
    if current.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        current.insert(0, 'v');
    }
    let Some(parts) = parsed_version_parts(&current) else {
        return (None, limited);
    };
    if !current.starts_with('v') {
        return (None, limited);
    }
    if parts == 3 {
        // An explicit version can be probed directly, even when the bounded
        // directory listing omitted it. Partial versions still use that list.
        let installed = executable(&dir.join("versions/node").join(&current).join("bin/node"));
        return (installed.then_some(current), limited);
    }
    let prefix = format!("{}.", current.trim_end_matches('.'));
    (
        names
            .iter()
            .copied()
            .find(|name| name.starts_with(&prefix))
            .map(str::to_owned),
        limited,
    )
}

fn parsed_version_parts(name: &str) -> Option<usize> {
    let name = name.strip_prefix('v').unwrap_or(name);
    let mut count = 0;
    for part in name.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse::<u64>().ok()?;
        count += 1;
    }
    (1..=3).contains(&count).then_some(count)
}

fn safe_relative_path(name: &str) -> Option<PathBuf> {
    // Alias files use slash-separated nvm names on every platform. Reject
    // traversal and Windows syntax even when discovering on Unix.
    if name.is_empty() || name.contains("..") || name.contains(['\\', ':']) {
        return None;
    }
    let path = Path::new(name);
    path.components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| path.to_path_buf())
}

fn confined(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

fn nvm_alias(alias: &Path, name: &str) -> (Option<String>, bool) {
    let mut name = name.to_owned();
    let mut limited = false;
    if let Some(raw) = name.strip_prefix("lts/-") {
        let Ok(index) = raw.parse::<usize>() else {
            return (None, limited);
        };
        if index == 0 {
            return (None, limited);
        }
        if !confined(&alias.join("lts"), alias) {
            return (None, false);
        }
        let Ok(entries) = std::fs::read_dir(alias.join("lts")) else {
            return (None, limited);
        };
        let mut lines = Vec::new();
        for (position, entry) in entries.enumerate() {
            if position == ALIAS_ENTRY_LIMIT {
                limited = true;
                break;
            }
            let Ok(entry) = entry else {
                limited = true;
                continue;
            };
            lines.push(entry.file_name());
        }
        if limited || lines.is_empty() {
            return (None, limited);
        }
        lines.sort();
        // nvm clamps old LTS ordinals to the first entry. Saturation also
        // handles usize::MAX without overflowing index + 1.
        let position = lines.len().saturating_sub(index.saturating_add(1));
        let selected = lines[position].to_string_lossy();
        if selected == "*" {
            return (None, limited);
        }
        name = format!("lts/{selected}");
    }
    let Some(relative) = safe_relative_path(&name) else {
        return (None, limited);
    };
    let path = alias.join(relative);
    let value = match read_bounded_file(&path, ALIAS_FILE_LIMIT, Some(alias)) {
        BoundedRead::Missing => None,
        BoundedRead::Limited => {
            limited = true;
            None
        }
        BoundedRead::Bytes(bytes) => String::from_utf8(bytes).ok().and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        }),
    };
    (value, limited)
}

fn npm_prefix(home: &Path, restrict_to_home: bool) -> (Option<PathBuf>, bool) {
    let confined_to = restrict_to_home.then_some(home);
    let text = match read_bounded_file(&home.join(".npmrc"), 64 * 1024, confined_to) {
        BoundedRead::Missing => return (None, false),
        BoundedRead::Limited => return (None, true),
        BoundedRead::Bytes(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return (None, false),
        },
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("prefix") {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value)
            .trim();
        let path = if value == "~" {
            home.to_path_buf()
        } else if let Some(rest) = value.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(value)
        };
        if !path.is_absolute()
            || (restrict_to_home
                && (!path.starts_with(home)
                    || path.components().any(|part| part == Component::ParentDir)))
        {
            continue;
        }
        return (Some(npm_bin(path)), false);
    }
    (None, false)
}

fn npm_bin(prefix: PathBuf) -> PathBuf {
    if cfg!(windows) {
        prefix
    } else {
        prefix.join("bin")
    }
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
fn version(name: &str) -> Option<(u64, u64, u64)> {
    fn number(part: &str) -> Option<u64> {
        (!part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    }
    let mut parts = name.strip_prefix('v').unwrap_or(name).split('.');
    let result = (
        number(parts.next()?)?,
        number(parts.next()?)?,
        number(parts.next()?)?,
    );
    parts.next().is_none().then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn launcher(dir: &Path, command: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let name = if cfg!(windows) {
            format!("{command}.cmd")
        } else {
            command.into()
        };
        let path = dir.join(name);
        fs::write(&path, "this is only a discovery fixture; never execute it").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    fn node_fixture(bin: &Path) {
        fs::create_dir_all(bin).unwrap();
        let path = bin.join("node");
        fs::write(&path, "passive node fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn isolated_home_has_no_machine_cli_or_config_and_refreshes_all_seven() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let discovery = Discovery::at_home(&home);
        for id in AgentId::ALL {
            let detection = discovery.search().detect(id, &[]);
            assert!(!detection.detected());
            assert!(!detection.search_limited);
        }
        assert!(
            !home.exists(),
            "discovery must not create a home or bin directory"
        );
        for id in AgentId::ALL {
            let path = launcher(&home.join(".local/bin"), id.id());
            let before = fs::read(&path).unwrap();
            let detection = discovery.search().detect(id, &[]);
            assert!(detection.detected());
            assert!(!detection.config_file && !detection.config_directory);
            let cli = detection.cli.unwrap();
            assert_eq!(cli.path, path);
            assert_eq!(cli.on_path, Some(false));
            assert_eq!(fs::read(&path).unwrap(), before);
            fs::remove_file(&path).unwrap();
            assert!(!discovery.search().detect(id, &[]).detected());
        }
    }

    #[test]
    fn config_file_directory_and_cli_are_independent_evidence() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("home/.claude/settings.json");
        let search = Discovery::at_home(&root.path().join("home")).search();
        let files = vec![file.clone()];
        assert!(!search.detect(AgentId::Claude, &files).detected());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let detection = search.detect(AgentId::Claude, &files);
        assert!(detection.config_directory && !detection.config_file);
        fs::write(&file, "malformed is still a configuration marker").unwrap();
        let detection = search.detect(AgentId::Claude, &files);
        assert!(detection.config_file && detection.config_directory);
        assert!(detection.cli.is_none());
    }

    #[test]
    fn path_order_beats_off_path_fallback_and_duplicates_preserve_path_evidence() {
        let root = tempfile::tempdir().unwrap();
        let fallback = launcher(&root.path().join("fallback"), "codex");
        let first = launcher(&root.path().join("first path with spaces"), "codex");
        let second = launcher(&root.path().join("second"), "codex");
        let mut discovery = Discovery::empty();
        discovery.path = vec![
            first.parent().unwrap().into(),
            second.parent().unwrap().into(),
        ];
        discovery.extra = vec![
            fallback.parent().unwrap().into(),
            first.parent().unwrap().into(),
        ];
        let cli = discovery.search().detect(AgentId::Codex, &[]).cli.unwrap();
        assert_eq!(cli.path, first);
        assert_eq!(cli.on_path, Some(true));
        assert_eq!(discovery.search().directories.len(), 3);
        fs::remove_file(first).unwrap();
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Codex, &[])
                .cli
                .unwrap()
                .path,
            second
        );
        fs::remove_file(second).unwrap();
        let cli = discovery.search().detect(AgentId::Codex, &[]).cli.unwrap();
        assert_eq!(cli.path, fallback);
        assert_eq!(cli.on_path, Some(false));
    }

    #[test]
    fn directory_named_like_a_command_and_unsupported_suffix_are_not_launchers() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join(".local/bin");
        fs::create_dir_all(bin.join(if cfg!(windows) { "codex.exe" } else { "codex" })).unwrap();
        fs::write(bin.join("codex.ps1"), "not a standalone CLI launcher").unwrap();
        assert!(
            Discovery::at_home(root.path())
                .search()
                .detect(AgentId::Codex, &[])
                .cli
                .is_none()
        );
        let path = launcher(&bin, "zeditor");
        assert_eq!(
            Discovery::at_home(root.path())
                .search()
                .detect(AgentId::Zed, &[])
                .cli
                .unwrap()
                .path,
            path
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_nonexecutable_and_broken_symlink_do_not_imply_cli_installation() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join(".local/bin");
        let path = launcher(&bin, "claude");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            Discovery::at_home(root.path())
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .is_none()
        );
        fs::remove_file(&path).unwrap();
        symlink(bin.join("missing-target"), &path).unwrap();
        assert!(
            Discovery::at_home(root.path())
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unrepresentable_cli_path_does_not_break_json_status_for_other_agents() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join(OsString::from_vec(b"bin-\xff".to_vec()));
        launcher(&bin, "codex");
        let mut discovery = Discovery::empty();
        discovery.path.push(bin);
        let detection = discovery.search().detect(AgentId::Codex, &[]);
        assert!(detection.search_limited && detection.cli.is_none());
        serde_json::to_value(detection).unwrap();
    }

    #[test]
    fn fnm_mise_asdf_and_platform_npm_fallback_layouts_are_supported() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let fnm = if cfg!(windows) {
            "installation"
        } else {
            "installation/bin"
        };
        let node = if cfg!(windows) { "" } else { "bin" };
        let paths = [
            (
                AgentId::Claude,
                home.join(".local/share/fnm/node-versions/v24.1.2")
                    .join(fnm),
            ),
            (
                AgentId::Codex,
                home.join(".local/share/mise/installs/node/24.1.2")
                    .join(node),
            ),
            (
                AgentId::Crush,
                home.join(".asdf/installs/nodejs/24.1.2").join(node),
            ),
            (
                AgentId::Goose,
                home.join(if cfg!(windows) {
                    "AppData/Roaming/npm"
                } else {
                    ".npm/bin"
                }),
            ),
            (
                AgentId::Gemini,
                home.join("Library/Application Support/fnm/node-versions/v24.1.2")
                    .join(fnm),
            ),
        ];
        let discovery = Discovery::at_home(home);
        for (id, bin) in paths {
            let expected = launcher(&bin, id.id());
            let cli = discovery.search().detect(id, &[]).cli.unwrap();
            assert_eq!(cli.path, expected);
            assert_eq!(cli.on_path, Some(false));
        }
    }

    #[test]
    fn npmrc_prefix_and_nvm_default_alias_are_passively_discovered() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let prefix = home.join("custom-npm");
        fs::write(
            home.join(".npmrc"),
            "registry=https://example.invalid\nprefix=\"~/custom-npm\"\n",
        )
        .unwrap();
        let npm_cli = launcher(
            &if cfg!(windows) {
                prefix.clone()
            } else {
                prefix.join("bin")
            },
            "opencode",
        );

        let nvm = home.join(".nvm");
        let default_version = nvm.join("versions/node/v20.0.0");
        let newer_version = nvm.join("versions/node/v24.0.0");
        fs::create_dir_all(default_version.join("bin")).unwrap();
        node_fixture(&default_version.join("bin"));
        let default_cli = launcher(&default_version.join("bin"), "claude");
        let newer_cli = launcher(&newer_version.join("bin"), "claude");
        fs::create_dir_all(nvm.join("alias")).unwrap();
        fs::write(nvm.join("alias/default"), "v20.0.0\n").unwrap();
        let discovery = Discovery::at_home(home);

        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .unwrap()
                .path,
            npm_cli
        );
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            default_cli
        );
        fs::remove_file(default_cli).unwrap();
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            newer_cli
        );
    }

    #[test]
    fn known_prefixes_and_version_managers_refresh_with_numeric_not_lexical_order() {
        let root = tempfile::tempdir().unwrap();
        let discovery = Discovery::at_home(root.path());
        let versions = root.path().join(".nvm/versions/node");
        let old = launcher(&versions.join("v9.20.0/bin"), "gemini");
        let new = launcher(&versions.join("v22.2.0/bin"), "gemini");
        let ignored = launcher(&versions.join("latest/bin"), "codex");
        let cli = discovery.search().detect(AgentId::Gemini, &[]).cli.unwrap();
        assert_eq!(cli.path, new);
        assert_eq!(cli.on_path, Some(false));
        assert!(discovery.search().detect(AgentId::Codex, &[]).cli.is_none());
        fs::remove_file(new).unwrap();
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Gemini, &[])
                .cli
                .unwrap()
                .path,
            old
        );
        assert!(ignored.is_file());
        let prefix = launcher(&root.path().join(".npm-global/bin"), "goose");
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Goose, &[])
                .cli
                .unwrap()
                .path,
            prefix
        );
        assert_eq!(version("v22.1.3"), Some((22, 1, 3)));
        for invalid in [
            "../22.1.3",
            "22.1",
            "22.1.3.4",
            "lts",
            "22.1.3-beta",
            "v+22.1.3",
            "18446744073709551616.0.0",
        ] {
            assert!(version(invalid).is_none());
        }
    }

    #[test]
    fn native_context_honors_absolute_prefixes_and_resolves_explicit_relative_path_entries() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("launch-directory");
        let from_path = launcher(&cwd.join("relative"), "codex");
        let from_empty_path = launcher(&cwd, "claude");
        let from_bun = launcher(&root.path().join("custom-bun/bin"), "gemini");
        let from_nvm = launcher(
            &root.path().join("custom-nvm/versions/node/v24.1.0/bin"),
            "goose",
        );
        let prefix = root.path().join("npm-prefix");
        let from_prefix = launcher(
            &if cfg!(windows) {
                prefix.clone()
            } else {
                prefix.join("bin")
            },
            "opencode",
        );
        let path = std::env::join_paths([PathBuf::from("relative"), PathBuf::new()]).unwrap();
        let env = |key: &str| match key {
            "PATH" => Some(path.clone()),
            "BUN_INSTALL" => Some(root.path().join("custom-bun").into_os_string()),
            "NVM_DIR" => Some(root.path().join("custom-nvm").into_os_string()),
            "npm_config_prefix" => Some(prefix.clone().into_os_string()),
            "VOLTA_HOME" => Some(OsString::from("ignored-relative-prefix")),
            _ => None,
        };
        let mut discovery = Discovery::from_environment(Some(&home), Some(&cwd), env);
        discovery.extra.retain(|p| p.starts_with(root.path())); // no global machine fixtures
        for (id, path, on_path) in [
            (AgentId::Codex, from_path, true),
            (AgentId::Claude, from_empty_path, true),
            (AgentId::Gemini, from_bun, false),
            (AgentId::Goose, from_nvm, false),
            (AgentId::Opencode, from_prefix, false),
        ] {
            let cli = discovery.search().detect(id, &[]).cli.unwrap();
            assert_eq!(cli.path, path);
            assert_eq!(cli.on_path, Some(on_path));
        }
        assert!(discovery.extra.iter().all(|p| p.is_absolute()));
        assert!(!home.exists(), "native context discovery is read-only");
    }

    #[test]
    fn explicit_environment_prefixes_override_home_discovery() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let home_prefix = home.join("home-npm");
        fs::write(
            home.join(".npmrc"),
            format!("prefix={}", home_prefix.display()),
        )
        .unwrap();
        let home_cli = launcher(
            &if cfg!(windows) {
                home_prefix.clone()
            } else {
                home_prefix.join("bin")
            },
            "opencode",
        );
        let home_nvm = home.join(".nvm");
        let home_version = home_nvm.join("versions/node/v20.0.0");
        fs::create_dir_all(home_version.join("bin")).unwrap();
        node_fixture(&home_version.join("bin"));
        let home_default = launcher(&home_version.join("bin"), "claude");
        fs::create_dir_all(home_nvm.join("alias")).unwrap();
        fs::write(home_nvm.join("alias/default"), "v20.0.0\n").unwrap();

        let env_prefix = root.path().join("env-npm");
        let env_cli = launcher(
            &if cfg!(windows) {
                env_prefix.clone()
            } else {
                env_prefix.join("bin")
            },
            "opencode",
        );
        let env_nvm = root.path().join("env-nvm");
        let env_version = env_nvm.join("versions/node/v18.0.0");
        fs::create_dir_all(env_version.join("bin")).unwrap();
        node_fixture(&env_version.join("bin"));
        let env_default = launcher(&env_version.join("bin"), "claude");
        fs::create_dir_all(env_nvm.join("alias")).unwrap();
        fs::write(env_nvm.join("alias/default"), "v18.0.0\n").unwrap();
        let discovery = Discovery::from_environment(Some(&home), None, |key| match key {
            "npm_config_prefix" => Some(env_prefix.clone().into_os_string()),
            "NVM_DIR" => Some(env_nvm.clone().into_os_string()),
            _ => None,
        });

        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .unwrap()
                .path,
            env_cli
        );
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            env_default
        );
        assert!(home_cli.is_file() && home_default.is_file());
    }

    #[test]
    fn sandbox_context_ignores_every_external_path_and_prefix_without_global_env_mutation() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("sandbox");
        let external = root.path().join("outside");
        launcher(&external, "claude");
        let discovery = Discovery::from_environment(None, None, |key| {
            Some(
                if key == "MAGPIE_LITE_HOME" {
                    home.clone()
                } else {
                    external.clone()
                }
                .into_os_string(),
            )
        });
        assert!(discovery.path.is_empty());
        assert!(discovery.extra.iter().all(|p| p.starts_with(&home)));
        assert!(discovery.versions.iter().all(|(p, _)| p.starts_with(&home)));
        assert!(discovery.nvm.iter().all(|p| p.starts_with(&home)));
        assert!(!discovery.search().detect(AgentId::Claude, &[]).detected());
    }

    #[test]
    fn captured_path_bound_omits_late_probes_and_marks_membership_unknown() {
        let root = tempfile::tempdir().unwrap();
        let late = launcher(&root.path().join(".local/bin"), "codex");
        let mut dirs: Vec<_> = (0..PATH_LIMIT)
            .map(|i| root.path().join(i.to_string()))
            .collect();
        dirs.push(late.parent().unwrap().into());
        let path = std::env::join_paths(dirs).unwrap();
        let discovery = Discovery::from_environment(Some(root.path()), None, |key| {
            (key == "PATH").then(|| path.clone())
        });
        assert_eq!(discovery.path.len(), PATH_LIMIT);
        let detection = discovery.search().detect(AgentId::Codex, &[]);
        assert!(detection.search_limited);
        let cli = detection.cli.unwrap();
        assert_eq!(cli.path, late);
        assert_eq!(cli.on_path, None);
    }

    #[test]
    fn truncated_path_never_claims_a_fallback_is_off_path() {
        let root = tempfile::tempdir().unwrap();
        let path = launcher(&root.path().join("fallback"), "claude");
        let mut discovery = Discovery::empty();
        discovery.extra.push(path.parent().unwrap().into());
        discovery.limited = true;
        discovery.path_limited = true;
        let detection = discovery.search().detect(AgentId::Claude, &[]);
        assert!(detection.search_limited);
        assert_eq!(detection.cli.unwrap().on_path, None);
        discovery.path.push(path.parent().unwrap().into());
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .on_path,
            Some(true)
        );
    }

    #[test]
    fn directory_and_version_bounds_are_reported_and_never_scan_recursively() {
        let root = tempfile::tempdir().unwrap();
        let mut discovery = Discovery::empty();
        discovery.path = (0..DIRECTORY_LIMIT)
            .map(|i| root.path().join(i.to_string()))
            .collect();
        let omitted = launcher(&root.path().join("omitted"), "claude");
        discovery.extra.push(omitted.parent().unwrap().into());
        let search = discovery.search();
        assert_eq!(search.directories.len(), DIRECTORY_LIMIT);
        let detection = search.detect(AgentId::Claude, &[]);
        assert!(detection.search_limited && detection.cli.is_none());
        let versions = root.path().join("versions");
        for i in 0..=VERSION_LIMIT {
            fs::create_dir_all(versions.join(format!("v1.0.{i}"))).unwrap();
        }
        let mut discovery = Discovery::empty();
        discovery.versions.push((versions, "bin"));
        assert!(
            discovery
                .search()
                .detect(AgentId::Codex, &[])
                .search_limited
        );
        launcher(
            &root.path().join(".local/bin/unrecognized/deep/bin"),
            "codex",
        );
        assert!(
            Discovery::at_home(root.path())
                .search()
                .detect(AgentId::Codex, &[])
                .cli
                .is_none()
        );
    }
    fn alias(nvm: &Path, name: &str, value: &str) {
        let path = nvm.join("alias").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }
    fn installed_node(nvm: &Path, name: &str, command: &str) -> PathBuf {
        let bin = nvm.join("versions/node").join(name).join("bin");
        node_fixture(&bin);
        launcher(&bin, command)
    }
    #[test]
    fn nvm_real_alias_files_refresh_chains_partial_versions_and_numeric_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        let nvm = root.path().join(".nvm");
        let old = installed_node(&nvm, "v9.20.0", "claude");
        let middle = installed_node(&nvm, "v22.1.3", "claude");
        let newest = installed_node(&nvm, "v22.2.0", "claude");
        let discovery = Discovery::at_home(root.path());
        alias(&nvm, "chosen", "\n  lts/jod \nignored-second-line\n");
        alias(&nvm, "lts/jod", "22.1");
        for (value, expected) in [
            ("chosen", &middle),
            ("9", &old),
            ("v9.20.0", &old),
            ("v22", &newest),
            ("node", &newest),
            ("stable", &newest),
            ("system", &newest),
            ("v99.0.0", &newest),
            ("nonexistent-alias", &newest),
        ] {
            alias(&nvm, "default", value);
            let detection = discovery.search().detect(AgentId::Claude, &[]);
            assert_eq!(&detection.cli.unwrap().path, expected, "default={value}");
            assert!(!detection.search_limited);
        }
        alias(&nvm, "default", "v9.20.0");
        fs::remove_file(nvm.join("versions/node/v9.20.0/bin/node")).unwrap();
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            newest
        );
        alias(&nvm, "default", "chosen");
        alias(&nvm, "chosen", "default");
        let detection = discovery.search().detect(AgentId::Claude, &[]);
        assert_eq!(detection.cli.unwrap().path, newest);
        assert!(
            !detection.search_limited,
            "a cycle is resolved, not a truncated search"
        );
    }
    #[test]
    fn nvm_alias_depth_and_regular_file_byte_limits_fail_closed_but_keep_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        let nvm = root.path().join(".nvm");
        installed_node(&nvm, "v9.0.0", "claude");
        let newest = installed_node(&nvm, "v24.0.0", "claude");
        let discovery = Discovery::at_home(root.path());
        alias(&nvm, "default", "chain0");
        for i in 0..ALIAS_DEPTH_LIMIT {
            alias(&nvm, &format!("chain{i}"), &format!("chain{}", i + 1));
        }
        alias(&nvm, &format!("chain{ALIAS_DEPTH_LIMIT}"), "v9.0.0");
        let detection = discovery.search().detect(AgentId::Claude, &[]);
        assert!(detection.search_limited);
        assert_eq!(detection.cli.unwrap().path, newest);
        alias(&nvm, "default", &"x".repeat(ALIAS_FILE_LIMIT as usize + 1));
        assert!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .search_limited
        );
        fs::write(nvm.join("alias/default"), [0xff]).unwrap();
        assert!(
            !discovery
                .search()
                .detect(AgentId::Claude, &[])
                .search_limited
        );
        fs::remove_file(nvm.join("alias/default")).unwrap();
        fs::create_dir(nvm.join("alias/default")).unwrap();
        let detection = discovery.search().detect(AgentId::Claude, &[]);
        assert_eq!(detection.cli.unwrap().path, newest);
        assert!(!detection.search_limited);
        assert!(
            fs::read_dir(root.path())
                .unwrap()
                .all(|e| e.unwrap().file_name() == ".nvm")
        );
    }
    #[test]
    fn nvm_lts_ordinals_clamp_without_overflow_and_bounded_listing_is_not_preferred() {
        let root = tempfile::tempdir().unwrap();
        let nvm = root.path().join(".nvm");
        let old = installed_node(&nvm, "v9.0.0", "claude");
        let newest = installed_node(&nvm, "v24.0.0", "claude");
        alias(&nvm, "lts/argon", "v9.0.0");
        alias(&nvm, "lts/boron", "v9.0.0");
        alias(&nvm, "lts/jod", "v24.0.0");
        let discovery = Discovery::at_home(root.path());
        for value in [
            "lts/argon".to_owned(),
            "lts/-1".into(),
            "lts/-2".into(),
            format!("lts/-{}", usize::MAX),
        ] {
            alias(&nvm, "default", &value);
            let detection = discovery.search().detect(AgentId::Claude, &[]);
            assert_eq!(detection.cli.unwrap().path, old, "{value}");
            assert!(!detection.search_limited);
        }
        alias(&nvm, "default", "lts/-0");
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            newest
        );
        for i in 0..ALIAS_ENTRY_LIMIT {
            alias(&nvm, &format!("lts/z{i:03}"), "v9.0.0");
        }
        alias(&nvm, "default", "lts/-1");
        let detection = discovery.search().detect(AgentId::Claude, &[]);
        assert!(detection.search_limited);
        assert_eq!(detection.cli.unwrap().path, newest);
    }
    #[cfg(unix)]
    #[test]
    fn nvm_lts_star_is_a_real_filename_and_alias_symlinks_cannot_escape() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let nvm = root.path().join(".nvm");
        installed_node(&nvm, "v9.0.0", "claude");
        installed_node(&nvm, "v24.0.0", "claude");
        alias(&nvm, "default", "lts/*");
        alias(&nvm, "lts/*", "lts/jod");
        alias(&nvm, "lts/jod", "v9.0.0");
        let names = ["v24.0.0", "v9.0.0"];
        assert_eq!(nvm_default(&nvm, &names), (Some("v9.0.0".into()), false));
        alias(&nvm, "default", "lts/-1");
        assert_eq!(
            nvm_default(&nvm, &names),
            (None, false),
            "ordinal selecting * is not a preferred version"
        );
        fs::write(root.path().join("outside"), "v9.0.0").unwrap();
        symlink(root.path().join("outside"), nvm.join("alias/escape")).unwrap();
        alias(&nvm, "default", "escape");
        assert_eq!(nvm_default(&nvm, &names), (None, false));
        fs::remove_file(nvm.join("alias/default")).unwrap();
        symlink(root.path().join("outside"), nvm.join("alias/default")).unwrap();
        assert_eq!(nvm_default(&nvm, &names), (None, false));
    }
    #[test]
    fn aliases_reject_traversal_and_versions_reject_invalid_or_overflowed_parts() {
        for invalid in [
            "",
            "v",
            "1..2",
            "1.2.3.4",
            "v+22.1",
            "22.1-beta",
            "18446744073709551616",
        ] {
            assert_eq!(parsed_version_parts(invalid), None, "{invalid}");
        }
        for (name, count) in [("22", 1), ("v22.1", 2), ("v22.1.3", 3)] {
            assert_eq!(parsed_version_parts(name), Some(count));
        }
        for invalid in [
            "",
            "../outside",
            "lts/../../outside",
            "/outside",
            "C:/outside",
            "..\\outside",
            "lts\\jod",
            "a..b",
        ] {
            assert!(safe_relative_path(invalid).is_none(), "{invalid}");
        }
        assert_eq!(
            safe_relative_path("lts/jod"),
            Some(PathBuf::from("lts/jod"))
        );
    }
    #[test]
    fn environment_nvm_root_replaces_home_even_without_default_and_path_still_wins() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let home_nvm = home.join(".nvm");
        installed_node(&home_nvm, "v24.0.0", "claude");
        alias(&home_nvm, "default", "v24.0.0");
        let env_nvm = root.path().join("external-nvm");
        let env_cli = installed_node(&env_nvm, "v18.0.0", "claude");
        let mut discovery = Discovery::from_environment(Some(&home), None, |key| {
            (key == "NVM_DIR").then(|| env_nvm.clone().into_os_string())
        });
        discovery.extra.retain(|p| p.starts_with(root.path()));
        assert_eq!(discovery.nvm, vec![env_nvm]);
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            env_cli
        );
        fs::remove_file(env_cli).unwrap();
        assert!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .is_none(),
            "~/.nvm must not be a fallback"
        );
        let path_cli = launcher(&root.path().join("captured-path"), "claude");
        discovery.path.push(path_cli.parent().unwrap().into());
        let cli = discovery.search().detect(AgentId::Claude, &[]).cli.unwrap();
        assert_eq!(cli.path, path_cli);
        assert_eq!(cli.on_path, Some(true));
    }
    #[test]
    fn npmrc_refresh_quotes_bounds_relative_values_and_home_escape() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let discovery = Discovery::at_home(&home);
        let a = launcher(&npm_bin(home.join("first")), "opencode");
        let b = launcher(&npm_bin(home.join("second")), "opencode");
        let outside = launcher(&npm_bin(root.path().join("outside")), "opencode");
        for (text, expected) in [
            (
                "; prefix=ignored\n# prefix=ignored\nPrEfIx = '~/first'\n".to_owned(),
                Some(&a),
            ),
            ("prefix=\"~/second\"\n".into(), Some(&b)),
            ("prefix=relative\nprefix=~/first\n".into(), Some(&a)),
            ("prefix=~/../outside\n".into(), None),
            (
                format!("prefix={}\n", root.path().join("outside").display()),
                None,
            ),
            ("prefix=${UNSUPPORTED}\n".into(), None),
            ("x".repeat(64 * 1024 + 1), None),
        ] {
            fs::write(home.join(".npmrc"), &text).unwrap();
            let detection = discovery.search().detect(AgentId::Opencode, &[]);
            assert_eq!(
                detection.cli.as_ref().map(|cli| &cli.path),
                expected,
                "{text:.100}"
            );
            assert_eq!(detection.search_limited, text.len() > 64 * 1024);
        }
        fs::write(
            home.join(".npmrc"),
            format!("prefix={}\n", root.path().join("outside").display()),
        )
        .unwrap();
        let mut native = Discovery::from_environment(Some(&home), None, |_| None);
        native.extra.retain(|p| p.starts_with(root.path()));
        assert_eq!(
            native
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .unwrap()
                .path,
            outside
        );
        fs::remove_file(home.join(".npmrc")).unwrap();
        assert!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .is_none()
        );
    }
    #[test]
    fn npm_environment_spelling_precedence_and_override_do_not_reuse_npmrc() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let file_cli = launcher(&npm_bin(home.join("file")), "opencode");
        let lower = root.path().join("lower");
        let upper = root.path().join("upper");
        let lower_cli = launcher(&npm_bin(lower.clone()), "opencode");
        launcher(&npm_bin(upper.clone()), "opencode");
        fs::write(home.join(".npmrc"), "prefix=~/file").unwrap();
        let mut discovery = Discovery::from_environment(Some(&home), None, |key| match key {
            "npm_config_prefix" => Some(lower.clone().into_os_string()),
            "NPM_CONFIG_PREFIX" => Some(upper.clone().into_os_string()),
            _ => None,
        });
        discovery.extra.retain(|p| p.starts_with(root.path()));
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .unwrap()
                .path,
            lower_cli
        );
        fs::remove_file(lower_cli).unwrap();
        assert!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .is_none()
        );
        assert!(file_cli.exists());
    }
    #[cfg(unix)]
    #[test]
    fn sandbox_rejects_symlinked_prefixes_launchers_version_roots_aliases_and_npmrc() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let outside = root.path().join("outside");
        fs::create_dir_all(&home).unwrap();
        let external_cli = launcher(&npm_bin(outside.clone()), "opencode");
        symlink(&outside, home.join("linked")).unwrap();
        fs::write(home.join(".npmrc"), "prefix=~/linked").unwrap();
        let discovery = Discovery::at_home(&home);
        assert!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .is_none()
        );
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        symlink(&external_cli, home.join(".local/bin/opencode")).unwrap();
        assert!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .is_none()
        );
        let external_nvm = outside.join("nvm");
        installed_node(&external_nvm, "v9.0.0", "claude");
        installed_node(&external_nvm, "v24.0.0", "claude");
        alias(&external_nvm, "default", "v9.0.0");
        symlink(&external_nvm, home.join(".nvm")).unwrap();
        assert!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .is_none()
        );
        fs::remove_file(home.join(".nvm")).unwrap();
        let nvm = home.join(".nvm");
        let newest = installed_node(&nvm, "v24.0.0", "claude");
        installed_node(&nvm, "v9.0.0", "claude");
        symlink(external_nvm.join("alias"), nvm.join("alias")).unwrap();
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            newest
        );
        fs::remove_file(home.join(".npmrc")).unwrap();
        fs::write(outside.join("npmrc"), "prefix=~/linked").unwrap();
        symlink(outside.join("npmrc"), home.join(".npmrc")).unwrap();
        assert_eq!(npm_prefix(&home, true), (None, false));
    }
    #[test]
    fn nvm_exact_default_does_not_depend_on_a_complete_version_listing() {
        let root = tempfile::tempdir().unwrap();
        installed_node(root.path(), "v18.0.0", "claude");
        alias(root.path(), "default", "v18.0.0");
        assert_eq!(
            nvm_default(root.path(), &[]),
            (Some("v18.0.0".into()), false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn passive_reads_ignore_non_regular_socket_inputs() {
        use std::os::unix::net::UnixListener;
        let root = tempfile::tempdir().unwrap();
        let npm = UnixListener::bind(root.path().join(".npmrc")).unwrap();
        assert_eq!(npm_prefix(root.path(), true), (None, false));
        fs::create_dir(root.path().join("alias")).unwrap();
        let nvm = UnixListener::bind(root.path().join("alias/default")).unwrap();
        assert_eq!(nvm_default(root.path(), &[]), (None, false));
        drop((npm, nvm));
    }
    #[cfg(windows)]
    #[test]
    fn sandbox_rejects_windows_junction_prefixes_version_roots_and_aliases() {
        use std::os::windows::process::CommandExt;
        fn junction(link: &Path, target: &Path) {
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let result = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "junction {} -> {}: {:?} {:?}",
                link.display(),
                target.display(),
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let outside = root.path().join("outside");
        launcher(&outside, "opencode");
        fs::create_dir_all(&home).unwrap();
        junction(&home.join("linked"), &outside);
        fs::write(home.join(".npmrc"), "prefix=~/linked").unwrap();
        let discovery = Discovery::at_home(&home);
        assert!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .is_none()
        );
        junction(&home.join(".local").join("bin"), &outside);
        assert!(
            discovery
                .search()
                .detect(AgentId::Opencode, &[])
                .cli
                .is_none()
        );
        let external_nvm = outside.join("nvm");
        installed_node(&external_nvm, "v9.0.0", "claude");
        alias(&external_nvm, "default", "v9.0.0");
        junction(&home.join(".nvm"), &external_nvm);
        assert!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .is_none()
        );
        fs::remove_dir(home.join(".nvm")).unwrap();
        let nvm = home.join(".nvm");
        let newest = installed_node(&nvm, "v24.0.0", "claude");
        installed_node(&nvm, "v9.0.0", "claude");
        junction(&nvm.join("alias"), &external_nvm.join("alias"));
        assert_eq!(
            discovery
                .search()
                .detect(AgentId::Claude, &[])
                .cli
                .unwrap()
                .path,
            newest
        );
        for link in [
            home.join("linked"),
            home.join(".local").join("bin"),
            nvm.join("alias"),
        ] {
            fs::remove_dir(link).unwrap();
        }
        assert!(outside.join("opencode.cmd").exists());
        assert!(external_nvm.join("alias/default").exists());
    }
}
