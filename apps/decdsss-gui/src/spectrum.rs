// made by CasualArclamp
//! The audio spectrum for the band view: the power in the SSB passband, a
//! waterfall row 60 times a second and a noise-floor estimate.

use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::sync::Arc;

/// FFT length.
const N: usize = 4096;
/// Highest frequency shown, Hz.
pub const TOP_HZ: f64 = 4000.0;
/// Waterfall rows a second: one for every frame of a 60 Hz display.
pub const ROW_RATE: f64 = 60.0;
/// Time constants, seconds, of the smoothed spectrum and of the noise floor.
const PSD_TAU_S: f64 = 0.35;
const FLOOR_TAU_S: f64 = 1.0;

pub struct Spectrum {
    bins: usize,
    rate: f64,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pending: Vec<f32>,
    /// Rows made since they were last taken: the noise floor then, and the row,
    /// dB per bin.
    new_rows: Vec<(f32, Vec<f32>)>,
    /// Smoothed power spectrum, dB.
    pub psd: Vec<f32>,
    /// Noise floor of the passband, dB (the 15th percentile of 300–3300 Hz).
    pub floor_db: f32,
    /// The first row has set the smoothed spectrum and the floor.
    primed: bool,
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
            bins,
            rate,
            fft,
            window,
            pending: Vec::new(),
            new_rows: Vec::new(),
            psd: vec![-120.0; bins],
            floor_db: -120.0,
            primed: false,
        }
    }

    pub fn bin_hz(&self) -> f64 {
        self.rate / N as f64
    }

    /// Feeds audio; returns true when new waterfall rows were made (take them
    /// with [`Spectrum::take_rows`]). A row comes from each window of `N`
    /// samples, the windows `rate / ROW_RATE` samples apart (about 80 % overlap).
    pub fn push(&mut self, audio: &[f32]) -> bool {
        self.pending.extend_from_slice(audio);
        let hop = ((self.rate / ROW_RATE).round() as usize).max(1);
        let before = self.new_rows.len();
        let mut buf = vec![Complex32::new(0.0, 0.0); N];
        while self.pending.len() >= N {
            for ((b, &x), &w) in buf.iter_mut().zip(&self.pending).zip(&self.window) {
                *b = Complex32::new(x * w, 0.0);
            }
            self.fft.process(&mut buf);
            self.pending.drain(..hop);
            self.finish_row(&buf);
        }
        self.new_rows.len() > before
    }

    /// The rows made since the last call, oldest first, each with the noise floor
    /// at the time.
    pub fn take_rows(&mut self) -> Vec<(f32, Vec<f32>)> {
        std::mem::take(&mut self.new_rows)
    }

    fn finish_row(&mut self, spectrum: &[Complex32]) {
        // Power per bin relative to a full-scale sine, dB: enough for a relative
        // display.
        let norm = (N as f64 / 4.0).powi(2);
        let row: Vec<f32> = spectrum
            .iter()
            .take(self.bins)
            .map(|z| (10.0 * (f64::from(z.norm_sqr()) / norm).max(1e-14).log10()) as f32)
            .collect();
        // The first row sets the smoothed spectrum and the floor outright, so the
        // waterfall does not start with a second of colours scaled to -120 dB.
        let a_psd = if self.primed {
            (1.0 - (-1.0 / (ROW_RATE * PSD_TAU_S)).exp()) as f32
        } else {
            1.0
        };
        for (p, r) in self.psd.iter_mut().zip(&row) {
            *p += a_psd * (r - *p);
        }
        let lo = (300.0 / self.bin_hz()) as usize;
        let hi = ((3300.0 / self.bin_hz()) as usize).min(row.len());
        let mut band: Vec<f32> = row[lo..hi].to_vec();
        band.sort_by(f32::total_cmp);
        let f = band[band.len() * 15 / 100];
        let a_floor = if self.primed {
            (1.0 - (-1.0 / (ROW_RATE * FLOOR_TAU_S)).exp()) as f32
        } else {
            1.0
        };
        self.floor_db += a_floor * (f - self.floor_db);
        self.primed = true;
        // Rows the display has not taken are capped (a hidden window).
        if self.new_rows.len() >= 600 {
            self.new_rows.remove(0);
        }
        self.new_rows.push((self.floor_db, row));
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
        // 60 rows a second: a second of audio, less the first window, makes about
        // 54 of them.
        let rows = sp.take_rows();
        assert!((50..=60).contains(&rows.len()), "{}", rows.len());
        assert!(sp.take_rows().is_empty());
        // The row at the tone's bin stands over the floor it carries.
        let (floor, row) = rows.last().unwrap();
        assert!(row[peak] > floor + 30.0);
    }
}
