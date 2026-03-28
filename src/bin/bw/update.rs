/// Check GitHub releases for a newer version. Returns `Some(tag)` if a newer
/// version is available, `None` if already up to date or the check fails
/// (including network errors — failures are logged at warn level and silently
/// ignored so an offline environment never surfaces errors in the TUI).
pub(crate) async fn check_latest() -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }

    let output = tokio::process::Command::new("gh")
        .args(["api", "repos/wkirschbaum/build-watcher/releases/latest"])
        .output()
        .await;

    let output = match output {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            tracing::warn!(
                "update check: gh api failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return None;
        }
        Err(e) => {
            tracing::warn!("update check failed: {e}");
            return None;
        }
    };

    let release = match serde_json::from_slice::<Release>(&output) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("update check: failed to parse response: {e}");
            return None;
        }
    };

    let latest = release.tag_name.trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        (Ok(remote), Ok(local)) if remote > local => Some(release.tag_name),
        _ => None,
    }
}
