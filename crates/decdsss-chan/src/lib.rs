// made by CasualArclamp
//! The channel simulator every threshold in DecDSSS is measured on. It works
//! on real audio, as a radio's audio path would see it:
//!
//! 1. sound-card clock offset (time warp by `clock_ppm`);
//! 2. Watterson HF fading: two equal-power Rayleigh paths with Gaussian
//!    Doppler spectra (CCIR Rec. 520 / ITU-R F.1487 presets);
//! 3. frequency offset and linear drift (SSB mistuning, Doppler);
//! 4. white Gaussian noise at a stated SNR in a stated bandwidth;
//!
//! and, separately ([`slip`]), the listener's own audio path skipping blocks.

use decdsss_dsp::hilbert::analytic;
use decdsss_dsp::interp::cubic;
use decdsss_dsp::rng::Rng;
use decdsss_dsp::{Iq, cis};
use std::f64::consts::{PI, TAU};

/// A two-path Watterson channel. `spread_hz` is the CCIR "frequency spread",
/// twice the standard deviation of each path's Gaussian Doppler spectrum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Watterson {
    pub delay_s: f64,
    pub spread_hz: f64,
}

impl Watterson {
    /// Mid-latitude quiet ("good"): 0.5 ms, 0.1 Hz.
    pub const GOOD: Watterson = Watterson {
        delay_s: 0.5e-3,
        spread_hz: 0.1,
    };
    /// Mid-latitude moderate: 1 ms, 0.5 Hz.
    pub const MODERATE: Watterson = Watterson {
        delay_s: 1.0e-3,
        spread_hz: 0.5,
    };
    /// Mid-latitude disturbed ("poor"): 2 ms, 1 Hz.
    pub const POOR: Watterson = Watterson {
        delay_s: 2.0e-3,
        spread_hz: 1.0,
    };
    /// Flutter: 0.5 ms, 10 Hz.
    pub const FLUTTER: Watterson = Watterson {
        delay_s: 0.5e-3,
        spread_hz: 10.0,
    };

    pub fn from_name(s: &str) -> Option<Watterson> {
        match s.trim().to_ascii_lowercase().as_str() {
            "good" | "quiet" => Some(Watterson::GOOD),
            "moderate" | "mod" => Some(Watterson::MODERATE),
            "poor" | "disturbed" => Some(Watterson::POOR),
            "flutter" => Some(Watterson::FLUTTER),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Channel {
    /// SNR in `bandwidth_hz`, dB; `None` for no noise.
    pub snr_db: Option<f64>,
    /// The bandwidth the SNR is stated in (the profile's occupied width).
    pub bandwidth_hz: f64,
    /// RMS of the clean signal while transmitting (the TX level), which the
    /// SNR is referred to.
    pub signal_rms: f64,
    pub offset_hz: f64,
    pub drift_hz_per_s: f64,
    pub fading: Option<Watterson>,
    pub clock_ppm: f64,
    pub seed: u64,
}

impl Channel {
    /// A clean channel referred to a signal of RMS `signal_rms`.
    pub fn clean(signal_rms: f64, bandwidth_hz: f64) -> Channel {
        Channel {
            snr_db: None,
            bandwidth_hz,
            signal_rms,
            offset_hz: 0.0,
            drift_hz_per_s: 0.0,
            fading: None,
            clock_ppm: 0.0,
            seed: 1,
        }
    }

    /// Runs `audio` (real, at `fs`) through the channel.
    pub fn apply(&self, audio: &[f32], fs: f64) -> Vec<f32> {
        let mut rng = Rng::new(self.seed);

        // 1. Clock: output sample n reads the input at n·(1 + ppm·1e-6).
        let warped: Vec<f32> = if self.clock_ppm == 0.0 {
            audio.to_vec()
        } else {
            let r = 1.0 + self.clock_ppm * 1e-6;
            let n = (audio.len() as f64 / r) as usize;
            let a: Vec<Iq> = audio.iter().map(|&v| Iq::new(v, 0.0)).collect();
            (0..n).map(|i| cubic(&a, i as f64 * r).re).collect()
        };

        let needs_complex =
            self.fading.is_some() || self.offset_hz != 0.0 || self.drift_hz_per_s != 0.0;
        let mut out: Vec<f32> = if needs_complex {
            let mut x = analytic(&warped, 255);
            if let Some(w) = self.fading {
                x = fade(&x, fs, w, &mut rng);
            }
            if self.offset_hz != 0.0 || self.drift_hz_per_s != 0.0 {
                for (n, z) in x.iter_mut().enumerate() {
                    let t = n as f64 / fs;
                    let turns = self.offset_hz * t + 0.5 * self.drift_hz_per_s * t * t;
                    *z *= cis((TAU * turns.fract()) as f32);
                }
            }
            x.iter().map(|z| z.re).collect()
        } else {
            warped
        };

        // 4. Noise: real white noise of variance σ² has σ²·2B/fs of its power
        // in a (one-sided) band B, so σ² = P·fs / (2·B·SNR).
        if let Some(snr_db) = self.snr_db {
            let p = self.signal_rms * self.signal_rms;
            let snr = 10f64.powf(snr_db / 10.0);
            let sigma = (p * fs / (2.0 * self.bandwidth_hz * snr)).sqrt();
            for v in &mut out {
                *v += (sigma * rng.gauss()) as f32;
            }
        }
        out
    }
}

/// Two equal-power Rayleigh paths, the second `delay_s` later, each with its
/// own tap process.
fn fade(x: &[Iq], fs: f64, w: Watterson, rng: &mut Rng) -> Vec<Iq> {
    let delay = w.delay_s * fs;
    let g0 = tap_process(x.len(), fs, w.spread_hz, rng);
    let g1 = tap_process(x.len(), fs, w.spread_hz, rng);
    let k = std::f32::consts::FRAC_1_SQRT_2;
    (0..x.len())
        .map(|n| {
            let late = cubic(x, n as f64 - delay);
            (x[n] * g0[n] + late * g1[n]) * k
        })
        .collect()
}

/// A complex Gaussian tap with E|g|² = 1 and a Gaussian Doppler power
/// spectrum of standard deviation σ = spread/2, at the audio rate.
///
/// White noise at a low rate (32σ, so the spectrum is far from Nyquist) goes
/// through a Gaussian FIR: h(t) = exp(−t²/2σt²) has power spectrum
/// exp(−f²/2σ²) with σt = 1/(2√2·πσ). The result is interpolated up.
pub fn tap_process(n: usize, fs: f64, spread_hz: f64, rng: &mut Rng) -> Vec<Iq> {
    let sigma_f = spread_hz / 2.0;
    let rate = (32.0 * sigma_f).max(2.0);
    let sigma_t = 1.0 / (2.0 * 2f64.sqrt() * PI * sigma_f);
    let half = (4.0 * sigma_t * rate).ceil() as i64;
    let h: Vec<f64> = (-half..=half)
        .map(|i| {
            let t = i as f64 / rate;
            (-t * t / (2.0 * sigma_t * sigma_t)).exp()
        })
        .collect();
    let norm = h.iter().map(|v| v * v).sum::<f64>().sqrt();
    let slow_len = (n as f64 / fs * rate).ceil() as usize + 4;
    let white: Vec<Iq> = (0..slow_len + h.len()).map(|_| rng.cgauss()).collect();
    let slow: Vec<Iq> = (0..slow_len)
        .map(|i| {
            let mut acc = Iq::new(0.0, 0.0);
            for (k, &hk) in h.iter().enumerate() {
                acc += white[i + k] * (hk / norm) as f32;
            }
            acc
        })
        .collect();
    (0..n)
        .map(|i| cubic(&slow, i as f64 / fs * rate + 1.0))
        .collect()
}

/// Audio that skips, as a live stream through a browser and a virtual cable does
/// (a KiwiSDR or WebSDR web client playing into VB-Cable): now and then a few
/// blocks are lost, or silence is played where they should be, so the signal
/// after that point arrives early or late by whole blocks. Both happen when two
/// sound clocks are bridged by a buffer that runs dry or overflows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slips {
    /// Mean time between slips, s (the gaps are exponential).
    pub every_s: f64,
    /// Samples a block (128: a Web Audio render quantum).
    pub block: usize,
    /// Each slip is 1 to `max_blocks` blocks, early or late with equal odds.
    pub max_blocks: usize,
}

/// Where a slip happened in the output of [`slip`], and by how much: samples
/// of silence inserted (positive) or input samples dropped (negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlipAt {
    pub at: usize,
    pub samples: i64,
}

/// Puts `audio` through a stream that skips as `s` says. Returns the audio and
/// the slips made.
pub fn slip(audio: &[f32], fs: f64, s: Slips, seed: u64) -> (Vec<f32>, Vec<SlipAt>) {
    let mut rng = Rng::new(seed ^ 0x5115_0000);
    let mut out = Vec::with_capacity(audio.len() + audio.len() / 50);
    let mut made = Vec::new();
    let mut i = 0usize;
    loop {
        // Exponential gap; 1 − u is in (0, 1], so its log is finite.
        let gap = (-(1.0 - rng.uniform()).ln() * s.every_s * fs) as usize;
        let end = (i + gap.max(1)).min(audio.len());
        out.extend_from_slice(&audio[i..end]);
        i = end;
        if i >= audio.len() {
            break;
        }
        let k = 1 + (rng.uniform() * s.max_blocks as f64) as usize;
        let n = k.min(s.max_blocks) * s.block;
        if rng.bit() == 1 {
            made.push(SlipAt {
                at: out.len(),
                samples: n as i64,
            });
            out.extend(std::iter::repeat_n(0.0, n));
        } else {
            made.push(SlipAt {
                at: out.len(),
                samples: -(n as i64),
            });
            i = (i + n).min(audio.len());
        }
    }
    (out, made)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slips_insert_and_drop_whole_blocks() {
        let audio: Vec<f32> = (0..480_000).map(|i| i as f32).collect();
        let s = Slips {
            every_s: 1.0,
            block: 128,
            max_blocks: 3,
        };
        let (out, made) = slip(&audio, 48_000.0, s, 7);
        assert!(made.len() > 4, "{made:?}");
        let net: i64 = made.iter().map(|m| m.samples).sum();
        assert_eq!(out.len() as i64, audio.len() as i64 + net);
        for m in &made {
            assert!(m.samples.unsigned_abs() % 128 == 0 && m.samples.unsigned_abs() <= 384);
        }
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn noise_level_matches_the_stated_snr() {
        // Silence in, noise out: its power in B = 2400 Hz of 48 kHz must be
        // P/SNR. Total power = σ², in-band fraction 2B/fs.
        let ch = Channel {
            snr_db: Some(-10.0),
            ..Channel::clean(0.1, 2400.0)
        };
        let out = ch.apply(&vec![0.0; 96_000], 48_000.0);
        let total = rms(&out).powi(2);
        let inband = total * 2.0 * 2400.0 / 48_000.0;
        // SNR −10 dB is 0.1, so the in-band noise is P / 0.1.
        let want = 0.01 / 0.1;
        assert!((10.0 * (inband / want).log10()).abs() < 0.1);
    }

    #[test]
    fn fading_keeps_average_power_and_spreads() {
        let mut rng = Rng::new(3);
        let g = tap_process(48_000 * 200, 48_000.0, 1.0, &mut rng);
        let p = g.iter().map(|z| z.norm_sqr() as f64).sum::<f64>() / g.len() as f64;
        assert!((p - 1.0).abs() < 0.15, "power {p}");
        // Rayleigh: deep fades happen (|g|² < 0.01 a few percent of the time;
        // P = 1 − e^{−0.01} ≈ 1 %).
        let deep = g.iter().filter(|z| z.norm_sqr() < 0.01).count() as f64 / g.len() as f64;
        assert!(deep > 0.002 && deep < 0.03, "deep fades {deep}");
    }

    #[test]
    fn offset_moves_a_tone() {
        let fs = 48_000.0;
        let x: Vec<f32> = (0..48_000)
            .map(|n| (TAU * 1000.0 * n as f64 / fs).cos() as f32)
            .collect();
        let ch = Channel {
            offset_hz: 37.0,
            ..Channel::clean(0.7, 2400.0)
        };
        let y = ch.apply(&x, fs);
        // Correlate against 1037 Hz: nearly all the power is there.
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &v) in y[2000..46_000].iter().enumerate() {
            let a = TAU * 1037.0 * (n + 2000) as f64 / fs;
            re += f64::from(v) * a.cos();
            im += f64::from(v) * a.sin();
        }
        let amp = 2.0 * (re * re + im * im).sqrt() / 44_000.0;
        assert!((amp - 1.0).abs() < 0.02, "{amp}");
    }
}
