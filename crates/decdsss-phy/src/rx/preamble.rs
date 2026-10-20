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
//!
//! **Slips.** A live stream through a browser and a virtual cable drops or
//! repeats whole blocks of audio, which moves everything after by that much:
//! the loop, a quarter chip a period at most, cannot follow. When a strong
//! preamble drops out, that period, then the next, is searched over ±[`SLIP_S`]
//! for the sequences; a lag that holds at least half the strongest period's power, with
//! the old lag empty after it and the new lag empty in the period before (a
//! fade of one path onto another fails this), is a slip. The track moves by it,
//! the period it happened in is left out of the clock line and the delay
//! profile, and the line is fitted to the boundaries with the slips taken off.

use super::frontend::{Buf, SPS};
use crate::codes::{PRE_A_PERIODS, PRE_B_PERIODS, preamble_a, preamble_b, preamble_len};
use decdsss_core::Profile;
use decdsss_dsp::Iq;

use super::acq::{Detection, rot};

/// A's periods the pattern test looks back over (before the B pair).
const LOOKBACK_A: usize = 3;
/// A period counts as signal when its correlation is at least this fraction
/// of the strongest seen (−5 dB).
const STRONG: f64 = 0.3;
/// Pattern score needed to declare the end of the preamble.
const SCORE_THRESHOLD: f64 = 0.7;
/// How far the slip search looks either way, seconds (the payload's rewind
/// searches ±60 ms too).
pub const SLIP_S: f64 = 0.06;
/// A slip must move the signal at least this many samples (half a chip);
/// anything smaller the loop follows.
const SLIP_MIN: f64 = 2.0;

/// Measurement switch (environment `DECDSSS_NO_PREAMBLE_SLIP`): no slip
/// search in the preamble. Read once.
fn no_preamble_slip() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("DECDSSS_NO_PREAMBLE_SLIP").is_some())
}

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

    /// Up to `max` fingers by deconvolution (the CLEAN algorithm): take the
    /// strongest peak, subtract the pulse's own response (the raised cosine of
    /// the chip shaping, squared, since the profile is a power) at that lag,
    /// and look again in what is left. Paths a chip apart overlap in a profile
    /// sampled this coarsely and show as one hump; their templates do not
    /// overlap once the first is taken off. A finger must stand `floor` (profile
    /// power units) and within `within_db` of the strongest; fingers closer than
    /// `sep` chips are not taken. Returns (lag in chips, power), strongest first.
    pub fn fingers(&self, max: usize, within_db: f64, floor: f32, sep: f64) -> Vec<(f64, f32)> {
        let n = self.power.len();
        let mut resid = self.power.clone();
        let mut out: Vec<(f64, f32)> = Vec::new();
        let mut top = 0.0f32;
        while out.len() < max {
            // The largest residual not within `sep` of a finger already taken.
            let Some((i, &v)) = resid
                .iter()
                .enumerate()
                .filter(|(i, _)| out.iter().all(|o| (Self::lag(*i) - o.0).abs() >= sep))
                .max_by(|a, b| a.1.total_cmp(b.1))
            else {
                break;
            };
            if out.is_empty() {
                top = v;
            }
            if v < floor || v < top * 10f32.powf(-(within_db as f32) / 10.0) {
                break;
            }
            // Quadratic through the peak and its neighbours: better than a
            // quarter chip.
            let (a, c) = (
                if i > 0 { resid[i - 1] } else { v },
                if i + 1 < n { resid[i + 1] } else { v },
            );
            let d = a - 2.0 * v + c;
            let frac = if d.abs() < 1e-12 {
                0.0
            } else {
                f64::from(0.5 * (a - c) / d).clamp(-0.5, 0.5)
            };
            let lag = Self::lag(i) + frac * Self::LAG_STEP;
            out.push((lag, v));
            for (j, r) in resid.iter_mut().enumerate() {
                *r -= v * pulse_power(Self::lag(j) - lag) as f32;
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
    /// Dominant correlation power of each period, and the largest so far.
    dom: Vec<f64>,
    pmax: f64,
    fresh: bool,
    /// Per period: the slips before its boundary, summed (a carrier step across
    /// a change of it is not taken), and whether a slip fell inside it (then it
    /// is in neither the line nor the profile, nor a carrier step).
    shift: Vec<f64>,
    split: Vec<bool>,
    /// All the slips so far, samples.
    total_shift: f64,
    /// Slips followed.
    pub slips: usize,
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
            dom: Vec::new(),
            pmax: 0.0,
            fresh: true,
            shift: Vec::new(),
            split: Vec::new(),
            total_shift: 0.0,
            slips: 0,
        }
    }

    /// Earliest sample the tracker still needs (the slip search looks a little
    /// before the oldest period).
    pub fn oldest_needed(&self) -> i64 {
        (self.bounds.first().copied().unwrap_or(self.pos) - 16.0 - SLIP_S * self.rate) as i64
    }

    /// The larger of the A and B prompt powers of the period at `pos`.
    fn power_at(&self, buf: &Buf, pos: f64, chip: f64) -> f64 {
        let (a, b) = (preamble_a(self.profile), preamble_b(self.profile));
        let pa = self.correlate(buf, a, pos, chip).norm_sqr();
        let pb = self.correlate(buf, b, pos, chip).norm_sqr();
        f64::from(pa.max(pb))
    }

    /// After a period came up weak: did the signal slip? Searches the period
    /// at `next` (the weak one or the one after it) over ±[`SLIP_S`] and returns
    /// the move, in samples.
    fn find_slip(&self, buf: &Buf, next: f64, chip: f64) -> Option<f64> {
        let prev = *self.bounds.last()?;
        let reach = (SLIP_S * self.rate).round() as i64;
        let p: Vec<f64> = (-reach..=reach)
            .map(|d| self.power_at(buf, next + d as f64, chip))
            .collect();
        let at = |d: i64| p[(d + reach) as usize];
        let (best, pk) = (-reach..=reach)
            .filter(|d| (*d as f64).abs() >= SLIP_MIN)
            .map(|d| (d, at(d)))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        // Strong enough to be the preamble, and gone from where it was.
        let old = (-1..=1).map(at).fold(0.0, f64::max);
        if pk < 0.5 * self.pmax || old > 0.25 * pk {
            return None;
        }
        // Not a path that was there all along.
        let before = self.power_at(buf, prev + best as f64, chip);
        let before0 = self.power_at(buf, prev, chip);
        if before > 0.25 * before0 {
            return None;
        }
        // Quadratic through the peak: better than a sample.
        let frac = if best > -reach && best < reach {
            let (a, c) = (at(best - 1), at(best + 1));
            let den = a - 2.0 * pk + c;
            if den.abs() > 1e-30 {
                (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            }
        } else {
            0.0
        };
        Some(best as f64 + frac)
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
            let mut ca = [-1.0, 0.0, 1.0].map(|d| self.correlate(buf, a, t + d, chip));
            let mut cb = [-1.0, 0.0, 1.0].map(|d| self.correlate(buf, b, t + d, chip));
            // A strong preamble that drops out: a fade, or a slip of the audio.
            // The search needs the next period and its reach in.
            // `slip`: the signal moved inside this period; `whole`: it had moved
            // by this period's start, so all of it is at the new lag.
            let mut slip = None;
            let weak = f64::from(ca[1].norm_sqr().max(cb[1].norm_sqr())) < STRONG * self.pmax;
            if weak && !self.fresh && !self.bounds.is_empty() && !no_preamble_slip() {
                let reach = SLIP_S * self.rate;
                if t + 2.0 * self.period + reach + 8.0 > buf.end() as f64 {
                    return State::Working;
                }
                let whole = self.find_slip(buf, t, chip);
                if whole.is_none() {
                    slip = self.find_slip(buf, t + self.period, chip);
                }
                if let Some(d) = whole.or(slip) {
                    // Judge the period at whichever lag holds more of it.
                    let na = [-1.0, 0.0, 1.0].map(|e| self.correlate(buf, a, t + d + e, chip));
                    let nb = [-1.0, 0.0, 1.0].map(|e| self.correlate(buf, b, t + d + e, chip));
                    if na[1].norm_sqr().max(nb[1].norm_sqr())
                        > ca[1].norm_sqr().max(cb[1].norm_sqr())
                    {
                        (ca, cb) = (na, nb);
                    }
                }
                if let Some(d) = whole {
                    // The period is whole at its new place: an ordinary period
                    // from there.
                    self.pos += d;
                    self.total_shift += d;
                    self.slips += 1;
                }
            }
            let (pa, pb) = (ca[1].norm_sqr(), cb[1].norm_sqr());
            let a_dom = pa >= pb;
            let c = if a_dom { ca } else { cb };
            // Delay-locked loop on the dominant sequence. (|L|−|E|)/(|L|+|E|)
            // is positive when the true boundary is later than ours; for a
            // raised-cosine correlation with ±1 sample (a quarter chip) it
            // is about 1.33 times the error in chips.
            let (e, l) = (c[0].norm(), c[2].norm());
            let d = if e + l > 0.0 { (l - e) / (e + l) } else { 0.0 };
            let err = if slip.is_some() {
                0.0
            } else {
                f64::from(d) * SPS as f64 / 1.33
            };
            // Periods before the preamble began (the search window starts a
            // little early) hold only noise: leave the loop alone for them.
            let dom = f64::from(pa.max(pb));
            self.pmax = self.pmax.max(dom);
            let strong = dom >= STRONG * self.pmax;
            let (kp, ki) = match (strong, self.fresh) {
                (false, _) => (0.0, 0.0),
                (true, true) => (0.8, 0.0),
                (true, false) => (0.5, 0.15),
            };
            if strong {
                self.fresh = false;
            }
            self.bounds.push(self.pos);
            self.shift.push(self.total_shift);
            self.split.push(slip.is_some());
            // Rust note: `if let` runs its block only when the Option holds a
            // value, bound to `d`.
            if let Some(d) = slip {
                // This boundary is before the slip, the next one after it.
                self.pos += d;
                self.total_shift += d;
                self.slips += 1;
            }
            self.errs.push(err);
            self.prompt.push((c[1], a_dom));
            self.powers.push((pa, pb));
            self.dom.push(dom);
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
        let pmax = self.dom[..=k].iter().copied().fold(0.0, f64::max);
        let good = |m: usize| self.dom[m] >= STRONG * pmax && !self.split[m];
        let used: Vec<usize> = (0..=k).filter(|&m| good(m)).collect();
        // The boundaries with the slips before them taken off lie on one line.
        let pts: Vec<(f64, f64, f64)> = used
            .iter()
            .map(|&m| {
                (
                    m as f64,
                    self.bounds[m] + self.errs[m] - self.shift[m],
                    self.dom[m],
                )
            })
            .collect();
        let wsum: f64 = pts.iter().map(|p| p.2).sum();
        let mbar = pts.iter().map(|p| p.2 * p.0).sum::<f64>() / wsum;
        let bbar = pts.iter().map(|p| p.2 * p.1).sum::<f64>() / wsum;
        let sxx: f64 = pts.iter().map(|p| p.2 * (p.0 - mbar).powi(2)).sum();
        let sxy: f64 = pts.iter().map(|p| p.2 * (p.0 - mbar) * (p.1 - bbar)).sum();
        let period = if pts.len() >= 4 && sxx > 0.0 {
            sxy / sxx
        } else {
            self.period
        };
        // Believe the fit only if it is within 300 ppm of the nominal clock.
        let period = if (period / self.n - 1.0).abs() < 3e-4 {
            period
        } else {
            self.period
        };
        let chip = period / self.p as f64;
        // The header comes after every slip so far.
        let header_start = bbar + period * (k as f64 + 1.0 - mbar) + self.total_shift;
        // Carrier: the phase step between consecutive same-template periods.
        let mut s = Iq::new(0.0, 0.0);
        for m in 1..=k {
            let (c1, d1) = self.prompt[m - 1];
            let (c2, d2) = self.prompt[m];
            // A slip between them turns the carrier phase too.
            let slipped = self.split[m - 1] || self.split[m] || self.shift[m] != self.shift[m - 1];
            if d1 == d2 && !slipped {
                s += c2 * c1.conj();
            }
        }
        let tp = period / self.rate;
        let f_res = f64::from(s.arg()) / (std::f64::consts::TAU * tp);
        // Delay profile: non-coherent sum over the A periods before the B pair.
        let a = preamble_a(self.profile);
        let mut power = vec![0.0f32; DelayProfile::LAGS];
        let first_b = k + 1 - PRE_B_PERIODS;
        for m in (0..first_b).filter(|&m| good(m)) {
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
mod pulse_tests {
    use super::*;

    fn profile_of(paths: &[(f64, f32)], noise: f32) -> DelayProfile {
        DelayProfile {
            power: (0..DelayProfile::LAGS)
                .map(|i| {
                    noise
                        + paths
                            .iter()
                            .map(|&(lag, p)| p * pulse_power(DelayProfile::lag(i) - lag) as f32)
                            .sum::<f32>()
                })
                .collect(),
        }
    }

    #[test]
    fn the_pulse_is_unity_at_zero_and_zero_a_chip_away() {
        assert!((pulse_power(0.0) - 1.0).abs() < 1e-9);
        assert!(pulse_power(1.0) < 1e-9);
        assert!((pulse_power(0.5) - 0.39).abs() < 0.02);
    }

    #[test]
    fn one_path_gives_one_finger_at_a_fractional_lag() {
        let f = profile_of(&[(1.3, 100.0)], 1.0).fingers(3, 12.0, 4.0, 0.85);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!((f[0].0 - 1.3).abs() < 0.08, "{f:?}");
    }

    #[test]
    fn two_paths_a_chip_apart_are_both_found() {
        for (a, b) in [(0.0, 0.96), (0.25, 1.15), (-2.0, -1.1)] {
            let f = profile_of(&[(a, 100.0), (b, 60.0)], 1.0).fingers(3, 12.0, 4.0, 0.85);
            assert_eq!(f.len(), 2, "{a} {b}: {f:?}");
            let mut lags: Vec<f64> = f.iter().map(|x| x.0).collect();
            lags.sort_by(f64::total_cmp);
            assert!((lags[0] - a.min(b)).abs() < 0.15, "{a} {b}: {f:?}");
            assert!((lags[1] - a.max(b)).abs() < 0.15, "{a} {b}: {f:?}");
        }
    }

    #[test]
    fn noise_alone_gives_no_finger_over_the_floor() {
        let f = profile_of(&[], 1.0).fingers(3, 12.0, 4.0, 0.85);
        assert!(f.is_empty());
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

/// Response of the correlator to one path, in power, at `t` chips from it: the
/// raised-cosine pulse of the chip shaping (roll-off 0.25, the root-raised
/// cosine at both ends), squared.
pub fn pulse_power(t: f64) -> f64 {
    pulse(t).powi(2)
}

/// The raised-cosine pulse itself (amplitude, 1 at `t` = 0, zero at the other
/// whole chips): what one path puts into the matched filter's output `t`
/// chips from its own instant.
pub fn pulse(t: f64) -> f64 {
    use decdsss_core::profile::ROLLOFF;
    let x = t.abs();
    let sinc = if x < 1e-9 {
        1.0
    } else {
        (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
    };
    let den = 1.0 - (2.0 * ROLLOFF * x).powi(2);
    if den.abs() < 1e-6 {
        // The removable singularity at 1/(2·roll-off).
        let y = 1.0 / (2.0 * ROLLOFF);
        std::f64::consts::FRAC_PI_4 * (std::f64::consts::PI * y).sin() / (std::f64::consts::PI * y)
    } else {
        sinc * (std::f64::consts::PI * ROLLOFF * x).cos() / den
    }
}
