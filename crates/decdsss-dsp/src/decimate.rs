// made by CasualArclamp
//! Integer-factor decimation of real audio: a Kaiser-windowed sinc low-pass
//! evaluated only at the samples kept.
//!
//! The receiver puts one of these in front of its bands, so a 44.1 or 48 kHz
//! sound card costs one short filter instead of five long ones: each band's
//! front end then reads about 11–12 kHz audio and its resampler is a quarter of
//! the length (the resampler's length grows with the input rate for a fixed
//! transition width).

use crate::resample::bessel_i0;
use std::f64::consts::PI;

pub struct Decimator {
    factor: usize,
    /// Taps reversed, so the dot product runs oldest-to-newest on both sides.
    taps: Vec<f32>,
    /// The last `taps.len() - 1` inputs, then whatever has come since.
    hist: Vec<f32>,
    /// Inputs still to skip before the next output is due.
    skip: usize,
}

impl Decimator {
    /// Keeps one sample in `factor`, passing up to `pass_hz` and attenuating by
    /// about 70 dB from `stop_hz` (both at the input rate's scale, below its
    /// Nyquist frequency).
    pub fn new(in_rate: f64, factor: usize, pass_hz: f64, stop_hz: f64) -> Decimator {
        assert!(factor >= 1);
        assert!(pass_hz < stop_hz && stop_hz <= in_rate / 2.0 + 1e-9);
        // Kaiser's estimate for 70 dB, as in the resampler.
        let dw = 2.0 * PI * (stop_hz - pass_hz) / in_rate;
        let half = ((70.0 - 8.0) / (2.285 * dw) / 2.0).ceil() as usize + 1;
        let beta = 0.1102 * (70.0 - 8.7);
        let fc = (pass_hz + stop_hz) / 2.0 / in_rate;
        let i0b = bessel_i0(beta);
        let taps: Vec<f32> = (0..=2 * half)
            .map(|i| {
                let n = i as f64 - half as f64;
                let r = n / half as f64;
                let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
                let s = if i == half {
                    2.0 * fc
                } else {
                    (2.0 * PI * fc * n).sin() / (PI * n)
                };
                (s * w) as f32
            })
            .rev()
            .collect();
        Decimator {
            factor,
            hist: vec![0.0; taps.len() - 1],
            taps,
            skip: 0,
        }
    }

    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Group delay, input samples.
    pub fn delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    pub fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let n = self.taps.len();
        self.hist.extend_from_slice(x);
        // `hist[i .. i + n]` ends at the input that output i lines up with.
        let mut i = self.skip;
        while i + n <= self.hist.len() {
            let w = &self.hist[i..i + n];
            out.push(w.iter().zip(&self.taps).map(|(a, b)| a * b).sum());
            i += self.factor;
        }
        // Keep the last n − 1 inputs; the next output is `i` inputs in, which
        // after the drain is `i - consumed`.
        let consumed = self.hist.len() - (n - 1);
        self.skip = i - consumed;
        self.hist.drain(..consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, fs: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * f * i as f64 / fs).cos() as f32)
            .collect()
    }

    fn power(x: &[f32]) -> f32 {
        x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32
    }

    #[test]
    fn passband_is_flat_and_what_would_alias_is_down() {
        // 44.1 kHz → 11025 Hz, passing the 3 kHz profile's top edge.
        for f in [300.0, 1500.0, 3400.0] {
            let mut d = Decimator::new(44_100.0, 4, 3400.0, 5512.5);
            let mut out = Vec::new();
            d.process(&tone(f, 44_100.0, 44_100), &mut out);
            // A cosine of amplitude 1 has power 1/2: −3 dB.
            let db = 10.0 * power(&out[100..]).log10();
            assert!((db + 3.01).abs() < 0.05, "{f} Hz: {db} dB");
        }
        // 8 kHz would land on 3025 Hz, inside the 3 kHz profile.
        let mut d = Decimator::new(44_100.0, 4, 3400.0, 5512.5);
        let mut out = Vec::new();
        d.process(&tone(8000.0, 44_100.0, 44_100), &mut out);
        assert!(10.0 * power(&out[100..]).log10() < -70.0);
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let x = tone(1234.0, 48_000.0, 10_000);
        let mut whole = Vec::new();
        Decimator::new(48_000.0, 4, 3400.0, 6000.0).process(&x, &mut whole);
        let mut parts = Vec::new();
        let mut d = Decimator::new(48_000.0, 4, 3400.0, 6000.0);
        // Odd sizes, including chunks shorter than the filter and than a factor.
        for c in x.chunks(3).take(10).chain(x[30..].chunks(777)) {
            d.process(c, &mut parts);
        }
        assert_eq!(whole, parts);
        // One output every 4 inputs (the history starts as zeros, so from the first).
        assert_eq!(whole.len(), x.len().div_ceil(4));
    }
}
