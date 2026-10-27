//! Default capture and playback devices via cpal, connected to the audio
//! thread through rtrb ring buffers of mono 48 kHz f32 samples.
//!
//! Callbacks only move samples and wake the audio thread (`Thread::unpark`,
//! a non-blocking syscall without locks or allocation).

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};
use assert_no_alloc::assert_no_alloc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, SampleFormat, StreamConfig, SupportedBufferSize, SupportedStreamConfigRange,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread::Thread;
use std::time::Duration;

/// Requested device buffer: 5 ms at 48 kHz, as in the latency budget.
pub const DEVICE_BUFFER: u32 = 240;

/// Ring capacity in samples (1 s). Capacity adds no latency: the playback
/// queue is kept short by [`playback_wants_frame`] and the audio thread
/// drains capture on every callback. It only has to exceed any device
/// period; devices that cannot go below half of it are rejected.
const RING: usize = 48_000;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Playback refill policy, shared by the audio thread and the latency
/// simulation: true while less than one device period is queued, i.e. the
/// next callback would run dry. Keeping the queue this short limits playback
/// buffering to about one period. `period` 0 (no callback yet) means the
/// requested [`DEVICE_BUFFER`]; the cap keeps a full ring from asking for
/// more forever.
pub fn playback_wants_frame(queued: usize, period: usize) -> bool {
    let period = if period == 0 {
        DEVICE_BUFFER as usize
    } else {
        period
    };
    queued < period.min(RING - FRAME_SAMPLES)
}

/// A running stream; stops when dropped.
pub struct Stream {
    _stream: cpal::Stream,
    stats: Arc<Stats>,
}

#[derive(Default)]
struct Stats {
    period: AtomicUsize,
    xruns: AtomicU64,
    latency_us: AtomicU64,
}

impl Stream {
    /// Samples per callback as actually delivered by the device; 0 before
    /// the first callback.
    pub fn period(&self) -> usize {
        self.stats.period.load(Relaxed)
    }

    /// Callbacks that over- or underran the ring buffer, plus stream errors.
    pub fn xruns(&self) -> u64 {
        self.stats.xruns.load(Relaxed)
    }

    /// Device latency from cpal's timestamps in the latest callback:
    /// capture instant to callback for input, callback to playback instant
    /// for output.
    pub fn latency(&self) -> Duration {
        Duration::from_micros(self.stats.latency_us.load(Relaxed))
    }
}

/// Captured samples, read by the audio thread.
pub struct Input(rtrb::Consumer<f32>);

impl Input {
    /// Passes everything captured so far to `f`, in one or two slices.
    pub fn drain(&mut self, mut f: impl FnMut(&[f32])) {
        if let Ok(chunk) = self.0.read_chunk(self.0.slots()) {
            let (a, b) = chunk.as_slices();
            f(a);
            f(b);
            chunk.commit_all();
        }
    }
}

/// Feeds the playback device from the audio thread.
pub struct Output {
    ring: rtrb::Producer<f32>,
    stats: Arc<Stats>,
}

impl Output {
    /// See [`playback_wants_frame`].
    pub fn wants_frame(&self) -> bool {
        playback_wants_frame(RING - self.ring.slots(), self.stats.period.load(Relaxed))
    }

    /// Queues a frame. Returns false, dropping it, if the ring is full.
    pub fn push(&mut self, frame: &Frame) -> bool {
        match self.ring.write_chunk_uninit(FRAME_SAMPLES) {
            Ok(chunk) => {
                chunk.fill_from_iter(frame.iter().copied());
                true
            }
            Err(_) => false,
        }
    }
}

/// Starts the default input device. Its first channel arrives in the
/// returned [`Input`]; a callback that does not fit is dropped whole.
pub fn capture(wake: Thread) -> Result<(Stream, Input), Error> {
    let (mut ring, consumer) = rtrb::RingBuffer::new(RING);
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no input device")?;
    let config = choose(device.supported_input_configs()?)?;
    let channels = config.channels as usize;
    let stats = Arc::new(Stats::default());
    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], info: &cpal::InputCallbackInfo| {
            assert_no_alloc(|| {
                let t = info.timestamp();
                let latency = t.callback.duration_since(t.capture);
                capture_callback(&mut ring, &cb_stats, data, channels, latency);
                wake.unpark();
            })
        },
        move |_| {
            err_stats.xruns.fetch_add(1, Relaxed);
        },
        None,
    )?;
    stream.play()?;
    Ok((
        Stream {
            _stream: stream,
            stats,
        },
        Input(consumer),
    ))
}

fn capture_callback(
    ring: &mut rtrb::Producer<f32>,
    stats: &Stats,
    data: &[f32],
    channels: usize,
    latency: Duration,
) {
    let n = data.len() / channels;
    stats.period.store(n, Relaxed);
    stats.latency_us.store(latency.as_micros() as u64, Relaxed);
    match ring.write_chunk_uninit(n) {
        Ok(chunk) => {
            chunk.fill_from_iter(data.iter().step_by(channels).copied());
        }
        Err(_) => {
            stats.xruns.fetch_add(1, Relaxed);
        }
    }
}

/// Starts the default output device, playing the returned [`Output`] on
/// every channel. Missing samples play as silence and count as an underrun.
pub fn playback(wake: Thread) -> Result<(Stream, Output), Error> {
    let (producer, mut ring) = rtrb::RingBuffer::new(RING);
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no output device")?;
    let config = choose(device.supported_output_configs()?)?;
    let channels = config.channels as usize;
    let stats = Arc::new(Stats::default());
    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            assert_no_alloc(|| {
                let t = info.timestamp();
                let latency = t.playback.duration_since(t.callback);
                playback_callback(&mut ring, &cb_stats, data, channels, latency);
                wake.unpark();
            })
        },
        move |_| {
            err_stats.xruns.fetch_add(1, Relaxed);
        },
        None,
    )?;
    stream.play()?;
    let output = Output {
        ring: producer,
        stats: stats.clone(),
    };
    Ok((
        Stream {
            _stream: stream,
            stats,
        },
        output,
    ))
}

fn playback_callback(
    ring: &mut rtrb::Consumer<f32>,
    stats: &Stats,
    data: &mut [f32],
    channels: usize,
    latency: Duration,
) {
    let n = data.len() / channels;
    stats.period.store(n, Relaxed);
    stats.latency_us.store(latency.as_micros() as u64, Relaxed);
    let available = n.min(ring.slots());
    if available < n {
        stats.xruns.fetch_add(1, Relaxed);
    }
    let mut out = data.chunks_exact_mut(channels);
    if let Ok(chunk) = ring.read_chunk(available) {
        let (a, b) = chunk.as_slices();
        for (frame, &s) in out.by_ref().zip(a.iter().chain(b)) {
            frame.fill(s);
        }
        chunk.commit_all();
    }
    out.for_each(|frame| frame.fill(0.0));
}

/// Picks 48 kHz f32 with the fewest channels and a 240-sample buffer if the
/// device allows it. No resampling: devices without 48 kHz are rejected.
fn choose(ranges: impl Iterator<Item = SupportedStreamConfigRange>) -> Result<StreamConfig, Error> {
    let range = ranges
        .filter(|r| {
            r.sample_format() == SampleFormat::F32
                && (r.min_sample_rate()..=r.max_sample_rate()).contains(&SAMPLE_RATE)
        })
        .min_by_key(|r| r.channels())
        .ok_or("device does not support 48 kHz f32")?;
    let buffer_size = match *range.buffer_size() {
        SupportedBufferSize::Range { min, .. } if min as usize > RING / 2 => {
            return Err(format!("device buffer of at least {min} samples is too large").into());
        }
        SupportedBufferSize::Range { min, max } if (min..=max).contains(&DEVICE_BUFFER) => {
            BufferSize::Fixed(DEVICE_BUFFER)
        }
        _ => BufferSize::Default,
    };
    Ok(StreamConfig {
        channels: range.channels(),
        sample_rate: SAMPLE_RATE,
        buffer_size,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn refill_stops_at_one_period() {
        assert!(playback_wants_frame(0, 0));
        assert!(playback_wants_frame(239, 240));
        assert!(!playback_wants_frame(240, 240));
        assert!(!playback_wants_frame(1024, 1024));
    }

    #[test]
    fn huge_period_cannot_make_the_audio_thread_spin() {
        let full = RING - FRAME_SAMPLES;
        assert!(!playback_wants_frame(full, RING * 2));
    }

    /// Device side of both rings, driven like the real stream callbacks.
    pub(crate) struct FakeDevices {
        capture: (rtrb::Producer<f32>, Stats),
        playback: (rtrb::Consumer<f32>, Arc<Stats>),
    }

    impl FakeDevices {
        /// Mono capture.
        pub(crate) fn capture(&mut self, data: &[f32]) {
            capture_callback(
                &mut self.capture.0,
                &self.capture.1,
                data,
                1,
                Duration::ZERO,
            );
        }

        /// Stereo playback.
        pub(crate) fn playback(&mut self, data: &mut [f32]) {
            let (ring, stats) = &mut self.playback;
            playback_callback(ring, stats, data, 2, Duration::ZERO);
        }
    }

    pub(crate) fn rings() -> (Input, Output, FakeDevices) {
        let (cap_tx, cap_rx) = rtrb::RingBuffer::new(RING);
        let (play_tx, play_rx) = rtrb::RingBuffer::new(RING);
        let play_stats = Arc::new(Stats::default());
        let output = Output {
            ring: play_tx,
            stats: play_stats.clone(),
        };
        let devices = FakeDevices {
            capture: (cap_tx, Stats::default()),
            playback: (play_rx, play_stats),
        };
        (Input(cap_rx), output, devices)
    }

    #[test]
    fn push_reports_full_ring() {
        let (_, mut output, _) = rings();
        let frame = [0.0; FRAME_SAMPLES];
        let pushed = (0..RING).take_while(|_| output.push(&frame)).count();
        assert_eq!(pushed, RING / FRAME_SAMPLES);
        assert!(!output.wants_frame());
    }
}
