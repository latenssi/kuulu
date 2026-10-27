//! Default capture and playback devices via cpal, connected to the audio
//! thread through rtrb ring buffers of mono 48 kHz f32 samples.
//!
//! Callbacks only move samples and wake the audio thread (`Thread::unpark`,
//! a non-blocking syscall without locks or allocation).

use crate::SAMPLE_RATE;
use assert_no_alloc::assert_no_alloc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, SampleFormat, StreamConfig, SupportedBufferSize, SupportedStreamConfigRange,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread::Thread;

/// Requested device buffer: 5 ms at 48 kHz, as in the latency budget.
pub const DEVICE_BUFFER: u32 = 240;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// A running stream; stops when dropped.
pub struct Stream {
    _stream: cpal::Stream,
    stats: Arc<Stats>,
}

#[derive(Default)]
struct Stats {
    period: AtomicUsize,
    xruns: AtomicU64,
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
}

/// Starts the default input device, pushing its first channel into `ring`.
/// A callback that does not fit in `ring` is dropped whole.
pub fn capture(mut ring: rtrb::Producer<f32>, wake: Thread) -> Result<Stream, Error> {
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
        move |data: &[f32], _: &_| {
            assert_no_alloc(|| {
                let n = data.len() / channels;
                cb_stats.period.store(n, Relaxed);
                match ring.write_chunk_uninit(n) {
                    Ok(chunk) => {
                        chunk.fill_from_iter(data.iter().step_by(channels).copied());
                    }
                    Err(_) => {
                        cb_stats.xruns.fetch_add(1, Relaxed);
                    }
                }
                wake.unpark();
            })
        },
        move |_| {
            err_stats.xruns.fetch_add(1, Relaxed);
        },
        None,
    )?;
    stream.play()?;
    Ok(Stream {
        _stream: stream,
        stats,
    })
}

/// Starts the default output device, playing `ring` on every channel.
/// Missing samples are played as silence and counted as an underrun.
pub fn playback(mut ring: rtrb::Consumer<f32>, wake: Thread) -> Result<Stream, Error> {
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
        move |data: &mut [f32], _: &_| {
            assert_no_alloc(|| {
                let n = data.len() / channels;
                cb_stats.period.store(n, Relaxed);
                let available = n.min(ring.slots());
                if available < n {
                    cb_stats.xruns.fetch_add(1, Relaxed);
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
                wake.unpark();
            })
        },
        move |_| {
            err_stats.xruns.fetch_add(1, Relaxed);
        },
        None,
    )?;
    stream.play()?;
    Ok(Stream {
        _stream: stream,
        stats,
    })
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
