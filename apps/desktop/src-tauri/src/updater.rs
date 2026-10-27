//! Background auto-update.
//!
//! Checks on startup and every six hours, downloads in the background and
//! installs only when the user is not in a call, then restarts the app.
//! Channels and endpoints are documented in docs/RELEASING.md.

use std::thread;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::session::Session;
use crate::settings::Store;

/// Stable users follow the newest published non-prerelease GitHub release.
const STABLE_ENDPOINT: &str =
    "https://github.com/latenssi/kuulu/releases/latest/download/latest.json";
/// Beta users follow a rolling release that every newer published release
/// (beta or stable) updates, so beta users also receive stable releases.
const BETA_ENDPOINT: &str =
    "https://github.com/latenssi/kuulu/releases/download/beta-channel/latest.json";

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
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

/// The single hook that defers installing an update: true while the user
/// is in a voice call.
fn in_call(app: &AppHandle) -> bool {
    app.state::<Session>().in_call()
}

async fn update(app: &AppHandle) -> tauri_plugin_updater::Result<()> {
    // A prerelease build starts on the beta channel.
    let default = !app.package_info().version.pre.is_empty();
    let endpoint = if app.state::<Store>().beta_updates(default) {
        BETA_ENDPOINT
    } else {
        STABLE_ENDPOINT
    };
    let Some(mut update) = app
        .updater_builder()
        .endpoints(vec![endpoint.parse().expect("valid endpoint URL")])?
        .timeout(CHECK_TIMEOUT)
        .build()?
        .check()
        .await?
    else {
        return Ok(());
    };
    // The builder timeout also caps the whole download; allow slow links.
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    let bytes = update.download(|_, _| {}, || {}).await?;
    if in_call(app) {
        // The UI shows this quietly; the call is never interrupted.
        let _ = app.emit("update-pending", &update.version);
        while in_call(app) {
            thread::sleep(IN_CALL_POLL);
        }
    }
    // On Windows the installer exits the app here and relaunches it itself.
    update.install(bytes)?;
    app.restart();
}
