//! Kuulu server: QUIC handshake and voice datagram forwarding within one channel.

mod config;
mod rate_limit;

pub use config::{Config, Identity};

use bytes::{BufMut, BytesMut};
use kuulu_proto::control::{
    self, ClientMessage, Reject, ServerMessage, Welcome, client_message, server_message,
};
use kuulu_proto::{VoiceHeader, frame};
use quinn::rustls;
use rate_limit::RateLimit;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{error::Error, io};

/// Datagrams per second a connection may send before the excess is dropped.
pub const RATE_LIMIT: u32 = 120;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CHANNEL_ID: u32 = 1;
const CHANNEL_NAME: &str = "General";

pub struct Server {
    endpoint: quinn::Endpoint,
    channel: Arc<Mutex<Channel>>,
}

/// The one channel. Sessions live only in memory.
#[derive(Default)]
struct Channel {
    last_session_id: u32,
    members: HashMap<u32, quinn::Connection>,
}

impl Server {
    /// Binds the UDP socket; call [`Server::run`] to start accepting.
    pub fn bind(addr: SocketAddr, identity: &Identity) -> io::Result<Self> {
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
        config.transport_config(Arc::new(kuulu_proto::transport_config()));
        let endpoint = quinn::Endpoint::server(config, addr)?;
        Ok(Self {
            endpoint,
            channel: Arc::default(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
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
            let channel = self.channel.clone();
            tokio::spawn(async move {
                let addr = incoming.remote_address();
                if let Err(e) = serve(incoming, channel).await {
                    eprintln!("{addr}: {e}");
                }
            });
        }
    }
}

async fn serve(
    incoming: quinn::Incoming,
    channel: Arc<Mutex<Channel>>,
) -> Result<(), Box<dyn Error>> {
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

    // Invites are not checked until they exist (M4/M5).
    if let Err(reason) = kuulu_proto::negotiate(&hello) {
        let reject = server_message::Msg::Reject(Reject {
            reason: reason.into(),
        });
        send.write_all(&frame::encode(&ServerMessage { msg: Some(reject) }))
            .await?;
        send.finish()?;
        // Closing right away would discard the Reject before the client reads it.
        let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, send.stopped()).await;
        return Err(format!("rejected: {}", reason.as_str_name()).into());
    }

    let session = Session::join(&channel, &conn);
    eprintln!("{}: session {} joined", conn.remote_address(), session.id);
    let welcome = server_message::Msg::Welcome(session.welcome());
    send.write_all(&frame::encode(&ServerMessage { msg: Some(welcome) }))
        .await?;

    // Nothing is expected from the client after Hello yet; unknown messages
    // are skipped so newer clients keep working. The stream ending means leave.
    let control = async {
        loop {
            if let Err(e) = frame::read::<ClientMessage>(&mut recv).await {
                return e;
            }
        }
    };
    let result: Result<(), Box<dyn Error>> = tokio::select! {
        e = control => Err(e.into()),
        e = session.forward_datagrams() => Err(e.into()),
    };
    eprintln!("{}: session {} left", conn.remote_address(), session.id);
    result
}

/// Channel membership of one connection; leaving happens on drop.
struct Session {
    id: u32,
    conn: quinn::Connection,
    channel: Arc<Mutex<Channel>>,
}

impl Session {
    fn join(channel: &Arc<Mutex<Channel>>, conn: &quinn::Connection) -> Self {
        let mut ch = channel.lock().unwrap();
        ch.last_session_id += 1;
        let id = ch.last_session_id;
        ch.members.insert(id, conn.clone());
        Self {
            id,
            conn: conn.clone(),
            channel: channel.clone(),
        }
    }

    fn welcome(&self) -> Welcome {
        let ch = self.channel.lock().unwrap();
        let mut members: Vec<_> = ch
            .members
            .keys()
            .map(|&session_id| control::Member { session_id })
            .collect();
        members.sort_by_key(|m| m.session_id);
        Welcome {
            session_id: self.id,
            server_version: env!("CARGO_PKG_VERSION").into(),
            channels: vec![control::Channel {
                id: CHANNEL_ID,
                name: CHANNEL_NAME.into(),
                members,
            }],
        }
    }

    /// Forwards each valid datagram to every other member without touching the
    /// payload. The sender field is overwritten so clients cannot spoof it.
    async fn forward_datagrams(&self) -> quinn::ConnectionError {
        let mut limit = RateLimit::new(RATE_LIMIT);
        loop {
            let packet = match self.conn.read_datagram().await {
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
            let ch = self.channel.lock().unwrap();
            for (&id, conn) in &ch.members {
                if id != self.id {
                    // Never blocks: quinn drops the oldest queued datagram instead.
                    let _ = conn.send_datagram(out.clone());
                }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.channel.lock().unwrap().members.remove(&self.id);
    }
}
