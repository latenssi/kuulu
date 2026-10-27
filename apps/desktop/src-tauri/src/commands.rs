//! Everything the webview may call. Errors are strings worded for the user.
//! Commands that may start audio are async: audio needs the tokio runtime.

use crate::audio::{self, Devices};
use crate::invite::Invite;
use crate::session::{AdminAction, Session, Snapshot};
use crate::settings::{Store, TalkMode};
use kuulu_client::Request;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub fn snapshot(session: State<Session>) -> Snapshot {
    session.snapshot()
}

#[tauri::command]
pub fn take_pending_invite(session: State<Session>) -> Option<String> {
    session.take_pending_invite()
}

/// Validates an invite link, returning the server address it points at.
#[tauri::command]
pub fn invite_address(link: String) -> Result<String> {
    Invite::parse(&link).map(|i| i.address())
}

#[tauri::command]
pub async fn join(session: State<'_, Session>, link: String, name: String) -> Result<()> {
    session.join(&link, name.trim()).await
}

#[tauri::command]
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

#[tauri::command]
pub fn set_muted(session: State<Session>, muted: bool) {
    session.set_muted(muted);
}

#[tauri::command]
pub fn set_deafened(session: State<Session>, deafened: bool) {
    session.set_deafened(deafened);
}

#[tauri::command]
pub fn set_volume(session: State<Session>, session_id: u32, percent: u8) {
    session.set_volume(session_id, percent);
}

#[tauri::command]
pub fn devices() -> Devices {
    audio::devices()
}

#[tauri::command]
pub async fn mic_test(session: State<'_, Session>, on: bool) -> Result<()> {
    session.mic_test(on)
}

#[tauri::command]
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
    ptt_shortcut: String,
    beta_updates: bool,
}

#[tauri::command]
pub fn preferences(app: AppHandle, store: State<Store>) -> Preferences {
    let beta_updates = store.beta_updates(!app.package_info().version.pre.is_empty());
    let s = store.get();
    Preferences {
        display_name: s.display_name,
        input_device: s.input_device,
        output_device: s.output_device,
        talk_mode: s.talk_mode,
        ptt_shortcut: s.ptt_shortcut,
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
    store.update(|s| {
        s.display_name = p.display_name;
        s.input_device = p.input_device;
        s.output_device = p.output_device;
        s.talk_mode = p.talk_mode;
        s.ptt_shortcut = p.ptt_shortcut;
        s.beta_updates = Some(p.beta_updates);
    });
    session.settings_changed();
    Ok(())
}
