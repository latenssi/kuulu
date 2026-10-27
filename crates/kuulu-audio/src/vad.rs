use crate::Frame;

/// Frames the gate stays open after the last loud frame (300 ms), so word
/// endings and short pauses are sent rather than chopped.
const HANGOVER: u32 = 30;
/// Opening level: frame RMS of about -45 dBFS. Room noise on a headset
/// microphone sits well below it, speech well above.
const THRESHOLD: f32 = 0.0056;

/// Voice activity gate: an energy threshold with hangover. Real-time safe.
pub struct Vad {
    /// Frames the gate stays open without another loud frame.
    open_for: u32,
}

impl Vad {
    pub fn new() -> Self {
        Self { open_for: 0 }
    }

    /// Whether `frame` should be sent.
    pub fn is_open(&mut self, frame: &Frame) -> bool {
        if level(frame) >= THRESHOLD {
            self.open_for = HANGOVER;
            true
        } else if self.open_for > 0 {
            self.open_for -= 1;
            true
        } else {
            false
        }
    }
}

impl Default for Vad {
    fn default() -> Self {
        Self::new()
    }
}

/// RMS of `frame`, 0 to about 1.
pub fn level(frame: &Frame) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    fn tone(amplitude: f32) -> Frame {
        std::array::from_fn(|i| (i as f32 * 0.06).sin() * amplitude)
    }

    #[test]
    fn opens_on_speech_and_holds_for_the_hangover() {
        let mut vad = Vad::new();
        assert!(!vad.is_open(&[0.0; FRAME_SAMPLES]));
        assert!(!vad.is_open(&tone(0.002)), "room noise stays closed");
        assert!(vad.is_open(&tone(0.1)));
        let quiet = tone(0.001);
        let held = (0..100).take_while(|_| vad.is_open(&quiet)).count();
        assert_eq!(held as u32, HANGOVER);
        // Speech during the hangover restarts it.
        assert!(vad.is_open(&tone(0.1)));
        assert!(vad.is_open(&quiet));
    }

    #[test]
    fn level_is_rms() {
        assert_eq!(level(&[0.5; FRAME_SAMPLES]), 0.5);
        assert_eq!(level(&[0.0; FRAME_SAMPLES]), 0.0);
    }
}
