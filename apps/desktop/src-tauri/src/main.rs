// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod commands;
mod identity;
mod invite;
mod latency;
mod ptt;
mod session;
mod settings;
mod updater;

use session::Session;
use settings::Store;
use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let config = app.path().app_config_dir().ok();
            app.manage(Store::open(config.map(|d| d.join(settings::FILE))));
            app.manage(Session::new(app.handle().clone()));
            open_invites(app.handle());
            updater::spawn(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::snapshot,
            commands::take_pending_invite,
            commands::invite_address,
            commands::join,
            commands::leave,
            commands::join_channel,
            commands::admin,
            commands::set_muted,
            commands::set_deafened,
            commands::set_volume,
            commands::devices,
            commands::mic_test,
            commands::mic_level,
            commands::preferences,
            commands::set_preferences,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Kuulu");
}

/// Routes `kuulu://` links to the join view. Without one, rejoins the
/// server we were in when the app last closed (e.g. for an update).
///
/// On Windows and Linux every opened link starts a new app instance with
/// the link as its argument (single-instance forwarding needs another
/// plugin); macOS delivers links to the running app.
fn open_invites(app: &tauri::AppHandle) {
    let links = app.deep_link();
    #[cfg(any(windows, target_os = "linux"))]
    if let Err(e) = links.register_all() {
        eprintln!("cannot register kuulu:// links: {e}");
    }
    let session = app.state::<Session>();
    let handle = app.clone();
    links.on_open_url(move |event| {
        if let Some(url) = event.urls().first() {
            handle.state::<Session>().offer_invite(url.to_string());
        }
    });
    match links
        .get_current()
        .ok()
        .flatten()
        .and_then(|u| u.first().cloned())
    {
        Some(url) => session.offer_invite(url.to_string()),
        None => {
            let settings = app.state::<Store>().get();
            if let Some(link) = settings.last_server {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let session = handle.state::<Session>();
                    // A failure shows in the UI as the session's phase.
                    let _ = session.join(&link, &settings.display_name).await;
                });
            }
        }
    }
}
