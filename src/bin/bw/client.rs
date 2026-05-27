use std::time::Duration;

use build_watcher::config::{NotificationConfig, NotificationLevel};
use build_watcher::events::WatchEvent;
use build_watcher::status::{DefaultsConfig, HistoryEntryView};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use super::app::SseUpdate;

// -- Transport --

enum Transport {
    Tcp {
        client: reqwest::Client,
        port: u16,
    },
    #[cfg(unix)]
    Unix {
        socket_path: std::path::PathBuf,
    },
}

// -- DaemonClient --

pub(crate) struct DaemonClient {
    transport: Transport,
}

impl DaemonClient {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            transport: Transport::Tcp {
                client: reqwest::Client::new(),
                port,
            },
        }
    }

    #[cfg(unix)]
    pub(crate) fn new_unix(socket_path: std::path::PathBuf) -> Self {
        Self {
            transport: Transport::Unix { socket_path },
        }
    }

    /// Smart constructor: prefers Unix socket if available, falls back to TCP.
    pub(crate) fn connect(port: u16) -> Self {
        #[cfg(unix)]
        {
            let socket_path = build_watcher::dirs::state_dir().join("daemon.sock");
            if socket_path.exists() {
                return Self::new_unix(socket_path);
            }
        }
        Self::new(port)
    }

    // -- Internal primitives --

    async fn raw_get(&self, path: &str) -> Result<(u16, bytes::Bytes), String> {
        match &self.transport {
            Transport::Tcp { client, port } => {
                let resp = client
                    .get(format!("http://127.0.0.1:{port}{path}"))
                    .send()
                    .await
                    .map_err(|e| format!("connect: {e}"))?;
                let status = resp.status().as_u16();
                let body = resp.bytes().await.map_err(|e| format!("body: {e}"))?;
                Ok((status, body))
            }
            #[cfg(unix)]
            Transport::Unix { socket_path } => unix_get(socket_path, path, &[]).await,
        }
    }

    async fn raw_get_q(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<(u16, bytes::Bytes), String> {
        match &self.transport {
            Transport::Tcp { client, port } => {
                let resp = client
                    .get(format!("http://127.0.0.1:{port}{path}"))
                    .query(params)
                    .send()
                    .await
                    .map_err(|e| format!("connect: {e}"))?;
                let status = resp.status().as_u16();
                let body = resp.bytes().await.map_err(|e| format!("body: {e}"))?;
                Ok((status, body))
            }
            #[cfg(unix)]
            Transport::Unix { socket_path } => unix_get(socket_path, path, params).await,
        }
    }

    async fn raw_post(&self, path: &str, json: Vec<u8>) -> Result<(u16, bytes::Bytes), String> {
        match &self.transport {
            Transport::Tcp { client, port } => {
                let resp = client
                    .post(format!("http://127.0.0.1:{port}{path}"))
                    .header("content-type", "application/json")
                    .body(json)
                    .send()
                    .await
                    .map_err(|e| format!("{path}: {e}"))?;
                let status = resp.status().as_u16();
                let body = resp.bytes().await.map_err(|e| format!("{path}: {e}"))?;
                Ok((status, body))
            }
            #[cfg(unix)]
            Transport::Unix { socket_path } => unix_post(socket_path, path, json).await,
        }
    }

    // -- Shared higher-level helpers --

    pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let (_, body) = self.raw_get(path).await?;
        serde_json::from_slice(&body).map_err(|e| format!("parse: {e}"))
    }

    async fn post_response<T: Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<serde_json::Value, String> {
        let json = serde_json::to_vec(body).map_err(|e| format!("serialize: {e}"))?;
        let (status, bytes) = self.raw_post(path, json).await?;
        if !(200..300).contains(&status) {
            return Err(format!("{path}: HTTP {status}"));
        }
        let val: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))?;
        if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
            return Err(err.to_string());
        }
        Ok(val)
    }

    async fn post_json<T: Serialize>(&self, path: &str, body: &T) -> Result<(), String> {
        self.post_response(path, body).await.map(|_| ())
    }

    async fn post_with_message<T: Serialize>(
        &self,
        path: &str,
        body: &T,
        default_msg: &str,
    ) -> Result<String, String> {
        let json = self.post_response(path, body).await?;
        Ok(json
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(default_msg)
            .to_string())
    }

    // -- Public API --

    pub(crate) async fn pause(&self, pause: bool) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req {
            pause: bool,
        }
        self.post_json("/pause", &Req { pause }).await
    }

    /// Pin or unpin a repo (when `branch` is `None`) or a specific branch.
    pub(crate) async fn pin(
        &self,
        repo: &str,
        branch: Option<&str>,
        pinned: bool,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            branch: Option<&'a str>,
            pinned: bool,
        }
        self.post_json(
            "/pin",
            &Req {
                repo,
                branch,
                pinned,
            },
        )
        .await
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

    pub(crate) async fn set_repo_notifications(
        &self,
        repo: &str,
        action: &str,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            action: &'a str,
        }
        self.post_json("/notifications", &Req { repo, action })
            .await
    }

    pub(crate) async fn get_notifications(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<NotificationConfig, String> {
        let (_, body) = self
            .raw_get_q("/notifications", &[("repo", repo), ("branch", branch)])
            .await?;
        serde_json::from_slice(&body).map_err(|e| format!("parse: {e}"))
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
            build_started: NotificationLevel,
            build_success: NotificationLevel,
            build_failure: NotificationLevel,
        }
        self.post_json(
            "/notifications",
            &Req {
                repo,
                branch,
                action: "set_levels",
                build_started: started,
                build_success: success,
                build_failure: failure,
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
        run_id: Option<u64>,
        failed_only: bool,
    ) -> Result<String, String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            run_id: Option<u64>,
            failed_only: bool,
        }
        self.post_with_message(
            "/rerun",
            &Req {
                repo,
                run_id,
                failed_only,
            },
            "Rerun triggered",
        )
        .await
    }

    pub(crate) async fn merge_pr(&self, repo: &str, number: u64) -> Result<String, String> {
        #[derive(Serialize)]
        struct Req<'a> {
            repo: &'a str,
            number: u64,
        }
        self.post_with_message("/merge", &Req { repo, number }, "PR merged")
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

    pub(crate) async fn set_defaults(&self, defaults: &DefaultsConfig) -> Result<(), String> {
        self.post_json("/defaults", defaults).await
    }

    pub(crate) async fn get_repo_config(
        &self,
        repo: &str,
    ) -> Result<build_watcher::status::RepoConfigView, String> {
        let (_, body) = self.raw_get_q("/repo-config", &[("repo", repo)]).await?;
        serde_json::from_slice(&body).map_err(|e| format!("parse: {e}"))
    }

    pub(crate) async fn set_repo_config(
        &self,
        config: &build_watcher::status::RepoConfigView,
    ) -> Result<(), String> {
        self.post_json("/repo-config", config).await
    }

    pub(crate) async fn get_history(
        &self,
        repo: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntryView>, String> {
        let limit_str = limit.to_string();
        let mut params = vec![("repo", repo), ("limit", &limit_str)];
        let branch_owned;
        if let Some(b) = branch {
            branch_owned = b;
            params.push(("branch", branch_owned));
        }
        let (status, bytes) = self.raw_get_q("/history", &params).await?;
        if !(200..300).contains(&status) {
            let body = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "history: {}",
                build_watcher::format::truncate(&body, 200)
            ));
        }
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    pub(crate) async fn get_all_history(
        &self,
        limit: u32,
    ) -> Result<Vec<HistoryEntryView>, String> {
        self.get_json::<Vec<HistoryEntryView>>(&format!("/history/all?limit={limit}"))
            .await
    }

    pub(crate) async fn get_auto_discover_rules(
        &self,
    ) -> Result<Vec<build_watcher::status::AutoDiscoverRuleView>, String> {
        self.get_json("/auto-discover-rules").await
    }

    pub(crate) async fn add_auto_discover_rule(
        &self,
        id: &str,
        org_pattern: Option<&str>,
        repo_pattern: Option<&str>,
        recently_updated: &str,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            org_pattern: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            repo_pattern: Option<&'a str>,
            recently_updated: &'a str,
        }
        self.post_json(
            "/auto-discover-rules",
            &Req {
                id,
                org_pattern,
                repo_pattern,
                recently_updated,
            },
        )
        .await
    }

    pub(crate) async fn remove_auto_discover_rule(&self, id: &str) -> Result<(), String> {
        #[derive(Serialize)]
        struct Req<'a> {
            id: &'a str,
        }
        self.post_json("/auto-discover-rules/remove", &Req { id })
            .await
    }
}

impl Clone for DaemonClient {
    fn clone(&self) -> Self {
        Self {
            transport: match &self.transport {
                Transport::Tcp { client, port } => Transport::Tcp {
                    client: client.clone(),
                    port: *port,
                },
                #[cfg(unix)]
                Transport::Unix { socket_path } => Transport::Unix {
                    socket_path: socket_path.clone(),
                },
            },
        }
    }
}

// -- Unix socket helpers --

/// Percent-encode a single query parameter value.
#[cfg(unix)]
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Build path + query string (e.g. `/notifications?repo=org%2Frepo&branch=main`).
#[cfg(unix)]
fn build_path(path: &str, params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return path.to_string();
    }
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, pct_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{qs}")
}

#[cfg(unix)]
async fn unix_handshake(
    socket_path: &std::path::Path,
) -> Result<hyper::client::conn::http1::SendRequest<http_body_util::Full<bytes::Bytes>>, String> {
    use hyper_util::rt::TokioIo;

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("handshake: {e}"))?;

    tokio::spawn(async move {
        let _ = conn.await;
    });

    Ok(sender)
}

#[cfg(unix)]
async fn unix_get(
    socket_path: &std::path::Path,
    path: &str,
    params: &[(&str, &str)],
) -> Result<(u16, bytes::Bytes), String> {
    use http_body_util::BodyExt as _;

    let mut sender = unix_handshake(socket_path).await?;
    let req = http::Request::get(build_path(path, params))
        .header("host", "localhost")
        .body(http_body_util::Full::<bytes::Bytes>::default())
        .map_err(|e| format!("build: {e}"))?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("body: {e}"))?
        .to_bytes();
    Ok((status, body))
}

#[cfg(unix)]
async fn unix_post(
    socket_path: &std::path::Path,
    path: &str,
    json: Vec<u8>,
) -> Result<(u16, bytes::Bytes), String> {
    use http_body_util::BodyExt as _;

    let mut sender = unix_handshake(socket_path).await?;
    let req = http::Request::post(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("content-length", json.len().to_string())
        .body(http_body_util::Full::new(bytes::Bytes::from(json)))
        .map_err(|e| format!("build: {e}"))?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("body: {e}"))?
        .to_bytes();
    Ok((status, body))
}

// -- SSE streaming --

async fn stream_sse(
    daemon: &DaemonClient,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    match &daemon.transport {
        Transport::Tcp { client, port } => {
            let url = format!("http://127.0.0.1:{port}/events");
            stream_sse_tcp(client, &url, tx, connected).await
        }
        #[cfg(unix)]
        Transport::Unix { socket_path } => stream_sse_unix(socket_path, tx, connected).await,
    }
}

async fn stream_sse_tcp(
    client: &reqwest::Client,
    url: &str,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("SSE connect failed: {e}");
            return false;
        }
    };

    *connected = true;
    if tx.send(SseUpdate::Connected).await.is_err() {
        return true;
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut pending_data: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!("SSE stream error: {e}");
                return false;
            }
        };
        if let Some(done) = feed_sse_chunk(&bytes, &mut buf, &mut pending_data, tx).await {
            return done;
        }
    }

    false
}

#[cfg(unix)]
async fn stream_sse_unix(
    socket_path: &std::path::Path,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    use http_body_util::BodyExt as _;
    use hyper_util::rt::TokioIo;

    let stream = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("SSE connect failed: {e}");
            return false;
        }
    };

    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("SSE handshake failed: {e}");
            return false;
        }
    };

    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = http::Request::get("/events")
        .header("host", "localhost")
        .body(http_body_util::Full::<bytes::Bytes>::default())
        .unwrap();

    let response = match sender.send_request(req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("SSE connect failed: {e}");
            return false;
        }
    };

    *connected = true;
    if tx.send(SseUpdate::Connected).await.is_err() {
        return true;
    }

    let mut body = response.into_body();
    let mut buf = String::new();
    let mut pending_data: Option<String> = None;

    loop {
        match body.frame().await {
            None => break,
            Some(Err(e)) => {
                tracing::debug!("SSE stream error: {e}");
                return false;
            }
            Some(Ok(frame)) => {
                let Ok(bytes) = frame.into_data() else {
                    continue;
                };
                if let Some(done) = feed_sse_chunk(&bytes, &mut buf, &mut pending_data, tx).await {
                    return done;
                }
            }
        }
    }

    false
}

/// Parse one chunk of SSE data into `buf`, dispatch complete events to `tx`.
/// Returns `Some(true)` if the channel closed, `Some(false)` for error, `None` to continue.
async fn feed_sse_chunk(
    chunk: &[u8],
    buf: &mut String,
    pending_data: &mut Option<String>,
    tx: &mpsc::Sender<SseUpdate>,
) -> Option<bool> {
    buf.push_str(&String::from_utf8_lossy(chunk));

    while let Some(pos) = buf.find('\n') {
        let line = buf[..pos].trim_end_matches('\r').to_string();
        buf.drain(..=pos);

        if line.is_empty() {
            if let Some(data) = pending_data.take() {
                match serde_json::from_str::<WatchEvent>(&data) {
                    Ok(event) => {
                        if tx.send(SseUpdate::Event(Box::new(event))).await.is_err() {
                            return Some(true);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("SSE parse error: {e}");
                    }
                }
            }
        } else if let Some(data) = line.strip_prefix("data: ") {
            if let Some(existing) = pending_data {
                existing.push('\n');
                existing.push_str(data);
            } else {
                *pending_data = Some(data.to_string());
            }
        }
    }

    None
}

/// SSE background task: connects, streams events, reconnects with exponential backoff.
pub(crate) async fn sse_task(daemon: DaemonClient, tx: mpsc::Sender<SseUpdate>) {
    let mut backoff_secs = 1u64;
    loop {
        let mut connected = false;
        if stream_sse(&daemon, &tx, &mut connected).await {
            break;
        }
        if tx.send(SseUpdate::Disconnected).await.is_err() {
            break;
        }
        if connected {
            backoff_secs = 1;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn feed_sse_chunk_concatenates_multiline_data() {
        // RFC 8895 says multiple "data:" lines in a single event are
        // joined with newline before dispatch.
        let (tx, mut rx) = mpsc::channel(8);
        let mut buf = String::new();
        let mut pending: Option<String> = None;

        // Simulate a valid two-line data event followed by a blank line.
        // The two data lines should be joined as "line1\nline2".
        let chunk = b"data: line1\ndata: line2\n\n";
        feed_sse_chunk(chunk, &mut buf, &mut pending, &tx).await;

        // We can't easily parse the WatchEvent here, but we can confirm
        // the channel received exactly one message (the parse might fail
        // since "line1\nline2" isn't valid JSON, but the concat happened).
        // To test concat in isolation, inspect pending before blank line:
        let mut buf2 = String::new();
        let mut pending2: Option<String> = None;

        // Feed just the two data lines without the blank line — pending should
        // contain the concatenated string.
        feed_sse_chunk(
            b"data: first\ndata: second\n",
            &mut buf2,
            &mut pending2,
            &tx,
        )
        .await;
        assert_eq!(
            pending2.as_deref(),
            Some("first\nsecond"),
            "multi-line data should be joined with newline"
        );

        drop(tx);
        // Drain channel (the first chunk parse may fail but that's OK here).
        while rx.try_recv().is_ok() {}
    }
}
