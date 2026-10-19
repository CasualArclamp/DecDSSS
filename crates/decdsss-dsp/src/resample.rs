// made by CasualArclamp
//! Arbitrary-ratio resampling of complex baseband: a Kaiser-windowed sinc
//! read from a polyphase table with linear interpolation between phases.
//!
//! The receiver uses it to go from the sound card's rate to 4 samples a chip
//! after mixing a profile down to 0 Hz. The specification is loose — pass the
//! signal band, stop what would alias into it — so the filter is short.

use crate::Iq;
use std::collections::VecDeque;
use std::f64::consts::PI;

/// Table phases an input sample.
const PHASES: usize = 256;

pub struct Resampler {
    /// Half-length of the prototype, input samples.
    half: usize,
    /// g(τ) for τ = −half … +half in steps of 1/PHASES input sample, already
    /// scaled for unity passband gain.
    table: Vec<f32>,
    input: VecDeque<Iq>,
    /// Index of `input[0]`.
    base: i64,
    /// Next output instant, input samples.
    x: f64,
    /// Input samples an output sample.
    step: f64,
}

impl Resampler {
    /// Passes `pass_hz` and attenuates by about 70 dB from `stop_hz` (both
    /// one-sided, both below the input's Nyquist frequency).
    pub fn new(in_rate: f64, out_rate: f64, pass_hz: f64, stop_hz: f64) -> Resampler {
        assert!(pass_hz < stop_hz && stop_hz <= in_rate / 2.0 + 1e-9);
        // Kaiser's estimate for 70 dB: N ≈ (A − 8) / (2.285 Δω).
        let dw = 2.0 * PI * (stop_hz - pass_hz) / in_rate;
        let half = ((70.0 - 8.0) / (2.285 * dw) / 2.0).ceil() as usize + 1;
        let beta = 0.1102 * (70.0 - 8.7);
        let fc = (pass_hz + stop_hz) / 2.0 / in_rate; // cycles a sample
        let n = 2 * half * PHASES + 1;
        let i0b = bessel_i0(beta);
        let table = (0..n)
            .map(|i| {
                let tau = i as f64 / PHASES as f64 - half as f64;
                let r = tau / half as f64;
                let w = if r.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(beta * (1.0 - r * r).sqrt()) / i0b
                };
                let s = if tau == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * PI * fc * tau).sin() / (PI * tau)
                };
                (s * w) as f32
            })
            .collect();
        Resampler {
            half,
            table,
            input: VecDeque::new(),
            base: 0,
            x: 0.0,
            step: in_rate / out_rate,
        }
    }

    /// Input samples of zeros that flush the last output samples out.
    pub fn tail_len(&self) -> usize {
        self.half + 1
    }

    /// Delay through the filter, output samples.
    pub fn delay_out(&self) -> f64 {
        self.half as f64 / self.step
    }

    fn g(&self, tau: f64) -> f32 {
        let p = (tau + self.half as f64) * PHASES as f64;
        if p < 0.0 {
            return 0.0;
        }
        let i = p as usize;
        if i + 1 >= self.table.len() {
            return 0.0;
        }
        let f = (p - i as f64) as f32;
        self.table[i] * (1.0 - f) + self.table[i + 1] * f
    }

    pub fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        self.input.extend(x.iter().copied());
        let h = self.half as i64;
        let last = self.base + self.input.len() as i64 - 1;
        while self.x.floor() as i64 + h <= last {
            let c = self.x.floor() as i64;
            let mut acc = Iq::new(0.0, 0.0);
            for n in (c - h + 1).max(self.base)..=c + h {
                acc += self.input[(n - self.base) as usize] * self.g(self.x - n as f64);
            }
            out.push(acc);
            self.x += self.step;
            let keep = self.x.floor() as i64 - h + 1;
            while self.base < keep && !self.input.is_empty() {
                self.input.pop_front();
                self.base += 1;
            }
        }
    }
}

/// Resamples real audio from `in_rate` to `out_rate`, passing up to `pass_hz` and
/// stopping from `stop_hz` (both below the lower of the two Nyquist rates). The
/// output is as long as the input is, in time, and lines up with it.
pub fn resample_real(
    x: &[f32],
    in_rate: f64,
    out_rate: f64,
    pass_hz: f64,
    stop_hz: f64,
) -> Vec<f32> {
    if (in_rate - out_rate).abs() < 1e-9 {
        return x.to_vec();
    }
    let mut r = Resampler::new(in_rate, out_rate, pass_hz, stop_hz);
    let mut out: Vec<Iq> = Vec::new();
    let input: Vec<Iq> = x.iter().map(|&v| Iq::new(v, 0.0)).collect();
    r.process(&input, &mut out);
    r.process(&vec![Iq::new(0.0, 0.0); r.tail_len()], &mut out);
    let want = (x.len() as f64 * out_rate / in_rate).round() as usize;
    out.truncate(want);
    out.into_iter().map(|c| c.re).collect()
}

/// Modified Bessel function of the first kind, order 0 (power series).
pub fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, fs: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|i| {
                let a = 2.0 * PI * f * i as f64 / fs;
                Iq::new(a.cos() as f32, a.sin() as f32)
            })
            .collect()
    }

    fn power(x: &[Iq]) -> f32 {
        x.iter().map(|z| z.norm_sqr()).sum::<f32>() / x.len() as f32
    }

    #[test]
    fn passband_is_flat_and_stopband_is_down() {
        // 48 kHz → 7680 Hz (2k4 at 4 samples a chip).
        for (f, want_db, tol) in [(0.0, 0.0, 0.05), (1000.0, 0.0, 0.05), (-1200.0, 0.0, 0.05)] {
            let mut r = Resampler::new(48_000.0, 7680.0, 1200.0, 6480.0);
            let mut out = Vec::new();
            r.process(&tone(f, 48_000.0, 48_000), &mut out);
            let db = 10.0 * power(&out[200..]).log10();
            assert!((db - want_db).abs() < tol, "{f} Hz: {db} dB");
        }
        let mut r = Resampler::new(48_000.0, 7680.0, 1200.0, 6480.0);
        let mut out = Vec::new();
        r.process(&tone(8000.0, 48_000.0, 48_000), &mut out);
        assert!(10.0 * power(&out[200..]).log10() < -65.0);
    }

    #[test]
    fn output_count_follows_the_ratio() {
        let mut r = Resampler::new(44_100.0, 9600.0, 1500.0, 8100.0);
        let mut out = Vec::new();
        for chunk in tone(500.0, 44_100.0, 44_100).chunks(333) {
            r.process(chunk, &mut out);
        }
        let expect = 9600.0 - r.delay_out();
        assert!((out.len() as f64 - expect).abs() < 3.0, "{}", out.len());
    }
}
