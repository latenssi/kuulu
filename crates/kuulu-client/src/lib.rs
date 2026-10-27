//! Client side of a Kuulu connection: handshake, identity, server state,
//! requests, voice datagrams and reconnects.
//!
//! [`Client`] moves voice packets: the audio pipeline hands it encoded frames
//! with [`Client::send`] and takes received ones from [`Client::recv`]. It
//! also mirrors the server's channels and members ([`Client::state`],
//! [`Client::events`]) and sends requests ([`Client::request`]). The
//! connection is re-established behind that interface when it drops, and the
//! channel and mute/deafen are restored.
//! [`voice::Voice`] connects it to the audio pipeline.

pub mod netem;
mod session;
mod state;
mod tls;
pub mod voice;

pub use kuulu_proto::identity::{IdentityKey, PublicKey};
pub use state::{Event, Member, ServerState};

use bytes::{BufMut, Bytes, BytesMut};
use kuulu_proto::control::{self, Hello, client_message::Msg, reject::Reason};
use kuulu_proto::{HEADER_LEN, VoiceHeader};
use session::{Dialer, PendingRequest, Status, VoiceState};
use std::net::SocketAddr;
use std::time::Duration;
use std::{fmt, io};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// How long [`Client::request`] waits for the server's answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A received voice packet. `header.sender` is the speaker's session id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoicePacket {
    pub header: VoiceHeader,
    pub payload: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The server refused us, or banned us while connected. Retrying cannot
    /// help, so the client stops.
    Rejected(Reason),
    /// The server's certificate is not the pinned one. The client stops.
    CertificateMismatch,
    /// An admin kicked us. The client stops; connecting again is allowed.
    Kicked,
    /// Transport failure. After the first connect these are retried.
    Connection(String),
}

impl Error {
    /// Whether retrying cannot help. Connected clients stop on these;
    /// others (transport failures, a full server) are worth retrying.
    pub fn is_fatal(&self) -> bool {
        // A full server may have room later.
        !matches!(
            self,
            Self::Connection(_) | Self::Rejected(Reason::ServerFull)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(Reason::TooOld) => {
                write!(
                    f,
                    "the server needs a newer version of Kuulu; please update"
                )
            }
            Self::Rejected(Reason::TooNew) => write!(
                f,
                "the server runs an older version of Kuulu; ask its admin to update it"
            ),
            Self::Rejected(Reason::InviteExpired) => {
                write!(
                    f,
                    "the invite has expired or is not valid; ask for a new one"
                )
            }
            Self::Rejected(Reason::Banned) => write!(f, "you are banned from this server"),
            Self::Rejected(Reason::InvalidIdentity) => {
                write!(f, "the server could not verify your identity key")
            }
            Self::Rejected(Reason::ServerFull) => write!(f, "the server is full"),
            Self::Rejected(Reason::Unspecified) => write!(f, "the server refused the connection"),
            Self::CertificateMismatch => write!(
                f,
                "the server's certificate does not match the invite; refusing to connect"
            ),
            Self::Kicked => write!(f, "you were kicked from the server"),
            Self::Connection(e) => write!(f, "connection failed: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<quinn::ConnectionError> for Error {
    fn from(e: quinn::ConnectionError) -> Self {
        if tls::is_pin_mismatch(&e) {
            Self::CertificateMismatch
        } else {
            Self::Connection(e.to_string())
        }
    }
}

macro_rules! connection_error_from {
    ($($t:ty),*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Self::Connection(e.to_string())
            }
        }
    )*};
}
connection_error_from!(io::Error, quinn::ConnectError, quinn::WriteError);

/// Something to ask of the server. All but [`Request::JoinChannel`] need
/// the admin role, which the server checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Move ourselves into a channel.
    JoinChannel(u32),
    CreateChannel(String),
    /// Members in it move to the first channel; the last channel stays.
    DeleteChannel(u32),
    RenameChannel(u32, String),
    MoveMember {
        session_id: u32,
        channel_id: u32,
    },
    Kick(u32),
    /// Bans the member's identity key and disconnects it.
    Ban(u32),
    /// Use [`Client::create_invite`], which returns the token.
    CreateInvite {
        uses: u32,
        expires_in_secs: u32,
    },
}

impl Request {
    fn into_msg(self) -> Msg {
        match self {
            Self::JoinChannel(channel_id) => Msg::JoinChannel(control::JoinChannel { channel_id }),
            Self::CreateChannel(name) => Msg::CreateChannel(control::CreateChannel { name }),
            Self::DeleteChannel(channel_id) => {
                Msg::DeleteChannel(control::DeleteChannel { channel_id })
            }
            Self::RenameChannel(channel_id, name) => {
                Msg::RenameChannel(control::RenameChannel { channel_id, name })
            }
            Self::MoveMember {
                session_id,
                channel_id,
            } => Msg::MoveMember(control::MoveMember {
                session_id,
                channel_id,
            }),
            Self::Kick(session_id) => Msg::Kick(control::Kick { session_id }),
            Self::Ban(session_id) => Msg::Ban(control::Ban { session_id }),
            Self::CreateInvite {
                uses,
                expires_in_secs,
            } => Msg::CreateInvite(control::CreateInvite {
                uses,
                expires_in_secs,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// Needs the admin role.
    Forbidden,
    /// No such channel or member.
    NotFound,
    /// Malformed, or not allowed now (e.g. deleting the last channel).
    Invalid,
    /// Not connected, or the connection dropped before the answer.
    NotConnected,
    /// The server did not answer in time.
    NoResponse,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Forbidden => "only admins can do that",
            Self::NotFound => "no such channel or user",
            Self::Invalid => "the server refused the request",
            Self::NotConnected => "not connected",
            Self::NoResponse => "the server did not answer",
        })
    }
}

impl std::error::Error for RequestError {}

pub struct Client {
    status: watch::Receiver<Status>,
    state: watch::Receiver<ServerState>,
    events: broadcast::Sender<Event>,
    requests: mpsc::UnboundedSender<PendingRequest>,
    voice: watch::Sender<VoiceState>,
    task: tokio::task::AbortHandle,
}

impl Client {
    /// [`Client::connect_as`] with a throwaway identity, no display name and
    /// no invite.
    pub async fn connect(addr: SocketAddr, fingerprint: &str) -> Result<Self, Error> {
        Self::connect_as(addr, fingerprint, &IdentityKey::generate(), "", None).await
    }

    /// Connects to `addr` as `key`, trusting only the certificate with this
    /// SHA-256 `fingerprint` (hex), and completes the handshake. Any failure
    /// here is returned; once connected, lost connections are retried with
    /// backoff.
    ///
    /// `invite` is the token from an invite link (`t=`). A key the server
    /// does not know yet needs one; without it the server rejects with
    /// [`Reason::InviteExpired`]. Once joined, the key is known and later
    /// connects need no invite.
    pub async fn connect_as(
        addr: SocketAddr,
        fingerprint: &str,
        key: &IdentityKey,
        display_name: &str,
        invite: Option<&str>,
    ) -> Result<Self, Error> {
        let hello = Hello {
            protocol_version: kuulu_proto::PROTOCOL_VERSION,
            min_protocol_version: kuulu_proto::MIN_PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").into(),
            display_name: display_name.into(),
            invite: invite.unwrap_or_default().into(),
            ..Default::default()
        };
        Self::connect_with(addr, fingerprint, key.clone(), hello).await
    }

    async fn connect_with(
        addr: SocketAddr,
        fingerprint: &str,
        key: IdentityKey,
        hello: Hello,
    ) -> Result<Self, Error> {
        let dialer = Dialer::new(addr, fingerprint, key, hello)?;
        let session = dialer.connect().await?;
        Ok(session::start(dialer, session))
    }

    /// Our session id while connected; it changes on reconnect.
    pub fn session_id(&self) -> Option<u32> {
        match &*self.status.borrow() {
            Status::Connected { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// Round-trip time to the server, smoothed by QUIC; `None` while
    /// reconnecting.
    pub fn rtt(&self) -> Option<Duration> {
        match &*self.status.borrow() {
            Status::Connected { conn, .. } => Some(conn.rtt()),
            _ => None,
        }
    }

    /// Waits until the client has stopped for good (e.g. kicked, banned, or
    /// a version reject on reconnect) and returns why.
    pub async fn stopped(&self) -> Error {
        let mut status = self.status.clone();
        loop {
            if let Status::Failed(e) = &*status.borrow_and_update() {
                return e.clone();
            }
            if status.changed().await.is_err() {
                return Error::Connection("client stopped".into());
            }
        }
    }

    /// The current server state.
    pub fn state(&self) -> ServerState {
        self.state.borrow().clone()
    }

    /// Changes from now on, each already applied to [`Client::state`]. A
    /// receiver that falls behind gets `Lagged` and should re-read the state.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Mutes our microphone: [`Client::send`] drops frames and the server
    /// drops anything already on the way. Kept across reconnects.
    pub fn set_self_mute(&self, mute: bool) {
        self.voice
            .send_if_modified(|v| std::mem::replace(&mut v.mute, mute) != mute);
    }

    /// Deafens us: the server sends us no voice. Deafen implies mute. Kept
    /// across reconnects.
    pub fn set_self_deaf(&self, deaf: bool) {
        self.voice
            .send_if_modified(|v| std::mem::replace(&mut v.deaf, deaf) != deaf);
    }

    /// Asks the server for a change and waits for its answer. The resulting
    /// state change arrives as an event before this returns.
    pub async fn request(&self, request: Request) -> Result<(), RequestError> {
        self.ask(request).await.map(|_| ())
    }

    /// Creates an invite usable `uses` times within `expires` (admin only)
    /// and returns its token. The link is
    /// `kuulu://<host>:<port>/join?t=<token>&fp=<fingerprint>` with the
    /// address and fingerprint this client connected with.
    pub async fn create_invite(
        &self,
        uses: u32,
        expires: Duration,
    ) -> Result<String, RequestError> {
        let expires_in_secs =
            u32::try_from(expires.as_secs()).map_err(|_| RequestError::Invalid)?;
        self.ask(Request::CreateInvite {
            uses,
            expires_in_secs,
        })
        .await
    }

    /// Sends `request`; the answer's invite token on success.
    async fn ask(&self, request: Request) -> Result<String, RequestError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send((request, reply))
            .map_err(|_| RequestError::NotConnected)?;
        match tokio::time::timeout(REQUEST_TIMEOUT, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RequestError::NotConnected),
            Err(_) => Err(RequestError::NoResponse),
        }
    }

    /// Sends one encoded frame now. Never blocks or queues: while
    /// disconnected or muted, or when the link is congested, frames are
    /// dropped, as are packets over [`kuulu_proto::MAX_VOICE_PACKET`].
    ///
    /// Takes a lock and allocates, so never call it from the real-time audio
    /// thread: hand frames to a network task through an rtrb ring buffer.
    pub fn send(&self, flags: u8, seq: u16, timestamp: u32, payload: &[u8]) {
        let voice = *self.voice.borrow();
        if voice.mute || voice.deaf {
            return;
        }
        let Status::Connected { conn, session_id } = &*self.status.borrow() else {
            return;
        };
        let header = VoiceHeader {
            flags,
            sender: *session_id,
            seq,
            timestamp,
        };
        let mut packet = BytesMut::with_capacity(HEADER_LEN + payload.len());
        packet.put_slice(&header.to_bytes());
        packet.put_slice(payload);
        let _ = conn.send_datagram(packet.freeze());
    }

    /// Waits for the next voice packet, across reconnects. Fails only when
    /// the client has stopped for good, e.g. after a version reject or a kick.
    pub async fn recv(&self) -> Result<VoicePacket, Error> {
        let mut status = self.status.clone();
        loop {
            let current = status.borrow_and_update().clone();
            let conn = match current {
                Status::Failed(e) => return Err(e),
                Status::Connecting => None,
                Status::Connected { conn, .. } => Some(conn),
            };
            let datagram = async {
                match conn {
                    Some(conn) => conn.read_datagram().await.ok(),
                    None => None,
                }
            };
            tokio::select! {
                // A lost connection disables this branch until the status changes.
                Some(bytes) = datagram => {
                    if let Ok((header, _)) = VoiceHeader::parse(&bytes) {
                        let payload = bytes.slice(HEADER_LEN..);
                        return Ok(VoicePacket { header, payload });
                    }
                }
                changed = status.changed() => {
                    if changed.is_err() {
                        return Err(Error::Connection("client stopped".into()));
                    }
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
        if let Status::Connected { conn, .. } = &*self.status.borrow() {
            conn.close(0u32.into(), b"bye");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuulu_server::{Identity, Server};
    use std::sync::Arc;

    async fn server() -> (SocketAddr, String) {
        let identity = Identity::generate();
        let server = Arc::new(Server::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap());
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move { server.run().await });
        (addr, identity.fingerprint())
    }

    fn hello(min: u32, max: u32) -> Hello {
        Hello {
            protocol_version: max,
            min_protocol_version: min,
            ..Default::default()
        }
    }

    async fn connect_hello(addr: SocketAddr, fp: &str, hello: Hello) -> Result<Client, Error> {
        Client::connect_with(addr, fp, IdentityKey::generate(), hello).await
    }

    #[tokio::test]
    async fn handshake() {
        let (addr, fp) = server().await;
        let client = Client::connect(addr, &fp).await.unwrap();
        assert_eq!(client.session_id(), Some(1));
        // Fingerprints are hex; case must not matter.
        let client = Client::connect(addr, &fp.to_uppercase()).await.unwrap();
        assert_eq!(client.session_id(), Some(2));
        let state = client.state();
        assert_eq!(state.server_name, "Kuulu");
        assert_eq!(state.protocol_version, kuulu_proto::PROTOCOL_VERSION);
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.members.len(), 2);
    }

    #[tokio::test]
    async fn version_reject_is_reported() {
        let (addr, fp) = server().await;
        let v = kuulu_proto::PROTOCOL_VERSION;
        let err = connect_hello(addr, &fp, hello(v + 1, v + 1))
            .await
            .err()
            .unwrap();
        assert_eq!(err, Error::Rejected(Reason::TooNew));
        assert!(err.to_string().contains("older version"), "{err}");
        let err = connect_hello(addr, &fp, hello(0, 0)).await.err().unwrap();
        assert_eq!(err, Error::Rejected(Reason::TooOld));
        assert!(err.is_fatal());
    }

    #[tokio::test]
    async fn wrong_fingerprint_is_refused() {
        let (addr, _) = server().await;
        let other = Identity::generate().fingerprint();
        let err = Client::connect(addr, &other).await.err().unwrap();
        assert_eq!(err, Error::CertificateMismatch);
        assert!(err.is_fatal());
    }

    /// Sends 10 frames past the client's own mute check, as a modified
    /// client would, and returns how many `to` hears.
    async fn heard_raw(from: &Client, to: &Client) -> usize {
        let Status::Connected { conn, .. } = from.status.borrow().clone() else {
            panic!("not connected");
        };
        for seq in 0..10 {
            let header = VoiceHeader {
                flags: 0,
                sender: 0,
                seq,
                timestamp: 0,
            };
            let _ = conn.send_datagram(Bytes::copy_from_slice(&header.to_bytes()));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut heard = 0;
        while tokio::time::timeout(Duration::from_millis(300), to.recv())
            .await
            .is_ok()
        {
            heard += 1;
        }
        heard
    }

    async fn until_seen(observer: &Client, of: &Client, mute: bool, deaf: bool) {
        let id = of.session_id().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !observer
                .state()
                .members
                .get(&id)
                .is_some_and(|m| (m.self_mute, m.self_deaf) == (mute, deaf))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mute_and_deafen_are_enforced_by_the_server() {
        let (addr, fp) = server().await;
        let a = Client::connect(addr, &fp).await.unwrap();
        let b = Client::connect(addr, &fp).await.unwrap();
        assert!(heard_raw(&a, &b).await > 0);

        // Deafen implies mute.
        b.set_self_deaf(true);
        until_seen(&a, &b, true, true).await;
        assert_eq!(heard_raw(&a, &b).await, 0);
        assert_eq!(heard_raw(&b, &a).await, 0);

        b.set_self_deaf(false);
        b.set_self_mute(true);
        until_seen(&a, &b, true, false).await;
        assert!(heard_raw(&a, &b).await > 0);
        assert_eq!(heard_raw(&b, &a).await, 0);

        b.set_self_mute(false);
        until_seen(&a, &b, false, false).await;
        assert!(heard_raw(&b, &a).await > 0);
    }

    /// A Hello captured from one connection must not log in on another.
    #[tokio::test]
    async fn replayed_identity_proof_is_rejected() {
        let (addr, fp) = server().await;
        let key = IdentityKey::generate();
        let v = kuulu_proto::PROTOCOL_VERSION;
        let dialer = Dialer::new(addr, &fp, key.clone(), hello(v, v)).unwrap();
        let first = dialer.connection().await.unwrap();
        let second = dialer.connection().await.unwrap();
        let captured = Hello {
            public_key: key.public_key().to_vec(),
            identity_proof: key.prove(&first),
            ..hello(v, v)
        };

        let err = session::handshake(second, captured.clone()).await.err();
        assert_eq!(err, Some(Error::Rejected(Reason::InvalidIdentity)));
        // Another key's proof, and no proof at all, fail the same way.
        let third = dialer.connection().await.unwrap();
        let wrong_key = Hello {
            public_key: IdentityKey::generate().public_key().to_vec(),
            identity_proof: key.prove(&third),
            ..hello(v, v)
        };
        let err = session::handshake(third, wrong_key).await.err();
        assert_eq!(err, Some(Error::Rejected(Reason::InvalidIdentity)));
        let err = session::handshake(dialer.connection().await.unwrap(), hello(v, v))
            .await
            .err();
        assert_eq!(err, Some(Error::Rejected(Reason::InvalidIdentity)));
        // The genuine connection is accepted.
        assert!(session::handshake(first, captured).await.is_ok());
    }
}
