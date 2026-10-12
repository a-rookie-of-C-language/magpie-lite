//! Serialized, persistent configuration updates shared by request handlers.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use axum::http::StatusCode;

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

impl ConfigStore {
    pub fn new(mut config: Config, path: Option<PathBuf>) -> Self {
        for (index, provider) in config.provider.iter_mut().enumerate() {
            provider.key_generation = index as u64 + 1;
        }
        let next_key_generation = AtomicU64::new(config.provider.len() as u64 + 1);
        Self(Arc::new(Inner {
            current: RwLock::new(config),
            revision: AtomicU64::new(1),
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
            if toml::to_string(&disk).ok() != toml::to_string(&current).ok() {
                return Err((
                    StatusCode::CONFLICT,
                    "configuration changed on disk; restart the gateway before editing".to_owned(),
                ));
            }
            next.save(path).map_err(|error| {
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
            let revision = store.0.revision.fetch_add(1, Ordering::AcqRel) + 1;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GroupConfig, RoutingPolicy};

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
                    routing: RoutingPolicy::default(),
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
                    fallback: vec![],
                    key_cooldown_ms: None,
                    capabilities: Default::default(),
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
}
