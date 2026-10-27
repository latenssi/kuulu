//! The background task behind [`Client`]: handshake, control stream,
//! reconnects, and restoring channel and mute/deafen after a reconnect.

use crate::state::{Event, ServerState};
use crate::{Client, Error, Request, RequestError, tls};
use kuulu_proto::control::{
    ClientMessage, Hello, ServerMessage, SetVoiceState, client_message, reject::Reason,
    response::Status as ResponseStatus, server_message,
};
use kuulu_proto::identity::IdentityKey;
use kuulu_proto::{close, frame};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
/// A session lasting this long counts as healthy and resets the backoff.
const STABLE_SESSION: Duration = Duration::from_secs(10);
/// Events buffered per subscriber before it lags.
const EVENT_CAPACITY: usize = 256;

pub(crate) type PendingRequest = (Request, oneshot::Sender<Result<(), RequestError>>);

/// The voice path's view of the connection.
#[derive(Clone)]
pub(crate) enum Status {
    Connecting,
    Connected {
        conn: quinn::Connection,
        session_id: u32,
    },
    Failed(Error),
}

/// What the user wants; re-applied after every reconnect.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VoiceState {
    pub mute: bool,
    pub deaf: bool,
}

impl VoiceState {
    fn to_msg(self) -> client_message::Msg {
        client_message::Msg::SetVoiceState(SetVoiceState {
            self_mute: self.mute,
            self_deaf: self.deaf,
        })
    }
}

/// Everything needed to (re)connect.
pub(crate) struct Dialer {
    endpoint: quinn::Endpoint,
    addr: SocketAddr,
    key: IdentityKey,
    hello: Hello,
}

impl Dialer {
    pub fn new(
        addr: SocketAddr,
        fingerprint: &str,
        key: IdentityKey,
        mut hello: Hello,
    ) -> Result<Self, Error> {
        let bind: SocketAddr = if addr.is_ipv6() {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        };
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(tls::client_config(fingerprint));
        hello.public_key = key.public_key().to_vec();
        Ok(Self {
            endpoint,
            addr,
            key,
            hello,
        })
    }

    pub async fn connect(&self) -> Result<Session, Error> {
        let conn = self.connection().await?;
        let mut hello = self.hello.clone();
        hello.identity_proof = self.key.prove(&conn);
        handshake(conn, hello).await
    }

    pub async fn connection(&self) -> Result<quinn::Connection, Error> {
        Ok(self.endpoint.connect(self.addr, tls::SERVER_NAME)?.await?)
    }
}

/// Sends `hello` on `conn` and waits for Welcome.
pub(crate) async fn handshake(conn: quinn::Connection, hello: Hello) -> Result<Session, Error> {
    let handshake = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        let hello = client_message::Msg::Hello(hello);
        write(&mut send, 0, hello).await?;
        match frame::read::<ServerMessage>(&mut recv).await?.msg {
            Some(server_message::Msg::Welcome(welcome)) => Ok(Session {
                conn: conn.clone(),
                send,
                recv,
                state: ServerState::from_welcome(welcome),
            }),
            Some(server_message::Msg::Reject(reject)) => Err(Error::Rejected(reject.reason())),
            _ => Err(Error::Connection("unexpected reply to Hello".into())),
        }
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| Error::Connection("handshake timed out".into()))?
}

async fn write(
    send: &mut quinn::SendStream,
    request_id: u32,
    msg: client_message::Msg,
) -> Result<(), quinn::WriteError> {
    let msg = ClientMessage {
        request_id,
        msg: Some(msg),
    };
    send.write_all(&frame::encode(&msg)).await
}

/// An established connection with its control stream, which must stay open:
/// the server ends the session when it closes.
pub(crate) struct Session {
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    /// From Welcome.
    state: ServerState,
}

struct Outputs {
    status: watch::Sender<Status>,
    state: watch::Sender<ServerState>,
    events: broadcast::Sender<Event>,
}

struct Inputs {
    requests: mpsc::UnboundedReceiver<PendingRequest>,
    voice: watch::Receiver<VoiceState>,
}

/// Publishes the first session and keeps the connection up in a task.
pub(crate) fn start(dialer: Dialer, first: Session) -> Client {
    let (status, status_rx) = watch::channel(Status::Connecting);
    let (state, state_rx) = watch::channel(ServerState::default());
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let (requests, requests_rx) = mpsc::unbounded_channel();
    let (voice, voice_rx) = watch::channel(VoiceState::default());
    let out = Outputs {
        status,
        state,
        events: events.clone(),
    };
    first.publish(&out);
    let inputs = Inputs {
        requests: requests_rx,
        voice: voice_rx,
    };
    let task = tokio::spawn(maintain(dialer, first, out, inputs)).abort_handle();
    Client {
        status: status_rx,
        state: state_rx,
        events,
        requests,
        voice,
        task,
    }
}

async fn maintain(dialer: Dialer, mut session: Session, out: Outputs, mut inputs: Inputs) {
    let mut backoff = MIN_BACKOFF;
    let mut restore_channel = None;
    loop {
        let started = Instant::now();
        let fatal = session.run(&out, &mut inputs, restore_channel).await;
        restore_channel = out.state.borrow().me().map(|m| m.channel_id);
        out.state.send_modify(|s| s.session_id = None);
        let _ = out.events.send(Event::Disconnected);
        if let Some(e) = fatal {
            out.status.send_replace(Status::Failed(e));
            return;
        }
        out.status.send_replace(Status::Connecting);
        // A server that welcomes and then drops us must not cause a tight
        // reconnect loop: only a session that lasted resets the backoff.
        if started.elapsed() >= STABLE_SESSION {
            backoff = MIN_BACKOFF;
        } else {
            idle(tokio::time::sleep(backoff), &mut inputs.requests).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        session = loop {
            match idle(dialer.connect(), &mut inputs.requests).await {
                Ok(session) => break session,
                Err(e) if e.is_fatal() => {
                    out.status.send_replace(Status::Failed(e));
                    return;
                }
                Err(_) => {
                    idle(tokio::time::sleep(backoff), &mut inputs.requests).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        };
        session.publish(&out);
    }
}

/// Runs `fut`, meanwhile failing requests with [`RequestError::NotConnected`].
async fn idle<T>(
    fut: impl Future<Output = T>,
    requests: &mut mpsc::UnboundedReceiver<PendingRequest>,
) -> T {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return r,
            Some((_, reply)) = requests.recv() => {
                let _ = reply.send(Err(RequestError::NotConnected));
            }
        }
    }
}

impl Session {
    fn publish(&self, out: &Outputs) {
        out.status.send_replace(Status::Connected {
            conn: self.conn.clone(),
            session_id: self.state.session_id.expect("set from Welcome"),
        });
        out.state.send_replace(self.state.clone());
        let _ = out.events.send(Event::Connected);
    }

    /// Serves the control stream until the connection ends. Returns the
    /// error that must stop reconnecting, if any.
    async fn run(
        self,
        out: &Outputs,
        inputs: &mut Inputs,
        restore_channel: Option<u32>,
    ) -> Option<Error> {
        let Self {
            conn,
            mut send,
            mut recv,
            state,
        } = self;
        // frame::read is not cancel-safe, so it gets its own task.
        let (tx, mut incoming) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            while let Ok(msg) = frame::read::<ServerMessage>(&mut recv).await {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Request id 0: the responses are not awaited.
        let mut restore = Vec::new();
        if let Some(channel) = restore_channel
            && state.channels.contains_key(&channel)
            && state.me().is_some_and(|me| me.channel_id != channel)
        {
            restore.push(Request::JoinChannel(channel).into_msg());
        }
        let voice = *inputs.voice.borrow_and_update();
        if voice != VoiceState::default() {
            restore.push(voice.to_msg());
        }
        let mut ok = true;
        for msg in restore {
            ok = ok && write(&mut send, 0, msg).await.is_ok();
        }

        let mut pending: HashMap<u32, oneshot::Sender<_>> = HashMap::new();
        let mut last_id = 0u32;
        while ok {
            tokio::select! {
                msg = incoming.recv() => match msg.map(|m| m.msg) {
                    Some(Some(server_message::Msg::Response(r))) => {
                        if let Some(reply) = pending.remove(&r.request_id) {
                            let _ = reply.send(response(r.status));
                        }
                    }
                    Some(Some(msg)) => {
                        let mut event = None;
                        out.state.send_modify(|s| event = s.apply(msg));
                        if let Some(event) = event {
                            let _ = out.events.send(event);
                        }
                    }
                    // Unknown message from a newer server.
                    Some(None) => {}
                    None => break,
                },
                Some((request, reply)) = inputs.requests.recv() => {
                    last_id = last_id.wrapping_add(1).max(1);
                    if write(&mut send, last_id, request.into_msg()).await.is_err() {
                        let _ = reply.send(Err(RequestError::NotConnected));
                        break;
                    }
                    pending.insert(last_id, reply);
                }
                Ok(()) = inputs.voice.changed() => {
                    let voice = *inputs.voice.borrow_and_update();
                    ok = write(&mut send, 0, voice.to_msg()).await.is_ok();
                }
                _ = conn.closed() => break,
            }
        }
        // Pending requests fail with NotConnected as their senders drop.
        reader.abort();
        let fatal = match conn.close_reason() {
            Some(quinn::ConnectionError::ApplicationClosed(c)) => {
                match u32::try_from(c.error_code.into_inner()) {
                    Ok(close::KICKED) => Some(Error::Kicked),
                    Ok(close::BANNED) => Some(Error::Rejected(Reason::Banned)),
                    _ => None,
                }
            }
            _ => None,
        };
        conn.close(0u32.into(), b"");
        fatal
    }
}

/// Unknown statuses from newer servers are failures, not prost's default `Ok`.
fn response(status: i32) -> Result<(), RequestError> {
    match ResponseStatus::try_from(status) {
        Ok(ResponseStatus::Ok) => Ok(()),
        Ok(ResponseStatus::Forbidden) => Err(RequestError::Forbidden),
        Ok(ResponseStatus::NotFound) => Err(RequestError::NotFound),
        Ok(ResponseStatus::Invalid) | Err(_) => Err(RequestError::Invalid),
    }
}
