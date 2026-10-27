//! Loss recovery compared on a simulated clock: redundancy (restricted
//! low-delay Opus, each packet also carrying the previous frame) against
//! Opus VOIP mode with in-band FEC (docs/PLAN.md, "Pakettihäviöt").
//!
//! Synthetic speech goes through the real encoder and jitter buffer. Packets
//! cross two netem hops (talker to server, server to listener) with seeded
//! loss and jitter; the listener plays one frame every 10 ms. A missing
//! frame is recovered from the next packet if that arrives before the
//! frame's turn and actually carries it: a redundant copy, or Opus LBRR
//! (FEC) data, which libopus only adds when told to expect enough loss. FEC
//! is swept over the encoder's expected-loss setting to find where it
//! activates.

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
/// Expected-loss settings (`OPUS_SET_PACKET_LOSS_PERC`) swept for FEC.
const FEC_LOSS_PERC: [i32; 5] = [2, 5, 6, 10, 20];
/// Bytes per packet on the wire besides the Opus payload: Kuulu header 12,
/// IPv4 20, UDP 8, QUIC short header about 11 (flags, 8-byte connection id,
/// packet number), DATAGRAM frame type 1, AEAD tag 16. IPv6 adds 20.
const WIRE_OVERHEAD: usize = 12 + 20 + 8 + 11 + 1 + 16;
/// Jitter buffer payload tags: how to decode the stored bytes.
const PRIMARY: u8 = 0;
const RECOVERY: u8 = 1;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Redundancy,
    /// VOIP mode with in-band FEC, told to expect this much loss (%).
    Fec(i32),
}

struct Report {
    /// Opus payload bitrate, and with [`WIRE_OVERHEAD`].
    kbps: f64,
    wire_kbps: f64,
    lookahead: usize,
    /// Share of packets that carry FEC data for the frame before.
    lbrr: f64,
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
    println!("Percentages are of frames played. 'recovered' counts only frames played from");
    println!("a redundant copy or real FEC (LBRR) data. 'wire' adds {WIRE_OVERHEAD} B per packet");
    println!("(Kuulu header, IPv4, UDP, QUIC). Quality is a waveform proxy: against the source");
    println!("it penalises SILK, so loss damage is measured against the same mode, with the");
    println!("same encoder settings, over a lossless (lan) network.");
    println!(
        "{:<12} {:<10} {:>6} {:>6} {:>9} {:>6} {:>8} {:>9} {:>9} {:>9} {:>10} {:>10} {:>7}",
        "profile",
        "mode",
        "kbit/s",
        "wire",
        "lookahead",
        "lbrr",
        "net loss",
        "recovered",
        "concealed",
        "jb target",
        "SNR src",
        "SNR lan",
        "poor"
    );
    let mut modes = vec![("redundancy".to_string(), Mode::Redundancy)];
    modes.extend(FEC_LOSS_PERC.map(|p| (format!("fec@{p}%"), Mode::Fec(p))));
    for (name, profile) in [
        ("home", Profile::HOME),
        ("poor-mobile", Profile::POOR_MOBILE),
    ] {
        for (mode_name, mode) in &modes {
            let r = simulate(&source, *mode, profile);
            let reference = simulate(&source, *mode, Profile::LAN);
            let absolute = compare(&source, &r.output, r.lookahead);
            let damage = compare(&reference.output, &r.output, 0);
            let pct = |n: usize| 100.0 * n as f64 / r.played as f64;
            println!(
                "{name:<12} {mode_name:<10} {:>6.1} {:>6.1} {:>6.1} ms {:>5.0}% {:>7.2}% {:>8.2}% {:>8.2}% {:>6.2} fr {:>7.1} dB {:>7.1} dB {:>6.2}%",
                r.kbps,
                r.wire_kbps,
                r.lookahead as f64 * 1000.0 / f64::from(SAMPLE_RATE),
                100.0 * r.lbrr,
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

fn has_lbrr(packet: &[u8]) -> bool {
    // SAFETY: libopus reads at most `len` bytes from `packet`.
    unsafe { opusic_sys::opus_packet_has_lbrr(packet.as_ptr(), packet.len() as i32) == 1 }
}

/// Runs `mode` over `network`.
fn simulate(source: &[f32], mode: Mode, network: Profile) -> Report {
    let frames: &[Frame] = source.as_chunks().0;
    // Packets as (arrival, seq, payload), sent one per frame.
    let mut packets = Vec::new();
    let (mut bytes, mut lbrr) = (0, 0);
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
                lbrr += usize::from(!previous.is_empty());
                send(seq, payload[..len].to_vec());
                previous = current;
            }
            LOW_DELAY_LOOKAHEAD
        }
        Mode::Fec(loss_perc) => {
            let mut encoder =
                opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).unwrap();
            encoder.set_bitrate(Bitrate::Bits(FEC_BITRATE)).unwrap();
            encoder.set_inband_fec(true).unwrap();
            encoder.set_packet_loss_perc(loss_perc).unwrap();
            for (seq, frame) in frames.iter().enumerate() {
                let packet = encoder
                    .encode_vec_float(frame, kuulu_audio::MAX_PAYLOAD)
                    .unwrap();
                lbrr += usize::from(has_lbrr(&packet));
                send(seq, packet);
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
            let (current, recovery) = match mode {
                Mode::Redundancy => redundancy::split(payload).unwrap(),
                // Without LBRR data a FEC decode is plain PLC: nothing to store.
                Mode::Fec(_) if has_lbrr(payload) => (&payload[..], &payload[..]),
                Mode::Fec(_) => (&payload[..], &[][..]),
            };
            jitter.push(*seq, &tagged(PRIMARY, current));
            if !recovery.is_empty() {
                jitter.push_redundant(previous, &tagged(RECOVERY, recovery));
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
                    decoder.decode_float(p, out, matches!(mode, Mode::Fec(_)))
                }
            };
            assert_eq!(decoded.unwrap(), FRAME_SAMPLES);
        };
        jitter.pop(&mut decode, out);
        target_sum += jitter.target();
    }

    let kbps = |bytes: usize| bytes as f64 * 8.0 / SECONDS as f64 / 1000.0;
    Report {
        kbps: kbps(bytes),
        wire_kbps: kbps(bytes + frames.len() * WIRE_OVERHEAD),
        lookahead,
        lbrr: lbrr as f64 / frames.len() as f64,
        lost,
        recovered,
        concealed,
        played,
        mean_target: target_sum as f64 / frames.len() as f64,
        output,
    }
}
