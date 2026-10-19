// made by CasualArclamp
//! The preamble tracker: after a detection, follows the preamble period by
//! period to find where it ends.
//!
//! Each period is correlated coherently with A and with B at early, prompt
//! and late timings (a quarter chip apart). A delay-locked loop on the
//! dominant sequence follows the period boundary and the clock; the phase
//! step between consecutive periods refines the carrier offset. The pattern
//! A…A B B is found from `z = (|c_B|² − |c_A|²) / (|c_B|² + |c_A|²)` per
//! period, which needs no noise estimate. The header starts where the last B
//! period ends.

use super::frontend::{Buf, SPS};
use crate::codes::{PRE_A_PERIODS, PRE_B_PERIODS, preamble_a, preamble_b, preamble_len};
use decdsss_core::Profile;
use decdsss_dsp::Iq;

use super::acq::{Detection, rot};

/// A's periods the pattern test looks back over (before the B pair).
const LOOKBACK_A: usize = 3;
/// Pattern score needed to declare the end of the preamble.
const SCORE_THRESHOLD: f64 = 0.7;

/// Power against delay: `power[i]` at lag `LAG0 + i·LAG_STEP` chips from the
/// main path.
#[derive(Clone, Debug, Default)]
pub struct DelayProfile {
    pub power: Vec<f32>,
}

impl DelayProfile {
    pub const LAG0: f64 = -6.0;
    pub const LAG_STEP: f64 = 0.25;
    pub const LAGS: usize = 105; // −6 … +20 chips

    pub fn lag(i: usize) -> f64 {
        Self::LAG0 + i as f64 * Self::LAG_STEP
    }

    /// Up to `max` fingers: local maxima within `within_db` of the peak and
    /// above `floor` (power units of the profile), at least `sep` chips
    /// apart, strongest first, as (lag in chips, power).
    pub fn fingers(&self, max: usize, within_db: f64, floor: f32, sep: f64) -> Vec<(f64, f32)> {
        let n = self.power.len();
        let peak = self.power.iter().copied().fold(0.0f32, f32::max);
        let thr = (peak * 10f32.powf(-(within_db as f32) / 10.0)).max(floor);
        let mut cand: Vec<(f64, f32)> = (0..n)
            .filter(|&i| {
                let p = self.power[i];
                p >= thr
                    && (i == 0 || p >= self.power[i - 1])
                    && (i + 1 == n || p > self.power[i + 1])
            })
            .map(|i| (Self::lag(i), self.power[i]))
            .collect();
        cand.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut out: Vec<(f64, f32)> = Vec::new();
        for c in cand {
            if out.len() < max && out.iter().all(|o| (o.0 - c.0).abs() >= sep) {
                out.push(c);
            }
        }
        out
    }
}

/// What the tracker found.
#[derive(Clone, Debug)]
pub struct PreambleLock {
    /// Absolute position (samples) of the header's first chip, on the main
    /// path.
    pub header_start: f64,
    /// Samples a chip, including the clock error.
    pub step: f64,
    /// Carrier offset, Hz.
    pub freq_hz: f64,
    pub profile: DelayProfile,
    /// Per-period correlation power of A and of B, relative to the noise
    /// mean of the other template, for the display.
    pub periods: Vec<(f32, f32)>,
}

pub enum State {
    Working,
    Locked(PreambleLock),
    Failed(&'static str),
}

pub struct PreambleTracker {
    profile: Profile,
    rate: f64,
    p: usize,
    /// Samples in a period at the nominal clock.
    n: f64,
    /// Reference frequency the correlations are derotated by.
    f_ref: f64,
    /// Boundary (absolute sample position) of the next period to process.
    pos: f64,
    /// Samples a period, tracked.
    period: f64,
    /// Number of periods processed.
    count: usize,
    max_periods: usize,
    z: Vec<f64>,
    scores: Vec<f64>,
    /// Boundary of each processed period.
    bounds: Vec<f64>,
    /// Prompt correlation and which template dominated, per period.
    prompt: Vec<(Iq, bool)>,
    powers: Vec<(f32, f32)>,
    /// The loop's timing error estimate of each period, samples.
    errs: Vec<f64>,
    fresh: bool,
}

impl PreambleTracker {
    /// Starts from a detection. `rate` is the front end's output rate.
    pub fn new(profile: Profile, rate: f64, det: &Detection) -> PreambleTracker {
        let p = preamble_len(profile);
        let n = (SPS * p) as f64;
        // Back up to the first period boundary in the oldest window used.
        let back = ((det.boundary - det.first_sample as f64) / n).floor();
        let pos = det.boundary - back * n;
        PreambleTracker {
            profile,
            rate,
            p,
            n,
            f_ref: det.freq_hz,
            pos,
            period: n,
            count: 0,
            max_periods: PRE_A_PERIODS + PRE_B_PERIODS + 4 + back as usize,
            z: Vec::new(),
            scores: Vec::new(),
            bounds: Vec::new(),
            prompt: Vec::new(),
            powers: Vec::new(),
            errs: Vec::new(),
            fresh: true,
        }
    }

    /// Earliest sample the tracker still needs.
    pub fn oldest_needed(&self) -> i64 {
        (self.bounds.first().copied().unwrap_or(self.pos) - 16.0) as i64
    }

    fn correlate(&self, buf: &Buf, seq: &[f32], pos: f64, step: f64) -> Iq {
        let dphi = rot(self.f_ref, step, self.rate);
        let mut ph = rot(self.f_ref, pos, self.rate);
        let mut acc = Iq::new(0.0, 0.0);
        for (k, &c) in seq.iter().enumerate() {
            acc += buf.at(pos + k as f64 * step) * ph * c;
            ph *= dphi;
            if k % 128 == 127 {
                ph /= ph.norm();
            }
        }
        acc / self.p as f32
    }

    pub fn step(&mut self, buf: &Buf) -> State {
        let (a, b) = (preamble_a(self.profile), preamble_b(self.profile));
        loop {
            // A period needs samples to its end plus interpolation margin.
            if self.pos + self.period + 8.0 > buf.end() as f64 {
                return State::Working;
            }
            if self.count >= self.max_periods {
                return State::Failed("no end-of-preamble marker");
            }
            let chip = self.period / self.p as f64;
            let t = self.pos;
            let ca = [-1.0, 0.0, 1.0].map(|d| self.correlate(buf, a, t + d, chip));
            let cb = [-1.0, 0.0, 1.0].map(|d| self.correlate(buf, b, t + d, chip));
            let (pa, pb) = (ca[1].norm_sqr(), cb[1].norm_sqr());
            let a_dom = pa >= pb;
            let c = if a_dom { ca } else { cb };
            // Delay-locked loop on the dominant sequence. (|L|−|E|)/(|L|+|E|)
            // is positive when the true boundary is later than ours; for a
            // raised-cosine correlation with ±1 sample (a quarter chip) it
            // is about 1.33 times the error in chips.
            let (e, l) = (c[0].norm(), c[2].norm());
            let d = if e + l > 0.0 { (l - e) / (e + l) } else { 0.0 };
            let err = f64::from(d) * SPS as f64 / 1.33;
            let (kp, ki) = if self.fresh { (0.8, 0.0) } else { (0.5, 0.15) };
            self.fresh = false;
            self.bounds.push(self.pos);
            self.errs.push(err);
            self.prompt.push((c[1], a_dom));
            self.powers.push((pa, pb));
            self.z
                .push(f64::from(pb - pa) / f64::from(pb + pa).max(1e-30));
            // Advance: one period plus the loop's correction. The integrator
            // learns the clock error as a change of the period length.
            self.period += ki * err;
            self.pos += self.period + kp * err;
            self.count += 1;

            // Pattern: B at m−1, m; A at m−2 … m−1−LOOKBACK_A.
            let m = self.z.len() - 1;
            let need = PRE_B_PERIODS + LOOKBACK_A;
            let mut score = f64::NEG_INFINITY;
            if self.z.len() >= need {
                let bz: f64 =
                    self.z[m + 1 - PRE_B_PERIODS..=m].iter().sum::<f64>() / PRE_B_PERIODS as f64;
                let az: f64 = self.z[m + 1 - need..m + 1 - PRE_B_PERIODS]
                    .iter()
                    .sum::<f64>()
                    / LOOKBACK_A as f64;
                score = bz - az;
            }
            self.scores.push(score);
            // Declare at a local maximum once the next period is in.
            if m >= 1 && self.scores.len() >= 3 {
                let k = self.scores.len() - 2;
                let s = self.scores[k];
                if s > SCORE_THRESHOLD && s >= self.scores[k - 1] && s > self.scores[k + 1] {
                    return State::Locked(self.finish(buf, k));
                }
            }
        }
    }

    /// The preamble ended with period index `k` (the last B period).
    fn finish(&self, buf: &Buf, k: usize) -> PreambleLock {
        // The period length and phase from a line through every period's
        // measured boundary (tracked boundary plus the loop's error estimate):
        // steadier than the loop's own running estimate.
        let pts: Vec<(f64, f64)> = (0..=k)
            .map(|m| (m as f64, self.bounds[m] + self.errs[m]))
            .collect();
        let mbar = pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64;
        let bbar = pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64;
        let sxx: f64 = pts.iter().map(|p| (p.0 - mbar).powi(2)).sum();
        let sxy: f64 = pts.iter().map(|p| (p.0 - mbar) * (p.1 - bbar)).sum();
        let period = if sxx > 0.0 { sxy / sxx } else { self.period };
        // Believe the fit only if it is within 300 ppm of the nominal clock.
        let period = if (period / self.n - 1.0).abs() < 3e-4 {
            period
        } else {
            self.period
        };
        let chip = period / self.p as f64;
        let header_start = bbar + period * (k as f64 + 1.0 - mbar);
        // Carrier: the phase step between consecutive same-template periods.
        let mut s = Iq::new(0.0, 0.0);
        for m in 1..=k {
            let (c1, d1) = self.prompt[m - 1];
            let (c2, d2) = self.prompt[m];
            if d1 == d2 {
                s += c2 * c1.conj();
            }
        }
        let tp = period / self.rate;
        let f_res = f64::from(s.arg()) / (std::f64::consts::TAU * tp);
        if std::env::var("DSSS_DEBUG").is_ok() {
            eprintln!(
                "preamble: f_ref {:.2} f_res {:.2} periods {} k {k} z {:?}",
                self.f_ref,
                f_res,
                self.count,
                self.z
                    .iter()
                    .map(|z| (z * 100.0).round() / 100.0)
                    .collect::<Vec<_>>()
            );
        }
        // Delay profile: non-coherent sum over the A periods before the B pair.
        let a = preamble_a(self.profile);
        let mut power = vec![0.0f32; DelayProfile::LAGS];
        let first_b = k + 1 - PRE_B_PERIODS;
        for m in 0..first_b {
            for (i, pw) in power.iter_mut().enumerate() {
                let lag = DelayProfile::lag(i) * chip;
                *pw += self
                    .correlate(buf, a, self.bounds[m] + lag, chip)
                    .norm_sqr();
            }
        }
        PreambleLock {
            header_start,
            step: chip,
            freq_hz: self.f_ref + f_res,
            profile: DelayProfile { power },
            periods: self.powers.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes::{PRE_A_PERIODS, PRE_B_PERIODS};
    use crate::rx::acq::Acquirer;
    use crate::rx::frontend::Frontend;
    use crate::tx::{chips_to_audio, preamble_chips};
    use decdsss_chan::{Channel, Watterson};

    /// Preamble then 6 s of silence: returns (lock, true header start) after
    /// running acquisition and tracking.
    fn lock_on(
        profile: Profile,
        snr_db: f64,
        offset_hz: f64,
        clock_ppm: f64,
        fading: Option<Watterson>,
        seed: u64,
    ) -> Option<(PreambleLock, f64)> {
        let chips = preamble_chips(profile);
        let lead = 1.0;
        let audio = chips_to_audio(&chips, profile, 48_000.0, 0.2, lead, 6.0);
        let ch = Channel {
            snr_db: Some(snr_db),
            offset_hz,
            clock_ppm,
            fading,
            seed,
            ..Channel::clean(0.2, profile.bandwidth_hz())
        };
        let audio = ch.apply(&audio, 48_000.0);
        let mut fe = Frontend::new(profile, 48_000.0);
        let mut acq = Acquirer::new(profile, 100.0);
        let mut buf = Buf::default();
        let mut tracker: Option<PreambleTracker> = None;
        let rate = fe.out_rate();
        // True position of the header's first chip (clock error scales time).
        let cr = f64::from(profile.chip_rate());
        let r = 1.0 + clock_ppm * 1e-6;
        let truth =
            ((lead + 10.0 / cr) * rate + fe.delay()) / r + chips.len() as f64 * SPS as f64 / r;
        for chunk in audio.chunks(2400) {
            let mut bb = Vec::new();
            fe.push(chunk, &mut bb);
            buf.push(&bb);
            if tracker.is_none()
                && let Some(d) = acq.run(&buf)
            {
                tracker = Some(PreambleTracker::new(profile, rate, &d));
            }
            if let Some(t) = tracker.as_mut() {
                match t.step(&buf) {
                    State::Locked(l) => return Some((l, truth)),
                    State::Failed(_) => return None,
                    State::Working => {}
                }
            }
        }
        None
    }

    #[test]
    fn finds_the_end_of_the_preamble() {
        for profile in Profile::ALL {
            let (lock, truth) = lock_on(profile, -8.0, 21.0, 0.0, None, 5)
                .unwrap_or_else(|| panic!("{profile}: no lock"));
            let err = lock.header_start - truth;
            assert!(
                err.abs() < 1.2,
                "{profile}: header start off by {err} samples"
            );
            assert!(
                (lock.freq_hz - 21.0).abs() < 0.5,
                "{profile}: freq {}",
                lock.freq_hz
            );
        }
    }

    #[test]
    fn finds_it_at_the_weakest_snr_with_clock_error() {
        let (lock, truth) = lock_on(Profile::W2400, -16.0, -42.0, 80.0, None, 11).expect("no lock");
        let err = lock.header_start - truth;
        eprintln!(
            "weak: err {err:.2} samples, freq {:.2}, step {:.5}",
            lock.freq_hz, lock.step
        );
        assert!(err.abs() < 2.5, "off by {err} samples");
        assert!((lock.freq_hz + 42.0).abs() < 1.0, "freq {}", lock.freq_hz);
        // 80 ppm: the chip is 80e-6 × 4 samples longer.
        assert!(
            (lock.step - 4.0 * (1.0 - 80e-6)).abs() < 0.002,
            "step {}",
            lock.step
        );
    }

    #[test]
    fn follows_the_preamble_through_fading() {
        let (lock, truth) = lock_on(
            Profile::W2400,
            -6.0,
            10.0,
            20.0,
            Some(Watterson::MODERATE),
            2,
        )
        .expect("no lock");
        // With two paths the lock sits on one of them: within the 1 ms delay.
        let err = lock.header_start - truth;
        eprintln!("fading: err {err:.2} samples");
        assert!(err > -2.0 && err < 12.0, "off by {err} samples");
        let f = lock.profile.fingers(3, 12.0, 0.0, 1.5);
        assert!(!f.is_empty());
        let _ = (PRE_A_PERIODS, PRE_B_PERIODS);
    }
}
