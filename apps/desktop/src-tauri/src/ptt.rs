//! The global push-to-talk key. tauri-plugin-global-shortcut handles
//! keyboard keys only (no mouse buttons) and works on Windows, macOS and
//! X11, not on Wayland. The OS delivers the key to Kuulu instead of the
//! focused app while it is registered, so it is registered only in a call.

use crate::audio::Gate;
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// Makes `shortcut` (e.g. "F8", "Ctrl+Backquote") drive `gate`, replacing
/// any earlier key. The error says why push-to-talk is unavailable, for the
/// user.
pub fn register(app: &AppHandle, shortcut: &str, gate: Arc<Gate>) -> Result<(), String> {
    unregister(app);
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Err("Global push-to-talk keys do not work on Wayland yet; \
                    using voice activity instead."
            .into());
    }
    app.global_shortcut()
        .on_shortcut(shortcut, move |_, _, event| {
            gate.set_key_held(event.state == ShortcutState::Pressed)
        })
        .map_err(|e| {
            format!(
                "Cannot use {shortcut} as the push-to-talk key ({e}); using voice activity instead."
            )
        })
}

pub fn unregister(app: &AppHandle) {
    let _ = app.global_shortcut().unregister_all();
}
