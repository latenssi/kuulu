//! Voice: the audio pipeline wired to a [`Client`].
//!
//! [`Voice`] lives on the real-time audio thread. [`Voice::capture`] encodes
//! each captured 10 ms frame and [`Voice::play`] produces the next 10 ms of
//! received voice, mixed over senders; neither allocates, locks nor blocks.
//! Packets cross to the network through rtrb rings: a sender thread, woken
//! per frame, calls [`Client::send`] (which locks and allocates), and a tokio
//! task copies received packets in.
//!
//! Every packet also carries the previous frame ([`flags::REDUNDANT`],
//! layout in [`redundancy`]). A lost frame is played from the next packet's
//! copy when that arrives in time, and concealed with Opus PLC otherwise.

use crate::Client;
use kuulu_audio::{Decoder, Encoder, FRAME_SAMPLES, Frame, JitterBuffer, Mixer};
use kuulu_proto::{HEADER_LEN, MAX_VOICE_PACKET, VoiceHeader, flags, redundancy};
use std::io;
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::Duration;

/// Senders mixed at once. Decoder state is allocated up front for each.
const MAX_PEERS: usize = 16;
/// A sender silent this long (5 s of frames) frees its slot.
const FORGET_AFTER: u32 = 500;
const MAX_PAYLOAD: usize = MAX_VOICE_PACKET - HEADER_LEN;
/// Outgoing packets waiting for the sender thread. It drains them at once,
/// so this only absorbs scheduling hiccups.
const OUTGOING: usize = 8;
/// Received packets waiting for the next [`Voice::play`].
const INCOMING: usize = 64;

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
struct Packet {
    header: VoiceHeader,
    len: usize,
    payload: [u8; MAX_PAYLOAD],
}

struct Peer {
    id: u32,
    /// Frames since the last packet; [`FORGET_AFTER`] or more: slot free.
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
    peers: Box<[Peer]>,
    mixer: Mixer,
    stats: Stats,
    receiver: Option<tokio::task::AbortHandle>,
}

impl Voice {
    /// Starts sending and receiving voice through `client`. Must be called
    /// within a tokio runtime; the network side stops when this is dropped.
    pub fn start(client: Arc<Client>) -> io::Result<Self> {
        let (outgoing, mut to_send) = rtrb::RingBuffer::<Packet>::new(OUTGOING);
        let (mut received, incoming) = rtrb::RingBuffer::<Packet>::new(INCOMING);
        let send_client = client.clone();
        let sender = thread::Builder::new()
            .name("kuulu-voice-send".into())
            .spawn(move || {
                loop {
                    while let Ok(p) = to_send.pop() {
                        let h = p.header;
                        send_client.send(h.flags, h.seq, h.timestamp, &p.payload[..p.len]);
                    }
                    if to_send.is_abandoned() {
                        return;
                    }
                    // Woken per frame by `capture`; the timeout notices a drop.
                    thread::park_timeout(Duration::from_millis(100));
                }
            })?;
        let receiver = tokio::spawn(async move {
            while let Ok(p) = client.recv().await {
                let len = p.payload.len();
                if len <= MAX_PAYLOAD {
                    let mut packet = Packet {
                        header: p.header,
                        len,
                        payload: [0; MAX_PAYLOAD],
                    };
                    packet.payload[..len].copy_from_slice(&p.payload);
                    // Full only if the audio thread stalls; the jitter buffer
                    // skips ahead once it resumes.
                    let _ = received.push(packet);
                }
            }
        });
        let mut voice = Self::new(outgoing, sender.thread().clone(), incoming);
        voice.receiver = Some(receiver.abort_handle());
        Ok(voice)
    }

    fn new(
        outgoing: rtrb::Producer<Packet>,
        sender: Thread,
        incoming: rtrb::Consumer<Packet>,
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
            peers,
            mixer: Mixer::new(),
            stats: Stats::default(),
            receiver: None,
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
        };
        let previous = &self.previous[..self.previous_len];
        if let Some(len) = redundancy::write(current, previous, &mut packet.payload) {
            packet.len = len;
            // Full means the sender thread is stuck: drop rather than queue.
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
        let Some(peer) = self.peer(packet.header.sender) else {
            return;
        };
        let (seq, payload) = (packet.header.seq, &packet.payload[..packet.len]);
        if packet.header.flags & flags::REDUNDANT == 0 {
            peer.jitter.push(seq, payload);
        } else if let Some((current, previous)) = redundancy::split(payload) {
            peer.jitter.push(seq, current);
            if !previous.is_empty() {
                peer.jitter.push_redundant(seq.wrapping_sub(1), previous);
            }
        }
        peer.idle = 0;
    }

    /// The sender's slot, claiming the longest idle free one for a new
    /// sender. `None` if all are in use.
    fn peer(&mut self, id: u32) -> Option<&mut Peer> {
        let active = |p: &Peer| p.idle < FORGET_AFTER;
        if let Some(i) = self.peers.iter().position(|p| active(p) && p.id == id) {
            return Some(&mut self.peers[i]);
        }
        let peer = self.peers.iter_mut().find(|p| !active(p))?;
        peer.id = id;
        peer.jitter = JitterBuffer::new();
        peer.decoder.reset();
        Some(peer)
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        if let Some(receiver) = &self.receiver {
            receiver.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::{AllocDisabler, assert_no_alloc};

    #[global_allocator]
    static ALLOC: AllocDisabler = AllocDisabler;

    /// Two senders' streams, one lost packet each; the network is the test.
    #[test]
    fn audio_thread_does_not_allocate() {
        let voice = || {
            let (out_tx, out_rx) = rtrb::RingBuffer::new(OUTGOING);
            let (in_tx, in_rx) = rtrb::RingBuffer::new(INCOMING);
            (Voice::new(out_tx, thread::current(), in_rx), out_rx, in_tx)
        };
        let (mut a, mut a_sent, _) = voice();
        let (mut b, mut b_sent, _) = voice();
        let (mut listener, _, mut network) = voice();
        let mut out = [0.0; FRAME_SAMPLES];
        assert_no_alloc(|| {
            for i in 0..300u32 {
                let tone: Frame =
                    std::array::from_fn(|n| ((i as usize * FRAME_SAMPLES + n) as f32 * 0.05).sin());
                a.capture(&tone);
                b.capture(&tone);
                for (id, sent) in [(1, &mut a_sent), (2, &mut b_sent)] {
                    let mut packet = sent.pop().unwrap();
                    packet.header.sender = id;
                    if packet.header.seq != 100 {
                        network.push(packet).ok().unwrap();
                    }
                }
                listener.play(&mut out);
            }
        });
        let stats = listener.stats();
        assert_eq!(stats.decoded + stats.concealed, 2 * 300);
        assert!((2..10).contains(&stats.concealed), "{stats:?}");
        assert!(out.iter().any(|&s| s.abs() > 0.1));
    }
}
