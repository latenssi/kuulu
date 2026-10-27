//! The session manager: joining a server from an invite, the call's local
//! state (mute, deafen, volumes, talk mode, audio), and the [`Snapshot`] the
//! UI renders. Every change is pushed to the webview as a `session` event;
//! audio and network stay in Rust.

use crate::audio::{Audio, Gate};
use crate::invite::Invite;
use crate::latency::{self, Latency};
use crate::settings::{Settings, Store, TalkMode};
use crate::{identity, ptt};
use kuulu_client::{Client, Event, IdentityKey, Request, ServerState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::broadcast::error::RecvError;

/// How often RTT and audio stats are refreshed in the UI.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Invites made from the client, as in the plan's CLI example.
const INVITE_USES: u32 = 10;
const INVITE_EXPIRES: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Per-sender volume, percent.
const DEFAULT_VOLUME: u8 = 100;
pub const MAX_VOLUME: u8 = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Connecting,
    Connected,
    /// Connection lost; the client retries by itself.
    Reconnecting,
    /// Gave up (refused, kicked, banned, unreachable). `reason` is for the
    /// user; `retryable` when trying again later may work (e.g. the server
    /// was full or unreachable).
    Failed {
        reason: String,
        retryable: bool,
    },
}

impl Phase {
    /// The phase after a client event.
    fn on(&self, event: &Event) -> Phase {
        match (self, event) {
            (Phase::Connected | Phase::Reconnecting, Event::Connected) => Phase::Connected,
            (Phase::Connected, Event::Disconnected) => Phase::Reconnecting,
            _ => self.clone(),
        }
    }

    fn failed(reason: impl Into<String>, retryable: bool) -> Phase {
        Phase::Failed {
            reason: reason.into(),
            retryable,
        }
    }

    fn client_failed(e: &kuulu_client::Error) -> Phase {
        Phase::failed(capitalize(&format!("{e}.")), !e.is_fatal())
    }

    fn in_call(&self) -> bool {
        matches!(self, Phase::Connected | Phase::Reconnecting)
    }
}

/// Everything the UI shows about the session.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub phase: Phase,
    /// The last known server while connected or reconnecting.
    pub server: Option<ServerView>,
    pub channels: Vec<ChannelView>,
    /// Our own session id while connected.
    pub me: Option<u32>,
    pub muted: bool,
    pub deafened: bool,
    pub talk: TalkView,
    /// Whether to offer admin actions; see [`is_admin`].
    pub admin: bool,
    pub rtt_ms: Option<f32>,
    pub latency: Latency,
    /// Why audio is not running, if it is not; the call goes on without it.
    pub audio_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ServerView {
    pub name: String,
    pub address: String,
    pub version: String,
    pub protocol: u32,
    pub online: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChannelView {
    pub id: u32,
    pub name: String,
    pub members: Vec<MemberView>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MemberView {
    pub id: u32,
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    /// Ours: the gate is open. Others': not known yet (needs per-sender
    /// activity from Voice, PR #5), always false.
    pub speaking: bool,
    /// Percent; ours is always 100.
    pub volume: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TalkView {
    /// What the user chose.
    pub mode: TalkMode,
    pub shortcut: String,
    /// Push-to-talk was chosen but cannot work; voice activity is used.
    pub fallback_reason: Option<String>,
}

/// Admin actions as the UI sends them.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminAction {
    CreateChannel { name: String },
    DeleteChannel { channel_id: u32 },
    RenameChannel { channel_id: u32, name: String },
    MoveMember { session_id: u32, channel_id: u32 },
    Kick { session_id: u32 },
    Ban { session_id: u32 },
}

impl From<AdminAction> for Request {
    fn from(action: AdminAction) -> Self {
        match action {
            AdminAction::CreateChannel { name } => Request::CreateChannel(name),
            AdminAction::DeleteChannel { channel_id } => Request::DeleteChannel(channel_id),
            AdminAction::RenameChannel { channel_id, name } => {
                Request::RenameChannel(channel_id, name)
            }
            AdminAction::MoveMember {
                session_id,
                channel_id,
            } => Request::MoveMember {
                session_id,
                channel_id,
            },
            AdminAction::Kick { session_id } => Request::Kick(session_id),
            AdminAction::Ban { session_id } => Request::Ban(session_id),
        }
    }
}

/// Whether to offer admin actions. The server still checks every request.
fn is_admin(state: &ServerState) -> bool {
    state.me().is_some_and(|m| m.admin)
}

/// State the UI changes locally, kept across reconnects.
#[derive(Default)]
struct Local {
    muted: bool,
    deafened: bool,
    /// Percent by session id, for this call only: members have no
    /// identity that outlives their session yet.
    volumes: HashMap<u32, u8>,
    /// Why push-to-talk is unavailable, from the last registration.
    ptt_unavailable: Option<String>,
    audio_error: Option<String>,
}

/// Pure projection of the session into what the UI shows.
fn snapshot(
    phase: &Phase,
    server: Option<(&ServerState, &Invite)>,
    local: &Local,
    settings: &Settings,
    rtt: Option<Duration>,
    jitter_target: Option<usize>,
    transmitting: bool,
) -> Snapshot {
    let talk = TalkView {
        mode: settings.talk_mode,
        shortcut: settings.ptt_shortcut.clone(),
        fallback_reason: match settings.talk_mode {
            TalkMode::PushToTalk => local.ptt_unavailable.clone(),
            TalkMode::VoiceActivity => None,
        },
    };
    let silenced = local.muted || local.deafened;
    let (server, channels, me, admin) = match server {
        Some((state, invite)) => {
            let channels = state
                .channels
                .iter()
                .map(|(&id, name)| ChannelView {
                    id,
                    name: name.clone(),
                    members: state
                        .members
                        .values()
                        .filter(|m| m.channel_id == id)
                        .map(|m| {
                            let mine = Some(m.session_id) == state.session_id;
                            MemberView {
                                id: m.session_id,
                                name: m.display_name.clone(),
                                muted: m.self_mute,
                                deafened: m.self_deaf,
                                speaking: mine && transmitting && !silenced,
                                volume: match mine {
                                    true => DEFAULT_VOLUME,
                                    false => volume(local, m.session_id),
                                },
                            }
                        })
                        .collect(),
                })
                .collect();
            let server = ServerView {
                name: state.server_name.clone(),
                address: invite.address(),
                version: state.server_version.clone(),
                protocol: state.protocol_version,
                online: state.members.len(),
            };
            (Some(server), channels, state.session_id, is_admin(state))
        }
        None => (None, Vec::new(), None, false),
    };
    Snapshot {
        phase: phase.clone(),
        server,
        channels,
        me,
        muted: silenced,
        deafened: local.deafened,
        talk,
        admin,
        rtt_ms: rtt.map(|r| r.as_secs_f32() * 1000.0),
        latency: latency::estimate(rtt, jitter_target),
        audio_error: local.audio_error.clone(),
    }
}

fn volume(local: &Local, session_id: u32) -> u8 {
    local
        .volumes
        .get(&session_id)
        .copied()
        .unwrap_or(DEFAULT_VOLUME)
}

struct Call {
    client: Arc<Client>,
    invite: Invite,
    watcher: tokio::task::AbortHandle,
}

impl Drop for Call {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

#[derive(Default)]
struct Inner {
    phase: Phase,
    call: Option<Call>,
    /// Mic test before joining, or the call's audio.
    audio: Option<Audio>,
    local: Local,
    /// Loaded from the keyring on first join.
    identity: Option<IdentityKey>,
    /// Bumped by every join and leave, so a join that finishes after the
    /// user gave up is discarded.
    attempt: u64,
    /// A deep link that arrived before the UI asked for it.
    pending_invite: Option<String>,
}

/// Tauri-managed state; commands call it, the watcher task updates it.
pub struct Session {
    app: AppHandle,
    gate: Arc<Gate>,
    inner: Mutex<Inner>,
    /// Mirrors `phase.in_call()` for the updater, without the lock.
    in_call: AtomicBool,
}

impl Session {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            gate: Arc::default(),
            inner: Mutex::default(),
            in_call: AtomicBool::new(false),
        }
    }

    /// True while connected or reconnecting.
    pub fn in_call(&self) -> bool {
        self.in_call.load(Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn settings(&self) -> Settings {
        self.app.state::<Store>().get()
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        let call = inner.call.as_ref();
        let state = call.map(|c| c.client.state());
        let server = state.as_ref().zip(call.map(|c| &c.invite));
        let audio = inner.audio.as_ref();
        snapshot(
            &inner.phase,
            server,
            &inner.local,
            &self.settings(),
            call.and_then(|c| c.client.rtt()),
            audio.and_then(Audio::jitter_target),
            audio.is_some_and(Audio::transmitting),
        )
    }

    fn emit(&self) {
        let _ = self.app.emit("session", self.snapshot());
    }

    fn set_phase(&self, inner: &mut Inner, phase: Phase) {
        self.in_call.store(phase.in_call(), Relaxed);
        inner.phase = phase;
    }

    /// Remembers a deep-linked invite and tells the UI.
    pub fn offer_invite(&self, link: String) {
        self.lock().pending_invite = Some(link.clone());
        let _ = self.app.emit("invite", link);
    }

    /// The deep-linked invite not yet shown, if any.
    pub fn take_pending_invite(&self) -> Option<String> {
        self.lock().pending_invite.take()
    }

    /// Connects to the server in `link` as `name`, leaving any current
    /// server. Settings must already hold the user's choices.
    pub async fn join(&self, link: &str, name: &str) -> Result<(), String> {
        let invite = Invite::parse(link)?;
        let attempt = {
            let mut inner = self.lock();
            inner.call = None;
            inner.audio = None;
            inner.local = Local::default();
            inner.attempt += 1;
            self.set_phase(&mut inner, Phase::Connecting);
            inner.attempt
        };
        self.emit();
        let result = self.connect(&invite, name).await;
        let mut inner = self.lock();
        if inner.attempt != attempt {
            return Ok(()); // Left or joined elsewhere meanwhile.
        }
        let client = match result {
            Ok(client) => Arc::new(client),
            Err(failed) => {
                let reason = match &failed {
                    Phase::Failed { reason, .. } => reason.clone(),
                    _ => unreachable!("connect fails with Phase::Failed"),
                };
                self.set_phase(&mut inner, failed);
                // Keep the link at hand for trying again.
                inner.pending_invite = Some(link.trim().into());
                drop(inner);
                let _ = self.app.emit("invite", link.trim());
                self.emit();
                return Err(reason);
            }
        };
        let watcher = tokio::spawn(watch(self.app.clone(), client.clone()));
        inner.call = Some(Call {
            client,
            invite,
            watcher: watcher.abort_handle(),
        });
        self.set_phase(&mut inner, Phase::Connected);
        self.app.state::<Store>().update(|s| {
            s.display_name = name.into();
            s.last_server = Some(link.trim().into());
        });
        self.restart_audio(&mut inner);
        drop(inner);
        self.emit();
        Ok(())
    }

    /// Fails with [`Phase::Failed`].
    async fn connect(&self, invite: &Invite, name: &str) -> Result<Client, Phase> {
        let key = self.lock().identity.clone();
        let key = match key {
            Some(key) => key,
            // The keyring may block on D-Bus or a system prompt.
            None => tauri::async_runtime::spawn_blocking(identity::load_or_create)
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
                .map_err(|e| Phase::failed(e, false))?,
        };
        self.lock().identity = Some(key.clone());
        let addr = invite.resolve().await.map_err(|e| Phase::failed(e, true))?;
        Client::connect_as(addr, &invite.fingerprint, &key, name, Some(&invite.token))
            .await
            .map_err(|e| Phase::client_failed(&e))
    }

    pub fn leave(&self) {
        let mut inner = self.lock();
        inner.attempt += 1;
        inner.call = None;
        inner.audio = None;
        inner.local = Local::default();
        self.set_phase(&mut inner, Phase::Idle);
        drop(inner);
        ptt::unregister(&self.app);
        self.app.state::<Store>().update(|s| s.last_server = None);
        self.emit();
    }

    /// (Re)starts audio for the current call, or stops it, from the current
    /// settings, and (re)registers the push-to-talk key.
    fn restart_audio(&self, inner: &mut Inner) {
        let settings = self.settings();
        inner.audio = None; // Release the devices first.
        let client = inner.call.as_ref().map(|c| c.client.clone());
        if client.is_none() {
            return;
        }
        let ptt = match settings.talk_mode {
            TalkMode::PushToTalk => {
                ptt::register(&self.app, &settings.ptt_shortcut, self.gate.clone()).err()
            }
            TalkMode::VoiceActivity => {
                ptt::unregister(&self.app);
                None
            }
        };
        self.gate
            .set_push_to_talk(settings.talk_mode == TalkMode::PushToTalk && ptt.is_none());
        inner.local.ptt_unavailable = ptt;
        match Audio::start(
            settings.input_device.as_deref(),
            settings.output_device.as_deref(),
            self.gate.clone(),
            client,
        ) {
            Ok(audio) => {
                inner.audio = Some(audio);
                inner.local.audio_error = None;
            }
            Err(e) => inner.local.audio_error = Some(e),
        }
    }

    /// Applies changed settings to a running call.
    pub fn settings_changed(&self) {
        let mut inner = self.lock();
        if inner.call.is_some() {
            self.restart_audio(&mut inner);
        }
        drop(inner);
        self.emit();
    }

    /// Starts or stops the microphone test of the join view. A call's own
    /// audio is left alone.
    pub fn mic_test(&self, on: bool) -> Result<(), String> {
        let mut inner = self.lock();
        if inner.call.is_some() {
            return Ok(());
        }
        inner.audio = None;
        if on {
            let input = self.settings().input_device;
            inner.audio = Some(Audio::start(
                input.as_deref(),
                None,
                self.gate.clone(),
                None,
            )?);
        }
        Ok(())
    }

    pub fn mic_level(&self) -> f32 {
        self.lock().audio.as_ref().map_or(0.0, Audio::level)
    }

    pub fn set_muted(&self, muted: bool) {
        self.set_voice(|l| {
            l.muted = muted;
            // Unmuting also undeafens: deafen implies mute.
            if !muted {
                l.deafened = false;
            }
        });
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.set_voice(|l| l.deafened = deafened);
    }

    fn set_voice(&self, change: impl FnOnce(&mut Local)) {
        let mut inner = self.lock();
        change(&mut inner.local);
        if let Some(call) = &inner.call {
            call.client.set_self_mute(inner.local.muted);
            call.client.set_self_deaf(inner.local.deafened);
        }
        drop(inner);
        self.emit();
    }

    /// Sets a member's volume in percent, up to [`MAX_VOLUME`].
    pub fn set_volume(&self, session_id: u32, percent: u8) {
        let percent = percent.min(MAX_VOLUME);
        let mut inner = self.lock();
        inner.local.volumes.insert(session_id, percent);
        if let Some(audio) = &inner.audio {
            audio.set_gain(session_id, f32::from(percent) / 100.0);
        }
        drop(inner);
        self.emit();
    }

    /// Sends a request; the change itself arrives as an event.
    pub async fn request(&self, request: Request) -> Result<(), String> {
        let client = self.client()?;
        client.request(request).await.map_err(request_failed)
    }

    /// Creates an invite (admin only) and returns its link.
    pub async fn create_invite(&self) -> Result<String, String> {
        let client = self.client()?;
        let token = client
            .create_invite(INVITE_USES, INVITE_EXPIRES)
            .await
            .map_err(request_failed)?;
        let inner = self.lock();
        let call = inner.call.as_ref().ok_or("Not connected.")?;
        Ok(Invite {
            token,
            ..call.invite.clone()
        }
        .link())
    }

    fn client(&self) -> Result<Arc<Client>, String> {
        let client = self.lock().call.as_ref().map(|c| c.client.clone());
        client.ok_or_else(|| "Not connected.".into())
    }

    fn on_event(&self, event: &Event) {
        let mut inner = self.lock();
        let phase = inner.phase.on(event);
        self.set_phase(&mut inner, phase);
        if let Event::MemberLeft(m) = event {
            inner.local.volumes.remove(&m.session_id);
        }
    }

    fn on_stopped(&self, e: &kuulu_client::Error) {
        let mut inner = self.lock();
        inner.call = None;
        inner.audio = None;
        self.set_phase(&mut inner, Phase::client_failed(e));
        drop(inner);
        ptt::unregister(&self.app);
    }
}

/// Follows a call's client until it stops: events and periodic stats
/// become `session` events.
async fn watch(app: AppHandle, client: Arc<Client>) {
    let session = app.state::<Session>();
    let mut events = client.events();
    let mut stats = tokio::time::interval(STATS_INTERVAL);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => session.on_event(&event),
                // The snapshot reads the client's state, which is current.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            e = client.stopped() => {
                session.on_stopped(&e);
                session.emit();
                return;
            }
            _ = stats.tick() => {}
        }
        session.emit();
    }
}

fn request_failed(e: kuulu_client::RequestError) -> String {
    capitalize(&format!("{e}."))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuulu_client::Member;

    fn member(id: u32, channel: u32, name: &str) -> Member {
        Member {
            session_id: id,
            display_name: name.into(),
            channel_id: channel,
            self_mute: false,
            self_deaf: false,
            admin: false,
        }
    }

    fn state() -> ServerState {
        ServerState {
            session_id: Some(2),
            server_name: "Perjantaiporukka".into(),
            server_version: "0.1.0".into(),
            protocol_version: 1,
            features: vec![],
            channels: [(1, "Lobby".into()), (3, "Raid night".into())].into(),
            members: [
                (1, member(1, 3, "Aino")),
                (2, member(2, 3, "Saara")),
                (
                    4,
                    Member {
                        self_mute: true,
                        ..member(4, 1, "Jere")
                    },
                ),
            ]
            .into(),
        }
    }

    fn invite() -> Invite {
        Invite::parse(&format!(
            "kuulu://perjantai.example:7700/join?t=x&fp={}",
            "a".repeat(64)
        ))
        .unwrap()
    }

    #[test]
    fn phase_follows_client_events() {
        use Phase::*;
        assert_eq!(Connected.on(&Event::Disconnected), Reconnecting);
        assert_eq!(Reconnecting.on(&Event::Connected), Connected);
        assert_eq!(
            Connected.on(&Event::MemberJoined(member(5, 1, "x"))),
            Connected
        );
        // Late events from an old call do not revive it.
        assert_eq!(Idle.on(&Event::Connected), Idle);
        let failed = Phase::failed("kicked", false);
        assert_eq!(failed.on(&Event::Connected), failed);
        assert!(Reconnecting.in_call() && !Connecting.in_call() && !failed.in_call());
    }

    #[test]
    fn snapshot_groups_members_by_channel() {
        let mut local = Local::default();
        local.volumes.insert(1, 85);
        let state = state();
        let invite = invite();
        let snap = snapshot(
            &Phase::Connected,
            Some((&state, &invite)),
            &local,
            &Settings::default(),
            Some(Duration::from_millis(22)),
            None,
            true,
        );
        let server = snap.server.as_ref().unwrap();
        assert_eq!(server.address, "perjantai.example:7700");
        assert_eq!(server.online, 3);
        assert_eq!(snap.me, Some(2));
        assert_eq!(snap.rtt_ms, Some(22.0));
        assert_eq!(snap.channels.len(), 2);
        let raid = &snap.channels[1];
        assert_eq!(raid.name, "Raid night");
        let names: Vec<_> = raid.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Aino", "Saara"]);
        assert_eq!(raid.members[0].volume, 85);
        assert!(!raid.members[0].speaking);
        assert!(raid.members[1].speaking, "our open gate shows as speaking");
        assert!(snap.channels[0].members[0].muted);
        assert_eq!(snap.latency.total_ms, 43.5 - 11.0 + 11.0);
        assert!(!snap.admin);

        let mut state = state;
        state.members.get_mut(&2).unwrap().admin = true;
        let snap = snapshot(
            &Phase::Connected,
            Some((&state, &invite)),
            &local,
            &Settings::default(),
            None,
            None,
            false,
        );
        assert!(snap.admin);
    }

    #[test]
    fn mute_silences_and_ptt_fallback_is_shown() {
        let local = Local {
            deafened: true,
            ptt_unavailable: Some("Wayland".into()),
            ..Local::default()
        };
        let state = state();
        let invite = invite();
        let snap = snapshot(
            &Phase::Reconnecting,
            Some((&state, &invite)),
            &local,
            &Settings::default(),
            None,
            None,
            true,
        );
        assert!(snap.muted && snap.deafened);
        assert!(!snap.channels[1].members[1].speaking);
        assert_eq!(snap.talk.mode, TalkMode::PushToTalk);
        assert_eq!(snap.talk.fallback_reason.as_deref(), Some("Wayland"));

        let vad = Settings {
            talk_mode: TalkMode::VoiceActivity,
            ..Settings::default()
        };
        let snap = snapshot(&Phase::Idle, None, &local, &vad, None, None, false);
        assert_eq!(snap.talk.fallback_reason, None);
        assert!(snap.server.is_none() && snap.channels.is_empty() && !snap.admin);
    }

    #[test]
    fn admin_actions_map_to_requests() {
        let action: AdminAction =
            serde_json::from_str(r#"{"action":"move_member","session_id":4,"channel_id":3}"#)
                .unwrap();
        assert_eq!(
            Request::from(action),
            Request::MoveMember {
                session_id: 4,
                channel_id: 3
            }
        );
        let action: AdminAction =
            serde_json::from_str(r#"{"action":"ban","session_id":4}"#).unwrap();
        assert_eq!(Request::from(action), Request::Ban(4));
    }
}
