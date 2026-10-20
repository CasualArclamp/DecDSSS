// made by CasualArclamp
//! Despreading: chips from the baseband at a tracked position, with the
//! carrier taken off and the scrambler removed; the noncoherent CSK metric.

use super::frontend::Buf;
use crate::codes::fwht;
use decdsss_dsp::Iq;
use std::f64::consts::TAU;

/// The timing and carrier state the despreader follows: where the next
/// symbol starts, how long a chip is, and the carrier's frequency and phase.
#[derive(Clone, Copy, Debug)]
pub struct Track {
    /// Absolute sample position of the next symbol's first chip, on the main
    /// path.
    pub pos: f64,
    /// Samples a chip.
    pub step: f64,
    /// Carrier offset, Hz.
    pub freq: f64,
    /// Carrier phase at `pos`, radians.
    pub phase: f64,
    /// Sample rate of the buffer, Hz.
    pub rate: f64,
}

impl Track {
    /// Radians the carrier advances in one chip.
    pub fn dphase(&self) -> f64 {
        TAU * self.freq * self.step / self.rate
    }

    /// Moves past `chips` chips: position and carrier phase advance.
    pub fn advance(&mut self, chips: usize) {
        self.pos += chips as f64 * self.step;
        self.phase = (self.phase + chips as f64 * self.dphase()).rem_euclid(TAU);
    }
}

/// Fills `out` with the despread chips of one symbol, one finger:
/// out[k] = r(pos + (k + delay)·step) · e^{-j(phase + k·dφ)} · conj(scr[k]).
/// `delay` is the finger's lag in chips; `shift` an extra offset in chips
/// (the early/late arms use ∓¼).
pub fn despread(buf: &Buf, t: &Track, scr: &[Iq], delay: f64, out: &mut [Iq]) {
    let dphi = t.dphase();
    let (s0, c0) = t.phase.sin_cos();
    let mut ph = Iq::new(c0 as f32, -(s0 as f32));
    let (ds, dc) = dphi.sin_cos();
    let rot = Iq::new(dc as f32, -(ds as f32));
    for (k, o) in out.iter_mut().enumerate() {
        let r = buf.at(t.pos + (k as f64 + delay) * t.step);
        *o = r * ph * scr[k].conj();
        ph *= rot;
    }
}

/// Sum of the despread chips: the symbol estimate of a DS-PSK symbol.
pub fn sum(u: &[Iq]) -> Iq {
    u.iter().sum::<Iq>() / u.len() as f32
}

/// The L Walsh correlations of the despread chips (one per code), unnormalised.
pub fn walsh_correlate(u: &[Iq]) -> Vec<Iq> {
    let mut w = u.to_vec();
    fwht(&mut w);
    w
}

/// A `dsc` symbol's data part: the despread chips correlated with the data's Walsh
/// code ([`decdsss_core::mode::CDM_DATA_CODE`]) over L, as [`sum`] is the pilot's
/// (code 0) over L. Both come out at the channel times the part's amplitude.
pub fn cdm_data(u: &[Iq]) -> Iq {
    walsh_one(u, decdsss_core::mode::CDM_DATA_CODE) / u.len() as f32
}

/// The correlation with one Walsh code only (for the early/late arms).
pub fn walsh_one(u: &[Iq], m: usize) -> Iq {
    u.iter()
        .enumerate()
        .map(|(k, &z)| {
            if (m & k).count_ones().is_multiple_of(2) {
                z
            } else {
                -z
            }
        })
        .sum()
}

/// ln I0(x) for x ≥ 0: the log-likelihood of a noncoherent amplitude
/// measurement. Series below 3.75, the asymptotic expansion above.
pub fn ln_i0(x: f64) -> f64 {
    if x < 3.75 {
        let q = x * x / 4.0;
        let mut term = 1.0;
        let mut sum = 1.0;
        for k in 1..30 {
            term *= q / (k * k) as f64;
            sum += term;
        }
        sum.ln()
    } else {
        // I0(x) ~ e^x/√(2πx) · (1 + 1/8x + 9/2!(8x)² + 225/3!(8x)³ + …).
        let u = 1.0 / (8.0 * x);
        x - 0.5 * (TAU * x).ln() + (1.0 + u + 4.5 * u * u + 37.5 * u * u * u).ln()
    }
}

/// Bit LLRs of one CSK symbol from per-code metrics Λ[m] (max-log): for
/// each of the `bits` bits, MSB first, max over codes with the bit 0 minus
/// max over codes with the bit 1. Positive means 0.
pub fn csk_bit_llrs(metric: &[f64], bits: usize, out: &mut Vec<f32>) {
    debug_assert_eq!(metric.len(), 1 << bits);
    for i in 0..bits {
        let shift = bits - 1 - i;
        let (mut m0, mut m1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (m, &v) in metric.iter().enumerate() {
            if (m >> shift) & 1 == 0 {
                m0 = m0.max(v);
            } else {
                m1 = m1.max(v);
            }
        }
        out.push((m0 - m1) as f32);
    }
}

/// ln Σ exp(x) without overflow.
fn log_sum_exp(x: impl Iterator<Item = f64> + Clone) -> f64 {
    let top = x.clone().fold(f64::NEG_INFINITY, f64::max);
    if !top.is_finite() {
        return top;
    }
    top + x.map(|v| (v - top).exp()).sum::<f64>().ln()
}

/// Test-only switch (environment `DECDSSS_BICSK_SIGN`) for measuring what the sign
/// change metric gains from summing over the codes: `naive` judges it on the two
/// decided codes alone, as a decision-directed receiver would, with no allowance for
/// either being wrong; `top1` sums the decided pair and the closed-form rest.
fn sign_ablation() -> Option<&'static str> {
    if !cfg!(test) {
        return None;
    }
    match std::env::var("DECDSSS_BICSK_SIGN").as_deref() {
        Ok("naive") => Some("naive"),
        Ok("top1") => Some("top1"),
        _ => None,
    }
}

/// Codes of each side that the sign-change metric sums over exactly; the rest of
/// the L² pairs are summed in closed form (see [`sign_llr`]).
const SIGN_TOP: usize = 6;

/// The `k` largest entries of `metric`, by index.
fn top_codes(metric: &[f64], k: usize) -> Vec<usize> {
    let k = k.min(metric.len());
    let mut idx: Vec<usize> = (0..metric.len()).collect();
    // Rust note: `select_nth_unstable_by` moves the k largest to the front, in no
    // particular order, without sorting the rest.
    idx.select_nth_unstable_by(k - 1, |&a, &b| metric[b].total_cmp(&metric[a]));
    idx.truncate(k);
    idx
}

/// LLR (positive: the sign is the same, bit 0) of the sign change between two
/// successive bi-orthogonal CSK symbols, from everything the two symbols' Walsh
/// correlations say, without deciding either code first.
///
/// `prev` and `cur` hold each symbol's correlations, finger after finger (`l` each);
/// `prev_metric` and `metric` are their per-code noncoherent metrics Λ[m] =
/// Σ_f ln I0(κ_f |y_f[m]|), the ones the code bits are taken from; `kappa` is κ_f =
/// 2A_f/σ_f² per finger.
///
/// *Derived.* Take codes a (before) and b (now) and a sign change d = ±1. At finger
/// f the two correlations are y₁ = h + n₁ and y₂ = d·h + n₂ with the same complex
/// channel gain h (the channel holds for one symbol), so with the phase of h unknown,
/// the likelihood against noise alone is exp(−κA) I0(κ|y₁ + d·y₂|); fingers multiply.
/// The sign change's posterior is the sum of that over all L² pairs (a, b), the same
/// for either d except at the pairs where the correlations are the signal. The pairs of
/// the `SIGN_TOP` largest codes on each side are summed with the phase; for the rest,
/// the relative phase of the two correlations averages the Bessel function to the
/// product of the two single-symbol terms (Graf's addition theorem,
/// avg_θ I0(κ|u + v·e^{jθ}|) = I0(κ|u|) I0(κ|v|)), which is exp(Λ₁[a] + Λ₂[b]) whatever
/// d is, and the sum of those over every pair is the product of the two sums of
/// exp(Λ) less the top pairs' share. Where either symbol's code is uncertain the
/// rest carries the weight, and the sign's LLR comes out small; where both are clear
/// it is the decision-directed statistic Re(Σ_f κ_f y₂ y₁*) up to its Bessel tail.
pub fn sign_llr(
    prev: &[Iq],
    cur: &[Iq],
    prev_metric: &[f64],
    metric: &[f64],
    l: usize,
    kappa: &[f64],
) -> f32 {
    let nf = prev.len() / l;
    let ablation = sign_ablation();
    let k = if ablation.is_some() { 1 } else { SIGN_TOP };
    let (ta, tb) = (top_codes(prev_metric, k), top_codes(metric, k));
    let (mut plus, mut minus, mut indep) = (Vec::new(), Vec::new(), Vec::new());
    for &a in &ta {
        for &b in &tb {
            let (mut p, mut m) = (0.0f64, 0.0f64);
            for f in 0..nf {
                let (u, v) = (prev[f * l + a], cur[f * l + b]);
                p += ln_i0(kappa[f] * f64::from((u + v).norm()));
                m += ln_i0(kappa[f] * f64::from((u - v).norm()));
            }
            plus.push(p);
            minus.push(m);
            indep.push(prev_metric[a] + metric[b]);
        }
    }
    if ablation == Some("naive") {
        // The decided codes taken as right, the rest of the pairs left out.
        return (plus[0] - minus[0]) as f32;
    }
    let all = log_sum_exp(prev_metric.iter().copied()) + log_sum_exp(metric.iter().copied());
    let top = log_sum_exp(indep.iter().copied());
    // The rest of the pairs: all less the top ones (the top ones are a part of all,
    // so the ratio is below one; held off one so a sum that is all top gives a
    // finite, tiny rest).
    let rest = all + (-(top - all).exp().min(1.0 - 1e-12)).ln_1p();
    let num = log_sum_exp([log_sum_exp(plus.iter().copied()), rest].into_iter());
    let den = log_sum_exp([log_sum_exp(minus.iter().copied()), rest].into_iter());
    (num - den) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clear_sign_change_is_a_confident_llr_and_an_unclear_code_is_not() {
        let l = 16;
        let kappa = [2.0 * 8.0 / 4.0];
        // Correlations: noise-level everywhere but the code, h = 8 at a phase.
        let h = Iq::from_polar(8.0, 0.7);
        let row = |code: usize, sign: f32, jitter: f32| -> Vec<Iq> {
            (0..l)
                .map(|m| {
                    if m == code {
                        h * sign
                    } else {
                        Iq::from_polar(0.8, 1.9 * m as f32 + jitter)
                    }
                })
                .collect()
        };
        let metric = |y: &[Iq]| -> Vec<f64> {
            y.iter()
                .map(|z| ln_i0(kappa[0] * f64::from(z.norm())))
                .collect()
        };
        let (y1, y2, y2n) = (row(3, 1.0, 0.0), row(9, 1.0, 0.4), row(9, -1.0, 0.4));
        let (m1, m2, m2n) = (metric(&y1), metric(&y2), metric(&y2n));
        let same = sign_llr(&y1, &y2, &m1, &m2, l, &kappa);
        let flip = sign_llr(&y1, &y2n, &m1, &m2n, l, &kappa);
        assert!(same > 3.0 && flip < -3.0, "{same} {flip}");
        // The same two symbols with the code of the second hidden in noise: no
        // opinion about the sign.
        let hidden = row(9, 1.0, 0.4).iter().map(|z| z * 0.1).collect::<Vec<_>>();
        let mh = metric(&hidden);
        let unsure = sign_llr(&y1, &hidden, &m1, &mh, l, &kappa);
        assert!(unsure.abs() < 0.1 * same, "{unsure} against {same}");
    }

    #[test]
    fn ln_i0_matches_the_series_across_the_switch() {
        // Reference by direct series to high order.
        let series = |x: f64| {
            let q = x * x / 4.0;
            let (mut t, mut s) = (1.0, 1.0);
            for k in 1..200 {
                t *= q / (k * k) as f64;
                s += t;
                if t < 1e-18 * s {
                    break;
                }
            }
            s.ln()
        };
        for x in [0.0, 0.5, 2.0, 3.7, 3.8, 6.0, 20.0, 80.0] {
            assert!(
                (ln_i0(x) - series(x)).abs() < 2e-3,
                "x = {x}: {} vs {}",
                ln_i0(x),
                series(x)
            );
        }
    }

    #[test]
    fn walsh_one_agrees_with_the_transform() {
        let u: Vec<Iq> = (0..64)
            .map(|k| Iq::new((k as f32 * 0.7).sin(), (k as f32 * 0.3).cos()))
            .collect();
        let w = walsh_correlate(&u);
        for m in [0, 1, 17, 63] {
            assert!((walsh_one(&u, m) - w[m]).norm() < 1e-3);
        }
    }

    #[test]
    fn csk_llrs_point_at_the_right_bits() {
        // Code 0b101 (5) of 8 wins: bits are 1, 0, 1 MSB first.
        let mut metric = vec![0.0; 8];
        metric[5] = 4.0;
        let mut out = Vec::new();
        csk_bit_llrs(&metric, 3, &mut out);
        assert!(out[0] < 0.0 && out[1] > 0.0 && out[2] < 0.0, "{out:?}");
    }
}
