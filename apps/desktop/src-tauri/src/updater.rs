//! Background auto-update.
//!
//! Checks on startup and every six hours, downloads in the background and
//! installs only when the user is not in a call, then restarts the app.
//! Channels and endpoints are documented in docs/RELEASING.md.

use std::thread;
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

/// Stable builds follow the newest published non-prerelease GitHub release.
const STABLE_ENDPOINT: &str =
    "https://github.com/latenssi/kuulu/releases/latest/download/latest.json";
/// Beta builds follow a rolling release that every published release
/// (beta or stable) updates, so beta users also receive stable releases.
const BETA_ENDPOINT: &str =
    "https://github.com/latenssi/kuulu/releases/download/beta-channel/latest.json";

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const IN_CALL_POLL: Duration = Duration::from_secs(30);

/// Starts the update loop on its own thread. No-op in debug builds, which
/// are not installed bundles and must not replace themselves.
pub fn spawn(app: AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    thread::spawn(move || {
        loop {
            if let Err(e) = tauri::async_runtime::block_on(update(&app)) {
                eprintln!("update check failed: {e}");
            }
            thread::sleep(CHECK_INTERVAL);
        }
    });
}

/// The single hook that defers installing an update. Returns true while the
/// user is in a voice call; wire it to the call state once calls exist.
fn in_call() -> bool {
    false
}

async fn update(app: &AppHandle) -> tauri_plugin_updater::Result<()> {
    // The channel follows the running build: a prerelease version is beta.
    let endpoint = if app.package_info().version.pre.is_empty() {
        STABLE_ENDPOINT
    } else {
        BETA_ENDPOINT
    };
    let Some(update) = app
        .updater_builder()
        .endpoints(vec![endpoint.parse().expect("valid endpoint URL")])?
        .build()?
        .check()
        .await?
    else {
        return Ok(());
    };
    let bytes = update.download(|_, _| {}, || {}).await?;
    while in_call() {
        thread::sleep(IN_CALL_POLL);
    }
    // On Windows the installer exits the app here and relaunches it itself.
    update.install(bytes)?;
    app.restart();
}
