//! Serialized, persistent configuration updates shared by request handlers.
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use axum::http::StatusCode;

use crate::config::{Config, ConfigSnapshot};

#[derive(Clone)]
pub struct ConfigStore(Arc<Inner>);

struct Inner {
    current: RwLock<Config>,
    updates: Mutex<()>,
    path: Option<PathBuf>,
}

pub type ConfigError = (StatusCode, String);

impl ConfigStore {
    pub fn new(config: Config, path: Option<PathBuf>) -> Self {
        Self(Arc::new(Inner {
            current: RwLock::new(config),
            updates: Mutex::new(()),
            path,
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

    /// Saving and publishing happen together even if the HTTP caller disconnects.
    pub async fn update<F>(&self, change: F) -> Result<ConfigSnapshot, ConfigError>
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
                tracing::error!(%error, "could not persist routing configuration");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not save configuration; no changes applied".to_owned(),
                )
            })?;
            let snapshot = next.snapshot();
            *store.0.current.write().unwrap_or_else(|p| p.into_inner()) = next;
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
    use crate::config::GroupConfig;

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
    async fn editing_without_a_path_is_disabled() {
        let store = ConfigStore::new(Config::default(), None);
        let error = store.update(|_| Ok(())).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
