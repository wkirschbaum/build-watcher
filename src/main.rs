mod config;
mod github;
mod platform;
mod server;
mod watcher;

use std::sync::Arc;

use anyhow::Result;
use config::{load_config, save_config, state_dir};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use server::BuildWatcher;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use watcher::{SharedConfig, Watches, load_watches, save_watches, startup_watches};

const DEFAULT_PORT: u16 = 8417;

async fn bind_with_fallback(preferred: u16) -> Result<tokio::net::TcpListener> {
    for port in preferred..=preferred.saturating_add(9) {
        match tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
            Ok(l) => return Ok(l),
            Err(_) if port < preferred.saturating_add(9) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("build_watcher=info".parse()?))
        .init();

    let port: u16 = std::env::var("BUILD_WATCHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let cfg = load_config();

    // Re-save config on startup to normalize schema (adds missing fields with defaults)
    save_config(&cfg);

    let config: SharedConfig = Arc::new(Mutex::new(cfg));
    let watches: Watches = Arc::new(Mutex::new(load_watches()));

    // Auto-watch all repos from config (resumes existing, starts new)
    startup_watches(&watches, &config).await;

    let ct = CancellationToken::new();
    let http_config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        sse_keep_alive: None,
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let watches_for_factory = watches.clone();
    let config_for_factory = config.clone();
    let service: StreamableHttpService<BuildWatcher, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(BuildWatcher::new(
                    watches_for_factory.clone(),
                    config_for_factory.clone(),
                ))
            },
            Default::default(),
            http_config,
        );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = bind_with_fallback(port).await?;
    let bound_port = listener.local_addr()?.port();

    // Write the actual port to the state dir so tooling can discover it
    let port_file = state_dir().join("port");
    if let Err(e) = std::fs::write(&port_file, bound_port.to_string()) {
        tracing::warn!("Failed to write port file {}: {e}", port_file.display());
    }

    if bound_port != port {
        tracing::warn!("Port {port} was occupied, using port {bound_port} instead");
        tracing::warn!("Re-run install.sh to update the MCP URL in ~/.claude.json");
    }
    tracing::info!("build-watcher listening on http://127.0.0.1:{bound_port}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            ct.cancel();
        })
        .await?;

    save_watches(&watches).await;
    let _ = std::fs::remove_file(&port_file);
    tracing::info!("State saved, goodbye.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::watcher::{parse_watch_key, watch_key};

    #[test]
    fn watch_key_format() {
        assert_eq!(watch_key("alice/myapp", "main"), "alice/myapp#main");
    }

    #[test]
    fn parse_watch_key_splits_correctly() {
        assert_eq!(parse_watch_key("alice/myapp#main"), ("alice/myapp", "main"));
    }

    #[test]
    fn parse_watch_key_falls_back_to_main() {
        assert_eq!(parse_watch_key("alice/myapp"), ("alice/myapp", "main"));
    }
}
