// made by CasualArclamp
//! The preamble tracker: after a detection, follows the preamble period by
//! period to find where it ends.
//!
//! Each period is correlated coherently with A and with B at early, prompt
//! and late timings (a quarter chip apart). A delay-locked loop on the
//! dominant sequence follows the period boundary and the clock; the phase
//! step between consecutive periods refines the carrier offset. The end of
//! the preamble is the period that best fits the whole pattern, twelve A then
//! two B, by the correlation powers themselves (`PreambleTracker::pattern`), so
//! a period lost in a fade counts for little either way. The header starts
//! where the last B period ends.
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

/// Periods after a candidate end the pattern test waits for: the end must fit
/// better than every one of these would.
const LOOKAHEAD: usize = 2;
/// A period counts as signal when its correlation is at least this fraction
/// of the strongest seen (−5 dB).
const STRONG: f64 = 0.3;
/// The B pair's evidence ([`PreambleTracker::pattern`], in nats) needed to end the
/// preamble there. From noise alone a period's [`glrt`] exceeds 2 when its power
/// is 4.5 times the mean or more, e^−4.5 = 1.1 % of periods (derived); a B
/// period at 6 dB of correlation SNR averages 2.4 (derived, at power 5 times the
/// noise: 5 − 1 − ln 5).
const B_EVIDENCE: f64 = 2.0;

/// The fit ([`PreambleTracker::pattern`], nats) that ends the preamble at its
/// best period when the B pair never stood out. Noise alone gives a period
/// 0.15 nats on average (derived: E[glrt] for an exponential of mean 1,
/// e^−1 − E₁(1)), so about 2 over the fourteen periods; a preamble at 6 dB of
/// correlation SNR a period about 34 (fourteen times 2.4).
const FALLBACK_FIT: f64 = 20.0;

/// Measurement switch (environment `DECDSSS_OLD_PREAMBLE_END`): end the preamble
/// by the per-period ratio test it used before 2026-10-20 (see
/// [`PreambleTracker::pattern`]). Read once.
fn old_preamble_end() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("DECDSSS_OLD_PREAMBLE_END").is_some())
}

/// The generalized log-likelihood ratio of a correlation of power `x` times
/// the noise's against noise alone, the path's gain unknown: maximised over
/// the gain, x − 1 − ln x for x > 1, and 0 below (no evidence).
fn glrt(x: f64) -> f64 {
    if x > 1.0 { x - 1.0 - x.ln() } else { 0.0 }
}

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
            max_periods: PRE_A_PERIODS + PRE_B_PERIODS + LOOKAHEAD + 4 + back as usize,
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

    /// The end of the preamble by the test used before 2026-10-20, for
    /// `DECDSSS_OLD_PREAMBLE_END`: per period z = (p_B − p_A)/(p_B + p_A); the
    /// score of period i is the mean z of the B pair to i less the mean of the
    /// three periods before; the end is a local maximum over 0.7, declared once
    /// the period after it is in.
    fn old_end(&self) -> Option<usize> {
        let z = |m: usize| {
            let (a, b) = self.powers[m];
            f64::from(b - a) / f64::from(b + a).max(1e-30)
        };
        let score = |i: usize| {
            if i + 1 < PRE_B_PERIODS + 3 {
                return f64::NEG_INFINITY;
            }
            let bz = (i + 1 - PRE_B_PERIODS..=i).map(z).sum::<f64>() / PRE_B_PERIODS as f64;
            let az = (i + 1 - PRE_B_PERIODS - 3..i + 1 - PRE_B_PERIODS)
                .map(z)
                .sum::<f64>()
                / 3.0;
            bz - az
        };
        let k = self.powers.len().checked_sub(2).filter(|&k| k >= 1)?;
        let s = score(k);
        (s > 0.7 && s >= score(k - 1) && s > score(k + 1)).then_some(k)
    }

    /// The noise power of a period's correlation with a template that is not
    /// there: the median of the weaker of the two over the periods so far,
    /// divided by ln 2 (the median of an exponential is ln 2 of its mean). In
    /// a period of A or B the weaker is the other template, noise plus their
    /// cross-correlation; in a period of noise it is the smaller of two
    /// exponentials, so this reads about half the noise there.
    fn noise(&self) -> f64 {
        let mut v: Vec<f32> = self.powers.iter().map(|&(a, b)| a.min(b)).collect();
        v.sort_by(f32::total_cmp);
        (f64::from(v[v.len() / 2]) / std::f64::consts::LN_2).max(1e-30)
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
        // Strong enough to be the preamble, gone from where it was, and clear
        // of the noise at the lags searched (near threshold the strongest
        // period itself is not far over the noise, so half of it is no test:
        // at 500 Hz, −13 dB in-band, a quarter of the locks were moved off the
        // path by the best of the noise lags).
        let old = (-1..=1).map(at).fold(0.0, f64::max);
        if pk < 0.5 * self.pmax || old > 0.25 * pk || !super::rewind::significant(&p, 1, pk) {
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
                // The B pair never stood out (it can fade away whole on one
                // path): the period that fits the whole pattern best, if the
                // A periods were there at all. A wrong guess costs a header
                // that fails its CRC, which a lost preamble costs anyway.
                if !old_preamble_end() {
                    let noise = self.noise();
                    let best = (0..self.powers.len())
                        .filter_map(|k| self.pattern(k, noise).map(|p| (k, p.0)))
                        .max_by(|a, b| a.1.total_cmp(&b.1));
                    if let Some((k, fit)) = best
                        && fit >= FALLBACK_FIT
                    {
                        return State::Locked(self.finish(buf, k));
                    }
                }
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
            // Advance: one period plus the loop's correction. The integrator
            // learns the clock error as a change of the period length.
            self.period += ki * err;
            self.pos += self.period + kp * err;
            self.count += 1;

            // The end: period k is the last B if the B pair is seen there, B
            // over A, and the pattern fits it at least as well as the period
            // before and better than the `LOOKAHEAD` after, which must be in.
            let m = self.powers.len() - 1;
            if old_preamble_end() {
                if let Some(k) = self.old_end() {
                    return State::Locked(self.finish(buf, k));
                }
            } else if m > LOOKAHEAD {
                let k = m - LOOKAHEAD;
                let noise = self.noise();
                let fit = |k: usize| self.pattern(k, noise).map_or(f64::NEG_INFINITY, |p| p.0);
                let (pa, pb) = (k.saturating_sub(1)..=k).fold((0.0, 0.0), |(a, b), i| {
                    (a + self.powers[i].0, b + self.powers[i].1)
                });
                if let Some((here, b)) = self.pattern(k, noise)
                    && b >= B_EVIDENCE
                    && pb > pa
                    && here >= fit(k - 1)
                    && (k + 1..=m).all(|j| here > fit(j))
                {
                    return State::Locked(self.finish(buf, k));
                }
            }
        }
    }

    /// The log-likelihood that period `k` is the last B period, from the whole
    /// pattern: A in the `PRE_A_PERIODS` before the B pair, B in the two to `k`,
    /// nothing known elsewhere. Each period's evidence for the template it is
    /// given is [`glrt`] of that template's correlation power over the noise
    /// `noise`, the fading gain of each period being unknown; summed over the
    /// periods, the log-likelihood of the pattern against noise alone. Also the
    /// B pair's own part. `None` before the pair could have been sent.
    ///
    /// Why not a per-period ratio, as before (`z = (p_B − p_A)/(p_B + p_A)`, the
    /// mean over the B pair less the mean over three A before it, over 0.7): the
    /// ratio of a period lost in a fade is noise, uniform on ±1, so two faded A
    /// periods after strong ones passed for the B pair three times in four
    /// (derived: p_B/(p_A + p_B) of two equal exponentials is uniform, so z is
    /// uniform on ±1; with z ≈ −1 on the A periods the pair's mean z need only
    /// exceed −0.3, and the sum of two uniforms on ±1 exceeds −0.6 with
    /// probability 1 − 1.4²/8 = 0.755); that ended the preamble a whole number of periods early on the
    /// 500 Hz profile, whose fading is flat (no second path to fill a fade in).
    /// Here a faded period adds about nothing to any candidate, and a wrong one
    /// is short of what the A or B periods it mislabels would have given.
    fn pattern(&self, k: usize, noise: f64) -> Option<(f64, f64)> {
        let first_b = (k + 1).checked_sub(PRE_B_PERIODS)?;
        if k >= self.powers.len() {
            return None;
        }
        let x = |m: usize, b: bool| {
            let (pa, pb) = self.powers[m];
            glrt(f64::from(if b { pb } else { pa }) / noise)
        };
        let b: f64 = (first_b..=k).map(|m| x(m, true)).sum();
        let a: f64 = (first_b.saturating_sub(PRE_A_PERIODS)..first_b)
            .map(|m| x(m, false))
            .sum();
        Some((a + b, b))
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
        lock_on_then(profile, &[], snr_db, offset_hz, clock_ppm, fading, seed)
    }

    /// [`lock_on`] with `after` sent straight after the preamble (a header).
    fn lock_on_then(
        profile: Profile,
        after: &[Iq],
        snr_db: f64,
        offset_hz: f64,
        clock_ppm: f64,
        fading: Option<Watterson>,
        seed: u64,
    ) -> Option<(PreambleLock, f64)> {
        let preamble = preamble_chips(profile);
        let mut chips = preamble.clone();
        chips.extend_from_slice(after);
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
            ((lead + 10.0 / cr) * rate + fe.delay()) / r + preamble.len() as f64 * SPS as f64 / r;
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

    /// Where the tracker puts the end of the preamble, against in-band SNR, on
    /// AWGN and the moderate channel, with a header after it (DSC64 1/2's, CSK256),
    /// offsets 1 ± 6 Hz and clocks 10 ± 30 ppm as `decdsss sweep --offset 1
    /// --clock-ppm 10` spreads them. Counts locks on a path (−1 to +3 chips from the
    /// first), locks a whole number of periods off (the wrong B pair), others, and
    /// no lock. Run with
    /// `cargo test -p decdsss-phy --release -- --ignored --nocapture
    /// end_of_preamble_curve`; `PREAMBLE_TRIALS` sets the trials a point (32),
    /// `PREAMBLE_SNR` one SNR only, `PREAMBLE_WORKERS` the threads; `PREAMBLE_SHOW`
    /// prints the seed of every lock that is not on a path.
    #[test]
    #[ignore]
    fn end_of_preamble_curve() {
        let trials: u64 = std::env::var("PREAMBLE_TRIALS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(32);
        for profile in [Profile::N500, Profile::W2400] {
            let h = crate::rx::testrig::header(profile, "dsc64-1/2", 2, 2);
            let header = crate::tx::header_chips(&h).unwrap();
            for (name, fading) in [("awgn", None), ("moderate", Some(Watterson::MODERATE))] {
                println!("{profile} {name}: in-band SNR: right / wrong period / other / no lock");
                for snr in [-16.0, -13.0, -10.0, -7.0, -4.0, -1.0, 2.0] {
                    if std::env::var("PREAMBLE_SNR").is_ok_and(|v| v.parse() != Ok(snr)) {
                        continue;
                    }
                    let tally = std::sync::Mutex::new([0usize; 4]);
                    let workers = std::env::var("PREAMBLE_WORKERS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| {
                            std::thread::available_parallelism().map_or(4, |n| n.get())
                        });
                    std::thread::scope(|sc| {
                        for w in 0..workers as u64 {
                            let (tally, header) = (&tally, &header);
                            sc.spawn(move || {
                                for t in (w..trials).step_by(workers) {
                                    let offset = 1.0 + (t % 7) as f64 * 2.0 - 6.0;
                                    let clock = 10.0 + (t % 5) as f64 * 15.0 - 30.0;
                                    let got = lock_on_then(
                                        profile,
                                        header,
                                        snr,
                                        offset,
                                        clock,
                                        fading,
                                        1000 + t,
                                    );
                                    let k = match got {
                                        None => 3,
                                        Some((l, truth)) => {
                                            let chips = (l.header_start - truth) / l.step;
                                            let period = preamble_len(profile) as f64;
                                            // On a path: the moderate channel's
                                            // second is 1 ms later (1.9 chips on
                                            // 2.4 kHz, 0.4 on 500 Hz).
                                            let on_path = |c: f64| c > -1.0 && c < 3.0;
                                            let periods = (chips / period).round();
                                            if on_path(chips) {
                                                0
                                            } else if periods != 0.0
                                                && on_path(chips - period * periods)
                                            {
                                                1
                                            } else {
                                                2
                                            }
                                        }
                                    };
                                    if k != 0 && std::env::var_os("PREAMBLE_SHOW").is_some() {
                                        println!(
                                            "    {profile} {name} {snr:+.0} dB seed {}: class {k}",
                                            1000 + t
                                        );
                                    }
                                    tally.lock().unwrap()[k] += 1;
                                }
                            });
                        }
                    });
                    let [right, wrong, other, none] = tally.into_inner().unwrap();
                    println!(
                        "  {snr:+5.0} dB: {right:3} / {wrong:3} / {other:3} / {none:3} of {trials}"
                    );
                }
            }
        }
    }

    /// Two transmissions on the moderate channel at −1 dB in-band whose preamble
    /// the per-period ratio test ended a period out (`end_of_preamble_curve`,
    /// seeds 1007 on 500 Hz and 1005 on 2.4 kHz, with `DECDSSS_OLD_PREAMBLE_END`):
    /// the whole-pattern fit ends both on a path.
    #[test]
    fn the_end_is_found_through_a_fade() {
        for (profile, offset, clock, seed) in [
            (Profile::N500, -5.0, 10.0, 1007),
            (Profile::W2400, 5.0, -20.0, 1005),
        ] {
            let h = crate::rx::testrig::header(profile, "dsc64-1/2", 2, 2);
            let header = crate::tx::header_chips(&h).unwrap();
            let (lock, truth) = lock_on_then(
                profile,
                &header,
                -1.0,
                offset,
                clock,
                Some(Watterson::MODERATE),
                seed,
            )
            .unwrap_or_else(|| panic!("{profile}: no lock"));
            let chips = (lock.header_start - truth) / lock.step;
            // On a path: the second is 1 ms after the first.
            assert!(
                chips > -1.0 && chips < 3.0,
                "{profile}: {chips:.1} chips out"
            );
        }
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
