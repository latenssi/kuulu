//! The session manager: joining a server from an invite, the call's local
//! state (mute, deafen, volumes, talk mode, audio), and the [`Snapshot`] the
//! UI renders. Every change is pushed to the webview as a `session` event;
//! audio and network stay in Rust.

use crate::audio::{Audio, Gate};
use crate::invite::Invite;
use crate::latency::{self, Latency};
use crate::settings::{DEFAULT_VOLUME, Settings, Store, TalkMode};
use crate::{identity, ptt};
use kuulu_client::{Client, Event, IdentityKey, Request, ServerState};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::broadcast::error::RecvError;

/// How often RTT and audio stats are refreshed in the UI.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Invites made from the client, as in the plan's CLI example.
const INVITE_USES: u32 = 10;
const INVITE_EXPIRES: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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

    /// A call is active or starting: no update may install now.
    fn blocks_update(&self) -> bool {
        matches!(
            self,
            Phase::Connecting | Phase::Connected | Phase::Reconnecting
        )
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
    /// Ours: the gate is open. Others': their voice is arriving.
    pub speaking: bool,
    /// Percent; ours is always 100.
    pub volume: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TalkView {
    /// What the user chose.
    pub mode: TalkMode,
    /// The push-to-talk input, named as in `ptt`.
    pub key: String,
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
    /// Why push-to-talk is unavailable, from the last attempt to start it.
    ptt_unavailable: Option<String>,
    audio_error: Option<String>,
}

/// Live measurements for a [`snapshot`].
struct Live<'a> {
    rtt: Option<Duration>,
    jitter_target: Option<usize>,
    /// Our gate is open.
    transmitting: bool,
    /// Whether a sender's voice is arriving, by session id.
    speaking: &'a dyn Fn(u32) -> bool,
}

#[cfg(test)]
impl Live<'_> {
    const NONE: Live<'static> = Live {
        rtt: None,
        jitter_target: None,
        transmitting: false,
        speaking: &|_| false,
    };
}

/// Linear gains for the other members whose volume is not the default.
fn gains(state: &ServerState, settings: &Settings) -> Vec<(u32, f32)> {
    state
        .members
        .values()
        .filter(|m| Some(m.session_id) != state.session_id)
        .map(|m| (m.session_id, settings.volume(&m.public_key)))
        .filter(|&(_, percent)| percent != DEFAULT_VOLUME)
        .map(|(id, percent)| (id, f32::from(percent) / 100.0))
        .collect()
}

/// Pure projection of the session into what the UI shows.
fn snapshot(
    phase: &Phase,
    server: Option<(&ServerState, &Invite)>,
    local: &Local,
    settings: &Settings,
    live: &Live,
) -> Snapshot {
    let Live {
        rtt, jitter_target, ..
    } = *live;
    let talk = TalkView {
        mode: settings.talk_mode,
        key: settings.ptt_key.clone(),
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
                                speaking: match mine {
                                    true => live.transmitting && !silenced,
                                    false => (live.speaking)(m.session_id),
                                },
                                volume: match mine {
                                    true => DEFAULT_VOLUME,
                                    false => settings.volume(&m.public_key),
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

/// What a reset takes out of [`Inner`]; dropping it closes the connection
/// and stops audio (joining the audio thread), so it is dropped only after
/// the lock is released.
type Teardown = (Option<Call>, Option<Audio>);

#[derive(Default)]
struct Inner {
    phase: Phase,
    call: Option<Call>,
    /// Mic test before joining, or the call's audio.
    audio: Option<Audio>,
    local: Local,
    /// Loaded from the keyring on first join.
    identity: Option<IdentityKey>,
    /// Bumped by every join and leave. Work done outside the lock installs
    /// its result only if this is unchanged, so a stale result is dropped.
    attempt: u64,
    /// A deep link that arrived before the UI asked for it.
    pending_invite: Option<String>,
    /// The updater is installing; joins are refused.
    installing: bool,
}

impl Inner {
    /// Starts a new attempt from a clean slate; see [`Teardown`].
    fn reset(&mut self, phase: Phase) -> (u64, Teardown) {
        self.attempt += 1;
        self.phase = phase;
        self.local = Local::default();
        (self.attempt, (self.call.take(), self.audio.take()))
    }
}

/// Tauri-managed state; commands call it, the watcher task updates it.
///
/// Lock discipline: `inner` is held only for quick reads and writes, never
/// across an await, disk I/O, starting or stopping audio, or changing the
/// push-to-talk hook (all of which can block, some on the main thread,
/// which sync commands also use). Such work takes what it needs, releases
/// the lock, works, and installs its result only if `attempt` is unchanged.
pub struct Session {
    app: AppHandle,
    gate: Arc<Gate>,
    inner: Mutex<Inner>,
}

impl Session {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            gate: Arc::default(),
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn settings(&self) -> Settings {
        self.app.state::<Store>().get()
    }

    /// For the updater: true, and joins are refused from now on, unless a
    /// call is active or starting. Checking and setting under one lock
    /// leaves no window for a join to slip in.
    pub fn try_begin_install(&self) -> bool {
        let mut inner = self.lock();
        inner.installing = !inner.phase.blocks_update();
        inner.installing
    }

    /// The install failed; joins are allowed again.
    pub fn install_failed(&self) {
        self.lock().installing = false;
    }

    pub fn snapshot(&self) -> Snapshot {
        let settings = self.settings();
        let inner = self.lock();
        let call = inner.call.as_ref();
        let state = call.map(|c| c.client.state());
        let server = state.as_ref().zip(call.map(|c| &c.invite));
        let audio = inner.audio.as_ref();
        let live = Live {
            rtt: call.and_then(|c| c.client.rtt()),
            jitter_target: audio.and_then(Audio::jitter_target),
            transmitting: audio.is_some_and(Audio::transmitting),
            speaking: &|id| audio.is_some_and(|a| a.speaking(id)),
        };
        snapshot(&inner.phase, server, &inner.local, &settings, &live)
    }

    /// Hands the per-user volumes to the call's audio, by session id.
    fn apply_gains(&self) {
        let settings = self.settings();
        let inner = self.lock();
        if let (Some(call), Some(audio)) = (&inner.call, &inner.audio) {
            audio.set_gains(&gains(&call.client.state(), &settings));
        }
    }

    fn emit(&self) {
        let _ = self.app.emit("session", self.snapshot());
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
        let (attempt, old) = {
            let mut inner = self.lock();
            if inner.installing {
                return Err("Kuulu is installing an update and restarts in a moment.".into());
            }
            inner.reset(Phase::Connecting)
        };
        drop(old);
        ptt::stop();
        self.emit();

        let client = match self.connect(&invite, name).await {
            Ok(client) => Arc::new(client),
            Err(failed) => {
                let reason = match &failed {
                    Phase::Failed { reason, .. } => reason.clone(),
                    _ => unreachable!("connect fails with Phase::Failed"),
                };
                let mut inner = self.lock();
                if inner.attempt == attempt {
                    inner.phase = failed;
                    // Keep the link at hand for trying again.
                    inner.pending_invite = Some(link.trim().into());
                    drop(inner);
                    let _ = self.app.emit("invite", link.trim());
                    self.emit();
                }
                return Err(reason);
            }
        };
        {
            let mut inner = self.lock();
            if inner.attempt != attempt {
                return Ok(()); // Left or joined elsewhere meanwhile.
            }
            let watcher = tokio::spawn(watch(self.app.clone(), client.clone(), attempt));
            inner.call = Some(Call {
                client,
                invite: invite.clone(),
                watcher: watcher.abort_handle(),
            });
            inner.phase = Phase::Connected;
        }
        self.app.state::<Store>().update(|s| {
            s.display_name = name.into();
            // The key is known now: rejoining needs no invite.
            s.last_server = Some(
                Invite {
                    token: None,
                    ..invite
                }
                .link(),
            );
        });
        self.restart_audio();
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
        let token = invite.token.as_deref();
        Client::connect_as(addr, &invite.fingerprint, &key, name, token)
            .await
            .map_err(|e| Phase::client_failed(&e))
    }

    pub fn leave(&self) {
        let (_, old) = self.lock().reset(Phase::Idle);
        drop(old);
        ptt::stop();
        self.app.state::<Store>().update(|s| s.last_server = None);
        self.emit();
    }

    /// Starts or stops the push-to-talk hook for the current settings.
    /// Returns why push-to-talk is unavailable, if it is.
    fn apply_talk_mode(&self, settings: &Settings) -> Option<String> {
        let unavailable = match settings.talk_mode {
            TalkMode::PushToTalk => ptt::start(&settings.ptt_key, self.gate.clone()).err(),
            TalkMode::VoiceActivity => {
                ptt::stop();
                None
            }
        };
        self.gate
            .set_push_to_talk(settings.talk_mode == TalkMode::PushToTalk && unavailable.is_none());
        unavailable
    }

    /// (Re)starts the call's push-to-talk and, with `devices`, its audio,
    /// from the current settings. Must run inside the tokio runtime.
    fn restart_call(&self, devices: bool) {
        let (attempt, client, old) = {
            let mut inner = self.lock();
            let client = inner.call.as_ref().map(|c| c.client.clone());
            let old = if devices { inner.audio.take() } else { None };
            (inner.attempt, client, old)
        };
        drop(old); // Release the devices first.
        let Some(client) = client else { return };
        let settings = self.settings();
        let unavailable = self.apply_talk_mode(&settings);
        let audio = devices.then(|| {
            Audio::start(
                settings.input_device.as_deref(),
                settings.output_device.as_deref(),
                self.gate.clone(),
                Some(client),
            )
        });
        let mut inner = self.lock();
        if inner.attempt != attempt {
            drop(inner);
            ptt::stop(); // The call this was for is gone.
            return;
        }
        inner.local.ptt_unavailable = unavailable;
        let replaced = match audio {
            Some(Ok(audio)) => {
                inner.local.audio_error = None;
                inner.audio.replace(audio)
            }
            Some(Err(e)) => {
                inner.local.audio_error = Some(e);
                inner.audio.take()
            }
            None => None,
        };
        drop(inner);
        drop(replaced);
        self.apply_gains();
    }

    fn restart_audio(&self) {
        self.restart_call(true);
    }

    /// Applies changed settings to a running call; `devices` when the audio
    /// devices changed.
    pub fn settings_changed(&self, devices: bool) {
        self.restart_call(devices);
        self.emit();
    }

    /// Starts or stops the microphone test of the join view. A call's own
    /// audio is left alone. Must run inside the tokio runtime.
    pub fn mic_test(&self, on: bool) -> Result<(), String> {
        let (attempt, old) = {
            let mut inner = self.lock();
            if inner.call.is_some() {
                return Ok(());
            }
            (inner.attempt, inner.audio.take())
        };
        drop(old);
        if !on {
            return Ok(());
        }
        let input = self.settings().input_device;
        let audio = Audio::start(input.as_deref(), None, self.gate.clone(), None)?;
        let mut inner = self.lock();
        if inner.attempt != attempt || inner.call.is_some() {
            return Ok(()); // `audio` is dropped after the guard.
        }
        let replaced = inner.audio.replace(audio);
        drop(inner);
        drop(replaced);
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

    /// Sets a member's volume in percent (see `Settings::set_volume`). It is kept
    /// by identity key, so it survives reconnects and restarts.
    pub fn set_volume(&self, session_id: u32, percent: u8) -> Result<(), String> {
        let state = self.client()?.state();
        let member = state.members.get(&session_id).ok_or("No such user.")?;
        self.app
            .state::<Store>()
            .update(|s| s.set_volume(&member.public_key, percent));
        self.apply_gains();
        self.emit();
        Ok(())
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
            token: Some(token),
            ..call.invite.clone()
        }
        .link())
    }

    fn client(&self) -> Result<Arc<Client>, String> {
        let client = self.lock().call.as_ref().map(|c| c.client.clone());
        client.ok_or_else(|| "Not connected.".into())
    }

    fn on_event(&self, event: &Event) {
        {
            let mut inner = self.lock();
            inner.phase = inner.phase.on(event);
        }
        // Session ids change on (re)connects and joins.
        if matches!(
            event,
            Event::Connected | Event::MemberJoined(_) | Event::MemberLeft(_)
        ) {
            self.apply_gains();
        }
    }

    /// The client stopped for good; ends the call. `attempt` is the call's.
    fn on_stopped(&self, attempt: u64, e: &kuulu_client::Error) {
        let old = {
            let mut inner = self.lock();
            if inner.attempt != attempt {
                return;
            }
            inner.reset(Phase::client_failed(e)).1
        };
        drop(old);
        ptt::stop();
    }
}

/// Follows a call's client until it stops: events and periodic stats
/// become `session` events.
async fn watch(app: AppHandle, client: Arc<Client>, attempt: u64) {
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
                session.on_stopped(attempt, &e);
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
            public_key: [id as u8; 32],
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
        assert!(Reconnecting.blocks_update() && Connecting.blocks_update());
        assert!(!failed.blocks_update() && !Idle.blocks_update());
    }

    #[test]
    fn snapshot_groups_members_by_channel() {
        let local = Local::default();
        let mut settings = Settings::default();
        // Volumes follow the identity key (member 1 has key [1; 32]).
        settings.set_volume(&[1; 32], 85);
        let state = state();
        let invite = invite();
        let snap = snapshot(
            &Phase::Connected,
            Some((&state, &invite)),
            &local,
            &settings,
            &Live {
                rtt: Some(Duration::from_millis(22)),
                transmitting: true,
                ..Live::NONE
            },
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
        // Only members away from the default get a gain.
        assert_eq!(gains(&state, &settings), [(1, 0.85)]);
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
            &Live::NONE,
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
            &Live {
                transmitting: true,
                ..Live::NONE
            },
        );
        assert!(snap.muted && snap.deafened);
        assert!(!snap.channels[1].members[1].speaking);
        assert_eq!(snap.talk.mode, TalkMode::PushToTalk);
        assert_eq!(snap.talk.fallback_reason.as_deref(), Some("Wayland"));

        let vad = Settings {
            talk_mode: TalkMode::VoiceActivity,
            ..Settings::default()
        };
        let snap = snapshot(&Phase::Idle, None, &local, &vad, &Live::NONE);
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
