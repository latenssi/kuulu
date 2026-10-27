//! Microphone → framing → Opus → jitter buffer → mixer → speakers, without
//! a network. Runs until interrupted, printing device stats every second.

use crate::latency::BUDGET_MS;
use assert_no_alloc::assert_no_alloc;
use kuulu_audio::device::{self, Input, Output};
use kuulu_audio::{Decoder, Encoder, FRAME_SAMPLES, Framer, JitterBuffer, Mixer, SAMPLE_RATE};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Opus restricted low-delay look-ahead, as measured by `latency`.
const LOOKAHEAD_MS: f64 = 2.5;

pub fn run() -> ExitCode {
    match start() {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("loopback: {e}");
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<std::convert::Infallible, device::Error> {
    let jitter_target = Arc::new(AtomicUsize::new(0));
    let (rings_tx, rings_rx) = mpsc::channel();
    let audio = {
        let jitter_target = jitter_target.clone();
        thread::Builder::new().name("audio".into()).spawn(move || {
            if let Ok((input, output)) = rings_rx.recv() {
                audio_thread(input, output, &jitter_target);
            }
        })?
    };
    // Streams wake the audio thread, so it exists first and gets its rings
    // once both devices are open.
    let (capture, input) = device::capture(audio.thread().clone())?;
    let (playback, output) = device::playback(audio.thread().clone())?;
    rings_tx
        .send((input, output))
        .map_err(|_| "audio thread exited")?;

    loop {
        thread::sleep(Duration::from_secs(1));
        let ms = |samples: usize| samples as f64 * 1000.0 / SAMPLE_RATE as f64;
        let (cap_ms, play_ms) = (
            capture.latency().as_secs_f64() * 1000.0,
            playback.latency().as_secs_f64() * 1000.0,
        );
        let target = jitter_target.load(Relaxed);
        // Same stages as the `latency` simulation, with the devices' measured
        // latency and periods in place of the modelled 5 ms periods.
        let estimate = cap_ms
            + ms(FRAME_SAMPLES.saturating_sub(capture.period()))
            + LOOKAHEAD_MS
            + ms(target * FRAME_SAMPLES)
            + ms(playback.period())
            + play_ms;
        println!(
            "period capture {} / playback {} samples, device latency capture {cap_ms:.1} / playback {play_ms:.1} ms, \
             xruns capture {} / playback {}, jitter target {target} frames, \
             worst-case mouth-to-ear {estimate:.1} ms (budget {BUDGET_MS} ms)",
            capture.period(),
            playback.period(),
            capture.xruns(),
            playback.xruns(),
        );
    }
}

fn audio_thread(mut input: Input, mut output: Output, jitter_target: &AtomicUsize) -> ! {
    let mut framer = Framer::new();
    let mut encoder = Encoder::new().expect("opus encoder");
    let mut decoder = Decoder::new().expect("opus decoder");
    let mut jitter = JitterBuffer::new();
    let mut mixer = Mixer::new();
    let mut seq = 0u16;
    let mut decoded = [0.0; FRAME_SAMPLES];
    let mut mixed = [0.0; FRAME_SAMPLES];
    loop {
        assert_no_alloc(|| {
            input.drain(|samples| {
                framer.push(samples, |frame| {
                    jitter.push(seq, encoder.encode(frame));
                    seq = seq.wrapping_add(1);
                })
            });
            while output.wants_frame() {
                jitter.pop(&mut |p, f| decoder.decode(p, f), &mut decoded);
                mixer.mix([(&decoded, 1.0)], &mut mixed);
                if !output.push(&mixed) {
                    break;
                }
            }
            jitter_target.store(jitter.target(), Relaxed);
        });
        // Woken by every capture and playback callback.
        thread::park();
    }
}
