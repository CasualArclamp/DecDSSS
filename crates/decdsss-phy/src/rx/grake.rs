// made by CasualArclamp
//! The generalised rake (G-RAKE) for DS-PSK: despread on a grid of delays
//! around the paths and combine with the weights that minimise the
//! impairment, instead of the rake's maximal-ratio weights on the fingers.
//!
//! Parametric G-RAKE after Bottomley, Ottosson and Wang, "A generalized RAKE
//! receiver for interference suppression", IEEE JSAC 18(8), 2000: the
//! impairment covariance is built from a model of the channel (paths, pulse
//! shape, noise level) rather than measured. Followed by despreading, a
//! chip-level LMMSE equaliser is the same linear receiver, so this is that
//! equaliser done where the receiver already works: on despread symbols.
//!
//! The model, in units where a chip has unit power and the pulse ρ (the raised
//! cosine of the chip shaping at both ends) is 1 at its own instant. The paths
//! are amplitudes a_q at lags τ_q (chips). A DS symbol d despread at a grid
//! delay δ_j gives
//!
//!   y_j = p₀(δ_j)·d + e_j,   p_m(δ) = Σ_q a_q·ρ(δ − τ_q − m).
//!
//! Chip m of the neighbours reaches delay δ through p_m. Under the scrambler
//! the neighbours are as good as random, so after despreading over L chips
//! they are noise of covariance (1/L)·Σ_{m≠0} p_m p_mᴴ. The matched filter's
//! noise adds (σ²/L)·ρ(δ_i − δ_j). With
//!
//!   Q = Σ_{m≠0} p_m p_mᴴ + σ²·C,   C_ij = ρ(δ_i − δ_j),   w = Q⁻¹ p₀,
//!
//! the combined symbol ŝ = wᴴy = γ·d + n, with γ = p₀ᴴ Q⁻¹ p₀ (the chip SINR
//! after combining) and var n = γ/L. So the QPSK bit LLRs are exactly
//! 2√2·L·Re ŝ and 2√2·L·Im ŝ, whatever γ is.
//!
//! The rake is the special case that leaves out the neighbours (Q = σ²·I on the
//! fingers alone). With two equal paths a whole chip apart, the chip one path
//! puts at the other's finger is the neighbour at full strength; maximal-ratio
//! combining then reaches a chip SIR of (|a₀|² + |a₁|²)² / (2|a₀|²|a₁|²) = 2,
//! 3 dB, however strong the signal (derived; `two_equal_paths_a_chip_apart`
//! checks it). At SF 4 that is a 9 dB symbol SINR ceiling, against about 6 dB
//! needed by DS4 2/3.

use super::preamble::pulse;
use num_complex::Complex64;

/// Spacing of the grid, chips. The signal is (1 + α)/2 = 0.625 cycles a chip
/// wide, so half a chip samples it without loss.
pub const GRID_STEP: f64 = 0.5;
/// The grid reaches this far either side of each finger, chips. A linear
/// combiner needs some length to undo two paths: two paths a chip apart, the
/// second at half the first's amplitude, 30 dB: 24.2 dB chip SINR at ±2 chips,
/// 27.9 at ±3, 28.6 at ±4 (computed from the model, `reach_curve`).
pub const GRID_REACH: f64 = 3.0;
/// At most this many grid delays (a 20-chip span at half a chip).
pub const MAX_TAPS: usize = 40;
/// Neighbouring chips modelled beyond the grid's span: the pulse's tail 6
/// chips out is under 1 % of its peak.
const ISI_REACH: f64 = 6.0;
/// Ridge on the path deconvolution: fingers closer than about half a chip
/// make it ill-conditioned.
const RIDGE: f64 = 1e-3;

/// The grid: delays (chips, on the reference's scale) spaced `GRID_STEP` and
/// aligned on the first finger, within `GRID_REACH` of any finger. If that is
/// more than `MAX_TAPS`, the delays nearest a finger are kept.
pub fn grid(lags: &[f64]) -> Vec<f64> {
    grid_reaching(lags, GRID_REACH)
}

fn grid_reaching(lags: &[f64], reach: f64) -> Vec<f64> {
    let Some(&main) = lags.first() else {
        return Vec::new();
    };
    let mut steps: Vec<i64> = Vec::new();
    for &l in lags {
        let lo = ((l - reach - main) / GRID_STEP).ceil() as i64;
        let hi = ((l + reach - main) / GRID_STEP).floor() as i64;
        steps.extend(lo..=hi);
    }
    steps.sort_unstable();
    steps.dedup();
    let mut out: Vec<f64> = steps
        .into_iter()
        .map(|j| main + j as f64 * GRID_STEP)
        .collect();
    if out.len() > MAX_TAPS {
        let near = |d: f64| lags.iter().map(|l| (d - l).abs()).fold(f64::MAX, f64::min);
        out.sort_by(|a, b| near(*a).total_cmp(&near(*b)));
        out.truncate(MAX_TAPS);
        out.sort_by(f64::total_cmp);
    }
    out
}

/// Path amplitudes from what the fingers measure: a finger at τ_f sees
/// h_f = Σ_q a_q·ρ(τ_f − τ_q), the paths' pulses overlapping. Solves that for
/// a (with a small ridge).
pub fn paths(lags: &[f64], h: &[Complex64]) -> Vec<Complex64> {
    let n = lags.len();
    let mut g = vec![Complex64::new(0.0, 0.0); n * n];
    for i in 0..n {
        for j in 0..n {
            g[i * n + j] = Complex64::new(pulse(lags[i] - lags[j]), 0.0);
        }
        g[i * n + i] += RIDGE;
    }
    cholesky_solve(&mut g, n, h).unwrap_or_else(|| h.to_vec())
}

/// The composite response p_m at each grid delay: chip `m` (0 the wanted one)
/// as it reaches the grid through the paths.
fn response(grid: &[f64], lags: &[f64], a: &[Complex64], m: f64) -> Vec<Complex64> {
    grid.iter()
        .map(|&d| {
            lags.iter()
                .zip(a)
                .map(|(&t, &aq)| aq * pulse(d - t - m))
                .sum()
        })
        .collect()
}

/// The neighbours' (m ≠ 0) power at delay `d`: Σ_{m≠0} |p_m(d)|².
pub fn isi_power(d: f64, lags: &[f64], a: &[Complex64]) -> f64 {
    let (lo, hi) = span(&[d], lags);
    (lo..=hi)
        .filter(|&m| m != 0)
        .map(|m| response(&[d], lags, a, m as f64)[0].norm_sqr())
        .sum()
}

/// Range of neighbour offsets m that reach the grid.
fn span(grid: &[f64], lags: &[f64]) -> (i64, i64) {
    let (gmin, gmax) = minmax(grid);
    let (tmin, tmax) = minmax(lags);
    (
        (gmin - tmax - ISI_REACH).floor() as i64,
        (gmax - tmin + ISI_REACH).ceil() as i64,
    )
}

fn minmax(v: &[f64]) -> (f64, f64) {
    v.iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)))
}

/// The impairment covariance Q (row-major, n × n) and p₀.
fn model(
    grid: &[f64],
    lags: &[f64],
    a: &[Complex64],
    sigma2: f64,
) -> (Vec<Complex64>, Vec<Complex64>) {
    let n = grid.len();
    let mut q = vec![Complex64::new(0.0, 0.0); n * n];
    let (lo, hi) = span(grid, lags);
    for m in (lo..=hi).filter(|&m| m != 0) {
        let p = response(grid, lags, a, m as f64);
        for i in 0..n {
            for j in 0..n {
                q[i * n + j] += p[i] * p[j].conj();
            }
        }
    }
    for i in 0..n {
        for j in 0..n {
            q[i * n + j] += sigma2 * pulse(grid[i] - grid[j]);
        }
    }
    (q, response(grid, lags, a, 0.0))
}

/// The G-RAKE weights w = Q⁻¹p₀ on `grid` for paths `a` at `lags` and matched-
/// filter noise `sigma2` (per chip), with γ = p₀ᴴw, the chip SINR they reach.
/// None if the model is degenerate (no signal, or Q not positive definite).
pub fn weights(
    grid: &[f64],
    lags: &[f64],
    a: &[Complex64],
    sigma2: f64,
) -> Option<(Vec<Complex64>, f64)> {
    if grid.is_empty() || lags.is_empty() {
        return None;
    }
    let (mut q, p0) = model(grid, lags, a, sigma2);
    let w = cholesky_solve(&mut q, grid.len(), &p0)?;
    let gamma = p0
        .iter()
        .zip(&w)
        .map(|(p, w)| p.conj() * w)
        .sum::<Complex64>()
        .re;
    (gamma.is_finite() && gamma > 0.0).then_some((w, gamma))
}

/// The chip SINR any weights `w` on `grid` reach under the model:
/// |wᴴp₀|² / wᴴQw. (For the tests, and to compare with the rake.)
pub fn sinr(grid: &[f64], lags: &[f64], a: &[Complex64], sigma2: f64, w: &[Complex64]) -> f64 {
    let n = grid.len();
    let (q, p0) = model(grid, lags, a, sigma2);
    let num = w
        .iter()
        .zip(&p0)
        .map(|(w, p)| w.conj() * p)
        .sum::<Complex64>()
        .norm_sqr();
    let mut den = Complex64::new(0.0, 0.0);
    for i in 0..n {
        for j in 0..n {
            den += w[i].conj() * q[i * n + j] * w[j];
        }
    }
    num / den.re
}

/// Solves Q x = b for Hermitian positive-definite Q (row-major n × n) by
/// Cholesky, Q = L·Lᴴ, overwriting Q's lower triangle with L. None if Q is not
/// positive definite.
pub fn cholesky_solve(q: &mut [Complex64], n: usize, b: &[Complex64]) -> Option<Vec<Complex64>> {
    for j in 0..n {
        let mut d = q[j * n + j].re;
        for k in 0..j {
            d -= q[j * n + k].norm_sqr();
        }
        if d <= 1e-12 * q[j * n + j].re.abs().max(1e-300) || !d.is_finite() {
            return None;
        }
        let d = d.sqrt();
        q[j * n + j] = Complex64::new(d, 0.0);
        for i in j + 1..n {
            let mut s = q[i * n + j];
            for k in 0..j {
                s -= q[i * n + k] * q[j * n + k].conj();
            }
            q[i * n + j] = s / d;
        }
    }
    // L y = b, then Lᴴ x = y.
    let mut x = b.to_vec();
    for i in 0..n {
        let mut s = x[i];
        for k in 0..i {
            s -= q[i * n + k] * x[k];
        }
        x[i] = s / q[i * n + i].re;
    }
    for i in (0..n).rev() {
        let mut s = x[i];
        for k in i + 1..n {
            s -= q[k * n + i].conj() * x[k];
        }
        x[i] = s / q[i * n + i].re;
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(x: f64) -> f64 {
        10.0 * x.log10()
    }

    /// The rake as weights on the grid: the composite response at the
    /// fingers, zero elsewhere (the fingers must be grid delays).
    fn rake_weights(grid: &[f64], lags: &[f64], a: &[Complex64]) -> Vec<Complex64> {
        let p0 = response(grid, lags, a, 0.0);
        grid.iter()
            .zip(p0)
            .map(|(d, p)| {
                if lags.iter().any(|l| (l - d).abs() < 1e-9) {
                    p
                } else {
                    Complex64::new(0.0, 0.0)
                }
            })
            .collect()
    }

    /// Chip SINR of the rake and of the G-RAKE at several grid reaches, two
    /// paths (the second rotated a quarter turn), from the model.
    /// `cargo test --release -p decdsss-phy reach_curve -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn reach_curve() {
        for (lags, a1) in [
            ([0.0, 1.0], 1.0),
            ([0.0, 1.0], 0.7),
            ([0.0, 1.0], 0.5),
            ([0.0, 2.0], 1.0),
            ([0.0, 0.5], 1.0),
        ] {
            let a = [Complex64::new(1.0, 0.0), Complex64::new(0.0, a1)];
            for snr in [10.0, 20.0, 30.0] {
                let s2 = 10f64.powf(-snr / 10.0);
                let g = grid_reaching(&lags, 0.0);
                let rake = sinr(&g, &lags, &a, s2, &rake_weights(&g, &lags, &a));
                let mut row = format!("{lags:?} a1 {a1} snr {snr}: rake {:5.1}", db(rake));
                for reach in [0.0, 1.0, 2.0, 3.0, 4.0, 6.0] {
                    let g = grid_reaching(&lags, reach);
                    let (_, gm) = weights(&g, &lags, &a, s2).unwrap();
                    row += &format!("  r{reach} {:5.1} ({})", db(gm), g.len());
                }
                println!("{row}");
            }
        }
    }

    #[test]
    fn cholesky_solves_a_hermitian_system() {
        let c = |re, im| Complex64::new(re, im);
        let q0 = vec![c(4.0, 0.0), c(1.0, 1.0), c(1.0, -1.0), c(3.0, 0.0)];
        let x = vec![c(0.5, -1.0), c(2.0, 0.25)];
        let b: Vec<Complex64> = (0..2)
            .map(|i| q0[i * 2] * x[0] + q0[i * 2 + 1] * x[1])
            .collect();
        let mut q = q0.clone();
        let got = cholesky_solve(&mut q, 2, &b).unwrap();
        for (g, x) in got.iter().zip(&x) {
            assert!((g - x).norm() < 1e-12, "{got:?}");
        }
    }

    #[test]
    fn paths_come_back_from_what_the_fingers_see() {
        let lags = [0.0, 0.75, 2.0];
        let a = [
            Complex64::new(1.0, 0.0),
            Complex64::new(0.3, -0.6),
            Complex64::new(-0.2, 0.1),
        ];
        let h = response(&lags, &lags, &a, 0.0);
        let back = paths(&lags, &h);
        for (b, a) in back.iter().zip(&a) {
            assert!((b - a).norm() < 0.01, "{back:?}");
        }
    }

    /// Two equal paths a whole chip apart at a high SNR: the rake stops at the
    /// derived 3 dB. The G-RAKE gets past it, though not far: with equal paths
    /// the channel has a null in the band, and a linear combiner of finite
    /// length only fills it so far (10.7 dB at 30 dB, `reach_curve`).
    #[test]
    fn two_equal_paths_a_chip_apart() {
        let lags = [0.0, 1.0];
        let a = [Complex64::new(1.0, 0.0), Complex64::new(0.0, 1.0)];
        let sigma2 = 1e-4; // 40 dB a path
        let g = grid(&lags);
        let rake = sinr(&g, &lags, &a, sigma2, &rake_weights(&g, &lags, &a));
        let (_, gamma) = weights(&g, &lags, &a, sigma2).unwrap();
        assert!((db(rake) - 3.0).abs() < 0.1, "rake {:.2} dB", db(rake));
        assert!(db(gamma) > 10.0, "G-RAKE {:.2} dB", db(gamma));
        // γ is what the weights reach.
        let (w, _) = weights(&g, &lags, &a, sigma2).unwrap();
        assert!((db(sinr(&g, &lags, &a, sigma2, &w)) - db(gamma)).abs() < 0.01);
    }

    /// Unequal paths leave no null: the G-RAKE comes near the noise.
    #[test]
    fn unequal_paths_a_chip_apart() {
        let lags = [0.0, 1.0];
        let a = [Complex64::new(1.0, 0.0), Complex64::new(0.0, 0.5)];
        let sigma2 = 1e-3;
        let g = grid(&lags);
        let rake = sinr(&g, &lags, &a, sigma2, &rake_weights(&g, &lags, &a));
        let (_, gamma) = weights(&g, &lags, &a, sigma2).unwrap();
        assert!(db(rake) < 5.5, "rake {:.2} dB", db(rake));
        assert!(db(gamma) > 26.0, "G-RAKE {:.2} dB", db(gamma));
    }

    /// One path in noise: nothing to suppress, so the G-RAKE is the matched
    /// filter, the rake's 1/σ².
    #[test]
    fn one_path_in_noise_is_the_matched_filter() {
        let lags = [0.0];
        let a = [Complex64::new(0.6, 0.8)];
        let sigma2 = 0.5;
        let g = grid(&lags);
        let (_, gamma) = weights(&g, &lags, &a, sigma2).unwrap();
        assert!(
            (db(gamma) - db(1.0 / sigma2)).abs() < 0.05,
            "{:.3} dB",
            db(gamma)
        );
    }

    #[test]
    fn grid_is_aligned_on_the_main_finger_and_capped() {
        let g = grid(&[0.3, 1.1]);
        assert!(g.iter().any(|d| (d - 0.3).abs() < 1e-12));
        assert!(g.windows(2).all(|w| (w[1] - w[0] - GRID_STEP).abs() < 1e-9));
        assert_eq!(grid(&[0.0, 10.0, 20.0]).len(), 39);
        assert_eq!(grid(&[0.0, 5.0, 11.0, 17.0]).len(), MAX_TAPS);
    }
}
