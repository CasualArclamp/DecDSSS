// made by CasualArclamp
//! The audio spectrum for the band view: Welch-averaged power in the SSB
//! passband, a waterfall history and a noise-floor estimate.

use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::collections::VecDeque;
use std::sync::Arc;

/// FFT length.
const N: usize = 4096;
/// Highest frequency shown, Hz.
pub const TOP_HZ: f64 = 4000.0;
/// Waterfall rows kept.
pub const ROWS: usize = 260;
/// Rows a second.
const ROW_RATE: f64 = 8.0;

pub struct Spectrum {
    rate: f64,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pending: Vec<f32>,
    /// Power sums over the current row's frames.
    acc: Vec<f64>,
    frames: usize,
    since_row: f64,
    /// Waterfall history, dB, oldest first; each row `bins()` wide.
    pub rows: VecDeque<Vec<f32>>,
    /// Smoothed power spectrum, dB.
    pub psd: Vec<f32>,
    /// Noise floor of the passband, dB (the 15th percentile of 300–3300 Hz).
    pub floor_db: f32,
}

impl Spectrum {
    pub fn new(rate: f64) -> Spectrum {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(N);
        // Hann window.
        let window = (0..N)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos())
            .collect();
        let bins = ((TOP_HZ / rate * N as f64) as usize).max(8);
        Spectrum {
            rate,
            fft,
            window,
            pending: Vec::new(),
            acc: vec![0.0; bins],
            frames: 0,
            since_row: 0.0,
            rows: VecDeque::with_capacity(ROWS),
            psd: vec![-120.0; bins],
            floor_db: -120.0,
        }
    }

    pub fn bin_hz(&self) -> f64 {
        self.rate / N as f64
    }

    /// Feeds audio; returns true when a new waterfall row was produced.
    pub fn push(&mut self, audio: &[f32]) -> bool {
        self.pending.extend_from_slice(audio);
        let hop = N / 2;
        let mut new_row = false;
        let mut buf = vec![Complex32::new(0.0, 0.0); N];
        while self.pending.len() >= N {
            for ((b, &x), &w) in buf.iter_mut().zip(&self.pending).zip(&self.window) {
                *b = Complex32::new(x * w, 0.0);
            }
            self.fft.process(&mut buf);
            for (a, z) in self.acc.iter_mut().zip(&buf) {
                *a += f64::from(z.norm_sqr());
            }
            self.frames += 1;
            self.pending.drain(..hop);
            self.since_row += hop as f64 / self.rate;
            if self.since_row >= 1.0 / ROW_RATE {
                new_row = true;
                self.finish_row();
            }
        }
        new_row
    }

    fn finish_row(&mut self) {
        // Power per bin relative to a full-scale sine: |X|² / (Σw/2)² · 2... kept
        // simple: dB re. 1 full-scale unit, enough for a relative display.
        let norm = (N as f64 / 4.0).powi(2) * self.frames.max(1) as f64;
        let row: Vec<f32> = self
            .acc
            .iter()
            .map(|&a| (10.0 * (a / norm).max(1e-14).log10()) as f32)
            .collect();
        for (p, r) in self.psd.iter_mut().zip(&row) {
            *p += 0.3 * (r - *p);
        }
        let lo = (300.0 / self.bin_hz()) as usize;
        let hi = ((3300.0 / self.bin_hz()) as usize).min(row.len());
        let mut band: Vec<f32> = row[lo..hi].to_vec();
        band.sort_by(f32::total_cmp);
        let f = band[band.len() * 15 / 100];
        self.floor_db += 0.2 * (f - self.floor_db);
        if self.rows.len() == ROWS {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
        self.acc.iter_mut().for_each(|a| *a = 0.0);
        self.frames = 0;
        self.since_row = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_shows_at_its_bin_and_the_floor_sits_below_it() {
        let rate = 48_000.0;
        let mut sp = Spectrum::new(rate);
        let audio: Vec<f32> = (0..48_000)
            .map(|n| {
                0.3 * (std::f64::consts::TAU * 1500.0 * n as f64 / rate).sin() as f32
                    + 0.001 * ((n * 7919 % 101) as f32 / 50.0 - 1.0)
            })
            .collect();
        assert!(sp.push(&audio));
        let peak = sp
            .psd
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        let hz = peak as f64 * sp.bin_hz();
        assert!((hz - 1500.0).abs() < 2.0 * sp.bin_hz(), "{hz}");
        assert!(sp.psd[peak] > sp.floor_db + 30.0);
        assert!(sp.rows.len() >= 4);
    }
}
