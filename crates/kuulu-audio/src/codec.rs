//! Opus in restricted low-delay (CELT-only) mode: 10 ms frames with 2.5 ms
//! look-ahead, the smallest algorithmic delay Opus offers.

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};
use opus::{Application, Bitrate, Channels};

/// Largest Opus packet for one frame (RFC 6716, section 3.4).
pub const MAX_PAYLOAD: usize = 1275;
const BITRATE: i32 = 32_000;

pub struct Encoder {
    inner: opus::Encoder,
    packet: [u8; MAX_PAYLOAD],
}

impl Encoder {
    pub fn new() -> Result<Self, opus::Error> {
        let mut inner = opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::LowDelay)?;
        inner.set_bitrate(Bitrate::Bits(BITRATE))?;
        Ok(Self {
            inner,
            packet: [0; MAX_PAYLOAD],
        })
    }

    /// Encodes one frame. Returns an empty payload if the encoder fails; the
    /// receiver then conceals the frame as lost.
    pub fn encode(&mut self, frame: &Frame) -> &[u8] {
        let n = self
            .inner
            .encode_float(frame, &mut self.packet)
            .unwrap_or(0);
        &self.packet[..n]
    }
}

pub struct Decoder(opus::Decoder);

impl Decoder {
    pub fn new() -> Result<Self, opus::Error> {
        Ok(Self(opus::Decoder::new(SAMPLE_RATE, Channels::Mono)?))
    }

    /// Decodes `payload` into `out`; `None` (a lost frame) or a corrupt
    /// payload produces Opus packet loss concealment instead.
    pub fn decode(&mut self, payload: Option<&[u8]>, out: &mut Frame) {
        let decoded = payload
            .and_then(|p| self.0.decode_float(p, out, false).ok())
            .or_else(|| self.0.decode_float(&[], out, false).ok())
            .unwrap_or(0);
        out[decoded.min(FRAME_SAMPLES)..].fill(0.0);
    }

    /// Forgets the previous stream, so a new one starts clean.
    pub fn reset(&mut self) {
        let _ = self.0.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_tone_with_lookahead_delay() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let tone =
            |i: usize| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.5;
        let mut out = [0.0; FRAME_SAMPLES];
        let mut decoded = Vec::new();
        for f in 0..10 {
            let frame: Frame = std::array::from_fn(|i| tone(f * FRAME_SAMPLES + i));
            let payload = enc.encode(&frame).to_vec();
            assert!(!payload.is_empty() && payload.len() < 100);
            dec.decode(Some(&payload), &mut out);
            decoded.extend_from_slice(&out);
        }
        let delay = 120; // 2.5 ms look-ahead
        let err: f32 = (FRAME_SAMPLES * 5..FRAME_SAMPLES * 9)
            .map(|i| (decoded[i] - tone(i - delay)).powi(2))
            .sum::<f32>()
            / (FRAME_SAMPLES * 4) as f32;
        assert!(err < 0.001, "mean squared error {err}");
    }

    #[test]
    fn missing_frame_is_concealed() {
        let mut dec = Decoder::new().unwrap();
        let mut out = [1.0; FRAME_SAMPLES];
        dec.decode(None, &mut out);
        assert!(out.iter().all(|s| s.abs() < 1e-3));
    }
}
