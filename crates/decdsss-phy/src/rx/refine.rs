// made by CasualArclamp
//! Refinement against known chips.
//!
//! Once a header or a block has decoded, the chips that were sent are known
//! exactly. Correlating the received signal with them, a short segment at a
//! time and at a fine grid of lags, gives the timing and the carrier phase
//! of each segment far more precisely than the tracking loops; a straight
//! line through the timings gives the clock error, one through the phases
//! the carrier error. That fixes the start-up errors (a clock a few tens of
//! ppm off drifts a chip in ten seconds) before they cost a block.

use super::despread::Track;
use super::frontend::Buf;
use decdsss_dsp::Iq;
use std::f64::consts::TAU;

/// The refined state at the end of the region.
#[derive(Clone, Copy, Debug)]
pub struct Refined {
    /// Position of the first chip after the region.
    pub pos_end: f64,
    pub step: f64,
    pub freq: f64,
    /// RMS scatter of the segment timings about the line, chips.
    pub timing_rms: f64,
    pub segments: usize,
}

/// Lags probed per segment, chips.
const LAGS: [f64; 13] = [
    -0.75, -0.625, -0.5, -0.375, -0.25, -0.125, 0.0, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75,
];

/// Refines timing, clock and carrier over a region of known chips `refs`
/// that starts at `t` (position, step, frequency, phase) with the main
/// finger `lag` chips after the reference. `seg` chips are correlated
/// coherently at a time. `None` if the region is too short or the fit is
/// not credible.
pub fn refine(buf: &Buf, t: &Track, refs: &[Iq], lag: f64, seg: usize) -> Option<Refined> {
    let n_seg = refs.len() / seg;
    if n_seg < 4 {
        return None;
    }
    let dphi = t.dphase();
    // Per segment: (centre chip, timing in chips, phase, weight).
    let mut pts: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(n_seg);
    for i in 0..n_seg {
        let k0 = i * seg;
        let mut mag = [0.0f64; LAGS.len()];
        let mut val = [Iq::new(0.0, 0.0); LAGS.len()];
        for (j, &l) in LAGS.iter().enumerate() {
            let mut acc = Iq::new(0.0, 0.0);
            for (k, r) in refs[k0..k0 + seg].iter().enumerate() {
                let kk = (k0 + k) as f64;
                let (s, c) = (t.phase + kk * dphi).sin_cos();
                let x = buf.at(t.pos + (kk + lag + l) * t.step);
                acc += x * Iq::new(c as f32, -(s as f32)) * r.conj();
            }
            val[j] = acc / seg as f32;
            mag[j] = f64::from(val[j].norm());
        }
        let (jb, &best) = mag
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        if jb == 0 || jb + 1 == LAGS.len() {
            // The peak is at the edge of the grid: no credible timing here.
            continue;
        }
        let (a, c) = (mag[jb - 1], mag[jb + 1]);
        let d = a - 2.0 * best + c;
        let frac = if d.abs() < 1e-12 {
            0.0
        } else {
            (0.5 * (a - c) / d).clamp(-0.5, 0.5)
        };
        let tau = LAGS[jb] + frac * (LAGS[1] - LAGS[0]);
        pts.push((
            (k0 as f64 + seg as f64 / 2.0),
            tau,
            f64::from(val[jb].arg()),
            best * best,
        ));
    }
    if pts.len() < 4 {
        return None;
    }
    // Weighted line through the timings.
    let wsum: f64 = pts.iter().map(|p| p.3).sum();
    let kbar: f64 = pts.iter().map(|p| p.3 * p.0).sum::<f64>() / wsum;
    let tbar: f64 = pts.iter().map(|p| p.3 * p.1).sum::<f64>() / wsum;
    let sxx: f64 = pts.iter().map(|p| p.3 * (p.0 - kbar).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| p.3 * (p.0 - kbar) * (p.1 - tbar)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let b = sxy / sxx;
    let a = tbar - b * kbar;
    let rms = (pts
        .iter()
        .map(|p| p.3 * (p.1 - (a + b * p.0)).powi(2))
        .sum::<f64>()
        / wsum)
        .sqrt();
    // Weighted line through the unwrapped phases.
    let mut un = Vec::with_capacity(pts.len());
    let mut prev = pts[0].2;
    un.push(prev);
    for p in &pts[1..] {
        let mut v = p.2;
        v += TAU * ((prev - v) / TAU).round();
        un.push(v);
        prev = v;
    }
    let pbar: f64 = pts.iter().zip(&un).map(|(p, u)| p.3 * u).sum::<f64>() / wsum;
    let sxp: f64 = pts
        .iter()
        .zip(&un)
        .map(|(p, u)| p.3 * (p.0 - kbar) * (u - pbar))
        .sum();
    // rad per chip → Hz.
    let df = sxp / sxx * (t.rate / t.step) / TAU;
    if b.abs() > 3e-4 || df.abs() > 2.0 {
        return None;
    }
    let step = t.step * (1.0 + b);
    let pos0 = t.pos + (a - b * kbar) * t.step;
    Some(Refined {
        pos_end: pos0 + refs.len() as f64 * step,
        step,
        freq: t.freq + df,
        timing_rms: rms,
        segments: pts.len(),
    })
}

/// The segment length for a profile: about a quarter of a second of chips.
pub fn segment_chips(chip_rate: f64) -> usize {
    ((0.25 * chip_rate).round() as usize).clamp(64, 1024)
}
