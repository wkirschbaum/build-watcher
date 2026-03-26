use std::collections::HashMap;

use async_trait::async_trait;

use crate::config::{self, Config, PersistError};
use crate::watcher::{PersistedWatch, WatchKey};

/// Abstraction over state/config persistence.
/// `FilePersistence` writes to disk; `NullPersistence` is a no-op for tests.
#[async_trait]
pub trait Persistence: Send + Sync {
    async fn save_watches(&self, watches: &HashMap<WatchKey, PersistedWatch>);
    async fn save_config(&self, config: &Config) -> Result<(), PersistError>;
}

/// Real persistence — writes JSON to the state/config directories.
pub struct FilePersistence;

#[async_trait]
impl Persistence for FilePersistence {
    async fn save_watches(&self, watches: &HashMap<WatchKey, PersistedWatch>) {
        let path = config::state_dir().join("watches.json");
        if let Err(e) = config::save_json_async(path, watches.clone()).await {
            tracing::error!("Failed to save watches: {e}");
        }
    }

    async fn save_config(&self, config: &Config) -> Result<(), PersistError> {
        config::save_config_async(config).await
    }
}

/// No-op persistence for tests.
pub struct NullPersistence;

#[async_trait]
impl Persistence for NullPersistence {
    async fn save_watches(&self, _watches: &HashMap<WatchKey, PersistedWatch>) {}
    async fn save_config(&self, _config: &Config) -> Result<(), PersistError> {
        Ok(())
    }
}
