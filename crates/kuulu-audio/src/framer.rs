use crate::{FRAME_SAMPLES, Frame};

/// Cuts a stream of arbitrarily sized capture buffers into 10 ms frames.
pub struct Framer {
    frame: Frame,
    len: usize,
}

impl Framer {
    pub fn new() -> Self {
        Self {
            frame: [0.0; FRAME_SAMPLES],
            len: 0,
        }
    }

    /// Appends `samples`, calling `on_frame` for each frame completed by them.
    pub fn push(&mut self, mut samples: &[f32], mut on_frame: impl FnMut(&Frame)) {
        while !samples.is_empty() {
            let n = samples.len().min(FRAME_SAMPLES - self.len);
            self.frame[self.len..self.len + n].copy_from_slice(&samples[..n]);
            self.len += n;
            samples = &samples[n..];
            if self.len == FRAME_SAMPLES {
                on_frame(&self.frame);
                self.len = 0;
            }
        }
    }
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_preserve_order_across_uneven_buffers() {
        let input: Vec<f32> = (0..FRAME_SAMPLES * 3 + 7).map(|i| i as f32).collect();
        let mut framer = Framer::new();
        let mut out = Vec::new();
        for chunk in input.chunks(173) {
            framer.push(chunk, |f| out.extend_from_slice(f));
        }
        assert_eq!(out, input[..FRAME_SAMPLES * 3]);
    }

    #[test]
    fn emits_frame_exactly_when_complete() {
        let mut framer = Framer::new();
        let mut frames = 0;
        framer.push(&[0.0; 240], |_| frames += 1);
        assert_eq!(frames, 0);
        framer.push(&[0.0; 240], |_| frames += 1);
        assert_eq!(frames, 1);
        framer.push(&[0.0; FRAME_SAMPLES * 2], |_| frames += 1);
        assert_eq!(frames, 3);
    }
}
