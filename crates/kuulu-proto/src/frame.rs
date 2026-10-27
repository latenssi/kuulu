//! Control stream framing: `u32` little-endian length, then one protobuf message.

use prost::Message;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest accepted message body; bounds memory a peer can make us allocate.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

pub fn encode(msg: &impl Message) -> Vec<u8> {
    let len = msg.encoded_len();
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    msg.encode(&mut out).expect("Vec grows as needed");
    out
}

/// Reads one frame. A stream that ends cleanly between frames yields `UnexpectedEof`.
pub async fn read<M: Message + Default>(r: &mut (impl AsyncRead + Unpin)) -> io::Result<M> {
    let len = r.read_u32_le().await? as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too long"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    M::decode(body.as_slice()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Hello, ServerMessage};

    #[tokio::test]
    async fn round_trip() {
        let hello = Hello {
            protocol_version: 3,
            app_version: "1.2.3".into(),
            features: vec!["x".into()],
            ..Default::default()
        };
        let mut bytes = encode(&hello);
        bytes.extend(encode(&Hello::default()));
        let mut r = bytes.as_slice();
        assert_eq!(read::<Hello>(&mut r).await.unwrap(), hello);
        assert_eq!(read::<Hello>(&mut r).await.unwrap(), Hello::default());
        assert_eq!(
            read::<Hello>(&mut r).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn rejects_oversized_and_garbage() {
        let mut r = &((MAX_FRAME_LEN + 1) as u32).to_le_bytes()[..];
        let err = read::<Hello>(&mut r).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut r = &[2, 0, 0, 0, 0xff, 0xff][..];
        let err = read::<ServerMessage>(&mut r).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Truncated body.
        let mut r = &[5, 0, 0, 0, 1][..];
        assert!(read::<ServerMessage>(&mut r).await.is_err());
    }

    /// Fields only ever get added: messages from older and newer peers decode.
    #[test]
    fn additive_compatibility() {
        use crate::control::{ClientMessage, client_message::Msg};
        // A Hello from before display_name and identity_proof existed.
        let old = [0x0a, 0x04, 0x08, 0x01, 0x10, 0x01];
        let msg = ClientMessage::decode(&old[..]).unwrap();
        let Some(Msg::Hello(hello)) = &msg.msg else {
            panic!("{msg:?}")
        };
        assert_eq!(hello.protocol_version, 1);
        assert!(hello.display_name.is_empty() && hello.identity_proof.is_empty());

        // A newer message with an unknown field (99) and an unknown oneof case (50).
        let mut newer = old.to_vec();
        newer.extend([0x98, 0x06, 0x07]);
        assert_eq!(ClientMessage::decode(&newer[..]).unwrap().msg, msg.msg);
        let unknown = [0x92, 0x03, 0x00, 0x78, 0x05];
        let msg = ClientMessage::decode(&unknown[..]).unwrap();
        assert_eq!((msg.msg, msg.request_id), (None, 5));
    }
}
