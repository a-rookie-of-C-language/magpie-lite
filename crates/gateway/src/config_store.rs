//! Serialized, persistent configuration updates shared by request handlers.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use fs2::FileExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::config::{Config, ConfigSnapshot};

#[derive(Clone)]
pub struct ConfigStore(Arc<Inner>);

struct Inner {
    current: RwLock<Config>,
    revision: AtomicU64,
    updates: Mutex<()>,
    path: Option<PathBuf>,
    next_key_generation: AtomicU64,
}

pub type ConfigError = (StatusCode, String);

/// A caller-scoped idempotency request for a persistent configuration mutation.
#[derive(Debug, Clone)]
pub struct IdempotencyRequest {
    pub scope: String,
    pub key: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdempotencyEntry {
    scope: String,
    key: String,
    digest: String,
    snapshot: Option<ConfigSnapshot>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IdempotencyLedger {
    entries: Vec<IdempotencyEntry>,
}

const IDEMPOTENCY_RETENTION: usize = 256;

fn idempotency_path(path: &Path) -> PathBuf {
    let mut result = path.as_os_str().to_os_string();
    result.push(".idempotency.json");
    PathBuf::from(result)
}

fn load_idempotency(path: &Path) -> Result<IdempotencyLedger, ConfigError> {
    let path = idempotency_path(path);
    if !path.exists() {
        return Ok(IdempotencyLedger::default());
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        tracing::error!(%error, path = %path.display(), "could not read idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read idempotency ledger; no changes applied".to_owned(),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        tracing::error!(%error, path = %path.display(), "could not parse idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not parse idempotency ledger; no changes applied".to_owned(),
        )
    })
}

fn save_idempotency(path: &Path, ledger: &IdempotencyLedger) -> Result<(), ConfigError> {
    let path = idempotency_path(path);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| {
        tracing::error!(%error, path = %parent.display(), "could not prepare idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not prepare idempotency ledger; no changes applied".to_owned(),
        )
    })?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        tracing::error!(%error, path = %path.display(), "could not create idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not create idempotency ledger; no changes applied".to_owned(),
        )
    })?;
    serde_json::to_writer_pretty(file.as_file_mut(), ledger).map_err(|error| {
        tracing::error!(%error, path = %path.display(), "could not serialize idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not serialize idempotency ledger; no changes applied".to_owned(),
        )
    })?;
    file.as_file().sync_all().map_err(|error| {
        tracing::error!(%error, path = %path.display(), "could not flush idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not flush idempotency ledger; no changes applied".to_owned(),
        )
    })?;
    file.persist(&path).map_err(|error| {
        tracing::error!(%error.error, path = %path.display(), "could not replace idempotency ledger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save idempotency ledger; no changes applied".to_owned(),
        )
    })?;
    Ok(())
}
fn lock_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || cfg!(windows) && error.raw_os_error() == Some(33)
}

const CONFIG_LOCK_TIMEOUT: Duration = Duration::from_millis(500);
const CONFIG_LOCK_RETRY: Duration = Duration::from_millis(10);

fn config_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn acquire_config_lock(path: &Path) -> Result<File, ConfigError> {
    let lock_path = config_lock_path(path);
    let parent = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| {
        tracing::error!(%error, path = %lock_path.display(), "could not prepare configuration lock");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not prepare configuration lock; no changes applied".to_owned(),
        )
    })?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            tracing::error!(%error, path = %lock_path.display(), "could not open configuration lock");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not open configuration lock; no changes applied".to_owned(),
            )
        })?;

    let deadline = Instant::now() + CONFIG_LOCK_TIMEOUT;
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => return Ok(lock_file),
            Err(error) if lock_is_contended(&error) => {
                if Instant::now() >= deadline {
                    return Err((
                        StatusCode::CONFLICT,
                        "configuration is locked by another gateway process; retry".to_owned(),
                    ));
                }
                thread::sleep(CONFIG_LOCK_RETRY);
            }
            Err(error) => {
                tracing::error!(%error, path = %lock_path.display(), "could not lock configuration");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not lock configuration; no changes applied".to_owned(),
                ));
            }
        }
    }
}

impl ConfigStore {
    pub fn new(mut config: Config, path: Option<PathBuf>) -> Self {
        for (index, provider) in config.provider.iter_mut().enumerate() {
            provider.key_generation = index as u64 + 1;
        }
        let next_key_generation = AtomicU64::new(config.provider.len() as u64 + 1);
        let revision = path
            .as_deref()
            .and_then(|path| Config::persisted_revision(path).ok())
            .unwrap_or(1);
        Self(Arc::new(Inner {
            current: RwLock::new(config),
            revision: AtomicU64::new(revision),
            updates: Mutex::new(()),
            path,
            next_key_generation,
        }))
    }

    /// Requests own a snapshot and never hold locks during upstream I/O.
    pub fn current(&self) -> Config {
        self.0
            .current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The current secret-free management snapshot, including its opaque revision.
    pub fn snapshot(&self) -> ConfigSnapshot {
        let config = self.0.current.read().unwrap_or_else(|p| p.into_inner());
        let revision = self.0.revision.load(Ordering::Acquire);
        config.snapshot_with_revision(revision)
    }

    /// Saving and publishing happen together even if the HTTP caller disconnects.
    pub async fn update<F>(&self, change: F) -> Result<ConfigSnapshot, ConfigError>
    where
        F: FnOnce(&mut Config) -> Result<(), ConfigError> + Send + 'static,
    {
        self.update_if_revision(None, change).await
    }

    /// Update configuration only if the client still owns the supplied revision.
    /// Missing revisions remain supported for CLI/backwards-compatible callers.
    pub async fn update_if_revision<F>(
        &self,
        expected_revision: Option<u64>,
        change: F,
    ) -> Result<ConfigSnapshot, ConfigError>
    where
        F: FnOnce(&mut Config) -> Result<(), ConfigError> + Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = store.0.updates.lock().unwrap_or_else(|p| p.into_inner());
            let path = store.0.path.as_ref().ok_or_else(|| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "configuration editing requires a config file path".to_owned(),
                )
            })?;
            // Serialize writers across gateway processes as well as within this
            // process. Keep the lock until disk persistence and publication are
            // complete so another process cannot observe a partial update.
            let _config_lock = acquire_config_lock(path)?;
            let current = store.current();
            let current_revision = store.0.revision.load(Ordering::Acquire);
            if let Some(expected) = expected_revision
                && expected != current_revision
            {
                return Err((
                    StatusCode::CONFLICT,
                    "configuration revision is stale; refresh before editing".to_owned(),
                ));
            }
            let mut next = current.clone();
            change(&mut next)?;
            // Avoid overwriting edits made by the CLI or another gateway instance.
            let disk = Config::load(path).map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "configuration file cannot be read; restart the gateway after fixing it"
                        .to_owned(),
                )
            })?;
            let disk_revision = Config::persisted_revision(path).map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "configuration revision cannot be read; restart the gateway after fixing it"
                        .to_owned(),
                )
            })?;
            if disk_revision != current_revision
                || toml::to_string(&disk).ok() != toml::to_string(&current).ok()
            {
                return Err((
                    StatusCode::CONFLICT,
                    "configuration changed on disk; restart the gateway before editing".to_owned(),
                ));
            }
            let revision = current_revision.saturating_add(1);
            next.save_with_revision(path, revision).map_err(|error| {
                tracing::error!(%error, "could not persist configuration");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not save configuration; no changes applied".to_owned(),
                )
            })?;
            // Only changes to the endpoint or effective credential pool start
            // a new schedule. Metadata/group edits preserve active cooldowns.
            for provider in &mut next.provider {
                provider.key_generation = match current.provider(&provider.id) {
                    Some(old)
                        if old.protocol == provider.protocol
                            && old.base_url == provider.base_url
                            && old.api_key_pool() == provider.api_key_pool() =>
                    {
                        old.key_generation
                    }
                    _ => store.0.next_key_generation.fetch_add(1, Ordering::Relaxed),
                };
            }
            // Publish configuration and revision under the same lock so readers
            // cannot pair old configuration with a new revision (or vice versa).
            let mut published = store.0.current.write().unwrap_or_else(|p| p.into_inner());
            store.0.revision.store(revision, Ordering::Release);
            let snapshot = next.snapshot_with_revision(revision);
            *published = next;
            Ok(snapshot)
        })
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "configuration update failed".to_owned(),
            )
        })?
    }
    /// Derive a token-keyed request digest without persisting the credential.
    pub fn idempotency_digest(&self, scope: &str, payload: &[u8]) -> String {
        let token = self.current().token;
        let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes())
            .expect("HMAC-SHA256 accepts keys of any length");
        mac.update(scope.as_bytes());
        mac.update(&[0]);
        mac.update(payload);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Apply a mutation once for an idempotency key and replay its secret-free
    /// result on an identical retry. Pending entries intentionally fail closed
    /// after a crash instead of risking a second side effect.
    pub async fn update_idempotent<F>(
        &self,
        expected_revision: Option<u64>,
        request: IdempotencyRequest,
        change: F,
    ) -> Result<(ConfigSnapshot, bool), ConfigError>
    where
        F: FnOnce(&mut Config) -> Result<(), ConfigError> + Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = store.0.updates.lock().unwrap_or_else(|p| p.into_inner());
            let path = store.0.path.as_ref().ok_or_else(|| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "configuration editing requires a config file path".to_owned(),
                )
            })?;
            let _config_lock = acquire_config_lock(path)?;
            let mut ledger = load_idempotency(path)?;
            if let Some(entry) = ledger
                .entries
                .iter()
                .find(|entry| entry.scope == request.scope && entry.key == request.key)
            {
                if entry.digest != request.digest {
                    return Err((
                        StatusCode::CONFLICT,
                        "idempotency key was already used for a different request".to_owned(),
                    ));
                }
                return match &entry.snapshot {
                    Some(snapshot) => Ok((snapshot.clone(), true)),
                    None => Err((
                        StatusCode::CONFLICT,
                        "idempotent operation is incomplete; inspect the configuration before retrying"
                            .to_owned(),
                    )),
                };
            }

            let current = store.current();
            let current_revision = store.0.revision.load(Ordering::Acquire);
            if let Some(expected) = expected_revision
                && expected != current_revision
            {
                return Err((
                    StatusCode::CONFLICT,
                    "configuration revision is stale; refresh before editing".to_owned(),
                ));
            }
            let mut next = current.clone();
            change(&mut next)?;
            let disk = Config::load(path).map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "configuration file cannot be read; restart the gateway after fixing it"
                        .to_owned(),
                )
            })?;
            let disk_revision = Config::persisted_revision(path).map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "configuration revision cannot be read; restart the gateway after fixing it"
                        .to_owned(),
                )
            })?;
            if disk_revision != current_revision
                || toml::to_string(&disk).ok() != toml::to_string(&current).ok()
            {
                return Err((
                    StatusCode::CONFLICT,
                    "configuration changed on disk; restart the gateway before editing".to_owned(),
                ));
            }

            let entry_index = ledger.entries.len();
            ledger.entries.push(IdempotencyEntry {
                scope: request.scope,
                key: request.key,
                digest: request.digest,
                snapshot: None,
            });
            save_idempotency(path, &ledger)?;

            let revision = current_revision.saturating_add(1);
            if let Err(error) = next.save_with_revision(path, revision) {
                tracing::error!(%error, "could not persist configuration");
                ledger.entries.pop();
                if let Err(cleanup_error) = save_idempotency(path, &ledger) {
                    tracing::error!(
                        error = %cleanup_error.1,
                        "could not remove failed idempotency operation from ledger"
                    );
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "could not save configuration; idempotency recovery is required"
                            .to_owned(),
                    ));
                }
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not save configuration; no changes applied".to_owned(),
                ));
            }
            for provider in &mut next.provider {
                provider.key_generation = match current.provider(&provider.id) {
                    Some(old)
                        if old.protocol == provider.protocol
                            && old.base_url == provider.base_url
                            && old.api_key_pool() == provider.api_key_pool() =>
                    {
                        old.key_generation
                    }
                    _ => store.0.next_key_generation.fetch_add(1, Ordering::Relaxed),
                };
            }

            let snapshot = next.snapshot_with_revision(revision);
            ledger.entries[entry_index].snapshot = Some(snapshot.clone());
            if ledger.entries.len() > IDEMPOTENCY_RETENTION {
                let remove = ledger.entries.len() - IDEMPOTENCY_RETENTION;
                ledger.entries.drain(..remove);
            }
            if let Err(error) = save_idempotency(path, &ledger) {
                tracing::error!(
                    error = %error.1,
                    "configuration saved but idempotency result could not be committed"
                );
                let mut published = store.0.current.write().unwrap_or_else(|p| p.into_inner());
                store.0.revision.store(revision, Ordering::Release);
                *published = next;
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "configuration was saved but idempotency recovery is required before retrying"
                        .to_owned(),
                ));
            }

            let mut published = store.0.current.write().unwrap_or_else(|p| p.into_inner());
            store.0.revision.store(revision, Ordering::Release);
            *published = next;
            Ok((snapshot, false))
        })
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "configuration update failed".to_owned(),
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GroupConfig, RoutingPolicy};
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn stale_revision_never_changes_memory_or_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config = Config::default();
        config.save(&path).unwrap();
        let store = ConfigStore::new(config, Some(path.clone()));
        assert_eq!(store.snapshot().revision, 1);
        let updated = store
            .update_if_revision(Some(1), |config| {
                config.key_cooldown_ms = 123;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(updated.revision, 2);
        let before = std::fs::read(&path).unwrap();
        let error = store
            .update_if_revision(Some(1), |_| {
                panic!("stale edits must not run their mutation");
            })
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(store.current().key_cooldown_ms, 123);
        assert_eq!(store.snapshot().revision, 2);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(store.update(|_| Ok(())).await.unwrap().revision, 3);
    }

    #[tokio::test]
    async fn revision_survives_gateway_restart_and_advances_from_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();

        let first = ConfigStore::new(Config::load(&path).unwrap(), Some(path.clone()));
        assert_eq!(first.snapshot().revision, 1);
        let committed = first
            .update(|config| {
                config.key_cooldown_ms = 123;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(committed.revision, 2);
        drop(first);

        let second = ConfigStore::new(Config::load(&path).unwrap(), Some(path.clone()));
        assert_eq!(second.snapshot().revision, 2);
        let committed = second
            .update_if_revision(Some(2), |config| {
                config.key_cooldown_ms = 456;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(committed.revision, 3);
        assert_eq!(Config::persisted_revision(&path).unwrap(), 3);
    }

    #[tokio::test]
    async fn external_revision_change_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let store = ConfigStore::new(Config::load(&path).unwrap(), Some(path.clone()));

        let disk = Config::load(&path).unwrap();
        disk.save_with_revision(&path, 9).unwrap();
        let error = store
            .update(|config| {
                config.key_cooldown_ms = 999;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(Config::persisted_revision(&path).unwrap(), 9);
        assert_eq!(store.snapshot().revision, 1);
    }
    #[tokio::test]
    async fn save_failure_does_not_publish_a_new_config() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, "this is a file, not a directory").unwrap();
        let store = ConfigStore::new(Config::default(), Some(blocked.join("config.toml")));
        let before = store.current();
        let result = store
            .update(|config| {
                config.group.push(GroupConfig {
                    id: "daily".into(),
                    name: String::new(),
                    routes: vec!["mock/model".into()],
                    weights: Vec::new(),
                    routing: RoutingPolicy::default(),
                    first_token_timeout_ms: 0,
                    affinity: Default::default(),
                });
                Ok(())
            })
            .await;
        assert_eq!(result.unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(store.current().group.is_empty());
        assert!(before.group.is_empty());
        assert_eq!(
            std::fs::read_to_string(blocked).unwrap(),
            "this is a file, not a directory"
        );
    }

    #[tokio::test]
    async fn provider_save_failure_does_not_publish_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, "file").unwrap();
        let store = ConfigStore::new(Config::default(), Some(blocked.join("config.toml")));
        let result = store
            .update(|config| {
                let provider = crate::config::ProviderConfig {
                    id: "local".into(),
                    name: "After".into(),
                    protocol: crate::config::Protocol::Chat,
                    base_url: "http://localhost/v1".into(),
                    api_key: "new-secret".into(),
                    api_keys: vec![],
                    models: vec![],
                    contexts: Default::default(),
                    fallback: vec![],
                    key_cooldown_ms: None,
                    capabilities: Default::default(),
                    routing: RoutingPolicy::Rotate,
                    weights: Vec::new(),
                    affinity: Default::default(),
                    key_generation: 0,
                };
                config.set_provider(provider, true).unwrap();
                Ok(())
            })
            .await;
        assert_eq!(result.unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(store.current().provider.is_empty());
        assert!(store.current().default_provider.is_empty());
    }

    #[tokio::test]
    async fn editing_without_a_path_is_disabled() {
        let store = ConfigStore::new(Config::default(), None);
        let error = store.update(|_| Ok(())).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_held_config_lock_rejects_updates_without_running_the_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config_lock_path(&path))
            .unwrap();
        lock.lock_exclusive().unwrap();

        let store = ConfigStore::new(Config::default(), Some(path.clone()));
        let ran = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&ran);
        let result = store
            .update(move |config| {
                marker.store(true, Ordering::Release);
                config.key_cooldown_ms = 123;
                Ok(())
            })
            .await;

        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(
            store.current().key_cooldown_ms,
            Config::default().key_cooldown_ms
        );
        assert_eq!(
            Config::load(&path).unwrap().key_cooldown_ms,
            Config::default().key_cooldown_ms
        );
        lock.unlock().unwrap();
    }

    #[tokio::test]
    async fn another_store_cannot_overwrite_a_change_after_the_lock_is_released() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let first = ConfigStore::new(Config::load(&path).unwrap(), Some(path.clone()));
        let second = ConfigStore::new(Config::load(&path).unwrap(), Some(path.clone()));

        first
            .update(|config| {
                config.key_cooldown_ms = 123;
                Ok(())
            })
            .await
            .unwrap();
        let result = second
            .update(|config| {
                config.key_cooldown_ms = 456;
                Ok(())
            })
            .await;

        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
        assert_eq!(
            second.current().key_cooldown_ms,
            Config::default().key_cooldown_ms
        );
        assert_eq!(Config::load(&path).unwrap().key_cooldown_ms, 123);
    }
    #[tokio::test]
    async fn idempotent_updates_replay_once_and_survive_restart_without_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config = Config {
            token: "gateway-secret".into(),
            ..Config::default()
        };
        config.save(&path).unwrap();
        let store = ConfigStore::new(config, Some(path.clone()));
        let request = IdempotencyRequest {
            scope: "provider.create".into(),
            key: "request-1".into(),
            digest: store.idempotency_digest("provider.create", br#"{"id":"local"}"#),
        };
        let runs = Arc::new(AtomicU64::new(0));
        let marker = Arc::clone(&runs);
        let first = store
            .update_idempotent(Some(1), request.clone(), move |config| {
                marker.fetch_add(1, Ordering::Relaxed);
                config
                    .set_provider(
                        crate::config::ProviderConfig {
                            id: "local".into(),
                            name: "Local".into(),
                            protocol: crate::config::Protocol::Chat,
                            base_url: "http://localhost/v1".into(),
                            api_key: "upstream-secret".into(),
                            api_keys: vec![],
                            models: vec!["model".into()],
                            contexts: Default::default(),
                            fallback: vec![],
                            key_cooldown_ms: None,
                            capabilities: Default::default(),
                            routing: RoutingPolicy::Rotate,
                            weights: Vec::new(),
                            affinity: Default::default(),
                            key_generation: 0,
                        },
                        true,
                    )
                    .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))
            })
            .await
            .unwrap();
        assert!(!first.1);
        assert_eq!(first.0.revision, 2);

        let replay = store
            .update_idempotent(Some(999), request.clone(), |_| {
                panic!("replayed idempotent mutation must not execute");
            })
            .await
            .unwrap();
        assert!(replay.1);
        assert_eq!(replay.0.revision, first.0.revision);
        assert_eq!(runs.load(Ordering::Relaxed), 1);

        let ledger = std::fs::read_to_string(idempotency_path(&path)).unwrap();
        assert!(!ledger.contains("gateway-secret"));
        assert!(!ledger.contains("upstream-secret"));

        drop(store);
        let restarted = ConfigStore::new(Config::load(&path).unwrap(), Some(path));
        let replay = restarted
            .update_idempotent(None, request, |_| {
                panic!("durable replay must not execute after restart");
            })
            .await
            .unwrap();
        assert!(replay.1);
        assert_eq!(replay.0.revision, 2);
    }

    #[tokio::test]
    async fn idempotency_digest_conflicts_without_running_a_second_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let store = ConfigStore::new(Config::default(), Some(path));
        let request = IdempotencyRequest {
            scope: "group.create".into(),
            key: "same-key".into(),
            digest: "digest-a".into(),
        };
        store
            .update_idempotent(None, request.clone(), |config| {
                config.key_cooldown_ms = 123;
                Ok(())
            })
            .await
            .unwrap();
        let mut different = request;
        different.digest = "digest-b".into();
        let error = store
            .update_idempotent(None, different, |_| {
                panic!("digest conflicts must not run a mutation");
            })
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(store.snapshot().revision, 2);
        assert_eq!(store.current().key_cooldown_ms, 123);
    }

    #[tokio::test]
    async fn pending_idempotency_entries_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let store = ConfigStore::new(Config::default(), Some(path.clone()));
        let request = IdempotencyRequest {
            scope: "group.create".into(),
            key: "pending".into(),
            digest: "digest".into(),
        };
        save_idempotency(
            &path,
            &IdempotencyLedger {
                entries: vec![IdempotencyEntry {
                    scope: request.scope.clone(),
                    key: request.key.clone(),
                    digest: request.digest.clone(),
                    snapshot: None,
                }],
            },
        )
        .unwrap();
        let error = store
            .update_idempotent(None, request, |_| {
                panic!("pending entries must fail closed");
            })
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(store.snapshot().revision, 1);
    }
}
