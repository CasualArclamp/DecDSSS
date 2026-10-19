// made by CasualArclamp
//! Preamble acquisition: a parallel code-phase × frequency search.
//!
//! The preamble is a periodic m-sequence (period P chips, four samples a
//! chip after the front end). Any window of exactly one period is a cyclic
//! shift of the template, so one FFT circular correlation gives the code
//! phase for a frequency hypothesis. Hypotheses are spaced half a bin
//! (1/2T_p) apart; the half-bin ones come from a second FFT of the window
//! times e^{-jπn/N}, the integer ones from cyclic shifts of the first. The
//! power surfaces of the last `M` windows are summed (non-coherent
//! integration), the noise level is taken from the median cell, and a cell
//! over the false-alarm threshold is a detection. The method is the one DVB-CID
//! uses in DecSAT (`decsat-modem/src/cid.rs`), with a different code.

use super::frontend::{Buf, SPS};
use crate::codes::{preamble_a, preamble_len};
use decdsss_core::Profile;
use decdsss_dsp::Iq;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::sync::Arc;

/// Windows summed.
pub const MAX_WINDOWS: usize = 8;
/// Fewest windows before a detection is allowed.
pub const MIN_WINDOWS: usize = 3;
/// False-alarm probability per window (all cells together).
pub const P_FA_WINDOW: f64 = 1e-4;
/// Windows to go on accumulating after the threshold is first crossed, so
/// the first (usually partial) window does not decide the peak.
pub const SETTLE_WINDOWS: usize = 2;

/// A detection.
#[derive(Clone, Debug)]
pub struct Detection {
    /// Absolute sample index of a period boundary (a cyclic shift of A starts
    /// here, and every N samples before and after).
    pub boundary: f64,
    /// Carrier offset, Hz.
    pub freq_hz: f64,
    /// Peak over threshold, dB.
    pub margin_db: f64,
    /// The summed power surface around the peak, for display: `rows`
    /// frequency hypotheses × `cols` code phases, normalised to the noise
    /// mean, row 0 the lowest frequency.
    pub surface: Surface,
    /// Absolute index of the first sample of the oldest window used.
    pub first_sample: i64,
}

/// A small piece of the search surface for the GUI's heatmap.
#[derive(Clone, Debug, Default)]
pub struct Surface {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<f32>,
    /// Frequency of row 0 and the row spacing, Hz.
    pub freq0_hz: f64,
    pub freq_step_hz: f64,
    /// Code phase (samples) of column 0.
    pub phase0: i64,
    pub peak_row: usize,
    pub peak_col: usize,
}

pub struct Acquirer {
    pub profile: Profile,
    /// Samples in a window: one preamble period.
    n: usize,
    /// Frequency hypothesis h is at h·step Hz, for h in −half..=half.
    half: i64,
    step_hz: f64,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// conj(FFT(template)).
    tmpl: Vec<Iq>,
    /// e^{-jπn/N}.
    half_bin: Vec<Iq>,
    ring: VecDeque<Vec<f32>>,
    sum: Vec<f32>,
    /// Absolute index where the next window starts.
    next: i64,
    /// Starts of the windows in the ring.
    starts: VecDeque<i64>,
    scratch: Vec<Iq>,
    /// Windows since the threshold was first crossed.
    crossed: Option<usize>,
}

impl Acquirer {
    pub fn new(profile: Profile, max_offset_hz: f64) -> Acquirer {
        let p = preamble_len(profile);
        let n = SPS * p;
        let cr = f64::from(profile.chip_rate());
        let step_hz = cr / (2.0 * p as f64);
        let half = (max_offset_hz / step_hz).ceil() as i64;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        // Template: the chips of A as impulses, one every SPS samples.
        let a = preamble_a(profile);
        let mut t = vec![Iq::new(0.0, 0.0); n];
        for (k, &c) in a.iter().enumerate() {
            t[k * SPS] = Iq::new(c, 0.0);
        }
        fft.process(&mut t);
        let tmpl = t.iter().map(|z| z.conj()).collect();
        let half_bin = (0..n)
            .map(|i| {
                let a = -std::f64::consts::PI * i as f64 / n as f64;
                Iq::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Acquirer {
            profile,
            n,
            half,
            step_hz,
            fft,
            ifft,
            tmpl,
            half_bin,
            ring: VecDeque::new(),
            sum: Vec::new(),
            next: 0,
            starts: VecDeque::new(),
            scratch: vec![Iq::new(0.0, 0.0); n],
            crossed: None,
        }
    }

    pub fn window(&self) -> usize {
        self.n
    }

    fn rows(&self) -> usize {
        (2 * self.half + 1) as usize
    }

    /// Forget the accumulated windows (after a detection that did not pan
    /// out, or when a station has just been decoded) and restart at `from`.
    pub fn reset(&mut self, from: i64) {
        self.ring.clear();
        self.starts.clear();
        self.sum.clear();
        self.crossed = None;
        self.next = from;
    }

    /// Starts the accumulation afresh but carries on from the current
    /// position (after a detection has been handed on).
    pub fn restart(&mut self) {
        self.ring.clear();
        self.starts.clear();
        self.sum.clear();
        self.crossed = None;
    }

    /// Absolute index of the next window; everything before the oldest
    /// window in use can be released.
    pub fn oldest_needed(&self) -> i64 {
        self.starts.front().copied().unwrap_or(self.next)
    }

    /// Processes every complete window in `buf`; returns the first detection.
    pub fn run(&mut self, buf: &Buf) -> Option<Detection> {
        if self.next < buf.base {
            // The buffer moved on while the search was idle.
            self.reset(buf.base);
        }
        while self.next + self.n as i64 <= buf.end() {
            let start = self.next;
            self.next += self.n as i64;
            let mut surf = self.window_power(buf.slice(start, start + self.n as i64));
            // Each window is scaled to a unit noise mean (the median of an
            // exponential is ln 2 times its mean). A fading signal changes the
            // level from window to window, which would fatten the tail of the
            // sum well beyond the gamma law the threshold assumes.
            let mut tmp = surf.clone();
            let mid = tmp.len() / 2;
            let (_, med, _) = tmp.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
            let scale = 1.0 / (f64::from(*med) / std::f64::consts::LN_2).max(1e-30);
            for v in &mut surf {
                *v = (f64::from(*v) * scale) as f32;
            }
            if self.sum.is_empty() {
                self.sum = vec![0.0; surf.len()];
            }
            for (s, v) in self.sum.iter_mut().zip(&surf) {
                *s += v;
            }
            self.ring.push_back(surf);
            self.starts.push_back(start);
            if self.ring.len() > MAX_WINDOWS {
                let old = self.ring.pop_front().unwrap();
                self.starts.pop_front();
                for (s, v) in self.sum.iter_mut().zip(&old) {
                    *s -= v;
                }
            }
            if self.ring.len() >= MIN_WINDOWS {
                match (self.detect(start), self.crossed) {
                    (Some(_), None) => self.crossed = Some(0),
                    (Some(d), Some(n)) if n + 1 >= SETTLE_WINDOWS => {
                        self.crossed = None;
                        return Some(d);
                    }
                    (Some(_), Some(n)) => self.crossed = Some(n + 1),
                    // The peak faded back under the threshold: it was noise.
                    (None, _) => self.crossed = None,
                }
            }
        }
        None
    }

    /// |correlation|² over code phase for every frequency hypothesis, one
    /// window: `rows × n`, row-major.
    fn window_power(&mut self, x: &[Iq]) -> Vec<f32> {
        let n = self.n;
        let mut plain = x.to_vec();
        self.fft.process(&mut plain);
        let mut shifted: Vec<Iq> = x.iter().zip(&self.half_bin).map(|(a, b)| a * b).collect();
        self.fft.process(&mut shifted);
        let mut out = vec![0.0f32; self.rows() * n];
        for (r, h) in (-self.half..=self.half).enumerate() {
            // Even h: integer-bin shift of the plain FFT; odd h: of the
            // half-bin one. x·e^{-j2π(h/2)n/N}: shift by floor(h/2) bins.
            let (src, shift) = if h.rem_euclid(2) == 0 {
                (&plain, h / 2)
            } else {
                (&shifted, (h - 1) / 2)
            };
            for (i, s) in self.scratch.iter_mut().enumerate() {
                let j = (i as i64 + shift).rem_euclid(n as i64) as usize;
                *s = src[j] * self.tmpl[i];
            }
            self.ifft.process(&mut self.scratch);
            let row = &mut out[r * n..(r + 1) * n];
            for (o, c) in row.iter_mut().zip(&self.scratch) {
                // rustfft's inverse is unnormalised: divide by n.
                *o = c.norm_sqr() / (n as f32 * n as f32);
            }
        }
        out
    }

    fn detect(&self, last_start: i64) -> Option<Detection> {
        let m = self.ring.len();
        // Noise level: the median cell of a sum of m exponentials is
        // m·(1 − 1/9m)³ times the per-window mean.
        let mut tmp = self.sum.clone();
        let mid = tmp.len() / 2;
        let (_, med, _) = tmp.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        let med = f64::from(*med);
        let mf = m as f64;
        let mean = med / (mf * (1.0 - 1.0 / (9.0 * mf)).powi(3));
        if mean <= 0.0 {
            return None;
        }
        let cells = self.sum.len() as f64;
        let thr = mean * gamma_threshold(m, P_FA_WINDOW / cells);
        let (idx, peak) = self
            .sum
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, &v)| (i, f64::from(v)))?;
        if peak < thr {
            return None;
        }
        let (row, col) = (idx / self.n, idx % self.n);
        // Parabolic refinement of the peak in frequency and code phase.
        let at = |r: usize, c: usize| f64::from(self.sum[r * self.n + c % self.n]).sqrt();
        let para = |a: f64, b: f64, c: f64| {
            let d = a - 2.0 * b + c;
            if d.abs() < 1e-12 {
                0.0
            } else {
                (0.5 * (a - c) / d).clamp(-0.5, 0.5)
            }
        };
        let df = if row > 0 && row + 1 < self.rows() {
            para(at(row - 1, col), at(row, col), at(row + 1, col))
        } else {
            0.0
        };
        let dt = para(at(row, col + self.n - 1), at(row, col), at(row, col + 1));
        let h = row as i64 - self.half;
        let freq_hz = (h as f64 + df) * self.step_hz;
        let boundary = last_start as f64 + col as f64 + dt;
        // A surface slice around the peak for display: ±8 hypotheses and
        // ±3 chips, normalised by the noise mean.
        let r0 = row.saturating_sub(8);
        let r1 = (row + 9).min(self.rows());
        let span = 3 * SPS as i64;
        let cols = (2 * span + 1) as usize;
        let mut data = Vec::with_capacity((r1 - r0) * cols);
        for r in r0..r1 {
            for c in -span..=span {
                let cc = (col as i64 + c).rem_euclid(self.n as i64) as usize;
                data.push((f64::from(self.sum[r * self.n + cc]) / mean / mf) as f32);
            }
        }
        Some(Detection {
            boundary,
            freq_hz,
            margin_db: 10.0 * (peak / thr).log10(),
            surface: Surface {
                rows: r1 - r0,
                cols,
                data,
                freq0_hz: (r0 as i64 - self.half) as f64 * self.step_hz,
                freq_step_hz: self.step_hz,
                phase0: col as i64 - span,
                peak_row: row - r0,
                peak_col: span as usize,
            },
            first_sample: self.starts.front().copied().unwrap_or(last_start),
        })
    }
}

/// x with P(Gamma(m, 1) > x) = p: the tail of a sum of m unit exponentials,
/// e^{-x} Σ_{j<m} x^j / j!, solved by bisection in the log domain.
pub fn gamma_threshold(m: usize, p: f64) -> f64 {
    let tail = |x: f64| {
        let mut term = 1.0;
        let mut sum = 1.0;
        for j in 1..m {
            term *= x / j as f64;
            sum += term;
        }
        (-x).exp() * sum
    };
    let (mut lo, mut hi) = (0.0, 200.0);
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if tail(mid) > p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// e^{-j2π f t} for use by the trackers: `f` in Hz, `t` in samples at
/// `rate`.
pub fn rot(f: f64, t: f64, rate: f64) -> Iq {
    let a = -TAU * f * t / rate;
    Iq::new(a.cos() as f32, a.sin() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rx::frontend::Frontend;
    use crate::tx::{chips_to_audio, preamble_chips};
    use decdsss_chan::Channel;

    #[test]
    fn gamma_thresholds_are_right() {
        // m = 1: P(Exp > x) = e^{-x}.
        assert!((gamma_threshold(1, 1e-6) - 13.8155).abs() < 1e-3);
        // m = 2: e^{-x}(1 + x) = 1e-6 at x ≈ 16.67.
        let x = gamma_threshold(2, 1e-6);
        assert!(((-x).exp() * (1.0 + x) - 1e-6).abs() < 1e-9);
    }

    /// Runs the acquirer on a preamble in noise; returns the detection.
    fn acquire(
        profile: Profile,
        snr_db: f64,
        offset_hz: f64,
        clock_ppm: f64,
        seed: u64,
    ) -> Option<(Detection, i64)> {
        let chips = preamble_chips(profile);
        let lead = 1.0;
        let audio = chips_to_audio(&chips, profile, 48_000.0, 0.2, lead, 1.0);
        let ch = Channel {
            snr_db: Some(snr_db),
            offset_hz,
            clock_ppm,
            seed,
            ..Channel::clean(0.2, profile.bandwidth_hz())
        };
        let audio = ch.apply(&audio, 48_000.0);
        let mut fe = Frontend::new(profile, 48_000.0);
        let mut acq = Acquirer::new(profile, 100.0);
        let mut buf = Buf::default();
        for chunk in audio.chunks(4800) {
            let mut bb = Vec::new();
            fe.push(chunk, &mut bb);
            buf.push(&bb);
            if let Some(d) = acq.run(&buf) {
                // Where the first chip should sit: lead + 10-chip pulse lead.
                let cr = f64::from(profile.chip_rate());
                let first = (lead + 10.0 / cr) * fe.out_rate() + fe.delay();
                return Some((d, first.round() as i64));
            }
        }
        None
    }

    #[test]
    fn finds_a_preamble_on_every_profile() {
        for profile in Profile::ALL {
            let (d, first) = acquire(profile, -10.0, 37.0, 0.0, 3)
                .unwrap_or_else(|| panic!("{profile}: no detection"));
            let n = (SPS * crate::codes::preamble_len(profile)) as f64;
            // The boundary is a period start: first chip + k·n.
            let off = (d.boundary - first as f64).rem_euclid(n);
            let off = off.min(n - off);
            assert!(off < 1.5, "{profile}: timing off by {off} samples");
            assert!(
                (d.freq_hz - 37.0).abs() < 1.5,
                "{profile}: freq {}",
                d.freq_hz
            );
        }
    }

    #[test]
    #[ignore]
    fn debug_freq() {
        for (snr, off) in [
            (0.0, -1.0),
            (0.0, 5.0),
            (-10.0, -1.0),
            (10.0, -1.0),
            (-17.0, -1.0),
        ] {
            for seed in [507u64, 3] {
                let (d, _) = acquire(Profile::W2400, snr, off, 5.0, seed).unwrap();
                println!(
                    "snr {snr} off {off} seed {seed}: freq {:.2} margin {:.1} dB (surface peak row {} of {})",
                    d.freq_hz, d.margin_db, d.surface.peak_row, d.surface.rows
                );
            }
        }
    }

    #[test]
    fn finds_it_at_the_weakest_mode_snr() {
        // −17 dB in the profile bandwidth, with a clock error.
        let (d, first) = acquire(Profile::W2400, -17.0, -55.0, 50.0, 8).expect("no detection");
        let n = (SPS * 511) as f64;
        let off = (d.boundary - first as f64).rem_euclid(n);
        let off = off.min(n - off);
        // 50 ppm over a few seconds moves the period by under a sample.
        assert!(off < 3.0, "timing off by {off}");
        assert!((d.freq_hz + 55.0).abs() < 2.0, "freq {}", d.freq_hz);
    }

    /// False detections per minute on payload-only audio (a strong signal that
    /// is not the preamble), by channel.
    #[test]
    #[ignore]
    fn false_alarms_on_payload() {
        use crate::rx::testrig::{FS, LEVEL, channel, segment};
        use crate::tx::segment_chips;
        use decdsss_chan::Watterson;
        let profile = Profile::W2400;
        let seg = segment(profile, "ds8-1/2", 4, 6);
        let chips = segment_chips(&seg).unwrap();
        // Payload only: drop the preamble and header.
        let lead = crate::tx::lead_chips(profile);
        let audio = chips_to_audio(&chips[lead..], profile, FS, LEVEL, 0.0, 0.0);
        println!("payload-only audio: {:.0} s", audio.len() as f64 / FS);
        for (name, fading, snr) in [
            ("awgn", None, 8.0),
            ("awgn", None, 30.0),
            ("good", Some(Watterson::GOOD), 8.0),
            ("moderate", Some(Watterson::MODERATE), 8.0),
            ("poor", Some(Watterson::POOR), 8.0),
        ] {
            let mut ch = channel(profile, snr);
            ch.fading = fading;
            let a = ch.apply(&audio, FS);
            let mut fe = Frontend::new(profile, FS);
            let mut acq = Acquirer::new(profile, 100.0);
            let mut buf = Buf::default();
            let mut hits = 0;
            let mut worst = f64::MIN;
            for chunk in a.chunks(4800) {
                let mut bb = Vec::new();
                fe.push(chunk, &mut bb);
                buf.push(&bb);
                while let Some(d) = acq.run(&buf) {
                    hits += 1;
                    worst = worst.max(d.margin_db);
                    acq.reset(buf.end());
                }
                buf.trim(acq.oldest_needed());
            }
            println!(
                "{name:9} {snr:4.0} dB: {hits} false detections in {:.0} s (worst margin {worst:.1} dB)",
                a.len() as f64 / FS
            );
        }
    }

    #[test]
    fn does_not_fire_on_noise() {
        let ch = Channel {
            snr_db: Some(-10.0),
            ..Channel::clean(0.2, 2400.0)
        };
        let audio = ch.apply(&vec![0.0; 48_000 * 60], 48_000.0);
        let mut fe = Frontend::new(Profile::W2400, 48_000.0);
        let mut acq = Acquirer::new(Profile::W2400, 100.0);
        let mut buf = Buf::default();
        for chunk in audio.chunks(4800) {
            let mut bb = Vec::new();
            fe.push(chunk, &mut bb);
            buf.push(&bb);
            assert!(acq.run(&buf).is_none(), "false alarm in noise");
            buf.trim(acq.oldest_needed());
        }
    }
}
