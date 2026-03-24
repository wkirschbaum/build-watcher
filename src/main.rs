mod config;
mod github;
mod platform;
mod server;
mod watcher;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("build_watcher=info".parse()?))
        .init();

    let config = Arc::new(Mutex::new(config::load_and_normalize()));
    let watches = Arc::new(Mutex::new(watcher::load_watches()));

    let ct = CancellationToken::new();
    let handle = watcher::WatcherHandle::new(ct.clone());
    watcher::startup_watches(&watches, &config, &handle).await;

    server::serve(watches, config, handle, ct).await
}
