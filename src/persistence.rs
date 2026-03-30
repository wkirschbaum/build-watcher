use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;

use crate::dirs::state_dir;
use crate::history::BuildHistory;
use crate::watcher::{PersistedWatch, WatchKey};

// -- Safe JSON persistence --
//
// Crash-safe write sequence:
// 1. Serialize → write to .draft → fsync  (crash here: .draft lost, primary intact)
// 2. Parse .draft back to verify           (crash here: .draft orphaned, primary intact)
// 3. Rename primary → .bak                 (crash here: .bak exists, load recovers from it)
// 4. Rename .draft → primary               (crash here: primary missing, load recovers from .bak)
//
// On load, we transparently fall back to .bak if the primary is missing or corrupt.

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("failed to serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("draft verification failed for {0}")]
    Verify(PathBuf),
    #[error("failed to rename {from} to {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
}

pub fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    if let Some(val) = try_parse_file::<T>(path) {
        return Some(val);
    }

    let bak = path.with_extension("json.bak");
    if let Some(val) = try_parse_file::<T>(&bak) {
        tracing::warn!("Primary {} corrupt, recovered from backup", path.display());
        let _ = std::fs::copy(&bak, path);
        return Some(val);
    }

    None
}

pub(crate) fn try_parse_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(val) => Some(val),
        Err(e) => {
            if !data.trim().is_empty() {
                tracing::warn!("{}: parse failed: {e}", path.display());
            }
            None
        }
    }
}

/// Async wrapper around `save_json` that runs the blocking I/O on a dedicated thread.
pub async fn save_json_async<T: Serialize + Send + 'static>(
    path: PathBuf,
    value: T,
) -> Result<(), PersistError> {
    match tokio::task::spawn_blocking(move || save_json(&path, &value)).await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("save_json_async: blocking task panicked: {e}");
            Err(PersistError::Serialize(serde_json::Error::io(
                std::io::Error::other("blocking task panicked"),
            )))
        }
    }
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistError> {
    let data = serde_json::to_string_pretty(value)?;

    let draft = path.with_extension("json.draft");

    // Write and fsync the draft file
    {
        let mut file = std::fs::File::create(&draft).map_err(|e| PersistError::Write {
            path: draft.clone(),
            source: e,
        })?;
        file.write_all(data.as_bytes())
            .map_err(|e| PersistError::Write {
                path: draft.clone(),
                source: e,
            })?;
        file.sync_all().map_err(|e| PersistError::Write {
            path: draft.clone(),
            source: e,
        })?;
    }

    // Verify the draft parses back as valid JSON before committing
    match std::fs::read_to_string(&draft) {
        Ok(readback) => {
            if let Err(e) = serde_json::from_str::<serde_json::Value>(&readback) {
                tracing::error!(
                    path = %draft.display(),
                    written_bytes = data.len(),
                    readback_bytes = readback.len(),
                    error = %e,
                    "Draft readback is not valid JSON"
                );
                let _ = std::fs::remove_file(&draft);
                return Err(PersistError::Verify(draft));
            }
        }
        Err(e) => {
            tracing::error!(
                path = %draft.display(),
                error = %e,
                "Failed to read draft back for verification"
            );
            let _ = std::fs::remove_file(&draft);
            return Err(PersistError::Verify(draft));
        }
    }

    // Backup current file, then promote draft
    let bak = path.with_extension("json.bak");
    if let Err(e) = std::fs::rename(path, &bak) {
        // NotFound is expected on the first save; anything else is worth logging.
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Failed to create backup {}: {e}", bak.display());
        }
    }
    if let Err(e) = std::fs::rename(&draft, path) {
        // Try to restore backup
        let _ = std::fs::rename(&bak, path);
        return Err(PersistError::Rename {
            from: draft,
            to: path.to_path_buf(),
            source: e,
        });
    }

    Ok(())
}

// -- Persistence trait --

/// Abstraction over watch-state and history persistence.
/// `FilePersistence` writes to disk; `NullPersistence` is a no-op for tests.
#[async_trait]
pub trait Persistence: Send + Sync {
    async fn save_watches(
        &self,
        watches: &HashMap<WatchKey, PersistedWatch>,
    ) -> Result<(), PersistError>;
    async fn save_history(&self, history: &BuildHistory) -> Result<(), PersistError>;

    /// Save both watches and history together. Logs errors without failing.
    async fn save_state(
        &self,
        watches: &HashMap<WatchKey, PersistedWatch>,
        history: &BuildHistory,
    ) {
        if let Err(e) = self.save_watches(watches).await {
            tracing::error!(error = %e, "Failed to persist watches");
        }
        if let Err(e) = self.save_history(history).await {
            tracing::error!(error = %e, "Failed to persist history");
        }
    }
}

/// Real persistence — writes JSON to the state directory.
pub struct FilePersistence;

#[async_trait]
impl Persistence for FilePersistence {
    async fn save_watches(
        &self,
        watches: &HashMap<WatchKey, PersistedWatch>,
    ) -> Result<(), PersistError> {
        let path = state_dir().join("watches.json");
        save_json_async(path, watches.clone()).await
    }

    async fn save_history(&self, history: &BuildHistory) -> Result<(), PersistError> {
        let path = state_dir().join("history.json");
        save_json_async(path, crate::history::pruned(history)).await
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
    async fn save_history(&self, _history: &BuildHistory) -> Result<(), PersistError> {
        Ok(())
    }
}
