use crate::Frame;

/// Output never exceeds this magnitude (about -0.3 dBFS).
const CEILING: f32 = 0.966;
/// Per-sample gain recovery towards 1.0 after limiting: about 50 ms time
/// constant at 48 kHz, slow enough not to distort speech peaks.
const RELEASE: f32 = 1.0 / 2400.0;

/// Sums senders' frames with per-sender gain and a peak limiter.
pub struct Mixer {
    limiter_gain: f32,
}

impl Mixer {
    pub fn new() -> Self {
        Self { limiter_gain: 1.0 }
    }

    /// Mixes `(frame, gain)` sources into `out`. Gains are linear.
    pub fn mix<'a>(
        &mut self,
        sources: impl IntoIterator<Item = (&'a Frame, f32)>,
        out: &mut Frame,
    ) {
        out.fill(0.0);
        for (frame, gain) in sources {
            for (o, s) in out.iter_mut().zip(frame) {
                *o += s * gain;
            }
        }
        // Instant attack keeps every sample within CEILING; release is gradual.
        for o in out.iter_mut() {
            let needed = if o.abs() > CEILING {
                CEILING / o.abs()
            } else {
                1.0
            };
            self.limiter_gain =
                (self.limiter_gain + (1.0 - self.limiter_gain) * RELEASE).min(needed);
            *o *= self.limiter_gain;
        }
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    #[test]
    fn sums_sources_with_gain() {
        let a = [0.1; FRAME_SAMPLES];
        let b = [0.2; FRAME_SAMPLES];
        let mut out = [0.0; FRAME_SAMPLES];
        Mixer::new().mix([(&a, 1.0), (&b, 0.5)], &mut out);
        assert!(out.iter().all(|&s| (s - 0.2).abs() < 1e-6));
    }

    #[test]
    fn limiter_holds_ceiling_and_recovers() {
        let mut mixer = Mixer::new();
        let loud = [0.9; FRAME_SAMPLES];
        let mut out = [0.0; FRAME_SAMPLES];
        mixer.mix([(&loud, 1.0), (&loud, 1.0), (&loud, 1.0)], &mut out);
        assert!(out.iter().all(|s| s.abs() <= CEILING));
        assert!((out[0] - CEILING).abs() < 1e-6);

        let quiet = [0.1; FRAME_SAMPLES];
        for _ in 0..100 {
            mixer.mix([(&quiet, 1.0)], &mut out);
        }
        assert!((out[FRAME_SAMPLES - 1] - 0.1).abs() < 1e-4);
    }

    #[test]
    fn no_sources_is_silence() {
        let mut out = [1.0; FRAME_SAMPLES];
        Mixer::new().mix([], &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
