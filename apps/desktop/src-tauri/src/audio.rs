//! The desktop's audio seam: devices, the real-time audio thread, the
//! push-to-talk / voice-activity gate and the voice pipeline. Audio never
//! leaves Rust; the rest of the app sees only [`Audio`] and [`Gate`].

use kuulu_audio::device::{self, DeviceInfo, Input, Output};
use kuulu_audio::{FRAME_SAMPLES, Frame, Framer, Vad};
use kuulu_client::Client;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

#[derive(Serialize)]
pub struct Devices {
    pub inputs: Vec<DeviceView>,
    pub outputs: Vec<DeviceView>,
}

#[derive(Serialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
}

pub fn devices() -> Devices {
    let view = |list: Vec<DeviceInfo>| {
        list.into_iter()
            .map(|d| DeviceView {
                id: d.id,
                name: d.name,
            })
            .collect()
    };
    Devices {
        inputs: view(device::inputs()),
        outputs: view(device::outputs()),
    }
}

/// When captured audio is sent. Shared with the audio thread, which only
/// reads it; it outlives audio restarts.
#[derive(Default)]
pub struct Gate {
    /// Push-to-talk; otherwise voice activity decides.
    push_to_talk: AtomicBool,
    key_held: AtomicBool,
}

impl Gate {
    pub fn set_push_to_talk(&self, on: bool) {
        self.push_to_talk.store(on, Relaxed);
        self.key_held.store(false, Relaxed);
    }

    pub fn set_key_held(&self, held: bool) {
        self.key_held.store(held, Relaxed);
    }
}

/// Written by the audio thread once per frame.
#[derive(Default)]
struct Meters {
    /// RMS of the latest captured frame, f32 bits.
    level: AtomicU32,
    /// The gate let the latest frame through.
    open: AtomicBool,
    /// Jitter buffer target of the busiest sender, frames.
    jitter_target: AtomicUsize,
}

/// Running audio: capture always, playback and voice only in a call. Stops
/// when dropped. Settings changes restart it; the gate carries over.
pub struct Audio {
    stop: Arc<AtomicBool>,
    meters: Arc<Meters>,
    thread: Option<JoinHandle<()>>,
    in_call: bool,
    // Dropped after the thread: streams wake it.
    _streams: (device::Stream, Option<device::Stream>),
}

impl Audio {
    /// Starts capture on `input`, and when `client` is given, playback on
    /// `output` and voice through the client. Device ids come from
    /// [`devices`]; `None` means the system default. Must run inside the
    /// tokio runtime.
    pub fn start(
        input: Option<&str>,
        output: Option<&str>,
        gate: Arc<Gate>,
        client: Option<Arc<Client>>,
    ) -> Result<Self, String> {
        let voice = client.map(Voice::start).transpose()?;
        let in_call = voice.is_some();
        let stop = Arc::new(AtomicBool::new(false));
        let meters = Arc::new(Meters::default());
        let (rings_tx, rings_rx) = mpsc::channel();
        let thread = {
            let (stop, meters) = (stop.clone(), meters.clone());
            thread::Builder::new()
                .name("kuulu-audio".into())
                .spawn(move || {
                    if let Ok((input, output)) = rings_rx.recv() {
                        audio_thread(input, output, voice, &gate, &meters, &stop);
                    }
                })
                .map_err(|e| e.to_string())?
        };
        // Streams wake the audio thread, so it exists first and gets its
        // rings once the devices are open. On error it exits as the sender
        // drops.
        let wake = thread.thread().clone();
        let fail = |e: device::Error| format!("Audio device error: {e}");
        let (capture, input) = device::capture(input, wake.clone()).map_err(fail)?;
        let (playback, output) = match in_call {
            true => {
                let (stream, output) = device::playback(output, wake).map_err(fail)?;
                (Some(stream), Some(output))
            }
            false => (None, None),
        };
        let _ = rings_tx.send((input, output));
        Ok(Self {
            stop,
            meters,
            thread: Some(thread),
            in_call,
            _streams: (capture, playback),
        })
    }

    /// Microphone level, 0 to about 1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.meters.level.load(Relaxed))
    }

    /// Whether the gate is passing the microphone to the call.
    pub fn transmitting(&self) -> bool {
        self.in_call && self.meters.open.load(Relaxed)
    }

    /// Jitter buffer target in frames; `None` until voice is wired (#5).
    pub fn jitter_target(&self) -> Option<usize> {
        Voice::WIRED.then(|| self.meters.jitter_target.load(Relaxed))
    }

    /// Volume for one sender, linear gain.
    // TODO(#5): needs per-sender gain in Voice::play.
    pub fn set_gain(&self, _session_id: u32, _gain: f32) {}
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Real-time loop: no allocation, locks, I/O or logging.
fn audio_thread(
    mut input: Input,
    mut output: Option<Output>,
    mut voice: Option<Voice>,
    gate: &Gate,
    meters: &Meters,
    stop: &AtomicBool,
) {
    let mut framer = Framer::new();
    let mut vad = Vad::new();
    let mut played = [0.0; FRAME_SAMPLES];
    while !stop.load(Relaxed) {
        input.drain(|samples| {
            framer.push(samples, |frame| {
                let voice_active = vad.is_open(frame);
                let open = match gate.push_to_talk.load(Relaxed) {
                    true => gate.key_held.load(Relaxed),
                    false => voice_active,
                };
                meters
                    .level
                    .store(kuulu_audio::level(frame).to_bits(), Relaxed);
                meters.open.store(open, Relaxed);
                if open && let Some(voice) = &mut voice {
                    voice.capture(frame);
                }
            })
        });
        if let (Some(output), Some(voice)) = (&mut output, &mut voice) {
            while output.wants_frame() {
                voice.play(&mut played);
                if !output.push(&played) {
                    break;
                }
            }
            meters
                .jitter_target
                .store(voice.stats().jitter_target, Relaxed);
        }
        // Woken by every capture and playback callback, and by Drop.
        thread::park();
    }
}

/// Stand-in for `kuulu_client::voice::Voice` from PR #5, with the calls
/// this module makes. TODO(#5): delete and use the real one.
struct Voice;

struct VoiceStats {
    jitter_target: usize,
}

impl Voice {
    const WIRED: bool = false;

    fn start(_client: Arc<Client>) -> Result<Self, String> {
        Ok(Self)
    }

    fn capture(&mut self, _frame: &Frame) {}

    fn play(&mut self, out: &mut Frame) {
        out.fill(0.0);
    }

    fn stats(&self) -> VoiceStats {
        VoiceStats { jitter_target: 0 }
    }
}
