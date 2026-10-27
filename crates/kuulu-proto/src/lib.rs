//! Control messages, voice packet header and protocol versions.
//!
//! One QUIC connection per client: control messages travel length-prefixed on
//! a single bidirectional stream ([`frame`]), voice travels as unreliable
//! datagrams carrying a [`VoiceHeader`].

pub mod frame;
mod header;
mod version;

pub use header::{HEADER_LEN, PACKET_VERSION, ParseError, VoiceHeader, flags};
pub use version::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, alpn_protocols, negotiate};

/// Protobuf control messages (`proto/control.proto`).
pub mod control {
    include!(concat!(env!("OUT_DIR"), "/kuulu.control.rs"));
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

/// QUIC transport settings shared by client and server.
///
/// Stale audio is worse than lost audio: the datagram buffers hold about one
/// MTU, and quinn drops the oldest queued datagram when a new one does not fit,
/// so a stalled link loses old frames instead of delaying new ones.
pub fn transport_config() -> quinn::TransportConfig {
    // quinn charges a fixed per-datagram bookkeeping overhead against the buffer.
    const BUFFER: usize = 1200 + 64;
    let mut config = quinn::TransportConfig::default();
    config
        .datagram_send_buffer_size(BUFFER)
        .datagram_receive_buffer_size(Some(BUFFER))
        .keep_alive_interval(Some(std::time::Duration::from_secs(2)))
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(10)
                .try_into()
                .expect("valid idle timeout"),
        ));
    config
}
