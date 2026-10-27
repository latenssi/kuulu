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
use state::{Member, Persist, State};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{error::Error, io};
use store::Store;
use tokio::sync::mpsc;

/// Datagrams per second a connection may send before the excess is dropped.
pub const RATE_LIMIT: u32 = 120;
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
    name: String,
    /// Held only briefly and never across I/O: voice forwarding takes it per packet.
    state: Mutex<State>,
    /// Lock order: `store` before `state`.
    store: Mutex<Store>,
}

impl Server {
    /// A server with the default configuration that persists nothing, for
    /// tests and benchmarks. Call [`Server::run`] to start accepting.
    pub fn bind(addr: SocketAddr, identity: &Identity) -> io::Result<Self> {
        Self::start(addr, identity, Store::in_memory(Config::default())?)
    }

    /// The real server: `kuulu.toml`, `cert.der`, `key.der` and `kuulu.db` in
    /// `dir`, each created on first run. Listens on the configured port.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let config_path = dir.join("kuulu.toml");
        let config = Config::load_or_create(&config_path)?;
        let identity = Identity::load_or_generate(dir)?;
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
        Self::start(addr, &identity, Store::open(dir, config_path, config)?)
    }

    fn start(addr: SocketAddr, identity: &Identity, store: Store) -> io::Result<Self> {
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
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(Arc::new(kuulu_proto::server_transport_config()));
        let endpoint = quinn::Endpoint::server(config, addr)?;
        let config = store.config();
        let state = State::new(&config.channels, config.admin_keys()?, store.banned()?);
        Ok(Self {
            endpoint,
            shared: Arc::new(Shared {
                name: config.name.clone(),
                state: Mutex::new(state),
                store: Mutex::new(store),
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
    pub async fn close(&self) {
        self.endpoint.close(0u32.into(), b"server shutting down");
        self.endpoint.wait_idle().await;
    }

    /// Accepts connections until the endpoint closes.
    pub async fn run(&self) {
        while let Some(incoming) = self.endpoint.accept().await {
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
    shared.store.lock().unwrap().seen(&key, &hello.display_name);

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
        loop {
            let msg = frame::read::<ClientMessage>(&mut recv).await?;
            // Unknown requests from newer clients get INVALID, not silence.
            let status = match msg.msg {
                Some(msg) => session.handle(msg).err().unwrap_or(Status::Ok),
                None => Status::Invalid,
            };
            let response = server_message::Msg::Response(Response {
                request_id: msg.request_id,
                status: status.into(),
            });
            // A full queue means the connection is being closed anyway.
            let _ = control.try_send(ServerMessage {
                msg: Some(response),
            });
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
        let persist = self.shared.state.lock().unwrap().handle(self.id, msg)?;
        match persist {
            Persist::Nothing => {}
            Persist::Ban(key) => self.shared.store.lock().unwrap().ban(&key),
            Persist::Channels => {
                // Snapshot under the store lock, so concurrent changes are
                // written in order and the last write holds the newest state.
                let mut store = self.shared.store.lock().unwrap();
                let channels = self.shared.state.lock().unwrap().channels();
                store.save_channels(channels);
            }
        }
        Ok(())
    }

    /// Forwards each valid datagram to the sender's channel without touching
    /// the payload. The sender field is overwritten so clients cannot spoof it.
    async fn forward_datagrams(&self, conn: &quinn::Connection) -> quinn::ConnectionError {
        let mut limit = RateLimit::new(RATE_LIMIT);
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
