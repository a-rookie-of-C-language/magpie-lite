//! Passive, bounded CLI discovery. Never execute a shell, an agent or an installer.
use crate::lifecycle::AgentId;
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const PATH_LIMIT: usize = 128;
const DIRECTORY_LIMIT: usize = 256;
const VERSION_LIMIT: usize = 64;

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
    limited: bool,
    path_limited: bool,
}
pub(crate) struct Search {
    directories: Vec<(PathBuf, bool)>,
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
        let mut discovery = home.map(Self::at_home).unwrap_or_else(Self::empty);
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
        for key in ["npm_config_prefix", "NPM_CONFIG_PREFIX"] {
            if let Some(dir) = absolute_env(key) {
                discovery
                    .extra
                    .push(if cfg!(windows) { dir } else { dir.join("bin") });
            }
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
            ("NVM_DIR", "versions/node", "bin"),
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
            limited: false,
            path_limited: false,
        }
    }
    pub(crate) fn at_home(home: &Path) -> Self {
        let mut discovery = Self::empty();
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
        discovery.versions.extend([
            (home.join(".nvm/versions/node"), "bin"),
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
            limited: self.limited,
            path_limited: self.path_limited,
        };
        for (dirs, on_path) in [(&self.path, true), (&self.extra, false)] {
            for dir in dirs {
                search.add(dir.clone(), on_path);
            }
        }
        let mut visited = Vec::new();
        for (root, suffix) in &self.versions {
            if visited.contains(&(root, suffix)) {
                continue;
            }
            visited.push((root, suffix));
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut versions = Vec::new();
            for (index, entry) in entries.enumerate() {
                if index == VERSION_LIMIT {
                    search.limited = true;
                    break;
                }
                let Ok(entry) = entry else {
                    search.limited = true;
                    continue;
                };
                if let Some(version) = entry.file_name().to_str().and_then(version) {
                    versions.push((version, entry.path().join(suffix)));
                }
            }
            versions.sort_by_key(|entry| std::cmp::Reverse(entry.0));
            for (_, dir) in versions {
                search.add(dir, false);
            }
        }
        search
    }
}
impl Search {
    fn add(&mut self, path: PathBuf, on_path: bool) {
        let path: PathBuf = path.components().collect();
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
                    if executable(&path) {
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
}
