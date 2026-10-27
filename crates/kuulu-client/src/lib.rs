//! Client side of a Kuulu connection: handshake, voice datagrams, reconnects.
//!
//! [`Client`] only moves packets. The audio pipeline hands it encoded frames
//! with [`Client::send`] and takes received ones from [`Client::recv`]; the
//! connection is re-established behind that interface when it drops.

pub mod netem;
mod tls;

use bytes::{BufMut, Bytes, BytesMut};
use kuulu_proto::control::{
    ClientMessage, Hello, ServerMessage, client_message, reject::Reason, server_message,
};
use kuulu_proto::{HEADER_LEN, VoiceHeader, frame};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use std::{fmt, io};
use tokio::sync::watch;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// A received voice packet. `header.sender` is the speaker's session id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoicePacket {
    pub header: VoiceHeader,
    pub payload: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The server refused us. Retrying cannot help, so the client stops.
    Rejected(Reason),
    /// The server's certificate is not the pinned one. The client stops.
    CertificateMismatch,
    /// Transport failure. After the first connect these are retried.
    Connection(String),
}

impl Error {
    fn is_fatal(&self) -> bool {
        !matches!(self, Self::Connection(_))
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
            Self::Rejected(Reason::InviteExpired) => write!(f, "the invite has expired"),
            Self::Rejected(Reason::Banned) => write!(f, "you are banned from this server"),
            Self::Rejected(Reason::Unspecified) => write!(f, "the server refused the connection"),
            Self::CertificateMismatch => write!(
                f,
                "the server's certificate does not match the invite; refusing to connect"
            ),
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

#[derive(Clone)]
enum State {
    Connecting,
    Connected {
        conn: quinn::Connection,
        session_id: u32,
    },
    Failed(Error),
}

pub struct Client {
    state: watch::Receiver<State>,
    task: tokio::task::AbortHandle,
}

impl Client {
    /// Connects to `addr`, trusting only the certificate with this SHA-256
    /// `fingerprint` (hex), and completes the handshake. Any failure here is
    /// returned; once connected, lost connections are retried with backoff.
    pub async fn connect(addr: SocketAddr, fingerprint: &str) -> Result<Self, Error> {
        let hello = Hello {
            protocol_version: kuulu_proto::PROTOCOL_VERSION,
            min_protocol_version: kuulu_proto::MIN_PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        };
        Self::connect_with(addr, fingerprint, hello).await
    }

    async fn connect_with(
        addr: SocketAddr,
        fingerprint: &str,
        hello: Hello,
    ) -> Result<Self, Error> {
        let bind: SocketAddr = if addr.is_ipv6() {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        };
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(tls::client_config(fingerprint));
        let link = Link {
            endpoint,
            addr,
            hello,
        };
        let session = link.connect().await?;
        let (tx, state) = watch::channel(session.state());
        let task = tokio::spawn(link.maintain(session, tx)).abort_handle();
        Ok(Self { state, task })
    }

    /// Our session id while connected; it changes on reconnect.
    pub fn session_id(&self) -> Option<u32> {
        match &*self.state.borrow() {
            State::Connected { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// Sends one encoded frame now. Never blocks or queues: while
    /// disconnected, or when the link is congested, frames are dropped.
    pub fn send(&self, flags: u8, seq: u16, timestamp: u32, payload: &[u8]) {
        let State::Connected { conn, session_id } = &*self.state.borrow() else {
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
    /// the client has stopped for good, e.g. after a version reject.
    pub async fn recv(&self) -> Result<VoicePacket, Error> {
        let mut state = self.state.clone();
        loop {
            let current = state.borrow_and_update().clone();
            let conn = match current {
                State::Failed(e) => return Err(e),
                State::Connecting => None,
                State::Connected { conn, .. } => Some(conn),
            };
            let datagram = async {
                match conn {
                    Some(conn) => conn.read_datagram().await.ok(),
                    None => None,
                }
            };
            tokio::select! {
                // A lost connection disables this branch until the state changes.
                Some(bytes) = datagram => {
                    if let Ok((header, _)) = VoiceHeader::parse(&bytes) {
                        let payload = bytes.slice(HEADER_LEN..);
                        return Ok(VoicePacket { header, payload });
                    }
                }
                changed = state.changed() => {
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
        if let State::Connected { conn, .. } = &*self.state.borrow() {
            conn.close(0u32.into(), b"bye");
        }
    }
}

/// Everything needed to (re)connect.
struct Link {
    endpoint: quinn::Endpoint,
    addr: SocketAddr,
    hello: Hello,
}

/// An established connection with its control stream, which must stay open:
/// the server ends the session when it closes.
struct Session {
    conn: quinn::Connection,
    session_id: u32,
    _send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl Session {
    fn state(&self) -> State {
        State::Connected {
            conn: self.conn.clone(),
            session_id: self.session_id,
        }
    }

    async fn closed(&mut self) {
        let control = async { while frame::read::<ServerMessage>(&mut self.recv).await.is_ok() {} };
        tokio::select! {
            _ = self.conn.closed() => {}
            _ = control => {}
        }
        self.conn.close(0u32.into(), b"");
    }
}

impl Link {
    async fn connect(&self) -> Result<Session, Error> {
        let conn = self.endpoint.connect(self.addr, tls::SERVER_NAME)?.await?;
        let handshake = async {
            let (mut send, mut recv) = conn.open_bi().await?;
            let hello = client_message::Msg::Hello(self.hello.clone());
            send.write_all(&frame::encode(&ClientMessage { msg: Some(hello) }))
                .await?;
            match frame::read::<ServerMessage>(&mut recv).await?.msg {
                Some(server_message::Msg::Welcome(welcome)) => Ok(Session {
                    conn: conn.clone(),
                    session_id: welcome.session_id,
                    _send: send,
                    recv,
                }),
                Some(server_message::Msg::Reject(reject)) => Err(Error::Rejected(reject.reason())),
                None => Err(Error::Connection("unexpected reply to Hello".into())),
            }
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| Error::Connection("handshake timed out".into()))?
    }

    async fn maintain(self, mut session: Session, state: watch::Sender<State>) {
        loop {
            session.closed().await;
            state.send_replace(State::Connecting);
            let mut backoff = MIN_BACKOFF;
            session = loop {
                match self.connect().await {
                    Ok(session) => break session,
                    Err(e) if e.is_fatal() => {
                        state.send_replace(State::Failed(e));
                        return;
                    }
                    Err(_) => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
            };
            state.send_replace(session.state());
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

    #[tokio::test]
    async fn handshake() {
        let (addr, fp) = server().await;
        let client = Client::connect(addr, &fp).await.unwrap();
        assert_eq!(client.session_id(), Some(1));
        // Fingerprints are hex; case must not matter.
        let client = Client::connect(addr, &fp.to_uppercase()).await.unwrap();
        assert_eq!(client.session_id(), Some(2));
    }

    #[tokio::test]
    async fn version_reject_is_reported() {
        let (addr, fp) = server().await;
        let v = kuulu_proto::PROTOCOL_VERSION;
        let err = Client::connect_with(addr, &fp, hello(v + 1, v + 1))
            .await
            .err()
            .unwrap();
        assert_eq!(err, Error::Rejected(Reason::TooNew));
        assert!(err.to_string().contains("older version"), "{err}");
        let err = Client::connect_with(addr, &fp, hello(0, 0))
            .await
            .err()
            .unwrap();
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
}
