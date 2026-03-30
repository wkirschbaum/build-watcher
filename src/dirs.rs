use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| {
        tracing::warn!("HOME is not set; falling back to /tmp for state/config directories");
        "/tmp".to_string()
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Unsupported platform: only Linux and macOS are supported");

#[cfg(target_os = "linux")]
fn default_state_dir() -> String {
    format!("{}/.local/state/build-watcher", home_dir())
}

#[cfg(target_os = "linux")]
fn default_config_dir() -> String {
    format!("{}/.config/build-watcher", home_dir())
}

#[cfg(target_os = "macos")]
fn default_state_dir() -> String {
    format!(
        "{}/Library/Application Support/build-watcher/state",
        home_dir()
    )
}

#[cfg(target_os = "macos")]
fn default_config_dir() -> String {
    format!(
        "{}/Library/Application Support/build-watcher/config",
        home_dir()
    )
}

fn init_dir(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!("Failed to create directory {}: {e}", dir.display());
    }
}

pub fn state_dir() -> &'static Path {
    STATE_DIR.get_or_init(|| {
        let dir = PathBuf::from(std::env::var("STATE_DIRECTORY").unwrap_or_else(|_| {
            #[cfg(test)]
            panic!("STATE_DIRECTORY must be set in tests to avoid writing to the real state dir");
            #[cfg(not(test))]
            default_state_dir()
        }));
        init_dir(&dir);
        dir
    })
}

pub fn config_dir() -> &'static Path {
    CONFIG_DIR.get_or_init(|| {
        let dir = PathBuf::from(
            std::env::var("CONFIGURATION_DIRECTORY").unwrap_or_else(|_| {
                #[cfg(test)]
                panic!(
                    "CONFIGURATION_DIRECTORY must be set in tests to avoid writing to the real config dir"
                );
                #[cfg(not(test))]
                default_config_dir()
            }),
        );
        init_dir(&dir);
        dir
    })
}
