mod notification;
mod platform;
mod register;
mod server;

use std::collections::HashMap;
use std::sync::Arc;

use build_watcher::config;
use build_watcher::events::EventBus;
use build_watcher::github::{GhCliClient, GitHubClient};
use build_watcher::history::load_history;
use build_watcher::persistence::FilePersistence;
use build_watcher::watcher::{
    PauseState, RateLimitState, WatcherHandle, load_persisted_watches, startup_watches,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use config::{ConfigManager, ConfigPersistence};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--register") {
        let port = args
            .iter()
            .position(|a| a == "--port")
            .and_then(|i| args.get(i + 1))
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(server::DEFAULT_PORT);
        return register::register(port);
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("build_watcher=info".parse()?))
        .init();

    // Acquire the instance lock before any expensive startup work (config saves,
    // GitHub API calls) so we fail fast if another daemon is already running.
    let lock = server::acquire_instance_lock()?;

    let config = Arc::new(ConfigManager::new(
        config::load_and_normalize(),
        ConfigPersistence::File,
    ));
    let persisted = load_persisted_watches();
    let watches = Arc::new(Mutex::new(HashMap::new()));
    let pause: PauseState = Arc::new(Mutex::new(None));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let events = EventBus::new();

    // Subscribe before starting watches so no events are missed.
    let notifier = platform::init().await;
    tokio::spawn(notification::run_notification_handler(
        events.subscribe(),
        config.clone(),
        pause.clone(),
        notifier,
    ));

    let ct = CancellationToken::new();
    let gh: Arc<dyn GitHubClient> = Arc::new(GhCliClient);
    let persistence: Arc<dyn build_watcher::persistence::Persistence> = Arc::new(FilePersistence);
    let history = Arc::new(Mutex::new(load_history()));
    let handle = WatcherHandle::new(
        ct.clone(),
        events,
        gh,
        persistence,
        history,
        config.changed().clone(),
    );
    startup_watches(&watches, &config, &handle, &rate_limit, persisted).await;

    let state = server::DaemonState {
        watches,
        config,
        handle,
        pause,
        rate_limit,
        started_at: std::time::Instant::now(),
    };
    server::serve(state, ct, lock).await.map_err(|e| e.into())
}
