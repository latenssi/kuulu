//! Audio pipeline: capture, Opus, jitter buffer, mixing and playback.
//!
//! Everything except stream setup in [`device`] and the codec constructors
//! is real-time safe: no allocation, locks, I/O or logging.

mod codec;
pub mod device;
mod framer;
mod jitter;
mod mixer;
#[cfg(test)]
mod no_alloc_tests;
mod vad;

pub use codec::{Decoder, Encoder, MAX_PAYLOAD};
pub use framer::Framer;
pub use jitter::JitterBuffer;
pub use mixer::Mixer;
pub use vad::{Vad, level};

pub const SAMPLE_RATE: u32 = 48_000;
/// 10 ms at 48 kHz: the unit of encoding, jitter buffering and mixing.
pub const FRAME_SAMPLES: usize = 480;
/// Mono f32 samples, nominally in [-1, 1].
pub type Frame = [f32; FRAME_SAMPLES];
