//! Everything the webview may call. Errors are strings worded for the user.
//! Every command runs async, off the main thread: some block (audio, the
//! input hook, disk), and sync commands run on the main thread, which the
//! OS may need meanwhile.

use crate::audio::{self, Devices};
use crate::invite::Invite;
use crate::ptt;
use crate::session::{AdminAction, Session, Snapshot};
use crate::settings::{Store, TalkMode};
use kuulu_client::Request;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tauri::{AppHandle, State};

type Result<T> = std::result::Result<T, String>;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[tauri::command(async)]
pub fn snapshot(session: State<Session>) -> Snapshot {
    session.snapshot()
}

#[tauri::command(async)]
pub fn take_pending_invite(session: State<Session>) -> Option<String> {
    session.take_pending_invite()
}

/// Validates an invite link, returning the server address it points at.
#[tauri::command(async)]
pub fn invite_address(link: String) -> Result<String> {
    Invite::parse(&link).map(|i| i.address())
}

#[tauri::command]
pub async fn join(session: State<'_, Session>, link: String, name: String) -> Result<()> {
    session.join(&link, name.trim()).await
}

#[tauri::command(async)]
pub fn leave(session: State<Session>) {
    session.leave();
}

#[tauri::command]
pub async fn join_channel(session: State<'_, Session>, channel_id: u32) -> Result<()> {
    session.request(Request::JoinChannel(channel_id)).await
}

#[tauri::command]
pub async fn admin(session: State<'_, Session>, action: AdminAction) -> Result<()> {
    session.request(action.into()).await
}

/// Creates an invite (admin only), returning its link.
#[tauri::command]
pub async fn create_invite(session: State<'_, Session>) -> Result<String> {
    session.create_invite().await
}

#[tauri::command(async)]
pub fn set_muted(session: State<Session>, muted: bool) {
    session.set_muted(muted);
}

#[tauri::command(async)]
pub fn set_deafened(session: State<Session>, deafened: bool) {
    session.set_deafened(deafened);
}

#[tauri::command(async)]
pub fn set_volume(session: State<Session>, session_id: u32, percent: u8) -> Result<()> {
    session.set_volume(session_id, percent)
}

#[tauri::command(async)]
pub fn devices() -> Devices {
    audio::devices()
}

#[tauri::command]
pub async fn mic_test(session: State<'_, Session>, on: bool) -> Result<()> {
    session.mic_test(on)
}

#[tauri::command(async)]
pub fn mic_level(session: State<Session>) -> f32 {
    session.mic_level()
}

/// The settings the user edits.
#[derive(Serialize, Deserialize)]
pub struct Preferences {
    display_name: String,
    input_device: Option<String>,
    output_device: Option<String>,
    talk_mode: TalkMode,
    ptt_key: String,
    beta_updates: bool,
}

#[tauri::command(async)]
pub fn preferences(app: AppHandle, store: State<Store>) -> Preferences {
    let beta_updates = store.beta_updates(!app.package_info().version.pre.is_empty());
    let s = store.get();
    Preferences {
        display_name: s.display_name,
        input_device: s.input_device,
        output_device: s.output_device,
        talk_mode: s.talk_mode,
        ptt_key: s.ptt_key,
        beta_updates,
    }
}

/// Saves the preferences and applies them at once, even mid-call.
#[tauri::command]
pub async fn set_preferences(
    store: State<'_, Store>,
    session: State<'_, Session>,
    preferences: Preferences,
) -> Result<()> {
    let p = preferences;
    let mut devices_changed = false;
    store.update(|s| {
        devices_changed =
            (&s.input_device, &s.output_device) != (&p.input_device, &p.output_device);
        s.display_name = p.display_name;
        s.input_device = p.input_device;
        s.output_device = p.output_device;
        s.talk_mode = p.talk_mode;
        s.ptt_key = p.ptt_key;
        s.beta_updates = Some(p.beta_updates);
    });
    session.settings_changed(devices_changed);
    Ok(())
}

/// Ends a running [`capture_ptt_key`] with no input.
#[tauri::command(async)]
pub fn cancel_ptt_capture() {
    ptt::cancel_capture();
}

/// Waits for the next key or mouse side button and makes it the
/// push-to-talk input. `None` if cancelled (Escape) or timed out.
#[tauri::command]
pub async fn capture_ptt_key(
    store: State<'_, Store>,
    session: State<'_, Session>,
) -> Result<Option<String>> {
    let captured = tauri::async_runtime::spawn_blocking(|| ptt::capture(CAPTURE_TIMEOUT))
        .await
        .map_err(|e| e.to_string())?;
    if let Ok(Some(key)) = &captured {
        store.update(|s| s.ptt_key = key.clone());
    }
    // Capturing stopped the call's push-to-talk hook.
    session.settings_changed(false);
    captured
}
