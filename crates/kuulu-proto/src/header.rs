//! Fixed little-endian voice packet header:
//! `version u8 | flags u8 | sender u32 | seq u16 | timestamp u32 | payload`.

/// Packet format version written by this build.
pub const PACKET_VERSION: u8 = 1;
/// Header size in bytes; the payload follows directly.
pub const HEADER_LEN: usize = 12;

/// Bits of [`VoiceHeader::flags`]. Unknown bits are preserved, not rejected.
pub mod flags {
    /// Push-to-talk released; this is the sender's last frame for now.
    pub const PTT_END: u8 = 1 << 0;
    /// Payload carries the previous frame too, as loss redundancy.
    pub const REDUNDANT: u8 = 1 << 1;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceHeader {
    pub flags: u8,
    /// Sender session id. The server overwrites it; the client's value is ignored.
    pub sender: u32,
    /// Frame sequence number, wraps around.
    pub seq: u16,
    /// Sample counter at 48 kHz.
    pub timestamp: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    TooShort,
    UnknownVersion(u8),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "voice packet shorter than header"),
            Self::UnknownVersion(v) => write!(f, "unknown voice packet version {v}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl VoiceHeader {
    /// Splits a packet into header and payload without allocating.
    pub fn parse(packet: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        let (head, payload) = packet
            .split_first_chunk::<HEADER_LEN>()
            .ok_or(ParseError::TooShort)?;
        if head[0] != PACKET_VERSION {
            return Err(ParseError::UnknownVersion(head[0]));
        }
        let header = Self {
            flags: head[1],
            sender: u32::from_le_bytes([head[2], head[3], head[4], head[5]]),
            seq: u16::from_le_bytes([head[6], head[7]]),
            timestamp: u32::from_le_bytes([head[8], head[9], head[10], head[11]]),
        };
        Ok((header, payload))
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0; HEADER_LEN];
        out[0] = PACKET_VERSION;
        out[1] = self.flags;
        out[2..6].copy_from_slice(&self.sender.to_le_bytes());
        out[6..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.timestamp.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let header = VoiceHeader {
            flags: flags::PTT_END | flags::REDUNDANT | 0x80,
            sender: 0xdead_beef,
            seq: u16::MAX,
            timestamp: 0x0102_0304,
        };
        let mut packet = header.to_bytes().to_vec();
        packet.extend_from_slice(b"opus");
        assert_eq!(VoiceHeader::parse(&packet), Ok((header, &b"opus"[..])));
    }

    #[test]
    fn little_endian_layout() {
        let packet = [1, 2, 3, 0, 0, 0, 4, 0, 5, 0, 0, 0];
        let (header, payload) = VoiceHeader::parse(&packet).unwrap();
        assert_eq!(
            header,
            VoiceHeader {
                flags: 2,
                sender: 3,
                seq: 4,
                timestamp: 5
            }
        );
        assert!(payload.is_empty());
    }

    #[test]
    fn malformed() {
        for len in 0..HEADER_LEN {
            let packet = vec![PACKET_VERSION; len];
            assert_eq!(VoiceHeader::parse(&packet), Err(ParseError::TooShort));
        }
        let mut packet = [0; HEADER_LEN];
        for version in [0, 2, u8::MAX] {
            packet[0] = version;
            assert_eq!(
                VoiceHeader::parse(&packet),
                Err(ParseError::UnknownVersion(version))
            );
        }
    }
}
