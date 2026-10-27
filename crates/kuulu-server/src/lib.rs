//! Kuulu server: QUIC handshake, channels, and voice datagram forwarding
//! within a channel.

mod config;
mod rate_limit;
mod state;
mod store;

pub use config::{ChannelConfig, Config, Identity};

use bytes::{BufMut, BytesMut};
use kuulu_proto::control::{
    ClientMessage, Reject, Response, ServerMessage, Welcome, client_message, reject::Reason,
    response::Status, server_message,
};
use kuulu_proto::{VoiceHeader, frame, identity};
use quinn::rustls;
use rate_limit::RateLimit;
use state::{MAX_USERS, Member, State, clean_name};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{error::Error, io};
use store::Store;
use tokio::sync::mpsc;

/// Datagrams per second a connection may send before the excess is dropped.
pub const RATE_LIMIT: u32 = 120;
/// Control requests per second per session, with bursts of
/// [`CONTROL_BURST`]. A person clicking makes a few per second and a
/// reconnect restores state with two; a client exceeding this is broken or
/// hostile and is disconnected, so no member can monopolise the state lock.
pub const CONTROL_RATE: u32 = 20;
pub const CONTROL_BURST: u32 = 10;
/// New connections per second per source IP, with bursts of
/// [`HANDSHAKE_BURST`]: enough for a LAN party behind one NAT joining at
/// once; excess connections are refused before any TLS work.
pub const HANDSHAKE_RATE: u32 = 2;
pub const HANDSHAKE_BURST: u32 = 10;
/// Source IPs tracked for [`HANDSHAKE_RATE`]; the table is reset beyond this.
const TRACKED_IPS: usize = 4096;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Control messages queued towards one client before it counts as stalled.
const CONTROL_QUEUE: usize = 256;
/// Capabilities announced in Welcome; none yet.
const FEATURES: &[&str] = &[];

pub struct Server {
    endpoint: quinn::Endpoint,
    shared: Arc<Shared>,
    fingerprint: String,
}

struct Shared {
    /// Cleaned like display names, so it fits the Welcome size bound.
    name: String,
    /// Held only briefly and never across I/O: voice forwarding takes it per packet.
    state: Mutex<State>,
    store: Store,
}

impl Server {
    /// A server with the default configuration that persists nothing, for
    /// tests and benchmarks. Call [`Server::run`] to start accepting.
    pub fn bind(addr: SocketAddr, identity: &Identity) -> io::Result<Self> {
        Self::start(addr, identity, &Config::default(), None)
    }

    /// The real server: `kuulu.toml`, `cert.der`, `key.der` and `kuulu.db` in
    /// `dir`, each created on first run. Listens on the configured port.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let config = Config::load_or_create(&dir.join("kuulu.toml"))?;
        let identity = Identity::load_or_generate(dir)?;
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
        Self::start(addr, &identity, &config, Some(dir))
    }

    fn start(
        addr: SocketAddr,
        identity: &Identity,
        config: &Config,
        dir: Option<&Path>,
    ) -> io::Result<Self> {
        let admins = config.admin_keys()?;
        let seed: Vec<_> = config
            .channels
            .iter()
            .map(|c| ChannelConfig {
                id: c.id,
                name: clean_name(&c.name),
            })
            .collect();
        if config.max_users > MAX_USERS {
            eprintln!("max_users {} lowered to {MAX_USERS}", config.max_users);
        }
        let max_users = config.max_users.min(MAX_USERS);
        let (store, loaded) = Store::open(dir, &seed)?;
        let state = State::new(loaded, max_users, admins, store.clone());

        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .and_then(|b| {
            b.with_no_client_auth()
                .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())
        })
        .map_err(io::Error::other)?;
        tls.alpn_protocols = kuulu_proto::alpn_protocols();
        let crypto =
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
        let mut quic = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        quic.transport_config(Arc::new(kuulu_proto::server_transport_config()));
        let endpoint = quinn::Endpoint::server(quic, addr)?;
        Ok(Self {
            endpoint,
            shared: Arc::new(Shared {
                name: clean_name(&config.name),
                state: Mutex::new(state),
                store,
            }),
            fingerprint: identity.fingerprint(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// SHA-256 of the certificate, which clients pin.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Disconnects everyone at once, so clients reconnect right away instead
    /// of waiting for an idle timeout, and stops [`Server::run`].
    /// Also waits until every change is in the database.
    pub async fn close(&self) {
        self.endpoint.close(0u32.into(), b"server shutting down");
        self.endpoint.wait_idle().await;
        let store = self.shared.store.clone();
        let _ = tokio::task::spawn_blocking(move || store.flush()).await;
    }

    /// Accepts connections until the endpoint closes.
    pub async fn run(&self) {
        let mut per_ip: HashMap<IpAddr, RateLimit> = HashMap::new();
        while let Some(incoming) = self.endpoint.accept().await {
            let ip = incoming.remote_address().ip();
            if per_ip.len() >= TRACKED_IPS && !per_ip.contains_key(&ip) {
                per_ip.clear();
            }
            let limit = per_ip
                .entry(ip)
                .or_insert_with(|| RateLimit::new(HANDSHAKE_RATE, HANDSHAKE_BURST));
            if !limit.allow(Instant::now()) {
                incoming.refuse();
                continue;
            }
            let shared = self.shared.clone();
            tokio::spawn(async move {
                let addr = incoming.remote_address();
                if let Err(e) = serve(incoming, shared).await {
                    eprintln!("{addr}: {e}");
                }
            });
        }
    }
}

async fn serve(incoming: quinn::Incoming, shared: Arc<Shared>) -> Result<(), Box<dyn Error>> {
    let conn = incoming.await?;
    let (mut send, mut recv, hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (send, mut recv) = conn.accept_bi().await?;
        let msg = frame::read::<ClientMessage>(&mut recv).await?;
        Ok::<_, Box<dyn Error>>((send, recv, msg))
    })
    .await??;
    let Some(client_message::Msg::Hello(hello)) = hello.msg else {
        return Err("expected Hello".into());
    };

    // Invites are not checked until they exist (M5).
    let protocol_version = match kuulu_proto::negotiate(&hello) {
        Ok(v) => v,
        Err(reason) => return reject(send, reason).await,
    };
    let Some(key) = identity::verify(&conn, &hello.public_key, &hello.identity_proof) else {
        return reject(send, Reason::InvalidIdentity).await;
    };
    let (control, mut outbox) = mpsc::channel(CONTROL_QUEUE);
    let member = Member::new(conn.clone(), control.clone(), key, &hello.display_name);
    let welcome = Welcome {
        server_version: env!("CARGO_PKG_VERSION").into(),
        server_name: shared.name.clone(),
        protocol_version,
        features: FEATURES.iter().map(|f| f.to_string()).collect(),
        ..Default::default()
    };
    let joined = shared.state.lock().unwrap().join(member, welcome);
    let session = match joined {
        Ok(id) => Session { id, shared },
        Err(reason) => return reject(send, reason).await,
    };
    eprintln!("{}: session {} joined", conn.remote_address(), session.id);

    // Responses share the queue with events, so they arrive after the events
    // their request caused.
    let write = async {
        while let Some(msg) = outbox.recv().await {
            send.write_all(&frame::encode(&msg)).await?;
        }
        Ok(())
    };
    let read = async {
        let mut limit = RateLimit::new(CONTROL_RATE, CONTROL_BURST);
        loop {
            let msg = frame::read::<ClientMessage>(&mut recv).await?;
            if !limit.allow(Instant::now()) {
                conn.close(0u32.into(), b"too many requests");
                return Err("too many requests".into());
            }
            // Unknown requests from newer clients get INVALID, not silence.
            let status = match msg.msg {
                Some(msg) => session.handle(msg).err().unwrap_or(Status::Ok),
                None => Status::Invalid,
            };
            let response = server_message::Msg::Response(Response {
                request_id: msg.request_id,
                status: status.into(),
            });
            // A client that does not read its responses is stalled, like one
            // that does not read events.
            if control
                .try_send(ServerMessage {
                    msg: Some(response),
                })
                .is_err()
            {
                conn.close(0u32.into(), b"control stream too slow");
                return Err("control stream too slow".into());
            }
        }
    };
    let result: Result<(), Box<dyn Error>> = tokio::select! {
        r = write => r,
        r = read => r,
        e = session.forward_datagrams(&conn) => Err(e.into()),
    };
    eprintln!("{}: session {} left", conn.remote_address(), session.id);
    result
}

async fn reject(mut send: quinn::SendStream, reason: Reason) -> Result<(), Box<dyn Error>> {
    let reject = server_message::Msg::Reject(Reject {
        reason: reason.into(),
    });
    send.write_all(&frame::encode(&ServerMessage { msg: Some(reject) }))
        .await?;
    send.finish()?;
    // Closing right away would discard the Reject before the client reads it.
    let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, send.stopped()).await;
    Err(format!("rejected: {}", reason.as_str_name()).into())
}

/// Membership of one connection; leaving happens on drop.
struct Session {
    id: u32,
    shared: Arc<Shared>,
}

impl Session {
    fn handle(&self, msg: client_message::Msg) -> Result<(), Status> {
        self.shared.state.lock().unwrap().handle(self.id, msg)
    }

    /// Forwards each valid datagram to the sender's channel without touching
    /// the payload. The sender field is overwritten so clients cannot spoof it.
    async fn forward_datagrams(&self, conn: &quinn::Connection) -> quinn::ConnectionError {
        let mut limit = RateLimit::new(RATE_LIMIT, RATE_LIMIT / 10);
        loop {
            let packet = match conn.read_datagram().await {
                Ok(packet) => packet,
                Err(e) => return e,
            };
            if !limit.allow(Instant::now()) {
                continue;
            }
            let Ok((mut header, payload)) = VoiceHeader::parse(&packet) else {
                continue;
            };
            header.sender = self.id;
            let mut out = BytesMut::with_capacity(packet.len());
            out.put_slice(&header.to_bytes());
            out.put_slice(payload);
            let out = out.freeze();
            self.shared.state.lock().unwrap().forward(self.id, &out);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().leave(self.id);
    }
}
