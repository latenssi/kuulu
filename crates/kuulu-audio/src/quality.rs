//! Synthetic speech and an objective quality proxy, for tests and benchmarks
//! that run the pipeline without audio devices.

use crate::{FRAME_SAMPLES, SAMPLE_RATE};
use std::f64::consts::PI;

/// Speech-like test signal of `len` samples, peak 0.5: a pulse train with
/// gliding pitch through two moving formant resonators, about four
/// syllables per second, and a 300 ms pause at the start of every 2 s.
/// Deterministic, and not periodic at any whole-frame lag, so alignment by
/// [`compare`] is unambiguous.
pub fn speech(len: usize) -> Vec<f32> {
    let rate = f64::from(SAMPLE_RATE);
    let mut phase = 0.0;
    let mut formants = [[0.0; 2]; 2];
    let mut coeffs = [(0.0, 0.0); 2];
    let mut out: Vec<f64> = Vec::with_capacity(len);
    for n in 0..len {
        let t = n as f64 / rate;
        if n % FRAME_SAMPLES == 0 {
            let f1 = 550.0 + 250.0 * (2.0 * PI * 0.37 * t).sin();
            let f2 = 1500.0 + 600.0 * (2.0 * PI * 0.23 * t + 1.0).sin();
            for (c, f) in coeffs.iter_mut().zip([f1, f2]) {
                let r = (-PI * 120.0 / rate).exp();
                *c = (2.0 * r * (2.0 * PI * f / rate).cos(), -r * r);
            }
        }
        let f0 = 135.0 + 35.0 * (2.0 * PI * 0.7 * t).sin() + 12.0 * (2.0 * PI * 1.9 * t).sin();
        phase += f0 / rate;
        // Sawtooth glottal source; each formant filters the previous stage.
        let mut x = phase.fract() - 0.5;
        for (y, (a1, a2)) in formants.iter_mut().zip(coeffs) {
            let v = x + a1 * y[0] + a2 * y[1];
            *y = [v, y[0]];
            x = v;
        }
        let cycle = t % 2.0;
        let envelope = if cycle < 0.3 {
            0.0
        } else {
            (PI * 4.0 * (cycle - 0.3)).sin().powi(2)
        };
        out.push(x * envelope);
    }
    let peak = out.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    out.iter().map(|x| (0.5 * x / peak) as f32).collect()
}

/// How closely an output follows its source, over voiced frames only.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quality {
    /// Mean segmental SNR, each frame clamped to -10..=40 dB.
    pub snr_db: f64,
    /// Share of frames whose waveform correlates below 0.5 with the source:
    /// concealed with audible damage, or missing.
    pub poor: f64,
    pub voiced_frames: usize,
}

/// Largest whole-frame lag searched, beyond `delay`.
const MAX_LAG_FRAMES: usize = 20;
/// Frames on each side whose alignment must agree.
const ALIGN_WINDOW: usize = 10;
/// Frames quieter than this RMS (about -40 dBFS) are not voiced.
const VOICED_RMS: f64 = 0.01;

/// Compares `output` with `source`. The output lags by `delay` samples (the
/// codec's) plus a whole number of frames, which the jitter buffer may change
/// at any time; each output frame is aligned to the frame lag that best fits
/// its neighbourhood on one side.
pub fn compare(source: &[f32], output: &[f32], delay: usize) -> Quality {
    let frames = output.len() / FRAME_SAMPLES;
    // Per output frame and lag k: (dot, source energy); and output energy.
    let mut stats = vec![[(0.0, 0.0); MAX_LAG_FRAMES]; frames];
    let mut out_energy = vec![0.0; frames];
    for (j, (row, oe)) in stats.iter_mut().zip(&mut out_energy).enumerate() {
        let o = &output[j * FRAME_SAMPLES..][..FRAME_SAMPLES];
        *oe = o.iter().map(|&x| f64::from(x).powi(2)).sum();
        for (k, cell) in row.iter_mut().enumerate() {
            let Some(start) = (j * FRAME_SAMPLES).checked_sub(delay + k * FRAME_SAMPLES) else {
                continue;
            };
            let Some(s) = source.get(start..start + FRAME_SAMPLES) else {
                continue;
            };
            let dot = s.iter().zip(o).map(|(&a, &b)| f64::from(a * b)).sum();
            *cell = (dot, s.iter().map(|&x| f64::from(x).powi(2)).sum());
        }
    }
    let quiet = |range: &std::ops::Range<usize>| {
        out_energy[range.clone()].iter().sum::<f64>() < VOICED_RMS.powi(2) * FRAME_SAMPLES as f64
    };
    // Lag with the best correlation over `range`, which must not be quiet.
    let best_lag = |range: std::ops::Range<usize>| {
        (0..MAX_LAG_FRAMES)
            .max_by(|&a, &b| {
                let score = |k: usize| {
                    let (mut dot, mut se, mut oe) = (0.0, 0.0, 0.0);
                    for i in range.clone() {
                        dot += stats[i][k].0;
                        se += stats[i][k].1;
                        oe += out_energy[i];
                    }
                    dot / (se * oe).sqrt().max(1e-12)
                };
                score(a).total_cmp(&score(b))
            })
            .unwrap()
    };
    let corr =
        |j: usize, k: usize| stats[j][k].0 / (stats[j][k].1 * out_energy[j]).sqrt().max(1e-12);

    let (mut snr_sum, mut poor, mut voiced) = (0.0, 0, 0);
    // Quiet stretches of output carry no alignment: keep the previous one.
    let mut k = 0;
    for j in 0..frames {
        let (left, right) = (
            j.saturating_sub(ALIGN_WINDOW)..j + 1,
            j..(j + ALIGN_WINDOW + 1).min(frames),
        );
        let left = if quiet(&left) { k } else { best_lag(left) };
        let right = if quiet(&right) { left } else { best_lag(right) };
        k = if corr(j, left) >= corr(j, right) {
            left
        } else {
            right
        };
        let (dot, se) = stats[j][k];
        if (se / FRAME_SAMPLES as f64).sqrt() < VOICED_RMS {
            continue;
        }
        voiced += 1;
        let error = (se + out_energy[j] - 2.0 * dot).max(1e-12);
        snr_sum += (10.0 * (se / error).log10()).clamp(-10.0, 40.0);
        if corr(j, k) < 0.5 {
            poor += 1;
        }
    }
    Quality {
        snr_db: snr_sum / voiced.max(1) as f64,
        poor: poor as f64 / voiced.max(1) as f64,
        voiced_frames: voiced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 4 * SAMPLE_RATE as usize;

    #[test]
    fn identical_signal_is_perfect() {
        let s = speech(LEN);
        let q = compare(&s, &s, 0);
        assert!(q.voiced_frames > 150, "{q:?}");
        assert_eq!((q.snr_db, q.poor), (40.0, 0.0));
    }

    #[test]
    fn finds_shifting_whole_frame_lags() {
        let s = speech(LEN);
        let delay = 120;
        // Lag 3 frames, then 5 after one second, as after jitter buffer growth.
        let mut out = vec![0.0; delay + 3 * FRAME_SAMPLES];
        out.extend_from_slice(&s[..SAMPLE_RATE as usize]);
        out.extend_from_slice(&[0.0; 2 * FRAME_SAMPLES]);
        out.extend_from_slice(&s[SAMPLE_RATE as usize..]);
        let q = compare(&s, &out, delay);
        assert!(q.snr_db > 39.0 && q.poor < 0.01, "{q:?}");
    }

    #[test]
    fn missing_frames_are_poor() {
        let s = speech(LEN);
        let mut out = s.clone();
        for frame in out.chunks_mut(FRAME_SAMPLES).step_by(10) {
            frame.fill(0.0);
        }
        let q = compare(&s, &out, 0);
        assert!((0.08..0.12).contains(&q.poor), "{q:?}");
    }

    #[test]
    fn opus_keeps_speech_waveform() {
        let s = speech(LEN);
        let (mut enc, mut dec) = (
            crate::Encoder::new().unwrap(),
            crate::Decoder::new().unwrap(),
        );
        let mut out = vec![0.0; LEN];
        for (i, o) in s.as_chunks().0.iter().zip(out.as_chunks_mut().0) {
            dec.decode(Some(enc.encode(i)), o);
        }
        let q = compare(&s, &out, 120);
        assert!(q.snr_db > 15.0 && q.poor < 0.01, "{q:?}");
    }

    #[test]
    fn speech_does_not_repeat_at_frame_lags() {
        let s = speech(LEN);
        let energy: f32 = s.iter().map(|a| a * a).sum();
        for lag in (1..MAX_LAG_FRAMES).map(|k| k * FRAME_SAMPLES) {
            let dot: f32 = s[lag..].iter().zip(&s).map(|(a, b)| a * b).sum();
            assert!(dot / energy < 0.3, "lag {lag}");
        }
    }
}
