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

#[cfg(test)]
mod tests {
    use super::*;

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
