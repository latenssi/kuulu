//! Control messages, voice packet header and protocol versions.
//!
//! One QUIC connection per client: control messages travel length-prefixed on
//! a single bidirectional stream ([`frame`]), voice travels as unreliable
//! datagrams carrying a [`VoiceHeader`].

pub mod frame;
mod header;
pub mod identity;
pub mod redundancy;
mod version;

pub use header::{HEADER_LEN, PACKET_VERSION, ParseError, VoiceHeader, flags};
pub use version::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, alpn_protocols, negotiate};

/// Protobuf control messages (`proto/control.proto`).
pub mod control {
    include!(concat!(env!("OUT_DIR"), "/kuulu.control.rs"));
}

/// QUIC application close codes the server uses. Clients must not reconnect
/// automatically after either.
pub mod close {
    /// An admin kicked this session.
    pub const KICKED: u32 = 1;
    /// An admin banned this identity; reconnecting gets `Reject(BANNED)`.
    pub const BANNED: u32 = 2;
}

/// Default server UDP port.
pub const DEFAULT_PORT: u16 = 7700;

/// SHA-256 of a DER certificate as lowercase hex: what invite links carry and
/// clients pin, since servers use self-signed certificates.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, cert_der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Datagram buffer sizing. Stale audio is worse than lost audio: when a new
// datagram does not fit, quinn drops the oldest queued one, so these sizes
// bound how far behind real time a stalled link can make a listener.

/// quinn charges each queued datagram its payload plus one `Bytes` handle
/// (32 B on 64-bit targets).
const DATAGRAM_OVERHEAD: usize = 32;
/// Typical packet: header + 10 ms Opus frame at 32 kbit/s (40 B) + the
/// redundant previous frame = 92 B.
const TYPICAL_VOICE_PACKET: usize = HEADER_LEN + 2 * 40;
/// Client send queue: two typical packets (248 B), i.e. the frame being sent
/// plus at most one 10 ms frame waiting.
const CLIENT_SEND_BUFFER: usize = 2 * (TYPICAL_VOICE_PACKET + DATAGRAM_OVERHEAD);
/// Largest voice packet a client can send (216 B): it must fit the client
/// send queue alone. Larger packets are dropped.
pub const MAX_VOICE_PACKET: usize = CLIENT_SEND_BUFFER - DATAGRAM_OVERHEAD;
/// Server queue towards each listener: one frame from each of 4 simultaneous
/// speakers (496 B). More than 4 people talking at once is unintelligible
/// anyway; with a single speaker a stall holds at most ~40 ms of audio.
const SERVER_SEND_BUFFER: usize = 4 * (TYPICAL_VOICE_PACKET + DATAGRAM_OVERHEAD);
/// Receive queue (7936 B): the network task drains it continuously, so it only
/// fills in bursts; room for one max-size frame from 32 speakers keeps a crowd
/// from evicting fresh audio. Late frames are the jitter buffer's to discard.
const RECEIVE_BUFFER: usize = 32 * (MAX_VOICE_PACKET + DATAGRAM_OVERHEAD);
const _: () = assert!(SERVER_SEND_BUFFER >= MAX_VOICE_PACKET + DATAGRAM_OVERHEAD);

/// QUIC transport settings for clients.
pub fn client_transport_config() -> quinn::TransportConfig {
    transport_config(CLIENT_SEND_BUFFER)
}

/// QUIC transport settings for the server.
pub fn server_transport_config() -> quinn::TransportConfig {
    transport_config(SERVER_SEND_BUFFER)
}

fn transport_config(send_buffer: usize) -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config
        .datagram_send_buffer_size(send_buffer)
        .datagram_receive_buffer_size(Some(RECEIVE_BUFFER))
        .keep_alive_interval(Some(std::time::Duration::from_secs(2)))
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(10)
                .try_into()
                .expect("valid idle timeout"),
        ));
    config
}
