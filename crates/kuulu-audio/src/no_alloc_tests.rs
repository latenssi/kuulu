//! The audio thread's per-frame work and the device callbacks must run
//! without heap allocation. The allocator aborts on any allocation inside
//! `assert_no_alloc` in this test binary.

use crate::device::tests::rings;
use crate::{Decoder, Encoder, FRAME_SAMPLES, Framer, JitterBuffer, MAX_PAYLOAD, Mixer};
use assert_no_alloc::{AllocDisabler, assert_no_alloc};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn audio_path_does_not_allocate() {
    let (mut input, mut output, mut devices) = rings();
    let mut framer = Framer::new();
    let mut encoder = Encoder::new().unwrap();
    let mut decoder = Decoder::new().unwrap();
    let mut jb = JitterBuffer::new();
    let mut mixer = Mixer::new();
    let mut decoded = [0.0; FRAME_SAMPLES];
    let mut mixed = [0.0; FRAME_SAMPLES];
    let mut device_out = [0.0; 480]; // one 240-sample stereo period
    let mut seq = 0u16;
    const LATE_SEQ: u16 = 50;
    let mut late = [0u8; MAX_PAYLOAD];
    let mut late_len = None;
    let mut max_target = 0;

    assert_no_alloc(|| {
        // A tone, then silence, so that the target grown by one late packet
        // later shrinks by dropping a silent frame.
        for period in 0..1200usize {
            let samples: [f32; 240] = std::array::from_fn(|i| match period < 300 {
                true => ((period * 240 + i) as f32 * 0.06).sin() * 0.5,
                false => 0.0,
            });
            devices.capture(&samples);
            input.drain(|s| {
                framer.push(s, |frame| {
                    let payload = encoder.encode(frame);
                    if seq == LATE_SEQ {
                        late[..payload.len()].copy_from_slice(payload);
                        late_len = Some(payload.len());
                    } else {
                        jb.push(seq, payload);
                    }
                    seq = seq.wrapping_add(1);
                })
            });
            devices.playback(&mut device_out);
            while output.wants_frame() {
                jb.pop(&mut |p, f| decoder.decode(p, f), &mut decoded);
                mixer.mix([(&decoded, 0.8)], &mut mixed);
                assert!(output.push(&mixed));
            }
            if seq > LATE_SEQ + 2
                && let Some(len) = late_len.take()
            {
                jb.push(LATE_SEQ, &late[..len]); // its frame was concealed
            }
            max_target = max_target.max(jb.target());
        }
    });
    assert_eq!((max_target, jb.target()), (2, 1));
}
