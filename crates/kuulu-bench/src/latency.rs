//! Algorithmic mouth-to-ear delay without network, on a simulated clock.
//!
//! A click (a 10 ms chirp) goes through the real
//! framing, Opus, jitter buffer and mixer code, driven the way the loopback
//! audio thread drives them. Devices are modelled with one 5 ms period:
//! - capture: every period the callback delivers the period just recorded;
//! - playback: every period the callback takes one period from the ring,
//!   which the device plays during the following period.
//!
//! After each callback the audio thread runs: it encodes complete frames
//! (zero-delay network into the jitter buffer) and tops up the playback ring.
//! The playback clock's phase relative to capture is swept across a whole
//! frame, so the worst case (a packet arriving just after a playout tick)
//! is included. Delay is found by cross-correlating the click with the
//! output, exact to the sample. Exits non-zero if it exceeds the budget.

use kuulu_audio::device::DEVICE_BUFFER;
use kuulu_audio::{
    Decoder, Encoder, FRAME_SAMPLES, Frame, Framer, JitterBuffer, Mixer, SAMPLE_RATE,
};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::process::ExitCode;

/// Latency budget without network, docs/PLAN.md: capture 5 + Opus 12.5 +
/// jitter buffer 10 + playback 5.
const BUDGET_MS: f64 = 32.5;
const PERIOD: usize = DEVICE_BUFFER as usize;
const CLICK_LEN: usize = 480;
/// The click starts this far into the run, after codec and buffer start-up.
const CLICK_FRAME: usize = 20;
const RUN: usize = 40 * FRAME_SAMPLES;
/// Longest delay searched for; also bounds the simulated output.
const MAX_DELAY: usize = 100 * SAMPLE_RATE as usize / 1000;
/// Sweep steps, in samples, over the click's and the playback clock's phase.
const CLICK_STEP: usize = 120;
const PLAYBACK_STEP: usize = 40;

/// Delays in samples.
#[derive(Clone, Copy)]
struct Stages {
    capture: usize,
    framing: usize,
    codec: usize,
    jitter: usize,
    total: usize,
}

pub fn run() -> ExitCode {
    // (click phase, stages) for every click and playback clock phase.
    let mut runs = Vec::new();
    for click_phase in (0..FRAME_SAMPLES).step_by(CLICK_STEP) {
        for playback_phase in (0..FRAME_SAMPLES).step_by(PLAYBACK_STEP) {
            let click_at = CLICK_FRAME * FRAME_SAMPLES + click_phase;
            runs.push((
                click_phase,
                Sim::new(click_at).run(FRAME_SAMPLES + playback_phase),
            ));
        }
    }
    let totals = || runs.iter().map(|(_, r)| r.total);
    let (best, worst) = (totals().min().unwrap(), totals().max().unwrap());

    // Stages are attributed per frame, so show a click at a frame start:
    // later in the frame, look-ahead pushes its start into the next frame.
    let shown = runs
        .iter()
        .filter(|(phase, _)| *phase == 0)
        .map(|(_, r)| *r)
        .max_by_key(|r| r.total)
        .unwrap();
    println!("worst case for a click at a frame start:");
    let playback = shown.total - shown.capture - shown.framing - shown.codec - shown.jitter;
    for (name, samples) in [
        ("capture", shown.capture),
        ("framing", shown.framing),
        ("opus look-ahead", shown.codec),
        ("jitter buffer", shown.jitter),
        ("playback", playback),
        ("total", shown.total),
    ] {
        println!("  {name:<16} {:>7.3} ms  ({samples} samples)", ms(samples));
    }
    println!(
        "total over {} click and clock phases: {:.3}..{:.3} ms, budget {BUDGET_MS} ms",
        runs.len(),
        ms(best),
        ms(worst)
    );
    if ms(worst) > BUDGET_MS {
        eprintln!("latency over budget");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn ms(samples: usize) -> f64 {
    samples as f64 * 1000.0 / SAMPLE_RATE as f64
}

struct Sim {
    click: Vec<f32>,
    click_at: usize,
    input: Vec<f32>,
    framer: Framer,
    encoder: Encoder,
    decoder: Decoder,
    jitter: JitterBuffer,
    mixer: Mixer,
    seq: u16,
    /// Capture samples not yet read by the audio thread.
    captured: Vec<f32>,
    playback_ring: VecDeque<f32>,
    /// Device output by time in samples.
    heard: Vec<f32>,
    /// Every decoded frame in play order.
    decoded: Vec<f32>,
    click_payload: Vec<u8>,
    captured_at: usize,
    framed_at: usize,
    popped_at: usize,
    /// Start of the click frame within `decoded`.
    popped_index: usize,
}

impl Sim {
    fn new(click_at: usize) -> Self {
        let click = click_signal();
        let mut input = vec![0.0; RUN];
        input[click_at..click_at + CLICK_LEN].copy_from_slice(&click);
        Self {
            click,
            click_at,
            input,
            framer: Framer::new(),
            encoder: Encoder::new().unwrap(),
            decoder: Decoder::new().unwrap(),
            jitter: JitterBuffer::new(),
            mixer: Mixer::new(),
            seq: 0,
            captured: Vec::new(),
            playback_ring: VecDeque::new(),
            heard: vec![0.0; click_at + MAX_DELAY + CLICK_LEN],
            decoded: Vec::new(),
            click_payload: Vec::new(),
            captured_at: 0,
            framed_at: 0,
            popped_at: 0,
            popped_index: 0,
        }
    }

    /// `playback_start` is the first playback callback; it sets that clock's
    /// phase. A playback callback coinciding with a capture one runs first.
    fn run(mut self, playback_start: usize) -> Stages {
        for now in 1..self.heard.len() - 2 * PERIOD {
            if now >= playback_start && (now - playback_start).is_multiple_of(PERIOD) {
                for t in now + PERIOD..now + 2 * PERIOD {
                    self.heard[t] = self.playback_ring.pop_front().unwrap_or(0.0);
                }
                self.audio_thread(now);
            }
            if now.is_multiple_of(PERIOD) && now <= RUN {
                self.captured
                    .extend_from_slice(&self.input[now - PERIOD..now]);
                if (now - PERIOD..now).contains(&self.click_at) {
                    self.captured_at = now;
                }
                self.audio_thread(now);
            }
        }
        assert!(self.popped_at > 0, "click frame never played");

        // The decoded frame holding the click corresponds to input frame
        // `click_at / FRAME_SAMPLES`; searching from there isolates the
        // codec's own delay.
        let decoded_click = self.popped_index + self.click_at % FRAME_SAMPLES;
        Stages {
            capture: self.captured_at - self.click_at,
            framing: self.framed_at - self.captured_at,
            codec: delay(&self.click, &self.decoded[decoded_click..], FRAME_SAMPLES),
            jitter: self.popped_at - self.framed_at,
            total: delay(&self.click, &self.heard[self.click_at..], MAX_DELAY),
        }
    }

    fn audio_thread(&mut self, now: usize) {
        let click_frame = self.click_at / FRAME_SAMPLES;
        let Self {
            framer,
            encoder,
            jitter,
            seq,
            click_payload,
            framed_at,
            ..
        } = self;
        framer.push(&self.captured, |frame| {
            let payload = encoder.encode(frame);
            if *seq as usize == click_frame {
                *framed_at = now;
                *click_payload = payload.to_vec();
            }
            jitter.push(*seq, payload);
            *seq = seq.wrapping_add(1);
        });
        self.captured.clear();

        while self.playback_ring.len() < PERIOD {
            let mut frame = [0.0; FRAME_SAMPLES];
            let Self {
                decoder,
                click_payload,
                popped_at,
                popped_index,
                decoded,
                ..
            } = self;
            let mut decode = |p: Option<&[u8]>, f: &mut Frame| {
                assert!(p.is_some(), "no frame may be lost without a network");
                if p == Some(&click_payload[..]) {
                    *popped_at = now;
                    *popped_index = decoded.len();
                }
                decoder.decode(p, f);
            };
            self.jitter.pop(&mut decode, &mut frame);
            self.decoded.extend_from_slice(&frame);
            let mut mixed = [0.0; FRAME_SAMPLES];
            self.mixer.mix([(&frame, 1.0)], &mut mixed);
            self.playback_ring.extend(mixed);
        }
    }
}

/// Lag in `0..max_lag` at which `signal` best matches `reference`. Panics
/// unless the match is clear, so a lost click cannot pass as a small delay.
fn delay(reference: &[f32], signal: &[f32], max_lag: usize) -> usize {
    let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>();
    let (lag, corr) = (0..max_lag)
        .map(|lag| {
            let corr: f32 = reference
                .iter()
                .zip(&signal[lag..])
                .map(|(a, b)| a * b)
                .sum();
            (lag, corr)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    let matched = &signal[lag..lag + reference.len()];
    let normalized = corr / (energy(reference) * energy(matched)).sqrt();
    assert!(
        normalized > 0.5,
        "click not found (correlation {normalized})"
    );
    lag
}

/// Hann-windowed linear chirp, 200 Hz to 4 kHz: unique at every lag, so the
/// correlation peak is unambiguous, and band-limited enough for Opus at
/// 32 kbit/s to keep the waveform.
fn click_signal() -> Vec<f32> {
    let (f0, f1) = (200.0, 4000.0);
    let len = CLICK_LEN as f32;
    let rate = SAMPLE_RATE as f32;
    (0..CLICK_LEN)
        .map(|i| {
            let t = i as f32 / rate;
            let phase = 2.0 * PI * (f0 * t + (f1 - f0) * t * t * rate / (2.0 * len));
            let window = 0.5 - 0.5 * (2.0 * PI * i as f32 / len).cos();
            0.5 * window * phase.sin()
        })
        .collect()
}
