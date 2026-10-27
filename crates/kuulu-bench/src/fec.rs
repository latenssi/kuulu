//! Loss recovery compared on a simulated clock: redundancy (restricted
//! low-delay Opus, each packet also carrying the previous frame) against
//! Opus VOIP mode with in-band FEC (docs/PLAN.md, "Pakettihäviöt").
//!
//! Synthetic speech goes through the real encoder and jitter buffer. Packets
//! cross two netem hops (talker to server, server to listener) with seeded
//! loss and jitter; the listener plays one frame every 10 ms. A missing
//! frame is recovered from the next packet (redundant copy, or Opus FEC
//! data) if that arrives before the frame's turn, and concealed otherwise.

use kuulu_audio::quality::{compare, speech};
use kuulu_audio::{Encoder, FRAME_SAMPLES, Frame, JitterBuffer, SAMPLE_RATE};
use kuulu_client::netem::{Link, Profile};
use kuulu_proto::redundancy;
use opus::{Application, Bitrate, Channels};
use std::process::ExitCode;
use std::time::Duration;

const SECONDS: usize = 60;
const SEED: u64 = 1;
/// Playback ticks fall this far into each send period.
const PLAYOUT_PHASE: Duration = Duration::from_millis(5);
/// Restricted low-delay look-ahead in samples.
const LOW_DELAY_LOOKAHEAD: usize = 120;
const FEC_BITRATE: i32 = 32_000;
/// Jitter buffer payload tags: how to decode the stored bytes.
const PRIMARY: u8 = 0;
const RECOVERY: u8 = 1;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Redundancy,
    Fec,
}

struct Report {
    kbps: f64,
    lookahead: usize,
    lost: usize,
    recovered: usize,
    concealed: usize,
    played: usize,
    mean_target: f64,
    output: Vec<f32>,
}

pub fn run() -> ExitCode {
    let source = speech(SECONDS * SAMPLE_RATE as usize);
    println!("{SECONDS} s synthetic speech, two netem hops, seed {SEED}.");
    println!("Percentages are of frames played. Quality is a waveform proxy: against the");
    println!("source it penalises SILK, so loss damage is measured against the same mode,");
    println!("with the same encoder settings, over a lossless (lan) network.");
    println!(
        "{:<12} {:<11} {:>6} {:>9} {:>8} {:>9} {:>9} {:>9} {:>11} {:>11} {:>8}",
        "profile",
        "mode",
        "kbit/s",
        "lookahead",
        "net loss",
        "recovered",
        "concealed",
        "jb target",
        "SNR vs src",
        "SNR vs lan",
        "poor"
    );
    let modes = [("redundancy", Mode::Redundancy), ("voip+fec", Mode::Fec)];
    for (name, profile) in [
        ("lan", Profile::LAN),
        ("home", Profile::HOME),
        ("poor-mobile", Profile::POOR_MOBILE),
    ] {
        for (mode_name, mode) in modes {
            let r = simulate(&source, mode, profile, profile);
            let reference = simulate(&source, mode, profile, Profile::LAN);
            let absolute = compare(&source, &r.output, r.lookahead);
            let damage = compare(&reference.output, &r.output, 0);
            let pct = |n: usize| 100.0 * n as f64 / r.played as f64;
            println!(
                "{name:<12} {mode_name:<11} {:>6.1} {:>6.1} ms {:>7.2}% {:>8.2}% {:>8.2}% {:>6.2} fr {:>8.1} dB {:>8.1} dB {:>7.2}%",
                r.kbps,
                r.lookahead as f64 * 1000.0 / f64::from(SAMPLE_RATE),
                pct(r.lost),
                pct(r.recovered),
                pct(r.concealed),
                r.mean_target,
                absolute.snr_db,
                damage.snr_db,
                100.0 * damage.poor,
            );
        }
    }
    ExitCode::SUCCESS
}

/// Runs `mode` over `network`, with the encoder tuned for `profile`.
fn simulate(source: &[f32], mode: Mode, profile: Profile, network: Profile) -> Report {
    let frames: &[Frame] = source.as_chunks().0;
    // Packets as (arrival, seq, payload), sent one per frame.
    let mut packets = Vec::new();
    let mut bytes = 0;
    let (mut up, mut down) = (Link::new(network, SEED), Link::new(network, SEED + 1));
    let mut send = |seq: usize, payload: Vec<u8>| {
        bytes += payload.len();
        if let (Some(a), Some(b)) = (up.delay(), down.delay()) {
            let sent = Duration::from_millis(10) * seq as u32;
            packets.push((sent + a + b, seq as u16, payload));
        }
    };
    let lookahead = match mode {
        Mode::Redundancy => {
            let mut encoder = Encoder::new().unwrap();
            let mut previous = Vec::new();
            for (seq, frame) in frames.iter().enumerate() {
                let current = encoder.encode(frame).to_vec();
                let mut payload = [0; 1 + 2 * kuulu_audio::MAX_PAYLOAD];
                let len = redundancy::write(&current, &previous, &mut payload).unwrap();
                send(seq, payload[..len].to_vec());
                previous = current;
            }
            LOW_DELAY_LOOKAHEAD
        }
        Mode::Fec => {
            let mut encoder =
                opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).unwrap();
            encoder.set_bitrate(Bitrate::Bits(FEC_BITRATE)).unwrap();
            encoder.set_inband_fec(true).unwrap();
            // Tell the encoder the end-to-end loss over both hops.
            let loss = 1.0 - (1.0 - profile.loss).powi(2);
            encoder
                .set_packet_loss_perc((100.0 * loss).round() as i32)
                .unwrap();
            for (seq, frame) in frames.iter().enumerate() {
                send(
                    seq,
                    encoder
                        .encode_vec_float(frame, kuulu_audio::MAX_PAYLOAD)
                        .unwrap(),
                );
            }
            encoder.get_lookahead().unwrap() as usize
        }
    };
    let lost = frames.len() - packets.len();
    packets.sort_by_key(|p| p.0);

    let mut jitter = JitterBuffer::new();
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let (mut recovered, mut concealed, mut played, mut target_sum) = (0, 0, 0, 0);
    let mut output = vec![0.0; source.len()];
    let mut arrivals = packets.iter().peekable();
    let tagged = |tag: u8, bytes: &[u8]| [&[tag][..], bytes].concat();
    for (tick, out) in output
        .as_chunks_mut::<FRAME_SAMPLES>()
        .0
        .iter_mut()
        .enumerate()
    {
        let now = Duration::from_millis(10) * tick as u32 + PLAYOUT_PHASE;
        while let Some((_, seq, payload)) = arrivals.next_if(|p| p.0 <= now) {
            let previous = seq.wrapping_sub(1);
            match mode {
                Mode::Redundancy => {
                    let (current, redundant) = redundancy::split(payload).unwrap();
                    jitter.push(*seq, &tagged(PRIMARY, current));
                    if !redundant.is_empty() {
                        jitter.push_redundant(previous, &tagged(RECOVERY, redundant));
                    }
                }
                Mode::Fec => {
                    jitter.push(*seq, &tagged(PRIMARY, payload));
                    jitter.push_redundant(previous, &tagged(RECOVERY, payload));
                }
            }
        }
        let mut decode = |payload: Option<&[u8]>, out: &mut Frame| {
            played += 1;
            let decoded = match payload.and_then(|p| p.split_first()) {
                None => {
                    concealed += 1;
                    decoder.decode_float(&[], out, false)
                }
                Some((&PRIMARY, p)) => decoder.decode_float(p, out, false),
                Some((_, p)) => {
                    recovered += 1;
                    decoder.decode_float(p, out, mode == Mode::Fec)
                }
            };
            assert_eq!(decoded.unwrap(), FRAME_SAMPLES);
        };
        jitter.pop(&mut decode, out);
        target_sum += jitter.target();
    }

    Report {
        kbps: bytes as f64 * 8.0 / SECONDS as f64 / 1000.0,
        lookahead,
        lost,
        recovered,
        concealed,
        played,
        mean_target: target_sum as f64 / frames.len() as f64,
        output,
    }
}
