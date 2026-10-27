//! Per-sender adaptive jitter buffer.
//!
//! Transport-agnostic: the caller pushes `(seq, opus payload)` as packets
//! arrive and pops exactly one frame per 10 ms of playback. `seq` counts
//! 10 ms frames and wraps at `u16::MAX`.
//!
//! Delay policy: playback starts as soon as `target` frames are buffered
//! (target 1 = play a frame on the first tick after it arrives). A late
//! packet (one whose frame was already concealed) raises the target by one
//! frame immediately and holds playback back one frame. Other old packets
//! (duplicates, leftovers) are ignored. Copies of a frame carried by a later
//! packet (loss redundancy) go through [`JitterBuffer::push_redundant`]: they
//! fill gaps but never count as late, since their frame was lost, not late. Every 2 s without
//! a late packet the target drops by one; if the buffer stayed deeper than
//! the target for that whole window, one silent frame is dropped. The same
//! mechanism absorbs clock drift between sender and receiver devices.

use crate::{Frame, MAX_PAYLOAD};

/// Power of two, so `seq % CAPACITY` stays continuous across wraparound.
/// Above MAX_TARGET so a burst slightly deeper than the target still fits.
const CAPACITY: usize = 8;
const MAX_TARGET: usize = 6;
/// Pops per adaptation window (2 s of 10 ms frames).
const WINDOW: u32 = 200;
/// Peak below which a decoded frame may be dropped unnoticed (about -50 dBFS).
const SILENCE_PEAK: f32 = 0.003;
/// Consecutive missing frames after which the sender is treated as stopped
/// and the next packet restarts the stream. Equals the largest jitter the
/// buffer absorbs, so anything missing longer is lost rather than late.
pub const STOP_AFTER_MISSING: u32 = MAX_TARGET as u32;

struct Slot {
    filled: bool,
    seq: u16,
    len: usize,
    payload: [u8; MAX_PAYLOAD],
}

pub struct JitterBuffer {
    slots: [Slot; CAPACITY],
    /// Some packet has arrived since the last (re)start.
    received: bool,
    playing: bool,
    /// Seq played by the next pop; meaningful only while `playing`.
    next: u16,
    newest: u16,
    /// Desired delay in frames, 1..=MAX_TARGET.
    target: usize,
    /// Concealed frames still to insert ahead of `next` to grow the delay.
    stretch: usize,
    /// Bit i set: seq `next - 1 - i` was concealed because it was missing.
    /// 16 bits cover more than MAX_TARGET frames of lateness.
    concealed: u16,
    missing: u32,
    window_pops: u32,
    window_late: bool,
    window_min_depth: usize,
    drop_silent: bool,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot {
                filled: false,
                seq: 0,
                len: 0,
                payload: [0; MAX_PAYLOAD],
            }),
            received: false,
            playing: false,
            next: 0,
            newest: 0,
            target: 1,
            stretch: 0,
            concealed: 0,
            missing: 0,
            window_pops: 0,
            window_late: false,
            window_min_depth: usize::MAX,
            drop_silent: false,
        }
    }

    /// Current delay target in frames.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Stores a packet. Payloads longer than [`MAX_PAYLOAD`] are truncated.
    pub fn push(&mut self, seq: u16, payload: &[u8]) {
        if self.playing {
            let ahead = seq.wrapping_sub(self.next) as i16;
            if ahead < 0 {
                let bit = 1u16.checked_shl((-ahead - 1) as u32).unwrap_or(0);
                if self.concealed & bit != 0 {
                    self.concealed &= !bit;
                    self.grow();
                }
                return;
            }
            if ahead as usize >= CAPACITY {
                self.skip_to(seq);
            }
        }
        self.store(seq, payload);
    }

    /// Stores frame `seq` as carried redundantly by a later packet, which
    /// must be [`push`](Self::push)ed first. Fills a gap if the frame has not
    /// been played yet; otherwise ignored.
    pub fn push_redundant(&mut self, seq: u16, payload: &[u8]) {
        let ahead = seq.wrapping_sub(self.next) as i16;
        let slot = &self.slots[seq as usize % CAPACITY];
        let newer_in_slot = slot.filled && slot.seq.wrapping_sub(seq) as i16 >= 0;
        if !self.received
            || newer_in_slot
            || (self.playing && !(0..CAPACITY as i16).contains(&ahead))
        {
            return;
        }
        self.store(seq, payload);
    }

    fn store(&mut self, seq: u16, payload: &[u8]) {
        if !self.received || seq.wrapping_sub(self.newest) as i16 > 0 {
            self.newest = seq;
        }
        self.received = true;
        let slot = &mut self.slots[seq as usize % CAPACITY];
        slot.len = payload.len().min(MAX_PAYLOAD);
        slot.payload[..slot.len].copy_from_slice(&payload[..slot.len]);
        slot.seq = seq;
        slot.filled = true;
    }

    /// Produces the next 10 ms frame into `out` through `decode`, which gets
    /// the payload or `None` for a frame to conceal (Opus PLC). Outputs
    /// silence without calling `decode` while no stream is playing.
    pub fn pop(&mut self, decode: &mut impl FnMut(Option<&[u8]>, &mut Frame), out: &mut Frame) {
        if !self.playing {
            if self.slots.iter().filter(|s| s.filled).count() < self.target {
                out.fill(0.0);
                return;
            }
            // Start with exactly `target` frames buffered; older ones are stale.
            self.playing = true;
            self.next = self.newest.wrapping_sub(self.target as u16 - 1);
        }
        self.window_min_depth = self.window_min_depth.min(self.depth());

        if self.stretch > 0 {
            self.stretch -= 1;
            decode(None, out);
        } else if self.play_next(decode, out) {
            self.missing = 0;
            if self.drop_silent && is_silent(out) && self.has(self.next) {
                self.drop_silent = false;
                self.play_next(decode, out);
            }
        } else {
            self.missing += 1;
            if self.missing >= STOP_AFTER_MISSING && self.depth() == 0 {
                self.restart();
                return;
            }
        }

        self.window_pops += 1;
        if self.window_pops == WINDOW {
            if !self.window_late && self.target > 1 {
                self.target -= 1;
            }
            if self.window_min_depth > self.target {
                self.drop_silent = true;
            }
            self.window_pops = 0;
            self.window_late = false;
            self.window_min_depth = usize::MAX;
        }
    }

    /// Decodes `next` (or conceals it if missing) and advances. Returns
    /// whether the frame was present.
    fn play_next(
        &mut self,
        decode: &mut impl FnMut(Option<&[u8]>, &mut Frame),
        out: &mut Frame,
    ) -> bool {
        let present = self.has(self.next);
        self.concealed = self.concealed << 1 | u16::from(!present);
        let slot = &mut self.slots[self.next as usize % CAPACITY];
        // Clearing every visited slot also evicts stale packets skipped at
        // start before their slot index comes round again.
        slot.filled = false;
        decode(present.then(|| &slot.payload[..slot.len]), out);
        self.next = self.next.wrapping_add(1);
        present
    }

    fn has(&self, seq: u16) -> bool {
        let slot = &self.slots[seq as usize % CAPACITY];
        slot.filled && slot.seq == seq
    }

    /// Buffered frames: from the next to play through the newest received
    /// one, gaps included. Before playback starts, the packets received.
    pub fn depth(&self) -> usize {
        if !self.playing {
            return self.slots.iter().filter(|s| s.filled).count();
        }
        (self.newest.wrapping_sub(self.next) as i16 + 1).max(0) as usize
    }

    fn grow(&mut self) {
        self.window_late = true;
        // Dropping a frame now would undo the growth.
        self.drop_silent = false;
        if self.target < MAX_TARGET {
            self.target += 1;
            self.stretch += 1;
        }
    }

    /// Jumps playback forward so `seq` is `target` frames ahead: the frames
    /// overtaken are stale. Called when `seq` would not fit in the slots.
    fn skip_to(&mut self, seq: u16) {
        self.next = seq.wrapping_sub(self.target as u16 - 1);
        self.concealed = 0;
        let next = self.next;
        for slot in &mut self.slots {
            if (slot.seq.wrapping_sub(next) as i16) < 0 {
                slot.filled = false;
            }
        }
    }

    /// Forgets the current stream but keeps the learned target.
    fn restart(&mut self) {
        *self = Self {
            target: self.target,
            ..Self::new()
        };
    }
}

fn is_silent(frame: &Frame) -> bool {
    frame.iter().all(|s| s.abs() < SILENCE_PEAK)
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    const LOUD: u8 = 1;
    const QUIET: u8 = 0;

    /// Test payload: seq plus a loudness flag. The fake decoder renders it
    /// as a constant frame and logs what was played (`None` = concealed).
    struct Harness {
        jb: JitterBuffer,
        played: Vec<Option<u16>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                jb: JitterBuffer::new(),
                played: Vec::new(),
            }
        }

        fn push(&mut self, seq: u16, loudness: u8) {
            let [a, b] = seq.to_le_bytes();
            self.jb.push(seq, &[a, b, loudness]);
        }

        fn pop(&mut self) {
            let played = &mut self.played;
            let mut decode = |p: Option<&[u8]>, f: &mut Frame| {
                played.push(p.map(|p| u16::from_le_bytes([p[0], p[1]])));
                f.fill(p.map_or(0.0, |p| p[2] as f32));
            };
            self.jb.pop(&mut decode, &mut [0.0; FRAME_SAMPLES]);
        }

        fn push_redundant(&mut self, seq: u16) {
            let [a, b] = seq.to_le_bytes();
            self.jb.push_redundant(seq, &[a, b, LOUD]);
        }

        /// Steady stream: one packet then one pop per tick.
        fn tick(&mut self, seq: u16, loudness: u8) {
            self.push(seq, loudness);
            self.pop();
        }
    }

    #[test]
    fn in_order_stream_plays_without_added_delay() {
        let mut h = Harness::new();
        h.pop(); // nothing yet: silence, decoder untouched
        for seq in 0..5 {
            h.tick(seq, LOUD);
        }
        assert_eq!(h.played, (0..5).map(Some).collect::<Vec<_>>());
        assert_eq!(h.jb.target(), 1);
    }

    #[test]
    fn reordered_packets_play_in_seq_order() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.push(2, LOUD);
        h.push(1, LOUD);
        h.pop();
        h.pop();
        h.tick(3, LOUD);
        assert_eq!(h.played, [Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(h.jb.target(), 1);
    }

    #[test]
    fn lost_packet_is_concealed_without_growing() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.pop(); // 1 never arrives
        h.tick(2, LOUD);
        assert_eq!(h.played, [Some(0), None, Some(2)]);
        assert_eq!(h.jb.target(), 1);
    }

    #[test]
    fn late_packet_grows_delay_immediately() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.pop(); // 1 not here in time: concealed
        h.push(1, LOUD);
        assert_eq!(h.jb.target(), 2);
        h.tick(2, LOUD); // held back one frame
        h.tick(3, LOUD);
        assert_eq!(h.played, [Some(0), None, None, Some(2)]);
    }

    #[test]
    fn target_is_capped() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        for _ in 1..MAX_TARGET {
            h.pop();
        }
        for seq in 1..=MAX_TARGET as u16 {
            h.push(seq, LOUD);
        }
        assert_eq!(h.jb.target(), MAX_TARGET);
    }

    /// Grows to target 2, then streams steadily for `windows` adaptation
    /// windows. Returns the target and how many frames playback lags.
    fn after_growth(loudness: u8, windows: u32) -> (usize, u16) {
        let (h, seq) = grown(loudness, windows);
        let last = h.played.last().unwrap().unwrap();
        (h.jb.target(), seq - 1 - last)
    }

    /// See [`after_growth`]; also returns the next seq to send.
    fn grown(loudness: u8, windows: u32) -> (Harness, u16) {
        let mut h = Harness::new();
        h.tick(0, loudness);
        h.pop();
        h.push(1, loudness);
        let mut seq = 2;
        for _ in 0..windows * WINDOW {
            h.tick(seq, loudness);
            seq += 1;
        }
        (h, seq)
    }

    #[test]
    fn growth_cancels_pending_drop() {
        let (mut h, seq) = grown(LOUD, 2);
        assert!(h.jb.drop_silent);
        h.pop();
        h.pop(); // `seq` concealed
        h.push(seq, QUIET);
        assert_eq!(h.jb.target(), 2);
        assert!(!h.jb.drop_silent);
    }

    #[test]
    fn duplicates_and_redundant_copies_do_not_grow() {
        let mut h = Harness::new();
        for seq in 0..50 {
            h.push(seq, LOUD);
            h.push(seq.wrapping_sub(1), LOUD); // redundancy: previous frame again
            h.pop();
            h.push(seq, LOUD); // duplicate after playing
        }
        assert_eq!(h.jb.target(), 1);
        assert!(h.played.iter().skip(1).all(|p| p.is_some()));
    }

    #[test]
    fn redundant_copy_fills_gap() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        // 1 is lost; 2 arrives early, carrying 1.
        h.push(2, LOUD);
        h.push_redundant(1);
        h.pop();
        h.pop();
        assert_eq!(h.played, [Some(0), Some(1), Some(2)]);
        assert_eq!(h.jb.target(), 1);
    }

    #[test]
    fn redundant_copy_after_concealment_does_not_grow() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.pop(); // 1 lost: concealed
        h.push(2, LOUD);
        h.push_redundant(1);
        h.pop();
        assert_eq!(h.played, [Some(0), None, Some(2)]);
        assert_eq!(h.jb.target(), 1);
    }

    #[test]
    fn redundant_copy_never_replaces_primary() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.push(1, QUIET);
        h.push(2, LOUD);
        h.push_redundant(1); // a LOUD copy
        let mut out = [1.0; FRAME_SAMPLES];
        h.jb.pop(&mut |p, f| f.fill(p.unwrap()[2] as f32), &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn late_packet_grows_only_once() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.pop();
        h.push(1, LOUD);
        h.push(1, LOUD);
        assert_eq!(h.jb.target(), 2);
    }

    #[test]
    fn long_gap_keeps_newer_buffered_packets() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.push(7, LOUD);
        for _ in 0..7 {
            h.pop();
        }
        assert_eq!(h.played.len(), 8);
        assert_eq!(h.played.last(), Some(&Some(7)));
    }

    #[test]
    fn burst_skips_ahead_to_newest() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        for seq in 1..=10 {
            h.push(seq, LOUD);
        }
        h.pop();
        h.pop();
        assert_eq!(h.played, [Some(0), Some(9), Some(10)]);
    }

    #[test]
    fn shrinks_by_dropping_a_silent_frame() {
        assert_eq!(after_growth(QUIET, 1), (2, 1));
        assert_eq!(after_growth(QUIET, 2), (1, 0));
    }

    #[test]
    fn never_drops_audible_frames() {
        assert_eq!(after_growth(LOUD, 3), (1, 1));
    }

    #[test]
    fn seq_wraps_around() {
        let mut h = Harness::new();
        let start = u16::MAX - 4;
        for i in 0..5 {
            h.tick(start.wrapping_add(i), LOUD);
        }
        h.pop(); // seq 0 concealed
        h.tick(1, LOUD);
        h.push(u16::MAX, LOUD); // duplicate across the wrap: ignored
        assert_eq!(h.jb.target(), 1);
        h.push(0, LOUD); // late across the wrap
        assert_eq!(h.jb.target(), 2);
        let mut expected: Vec<_> = (0..5).map(|i| Some(start.wrapping_add(i))).collect();
        expected.extend([None, Some(1)]);
        assert_eq!(h.played, expected);
    }

    #[test]
    fn jump_far_ahead_skips_to_it() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        h.tick(1000, LOUD);
        h.tick(1001, LOUD);
        assert_eq!(h.played, [Some(0), Some(1000), Some(1001)]);
    }

    #[test]
    fn depth_counts_buffered_frames() {
        let mut h = Harness::new();
        h.push(40, LOUD);
        assert_eq!(h.jb.depth(), 1);
        h.pop();
        assert_eq!(h.jb.depth(), 0);
        h.push(42, LOUD);
        assert_eq!(h.jb.depth(), 2); // 41 is a gap
    }

    #[test]
    fn stopped_sender_restarts_on_next_packet() {
        let mut h = Harness::new();
        h.tick(0, LOUD);
        for _ in 0..20 {
            h.pop();
        }
        h.tick(500, LOUD);
        let concealed = h.played.iter().filter(|p| p.is_none()).count();
        assert_eq!(concealed, STOP_AFTER_MISSING as usize);
        assert_eq!(h.played.last(), Some(&Some(500)));
    }
}
