//! The whole per-frame pipeline must run without heap allocation.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use kuulu_audio::{Decoder, Encoder, FRAME_SAMPLES, Framer, JitterBuffer, Mixer};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn pipeline_does_not_allocate() {
    let mut framer = Framer::new();
    let mut encoder = Encoder::new().unwrap();
    let mut decoder = Decoder::new().unwrap();
    let mut jb = JitterBuffer::new();
    let mut mixer = Mixer::new();
    let capture = [0.25f32; 240];
    let mut decoded = [0.0; FRAME_SAMPLES];
    let mut out = [0.0; FRAME_SAMPLES];
    let mut seq = 0u16;

    assert_no_alloc(|| {
        for i in 0..1000 {
            framer.push(&capture, |frame| {
                // Skip ahead now and then to exercise loss, lateness and restart.
                seq = seq.wrapping_add(if i % 97 == 0 { 50 } else { 1 });
                jb.push(seq, encoder.encode(frame));
                if i % 13 == 0 {
                    jb.push(seq.wrapping_sub(3), &[]);
                }
            });
            if i % 2 == 0 {
                jb.pop(&mut |p, f| decoder.decode(p, f), &mut decoded);
                mixer.mix([(&decoded, 0.8)], &mut out);
            }
        }
    });
}
