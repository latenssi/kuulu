//! Background auto-update.
//!
//! Checks on startup and every six hours, downloads in the background and
//! installs only when the user is not in a call, then restarts the app.
//! Channels and endpoints are documented in docs/RELEASING.md.

use std::path::Path;
use std::time::Duration;
use std::{fs, io, thread};

use serde_json::{Map, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

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

/// App settings file in the app config dir. Shared with future settings.
const SETTINGS_FILE: &str = "settings.json";
const BETA_SETTING: &str = "beta_updates";

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

/// Whether this installation follows the beta channel: the `beta_updates`
/// setting, which the first run initialises from the installed version (a
/// prerelease means beta). Persisting it keeps beta users on beta after they
/// update to a stable release.
fn beta_channel(app: &AppHandle) -> bool {
    let default = !app.package_info().version.pre.is_empty();
    match app.path().app_config_dir() {
        Ok(dir) => beta_setting(&dir.join(SETTINGS_FILE), default),
        Err(e) => {
            eprintln!("no config dir, using default update channel: {e}");
            default
        }
    }
}

/// Reads the beta setting from the JSON settings file at `path`, storing
/// `default` there if it is unset. Falls back to `default` on any error.
fn beta_setting(path: &Path, default: bool) -> bool {
    let mut settings = match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Map<String, Value>>(&text) {
            Ok(settings) => settings,
            Err(e) => {
                // Never overwrite a file we cannot parse.
                eprintln!("ignoring unreadable {}: {e}", path.display());
                return default;
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            return default;
        }
    };
    if let Some(beta) = settings.get(BETA_SETTING).and_then(Value::as_bool) {
        return beta;
    }
    settings.insert(BETA_SETTING.into(), default.into());
    if let Err(e) = write_json(path, &settings) {
        eprintln!("cannot save {}: {e}", path.display());
    }
    default
}

fn write_json(path: &Path, value: &Map<String, Value>) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)?)
}

async fn update(app: &AppHandle) -> tauri_plugin_updater::Result<()> {
    let endpoint = if beta_channel(app) {
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
    while in_call() {
        thread::sleep(IN_CALL_POLL);
    }
    // On Windows the installer exits the app here and relaunches it itself.
    update.install(bytes)?;
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kuulu-updater-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join(SETTINGS_FILE)
    }

    #[test]
    fn first_run_persists_default() {
        let path = temp_settings("first");
        assert!(beta_setting(&path, true));
        // A later stable build keeps following beta.
        assert!(beta_setting(&path, false));
    }

    #[test]
    fn keeps_other_settings() {
        let path = temp_settings("other");
        write_json(&path, &serde_json::from_str(r#"{"x":1}"#).unwrap()).unwrap();
        assert!(!beta_setting(&path, false));
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved, serde_json::json!({"x": 1, "beta_updates": false}));
    }

    #[test]
    fn never_overwrites_unparseable_file() {
        let path = temp_settings("broken");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{oops").unwrap();
        assert!(beta_setting(&path, true));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{oops");
    }
}
