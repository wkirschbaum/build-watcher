use std::time::Duration;

use build_watcher::config::{NotificationConfig, NotificationLevel};
use build_watcher::dirs::state_dir;
use build_watcher::events::WatchEvent;
use build_watcher::status::HistoryEntryView;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use super::app::SseUpdate;

#[derive(Clone)]
pub(crate) struct DaemonClient {
    client: reqwest::Client,
    port: u16,
}

impl DaemonClient {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            client: reqwest::Client::new(),
            port,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let resp = self
            .client
            .get(self.url(path))
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        resp.json::<T>().await.map_err(|e| format!("parse: {e}"))
    }

    pub(crate) async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<(), String> {
        let resp = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("{path}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{path}: HTTP {}", resp.status()));
        }
        // Daemon handlers return {"error": "..."} on validation failures (with 200 status).
        let json: serde_json::Value = resp.json().await.map_err(|e| format!("{path}: {e}"))?;
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            return Err(err.to_string());
        }
        Ok(())
    }

    pub(crate) async fn pause(&self, pause: bool) -> Result<(), String> {
        self.post_json("/pause", &serde_json::json!({ "pause": pause }))
            .await
    }

    pub(crate) async fn watch(&self, repo: &str) -> Result<(), String> {
        self.post_json("/watch", &serde_json::json!({ "repos": [repo] }))
            .await
    }

    pub(crate) async fn unwatch(&self, repo: &str) -> Result<(), String> {
        self.post_json("/unwatch", &serde_json::json!({ "repos": [repo] }))
            .await
    }

    pub(crate) async fn set_notifications(
        &self,
        repo: &str,
        branch: &str,
        action: &str,
    ) -> Result<(), String> {
        self.post_json(
            "/notifications",
            &serde_json::json!({ "repo": repo, "branch": branch, "action": action }),
        )
        .await
    }

    pub(crate) async fn get_notifications(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<NotificationConfig, String> {
        let resp = self
            .client
            .get(self.url("/notifications"))
            .query(&[("repo", repo), ("branch", branch)])
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        resp.json::<NotificationConfig>()
            .await
            .map_err(|e| format!("parse: {e}"))
    }

    pub(crate) async fn set_notification_levels(
        &self,
        repo: &str,
        branch: &str,
        started: NotificationLevel,
        success: NotificationLevel,
        failure: NotificationLevel,
    ) -> Result<(), String> {
        self.post_json(
            "/notifications",
            &serde_json::json!({
                "repo": repo,
                "branch": branch,
                "action": "set_levels",
                "build_started": started.to_string(),
                "build_success": success.to_string(),
                "build_failure": failure.to_string(),
            }),
        )
        .await
    }

    pub(crate) async fn set_branches(&self, repo: &str, branches: &[String]) -> Result<(), String> {
        self.post_json(
            "/branches",
            &serde_json::json!({ "repo": repo, "branches": branches }),
        )
        .await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.post_json("/shutdown", &serde_json::json!({})).await
    }

    pub(crate) async fn get_defaults(&self) -> Result<Defaults, String> {
        self.get_json("/defaults").await
    }

    pub(crate) async fn set_defaults(
        &self,
        default_branches: Option<Vec<String>>,
        ignored_workflows: Option<Vec<String>>,
        poll_aggression: Option<String>,
    ) -> Result<(), String> {
        self.post_json(
            "/defaults",
            &serde_json::json!({
                "default_branches": default_branches,
                "ignored_workflows": ignored_workflows,
                "poll_aggression": poll_aggression,
            }),
        )
        .await
    }

    pub(crate) async fn get_history(
        &self,
        repo: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntryView>, String> {
        let mut url = format!(
            "http://127.0.0.1:{}/history?repo={}&limit={}",
            self.port, repo, limit
        );
        if let Some(b) = branch {
            url.push_str(&format!("&branch={}", b));
        }
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("history: {body}"));
        }
        resp.json::<Vec<HistoryEntryView>>()
            .await
            .map_err(|e| e.to_string())
    }

    pub(crate) async fn get_all_history(
        &self,
        limit: u32,
    ) -> Result<Vec<HistoryEntryView>, String> {
        self.get_json::<Vec<HistoryEntryView>>(&format!("/history/all?limit={limit}"))
            .await
    }

    /// Inner client ref for the SSE background task (which needs `bytes_stream`).
    pub(crate) fn inner(&self) -> &reqwest::Client {
        &self.client
    }
}

/// Global defaults returned by `GET /defaults`.
#[derive(serde::Deserialize)]
pub(crate) struct Defaults {
    pub(crate) default_branches: Vec<String>,
    pub(crate) ignored_workflows: Vec<String>,
    #[serde(default)]
    pub(crate) poll_aggression: String,
}

async fn stream_sse(
    client: &reqwest::Client,
    port: u16,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    let url = format!("http://127.0.0.1:{port}/events");
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };

    *connected = true;
    if tx.send(SseUpdate::Connected).await.is_err() {
        return true; // channel closed — main task exited
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut pending_data: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(_) => return false,
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);

            if line.is_empty() {
                // End of SSE frame — dispatch accumulated data.
                if let Some(data) = pending_data.take()
                    && let Ok(event) = serde_json::from_str::<WatchEvent>(&data)
                    && tx.send(SseUpdate::Event(Box::new(event))).await.is_err()
                {
                    return true;
                }
            } else if let Some(data) = line.strip_prefix("data: ") {
                pending_data = Some(data.to_string());
                // Lines starting with "event:", "id:", or ":" (comments) are ignored.
            }
        }
    }

    false // stream ended cleanly
}

/// SSE background task: connects, streams events, reconnects with exponential backoff.
pub(crate) async fn sse_task(client: reqwest::Client, port: u16, tx: mpsc::Sender<SseUpdate>) {
    let mut backoff_secs = 1u64;
    loop {
        let mut connected = false;
        if stream_sse(&client, port, &tx, &mut connected).await {
            break; // channel closed
        }
        if tx.send(SseUpdate::Disconnected).await.is_err() {
            break;
        }
        if connected {
            backoff_secs = 1; // successful connection — reset backoff
        } else {
            backoff_secs = (backoff_secs * 2).min(30);
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
    }
}

// -- Actions --

pub(crate) fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// -- Entry point --

/// Read the daemon port from the port file, or start the daemon if it's not running.
pub(crate) fn discover_or_start_daemon() -> Result<u16, Box<dyn std::error::Error>> {
    let port_file = state_dir().join("port");

    // Try reading existing port file first.
    if let Ok(contents) = std::fs::read_to_string(&port_file)
        && let Ok(port) = contents.trim().parse::<u16>()
    {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return Ok(port);
        }
        // Port file exists but daemon is not responding — stale file.
        let _ = std::fs::remove_file(&port_file);
    }

    // Daemon not running — try to start it.
    eprintln!("Daemon not running, starting build-watcher…");
    let exe = std::env::current_exe()?;
    let daemon_bin = exe
        .parent()
        .ok_or("cannot resolve binary directory")?
        .join("build-watcher");

    if !daemon_bin.exists() {
        return Err(format!(
            "build-watcher binary not found at {}\nInstall it with ./install.sh",
            daemon_bin.display()
        )
        .into());
    }

    std::process::Command::new(&daemon_bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start daemon: {e}"))?;

    // Wait for the port file to appear (up to 5 seconds).
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(contents) = std::fs::read_to_string(&port_file)
            && let Ok(port) = contents.trim().parse::<u16>()
            && std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
        {
            return Ok(port);
        }
    }

    Err("Timed out waiting for daemon to start".into())
}
