use self_update::cargo_crate_version;

/// Check GitHub releases for a newer version. Returns `Some(tag)` if a newer
/// version is available, `None` if already up to date or the check fails
/// (including network errors — failures are logged at warn level and silently
/// ignored so an offline environment never surfaces errors in the TUI).
pub(crate) async fn check_latest(client: &reqwest::Client) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }

    let resp = client
        .get("https://api.github.com/repos/wkirschbaum/build-watcher/releases/latest")
        .header(
            "User-Agent",
            concat!("build-watcher/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("update check failed: {e}");
            return None;
        }
    };

    let release = match resp.json::<Release>().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("update check: failed to parse response: {e}");
            return None;
        }
    };

    let latest = release.tag_name.trim_start_matches('v');
    (latest != env!("CARGO_PKG_VERSION")).then_some(release.tag_name)
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner("wkirschbaum")
        .repo_name("build-watcher")
        .bin_name("bw")
        .show_download_progress(true)
        .no_confirm(true)
        .current_version(cargo_crate_version!())
        .build()?
        .update()?;

    match status {
        self_update::Status::UpToDate(v) => println!("bw is already up to date ({})", v),
        self_update::Status::Updated(v) => println!("Updated bw to {}", v),
    }

    Ok(())
}
