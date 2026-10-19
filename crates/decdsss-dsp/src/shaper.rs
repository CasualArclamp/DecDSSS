// made by CasualArclamp
//! The transmitter's pulse shaper and upconverter: complex chips in, real
//! audio out at any sample rate.
//!
//! Each output sample is evaluated directly as Σ c[k]·h(t − k) (t in chips),
//! with h the RRC pulse read from a finely sampled table. That way the output
//! rate does not have to be a multiple of the chip rate, and a 44.1 kHz
//! device needs no separate resampler.

use crate::Iq;
use crate::rrc::rrc_at;
use std::collections::VecDeque;
use std::f64::consts::TAU;

/// Table points a chip.
const TABLE_STEPS: usize = 512;

pub struct Shaper {
    /// One-sided pulse span, chips.
    span: usize,
    /// h(t) for t = −span … +span in steps of 1/TABLE_STEPS chip.
    table: Vec<f32>,
    /// Chips not yet fully used; `chips[0]` has index `base`.
    chips: VecDeque<Iq>,
    base: i64,
    /// Time of the next output sample, in chips (chip k peaks at t = k).
    t: f64,
    /// Chips an output sample.
    step: f64,
    /// Carrier phase, turns, and its increment a sample.
    phase: f64,
    dphase: f64,
    /// Output scale: √2 · level, so the real signal's RMS is `level` for
    /// unit-power chips.
    gain: f32,
}

impl Shaper {
    /// `chip_rate` and `sample_rate` in Hz, `centre_hz` the audio carrier,
    /// `alpha` the roll-off, `span` the one-sided pulse length in chips,
    /// `level` the output RMS for unit-power chips.
    pub fn new(
        chip_rate: f64,
        sample_rate: f64,
        centre_hz: f64,
        alpha: f64,
        span: usize,
        level: f32,
    ) -> Shaper {
        let n = 2 * span * TABLE_STEPS + 1;
        let table = (0..n)
            .map(|i| rrc_at(i as f64 / TABLE_STEPS as f64 - span as f64, alpha) as f32)
            .collect();
        Shaper {
            span,
            table,
            chips: VecDeque::new(),
            base: 0,
            // Start a span early so the first chip's pulse ramps up.
            t: -(span as f64),
            step: chip_rate / sample_rate,
            phase: 0.0,
            dphase: centre_hz / sample_rate,
            gain: std::f32::consts::SQRT_2 * level,
        }
    }

    /// h(dt), dt in chips, by linear interpolation in the table.
    fn pulse(&self, dt: f64) -> f32 {
        let x = (dt + self.span as f64) * TABLE_STEPS as f64;
        if x < 0.0 {
            return 0.0;
        }
        let i = x as usize;
        if i + 1 >= self.table.len() {
            return 0.0;
        }
        let f = (x - i as f64) as f32;
        self.table[i] * (1.0 - f) + self.table[i + 1] * f
    }

    /// Appends the chips and writes every output sample they complete.
    pub fn push(&mut self, chips: &[Iq], out: &mut Vec<f32>) {
        self.chips.extend(chips.iter().copied());
        self.run(out, false);
    }

    /// Writes the remaining samples, letting the last pulses ring out.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        self.run(out, true);
    }

    fn run(&mut self, out: &mut Vec<f32>, flushing: bool) {
        let span = self.span as i64;
        let last = self.base + self.chips.len() as i64 - 1;
        loop {
            // The sample at t needs chips up to floor(t) + span; when
            // flushing, chips past the end are zero.
            let hi = self.t.floor() as i64 + span;
            if !flushing && hi > last {
                break;
            }
            if flushing && self.t - span as f64 > last as f64 {
                break;
            }
            let lo = (self.t.ceil() as i64 - span).max(self.base);
            let mut acc = Iq::new(0.0, 0.0);
            for k in lo..=hi.min(last) {
                let h = self.pulse(self.t - k as f64);
                acc += self.chips[(k - self.base) as usize] * h;
            }
            let (s, c) = (TAU * self.phase).sin_cos();
            // Re{acc · e^{jωt}} = re·cos − im·sin.
            out.push(self.gain * (acc.re * c as f32 - acc.im * s as f32));
            self.phase = (self.phase + self.dphase).fract();
            self.t += self.step;
            // Drop chips no later sample can reach.
            let keep_from = self.t.ceil() as i64 - span;
            while self.base < keep_from && !self.chips.is_empty() {
                self.chips.pop_front();
                self.base += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_power(x: &[f32], f: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &v) in x.iter().enumerate() {
            let a = TAU * f * n as f64 / fs;
            re += v as f64 * a.cos();
            im -= v as f64 * a.sin();
        }
        (re * re + im * im) / (x.len() as f64).powi(2)
    }

    #[test]
    fn level_is_the_rms() {
        // Random QPSK chips: the real output's RMS is the requested level.
        let mut s = Shaper::new(1920.0, 48_000.0, 1500.0, 0.25, 8, 0.25);
        let mut seed = 1u32;
        let chips: Vec<Iq> = (0..20_000)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let k = std::f32::consts::FRAC_1_SQRT_2;
                Iq::new(
                    if seed & 1 == 0 { k } else { -k },
                    if seed & 2 == 0 { k } else { -k },
                )
            })
            .collect();
        let mut out = Vec::new();
        s.push(&chips, &mut out);
        let mid = &out[out.len() / 10..out.len() * 9 / 10];
        let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.25).abs() < 0.01, "rms {rms}");
    }

    #[test]
    fn spectrum_stays_in_band() {
        // A long run of random chips: power outside 300–2700 Hz is far down.
        let mut s = Shaper::new(1920.0, 48_000.0, 1500.0, 0.25, 10, 0.25);
        let mut seed = 7u32;
        let chips: Vec<Iq> = (0..4000)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                Iq::new(if seed >> 31 == 0 { 1.0 } else { -1.0 }, 0.0)
            })
            .collect();
        let mut out = Vec::new();
        s.push(&chips, &mut out);
        s.flush(&mut out);
        let inband: f64 = [800.0, 1500.0, 2200.0]
            .iter()
            .map(|&f| tone_power(&out, f, 48_000.0))
            .sum::<f64>()
            / 3.0;
        let outband: f64 = [100.0, 3200.0, 5000.0]
            .iter()
            .map(|&f| tone_power(&out, f, 48_000.0))
            .sum::<f64>()
            / 3.0;
        assert!(10.0 * (inband / outband).log10() > 35.0);
    }

    #[test]
    fn any_output_rate() {
        // 44.1 kHz: the number of samples follows the duration.
        let mut s = Shaper::new(1920.0, 44_100.0, 1500.0, 0.25, 8, 0.25);
        let mut out = Vec::new();
        s.push(&vec![Iq::new(1.0, 0.0); 1920], &mut out);
        s.flush(&mut out);
        let expect = (1920.0 + 16.0) / 1920.0 * 44_100.0;
        assert!((out.len() as f64 - expect).abs() < 30.0, "{}", out.len());
    }
}
