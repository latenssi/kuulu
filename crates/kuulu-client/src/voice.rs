//! Voice: the audio pipeline wired to a [`Client`].
//!
//! [`Voice`] lives on the real-time audio thread. [`Voice::capture`] encodes
//! each captured 10 ms frame and [`Voice::play`] produces the next 10 ms of
//! received voice, mixed over senders; neither allocates, locks nor waits.
//! Packets cross to the network through rtrb rings: `capture` wakes a sender
//! thread with `Thread::unpark` (a non-blocking futex-style wake, no lock or
//! allocation), which calls [`Client::send`] (which locks and allocates), and
//! a tokio task copies received packets in.
//!
//! Every packet also carries the previous frame ([`flags::REDUNDANT`],
//! layout in [`redundancy`]). A lost frame is played from the next packet's
//! copy when that arrives in time, and concealed with Opus PLC otherwise.

use crate::{Client, Event};
use kuulu_audio::{
    Decoder, Encoder, FRAME_SAMPLES, Frame, JitterBuffer, Mixer, STOP_AFTER_MISSING,
};
use kuulu_proto::{HEADER_LEN, MAX_VOICE_PACKET, VoiceHeader, flags, redundancy};
use std::io;
use std::sync::Arc;
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

/// Senders mixed at once. Decoder state is allocated up front for each.
const MAX_PEERS: usize = 16;
/// Frames of silence after which a sender is no longer decoded or mixed
/// (5 s); its slot is free.
const FORGET_AFTER: u32 = 500;
const MAX_PAYLOAD: usize = MAX_VOICE_PACKET - HEADER_LEN;
/// Outgoing packets waiting for the sender thread. It takes them at once,
/// so this only absorbs scheduling hiccups.
const OUTGOING: usize = 8;
/// An outgoing packet older than one frame is stale: the next one, which
/// carries this frame as redundancy, is due or already queued.
const MAX_SEND_AGE: Duration = Duration::from_millis(10);
/// Received packets waiting for the next [`Voice::play`].
const INCOMING: usize = 64;
/// Session ids of members who left, waiting for the next [`Voice::play`].
const LEFT: usize = 64;

/// Counters since start; jitter figures are the largest over active senders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames decoded from received packets, including redundant copies.
    pub decoded: u64,
    /// Frames concealed by Opus PLC: lost, late, or inserted to grow delay.
    pub concealed: u64,
    /// Jitter buffer delay target, frames.
    pub jitter_target: usize,
    /// Frames buffered after the latest [`Voice::play`].
    pub jitter_depth: usize,
}

/// A voice packet in a ring: fixed size, so moving it never allocates.
#[derive(Clone)]
struct Packet {
    header: VoiceHeader,
    len: usize,
    payload: [u8; MAX_PAYLOAD],
    /// When it was captured (outgoing) or received (incoming).
    at: Instant,
}

struct Peer {
    id: u32,
    /// Frames since the last well-formed packet.
    idle: u32,
    jitter: JitterBuffer,
    decoder: Decoder,
    frame: Frame,
}

pub struct Voice {
    encoder: Encoder,
    seq: u16,
    timestamp: u32,
    previous: [u8; u8::MAX as usize],
    previous_len: usize,
    outgoing: rtrb::Producer<Packet>,
    sender: Thread,
    incoming: rtrb::Consumer<Packet>,
    left: rtrb::Consumer<u32>,
    peers: Box<[Peer]>,
    mixer: Mixer,
    stats: Stats,
    /// Network tasks, stopped on drop.
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Voice {
    /// Starts sending and receiving voice through `client`. Must be called
    /// within a tokio runtime; the network side stops when this is dropped.
    pub fn start(client: Arc<Client>) -> io::Result<Self> {
        let (outgoing, to_send) = rtrb::RingBuffer::<Packet>::new(OUTGOING);
        let (mut received, incoming) = rtrb::RingBuffer::<Packet>::new(INCOMING);
        let (mut leaving, left) = rtrb::RingBuffer::<u32>::new(LEFT);
        let send_client = client.clone();
        let sender = spawn_sender(to_send, move |p| {
            let h = p.header;
            send_client.send(h.flags, h.seq, h.timestamp, &p.payload[..p.len]);
        })?;
        let mut events = client.events();
        let members = tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match events.recv().await {
                    Ok(Event::MemberLeft(m)) => {
                        let _ = leaving.push(m.session_id);
                    }
                    // Missed leaves are caught by idle eviction instead.
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                }
            }
        });
        let receiver = tokio::spawn(async move {
            while let Ok(p) = client.recv().await {
                let len = p.payload.len();
                if len <= MAX_PAYLOAD {
                    let mut packet = Packet {
                        header: p.header,
                        len,
                        payload: [0; MAX_PAYLOAD],
                        at: Instant::now(),
                    };
                    packet.payload[..len].copy_from_slice(&p.payload);
                    // Full only if the audio thread stalls; the jitter buffer
                    // skips ahead once it resumes.
                    let _ = received.push(packet);
                }
            }
        });
        let mut voice = Self::new(outgoing, sender.thread().clone(), incoming, left);
        voice.tasks = vec![receiver.abort_handle(), members.abort_handle()];
        Ok(voice)
    }

    fn new(
        outgoing: rtrb::Producer<Packet>,
        sender: Thread,
        incoming: rtrb::Consumer<Packet>,
        left: rtrb::Consumer<u32>,
    ) -> Self {
        let peers = (0..MAX_PEERS)
            .map(|_| Peer {
                id: 0,
                idle: FORGET_AFTER,
                jitter: JitterBuffer::new(),
                decoder: Decoder::new().expect("opus decoder"),
                frame: [0.0; FRAME_SAMPLES],
            })
            .collect();
        Self {
            encoder: Encoder::new().expect("opus encoder"),
            seq: 0,
            timestamp: 0,
            previous: [0; u8::MAX as usize],
            previous_len: 0,
            outgoing,
            sender,
            incoming,
            left,
            peers,
            mixer: Mixer::new(),
            stats: Stats::default(),
            tasks: Vec::new(),
        }
    }

    /// Encodes and sends one captured frame. Real-time safe.
    pub fn capture(&mut self, frame: &Frame) {
        let current = self.encoder.encode(frame);
        let mut packet = Packet {
            header: VoiceHeader {
                flags: flags::REDUNDANT,
                sender: 0,
                seq: self.seq,
                timestamp: self.timestamp,
            },
            len: 0,
            payload: [0; MAX_PAYLOAD],
            at: Instant::now(),
        };
        let previous = &self.previous[..self.previous_len];
        if let Some(len) = redundancy::write(current, previous, &mut packet.payload) {
            packet.len = len;
            // Full means the sender thread is stuck: drop rather than queue;
            // it discards what is queued as stale once it runs again.
            if self.outgoing.push(packet).is_ok() {
                self.sender.unpark();
            }
        }
        self.previous_len = if current.len() <= self.previous.len() {
            self.previous[..current.len()].copy_from_slice(current);
            current.len()
        } else {
            0
        };
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
    }

    /// Produces the next frame of received voice into `out`. Real-time safe.
    pub fn play(&mut self, out: &mut Frame) {
        while let Ok(id) = self.left.pop() {
            for peer in self.peers.iter_mut().filter(|p| p.id == id) {
                peer.idle = FORGET_AFTER;
            }
        }
        while let Ok(packet) = self.incoming.pop() {
            self.receive(&packet);
        }
        let Self { peers, stats, .. } = self;
        for peer in peers.iter_mut().filter(|p| p.idle < FORGET_AFTER) {
            peer.idle += 1;
            let decoder = &mut peer.decoder;
            let mut decode = |payload: Option<&[u8]>, frame: &mut Frame| {
                match payload {
                    Some(_) => stats.decoded += 1,
                    None => stats.concealed += 1,
                }
                decoder.decode(payload, frame);
            };
            peer.jitter.pop(&mut decode, &mut peer.frame);
        }
        let active = self.peers.iter().filter(|p| p.idle < FORGET_AFTER);
        self.mixer.mix(active.map(|p| (&p.frame, 1.0)), out);
    }

    pub fn stats(&self) -> Stats {
        let active = || self.peers.iter().filter(|p| p.idle < FORGET_AFTER);
        Stats {
            jitter_target: active().map(|p| p.jitter.target()).max().unwrap_or(0),
            jitter_depth: active().map(|p| p.jitter.depth()).max().unwrap_or(0),
            ..self.stats
        }
    }

    fn receive(&mut self, packet: &Packet) {
        let (seq, payload) = (packet.header.seq, &packet.payload[..packet.len]);
        let (current, previous) = if packet.header.flags & flags::REDUNDANT == 0 {
            (payload, &[][..])
        } else {
            match redundancy::split(payload) {
                Some(split) => split,
                None => return,
            }
        };
        // Malformed packets neither claim nor keep a slot.
        if current.is_empty() {
            return;
        }
        let Some(peer) = self.peer(packet.header.sender) else {
            return;
        };
        peer.jitter.push(seq, current);
        if !previous.is_empty() {
            peer.jitter.push_redundant(seq.wrapping_sub(1), previous);
        }
        peer.idle = 0;
    }

    /// The sender's slot. A new sender takes a slot freed by a member who
    /// left, or else the longest idle one if that stream has stopped (see
    /// [`STOP_AFTER_MISSING`]); `None` if all senders are still talking.
    fn peer(&mut self, id: u32) -> Option<&mut Peer> {
        if let Some(i) = self
            .peers
            .iter()
            .position(|p| p.idle < FORGET_AFTER && p.id == id)
        {
            return Some(&mut self.peers[i]);
        }
        let peer = self.peers.iter_mut().max_by_key(|p| p.idle)?;
        if peer.idle <= STOP_AFTER_MISSING {
            return None;
        }
        peer.id = id;
        peer.jitter = JitterBuffer::new();
        peer.decoder.reset();
        Some(peer)
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Runs the sender thread: on every wake, [`send_newest`]. Exits once the
/// producer side is dropped.
fn spawn_sender(
    mut ring: rtrb::Consumer<Packet>,
    mut send: impl FnMut(&Packet) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("kuulu-voice-send".into())
        .spawn(move || {
            loop {
                send_newest(&mut ring, &mut send);
                if ring.is_abandoned() {
                    return;
                }
                // Woken per frame by `capture`; the timeout notices a drop.
                thread::park_timeout(Duration::from_millis(100));
            }
        })
}

/// Empties the ring and sends only the newest packet, which also carries
/// the frame before it, and only if it is not stale: after a stall, queued
/// audio is dropped rather than sent late.
fn send_newest(ring: &mut rtrb::Consumer<Packet>, send: &mut impl FnMut(&Packet)) {
    let mut newest = None;
    while let Ok(p) = ring.pop() {
        newest = Some(p);
    }
    if let Some(p) = newest
        && p.at.elapsed() <= MAX_SEND_AGE
    {
        send(&p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netem::{Link, Profile};
    use assert_no_alloc::{AllocDisabler, assert_no_alloc};
    use std::sync::mpsc;

    #[global_allocator]
    static ALLOC: AllocDisabler = AllocDisabler;

    /// A Voice whose network side is the test: returns it with the ring of
    /// packets it sends, the ring that delivers packets to it and the one
    /// that reports members leaving.
    fn voice() -> (
        Voice,
        rtrb::Consumer<Packet>,
        rtrb::Producer<Packet>,
        rtrb::Producer<u32>,
    ) {
        let (out_tx, out_rx) = rtrb::RingBuffer::new(OUTGOING);
        let (in_tx, in_rx) = rtrb::RingBuffer::new(INCOMING);
        let (left_tx, left_rx) = rtrb::RingBuffer::new(LEFT);
        let voice = Voice::new(out_tx, thread::current(), in_rx, left_rx);
        (voice, out_rx, in_tx, left_tx)
    }

    fn tone(i: usize) -> Frame {
        std::array::from_fn(|n| ((i * FRAME_SAMPLES + n) as f32 * 0.05).sin())
    }

    fn from(id: u32, packet: &Packet) -> Packet {
        let mut packet = packet.clone();
        packet.header.sender = id;
        packet
    }

    fn holders(voice: &Voice) -> Vec<u32> {
        let mut ids: Vec<_> = (voice.peers.iter())
            .filter(|p| p.idle < FORGET_AFTER)
            .map(|p| p.id)
            .collect();
        ids.sort();
        ids
    }

    /// 17 senders, one more than there are slots. Sender 16 stops at frame
    /// 150 and sender 17 takes its slot. Sender 1's packets arrive two
    /// frames late; 100 is lost, but 101 comes a frame early and fills it.
    #[test]
    fn audio_thread_does_not_allocate() {
        let (mut talker, mut sent, ..) = voice();
        let (mut listener, _, mut network, _) = voice();
        let mut delayed: [Option<Packet>; 4] = std::array::from_fn(|_| None);
        let mut out = [0.0; FRAME_SAMPLES];
        assert_no_alloc(|| {
            for i in 0..300 {
                talker.capture(&tone(i));
                let packet = sent.pop().unwrap();
                match i {
                    100 => {}
                    101 => delayed[(i + 1) % 4] = Some(from(1, &packet)),
                    _ => delayed[(i + 2) % 4] = Some(from(1, &packet)),
                }
                if let Some(p) = delayed[i % 4].take() {
                    network.push(p).ok().unwrap();
                }
                for id in (2..=16).filter(|&id| id != 16 || i < 150) {
                    network.push(from(id, &packet)).ok().unwrap();
                }
                // Once sender 1 holds the last free slot.
                if i > 2 {
                    network.push(from(17, &packet)).ok().unwrap();
                }
                listener.play(&mut out);
            }
        });
        let stats = listener.stats();
        // Concealed: only sender 16's last frames, until its stream stops.
        assert_eq!(stats.concealed, u64::from(STOP_AFTER_MISSING));
        // Senders 2-15 throughout, 16 until it stopped, 1 from its third
        // frame, and 17 from 7 frames after 16 stopped.
        assert_eq!(stats.decoded, 14 * 300 + 150 + 298 + 144);
        assert!(out.iter().any(|&s| s.abs() > 0.1));
    }

    #[test]
    fn reconnecting_senders_take_over_stopped_slots() {
        let (mut talker, mut sent, ..) = voice();
        let (mut listener, _, mut network, _) = voice();
        let mut out = [0.0; FRAME_SAMPLES];
        let mut tick = |ids: &[u32], listener: &mut Voice| {
            talker.capture(&tone(0));
            let packet = sent.pop().unwrap();
            for &id in ids {
                network.push(from(id, &packet)).ok().unwrap();
            }
            listener.play(&mut out);
        };
        let old: Vec<u32> = (1..=16).collect();
        for _ in 0..20 {
            tick(&old, &mut listener);
        }
        assert_eq!(holders(&listener), old);
        // Everyone reconnects with a new session id. Until the old streams
        // have stopped, there is no room; then the new ids take over.
        let new: Vec<u32> = (101..=116).collect();
        for _ in 0..STOP_AFTER_MISSING {
            tick(&new, &mut listener);
            assert_eq!(holders(&listener), old);
        }
        tick(&new, &mut listener);
        assert_eq!(holders(&listener), new);
    }

    #[test]
    fn member_who_left_frees_the_slot_at_once() {
        let (mut talker, mut sent, ..) = voice();
        let (mut listener, _, mut network, mut leaving) = voice();
        let mut out = [0.0; FRAME_SAMPLES];
        let old: Vec<u32> = (1..=16).collect();
        for i in 0..20 {
            talker.capture(&tone(i));
            let packet = sent.pop().unwrap();
            for &id in &old {
                network.push(from(id, &packet)).ok().unwrap();
            }
            listener.play(&mut out);
        }
        // Member 3 leaves and 17 starts talking right away, while the others
        // are still talking.
        leaving.push(3).unwrap();
        talker.capture(&tone(20));
        let packet = sent.pop().unwrap();
        for id in old.iter().copied().filter(|&id| id != 3).chain([17]) {
            network.push(from(id, &packet)).ok().unwrap();
        }
        listener.play(&mut out);
        let mut expected: Vec<u32> = old.into_iter().filter(|&id| id != 3).collect();
        expected.push(17);
        assert_eq!(holders(&listener), expected);
    }

    #[test]
    fn malformed_packets_do_not_keep_a_slot() {
        let (mut listener, _, mut network, _) = voice();
        let bad = Packet {
            header: VoiceHeader {
                flags: flags::REDUNDANT,
                sender: 7,
                seq: 0,
                timestamp: 0,
            },
            len: 2,
            payload: [9; MAX_PAYLOAD], // previous_len 9 > payload
            at: Instant::now(),
        };
        network.push(bad.clone()).ok().unwrap();
        listener.play(&mut [0.0; FRAME_SAMPLES]);
        assert!(holders(&listener).is_empty());
    }

    #[test]
    fn stalled_sender_sends_fresh_audio_not_old() {
        let (mut talker, mut ring, ..) = voice();
        let mut sent = Vec::new();
        for i in 0..10 {
            talker.capture(&tone(i)); // ring holds 8: the last 2 are dropped
        }
        thread::sleep(MAX_SEND_AGE * 3);
        send_newest(&mut ring, &mut |p| sent.push(p.header.seq));
        assert!(sent.is_empty(), "stale audio sent: {sent:?}");
        talker.capture(&tone(10));
        talker.capture(&tone(11));
        send_newest(&mut ring, &mut |p| sent.push(p.header.seq));
        assert_eq!(sent, [11]);
    }

    /// From `capture` to the sender thread's send call, frame by frame.
    #[test]
    fn hop_to_sender_thread_is_short() {
        let (outgoing, ring) = rtrb::RingBuffer::new(OUTGOING);
        let (tx, rx) = mpsc::channel();
        let sender = spawn_sender(ring, move |p| tx.send(p.at.elapsed()).unwrap()).unwrap();
        let (_, incoming) = rtrb::RingBuffer::new(INCOMING);
        let (_, left) = rtrb::RingBuffer::new(LEFT);
        let mut voice = Voice::new(outgoing, sender.thread().clone(), incoming, left);
        for i in 0..300 {
            voice.capture(&tone(i));
            thread::sleep(Duration::from_millis(10));
        }
        drop(voice);
        sender.join().unwrap();
        let mut hops: Vec<Duration> = rx.iter().collect();
        hops.sort();
        let at = |q: usize| hops[(hops.len() - 1) * q / 100];
        println!(
            "capture to send: {} sent of 300, p50 {:?}, p99 {:?}, max {:?}",
            hops.len(),
            at(50),
            at(99),
            at(100)
        );
        assert!(hops.len() >= 290 && at(50) < Duration::from_millis(2));
    }

    /// What one simulated run measured at the listener.
    #[derive(Debug)]
    struct Sim {
        max_target: usize,
        /// Share of frames played with a target above 2.
        over_2: f64,
        p95_depth: usize,
        mean_depth: f64,
        concealed: f64,
    }

    /// The talker's frames cross two hops of `profile` (talker to server,
    /// server to listener) drawn by netem's own [`Link`], on a simulated
    /// clock: no runner scheduling, the same result on every machine. The
    /// listener plays a frame every 10 ms, `phase` after each capture.
    fn simulate(source: &[f32], profile: Profile, seed: u64, phase: Duration) -> Sim {
        let (mut talker, mut sent, ..) = voice();
        let (mut listener, _, mut network, _) = voice();
        let (mut up, mut down) = (Link::new(profile, seed), Link::new(profile, seed + 1));
        let mut in_flight: Vec<(Duration, Packet)> = Vec::new();
        let (mut max_target, mut over_2, mut depths) = (0, 0, Vec::new());
        let mut out = [0.0; FRAME_SAMPLES];
        for (i, frame) in source.as_chunks().0.iter().enumerate() {
            let now = Duration::from_millis(10) * i as u32;
            talker.capture(frame);
            let packet = from(1, &sent.pop().unwrap());
            if let (Some(a), Some(b)) = (up.delay(), down.delay()) {
                in_flight.push((now + a + b, packet));
            }
            in_flight.sort_by_key(|p| p.0);
            let due = in_flight.partition_point(|p| p.0 <= now + phase);
            for (_, packet) in in_flight.drain(..due) {
                network.push(packet).ok().unwrap();
            }
            listener.play(&mut out);
            let s = listener.stats();
            max_target = max_target.max(s.jitter_target);
            over_2 += usize::from(s.jitter_target > 2);
            depths.push(s.jitter_depth);
        }
        let s = listener.stats();
        let mean_depth = depths.iter().sum::<usize>() as f64 / depths.len() as f64;
        depths.sort();
        Sim {
            max_target,
            over_2: over_2 as f64 / depths.len() as f64,
            p95_depth: depths[depths.len() * 95 / 100],
            mean_depth,
            concealed: s.concealed as f64 / (s.decoded + s.concealed) as f64,
        }
    }

    /// M3 criterion (docs/PLAN.md): over the home profile the jitter buffer
    /// stays within two frames (95th percentile of depth). Ten seeds of 60 s,
    /// playout phases spread over a frame.
    #[test]
    fn home_profile_keeps_jitter_buffer_within_two_frames() {
        let source = kuulu_audio::quality::speech(60 * kuulu_audio::SAMPLE_RATE as usize);
        let runs: Vec<_> = (1..=10u64)
            .map(|seed| {
                let phase = Duration::from_millis(seed % 10);
                let sim = simulate(&source, Profile::HOME, seed * 2, phase);
                println!(
                    "seed {seed}, phase {phase:?}: target max {} (above 2 for {:.3} of \
                     frames), depth p95 {} mean {:.2}, concealed {:.4}",
                    sim.max_target, sim.over_2, sim.p95_depth, sim.mean_depth, sim.concealed
                );
                sim
            })
            .collect();
        for sim in &runs {
            // The criterion (board, 27.10.2026): p95 depth at most 2 frames.
            // Two ±5 ms hops can need a third frame at some playout phases;
            // the target bounds only catch regressions.
            assert!(sim.p95_depth <= 2, "{runs:#?}");
            assert!(sim.max_target <= 3 && sim.over_2 <= 0.10, "{runs:#?}");
        }
    }
}
