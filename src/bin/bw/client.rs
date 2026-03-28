use std::time::Duration;

use build_watcher::config::{NotificationConfig, NotificationLevel};
use build_watcher::events::WatchEvent;
use build_watcher::status::{DefaultsConfig, HistoryEntryView};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use super::app::SseUpdate;

#[derive(Clone)]
pub(crate) struct DaemonClient {
    pub(crate) client: reqwest::Client,
    port: u16,
}

impl DaemonClient {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            client: reqwest::Client::new(),
            port,
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
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

    async fn post_json<T: Serialize>(&self, path: &str, body: &T) -> Result<(), String> {
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
        #[derive(Serialize)]
        struct Req {
            pause: bool,
        }
        self.post_json("/pause", &Req { pause }).await
    }

    pub(crate) async fn watch(&self, repo: &str) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repos: [&'a str; 1],
        }
        self.post_json("/watch", &Req { repos: [repo] }).await
    }

    pub(crate) async fn unwatch(&self, repo: &str) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repos: [&'a str; 1],
        }
        self.post_json("/unwatch", &Req { repos: [repo] }).await
    }

    pub(crate) async fn set_notifications(
        &self,
        repo: &str,
        branch: &str,
        action: &str,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            branch: &'a str,
            action: &'a str,
        }
        self.post_json(
            "/notifications",
            &Req {
                repo,
                branch,
                action,
            },
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
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            branch: &'a str,
            action: &'static str,
            build_started: String,
            build_success: String,
            build_failure: String,
        }
        self.post_json(
            "/notifications",
            &Req {
                repo,
                branch,
                action: "set_levels",
                build_started: started.to_string(),
                build_success: success.to_string(),
                build_failure: failure.to_string(),
            },
        )
        .await
    }

    pub(crate) async fn set_branches(&self, repo: &str, branches: &[String]) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            branches: &'a [String],
        }
        self.post_json("/branches", &Req { repo, branches }).await
    }

    pub(crate) async fn rerun(
        &self,
        repo: &str,
        run_id: u64,
        failed_only: bool,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            run_id: u64,
            failed_only: bool,
        }
        self.post_json(
            "/rerun",
            &Req {
                repo,
                run_id,
                failed_only,
            },
        )
        .await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req {}
        self.post_json("/shutdown", &Req {}).await
    }

    pub(crate) async fn get_defaults(&self) -> Result<DefaultsConfig, String> {
        self.get_json("/defaults").await
    }

    pub(crate) async fn set_defaults(
        &self,
        default_branches: Option<Vec<String>>,
        ignored_workflows: Option<Vec<String>>,
        poll_aggression: Option<String>,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req {
            default_branches: Option<Vec<String>>,
            ignored_workflows: Option<Vec<String>>,
            poll_aggression: Option<String>,
        }
        self.post_json(
            "/defaults",
            &Req {
                default_branches,
                ignored_workflows,
                poll_aggression,
            },
        )
        .await
    }

    pub(crate) async fn get_history(
        &self,
        repo: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntryView>, String> {
        let mut query = vec![("repo", repo.to_string()), ("limit", limit.to_string())];
        if let Some(b) = branch {
            query.push(("branch", b.to_string()));
        }
        let resp = self
            .client
            .get(self.url("/history"))
            .query(&query)
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
}

// -- SSE streaming --

async fn stream_sse(
    daemon: &DaemonClient,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    let response = match daemon.client.get(daemon.url("/events")).send().await {
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
pub(crate) async fn sse_task(daemon: DaemonClient, tx: mpsc::Sender<SseUpdate>) {
    let mut backoff_secs = 1u64;
    loop {
        let mut connected = false;
        if stream_sse(&daemon, &tx, &mut connected).await {
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

// -- Utilities --

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
