use std::collections::HashMap;

use async_trait::async_trait;

use crate::config::{self, Config, PersistError};
use crate::history::{BuildHistory, MAX_HISTORY};
use crate::watcher::{PersistedWatch, WatchKey};

/// Abstraction over state/config persistence.
/// `FilePersistence` writes to disk; `NullPersistence` is a no-op for tests.
#[async_trait]
pub trait Persistence: Send + Sync {
    async fn save_watches(
        &self,
        watches: &HashMap<WatchKey, PersistedWatch>,
    ) -> Result<(), PersistError>;
    async fn save_config(&self, config: &Config) -> Result<(), PersistError>;
    async fn save_history(&self, history: &BuildHistory) -> Result<(), PersistError>;
}

/// Real persistence — writes JSON to the state/config directories.
pub struct FilePersistence;

#[async_trait]
impl Persistence for FilePersistence {
    async fn save_watches(
        &self,
        watches: &HashMap<WatchKey, PersistedWatch>,
    ) -> Result<(), PersistError> {
        let path = config::state_dir().join("watches.json");
        config::save_json_async(path, watches.clone()).await
    }

    async fn save_config(&self, config: &Config) -> Result<(), PersistError> {
        config::save_config_async(config).await
    }

    async fn save_history(&self, history: &BuildHistory) -> Result<(), PersistError> {
        let pruned: BuildHistory = history
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().take(MAX_HISTORY).cloned().collect()))
            .collect();
        let path = config::state_dir().join("history.json");
        config::save_json_async(path, pruned).await
    }
}

/// No-op persistence for tests.
pub struct NullPersistence;

#[async_trait]
impl Persistence for NullPersistence {
    async fn save_watches(
        &self,
        _watches: &HashMap<WatchKey, PersistedWatch>,
    ) -> Result<(), PersistError> {
        Ok(())
    }
    async fn save_config(&self, _config: &Config) -> Result<(), PersistError> {
        Ok(())
    }
    async fn save_history(&self, _history: &BuildHistory) -> Result<(), PersistError> {
        Ok(())
    }
}
