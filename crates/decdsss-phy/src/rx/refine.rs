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
use super::header::{Finger, pick_fingers};
use super::preamble::DelayProfile;
use decdsss_dsp::Iq;
use std::f64::consts::TAU;

/// The refined state at the end of the region.
#[derive(Clone, Copy, Debug)]
pub struct Refined {
    /// Position of the region's first chip on the reference grid (the line
    /// through all the timings, for correlating the whole region).
    pub pos_start: f64,
    /// Position of the first chip after the region, anchored on the last
    /// quarter of the segments: where the next block starts matters more than
    /// where the line crosses the middle, and a wrong slope then costs little.
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
/// coherently at a time. The clock correction is limited to `max_clock`
/// (a fraction, 60e-6 for 60 ppm): with several paths the one that dominates a
/// segment changes with the fading, so the line through the timings is not the
/// clean regression its scatter suggests, and a wild slope would cost more than
/// the loops can take back. `None` if the region is too short or the fit is
/// not credible.
pub fn refine(
    buf: &Buf,
    t: &Track,
    refs: &[Iq],
    lag: f64,
    seg: usize,
    max_clock: f64,
) -> Option<Refined> {
    let n_seg = refs.len() / seg;
    if n_seg < 4 {
        return None;
    }
    let dphi = t.dphase();
    // Per segment: (centre chip, timing in chips, phase, weight). The lag
    // grid follows the timing: each segment is probed around where the last
    // one peaked, so a clock error that moves the signal several chips over the
    // region (a few hundred ppm over a header) is followed, not lost off the
    // edge of the grid.
    let mut pts: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(n_seg);
    let mut centre = 0.0f64;
    for i in 0..n_seg {
        let k0 = i * seg;
        for _try in 0..4 {
            let mut mag = [0.0f64; LAGS.len()];
            let mut val = [Iq::new(0.0, 0.0); LAGS.len()];
            for (j, &l) in LAGS.iter().enumerate() {
                let mut acc = Iq::new(0.0, 0.0);
                for (k, r) in refs[k0..k0 + seg].iter().enumerate() {
                    let kk = (k0 + k) as f64;
                    let (s, c) = (t.phase + kk * dphi).sin_cos();
                    let x = buf.at(t.pos + (kk + lag + centre + l) * t.step);
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
                // The peak is at the edge: move the grid half a chip towards
                // it and look again.
                centre += LAGS[jb] * 0.5;
                continue;
            }
            let (a, c) = (mag[jb - 1], mag[jb + 1]);
            let d = a - 2.0 * best + c;
            let frac = if d.abs() < 1e-12 {
                0.0
            } else {
                (0.5 * (a - c) / d).clamp(-0.5, 0.5)
            };
            let tau = centre + LAGS[jb] + frac * (LAGS[1] - LAGS[0]);
            pts.push((
                (k0 as f64 + seg as f64 / 2.0),
                tau,
                f64::from(val[jb].arg()),
                best * best,
            ));
            // Follow slowly: a jump to another path should not drag the grid
            // all the way over.
            centre += 0.5 * (tau - centre);
            break;
        }
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
    let b = (sxy / sxx).clamp(-max_clock, max_clock);
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
    if df.abs() > 2.0 {
        return None;
    }
    if std::env::var("DSSS_DEBUG").is_ok() {
        eprintln!(
            "refine: {} segments, clock {:+.1} ppm, rms {:.3} chip, carrier {:+.3} Hz",
            pts.len(),
            b * 1e6,
            rms,
            df
        );
    }
    let step = t.step * (1.0 + b);
    let pos0 = t.pos + (a - b * kbar) * t.step;
    // Timing at the end, from the last quarter of the segments, each carried
    // to the end along the fitted slope.
    let total = refs.len() as f64;
    let tail = (pts.len() / 4).max(3).min(pts.len());
    let last = &pts[pts.len() - tail..];
    let wl: f64 = last.iter().map(|p| p.3).sum();
    let tau_end = last
        .iter()
        .map(|p| p.3 * (p.1 + b * (total - p.0)))
        .sum::<f64>()
        / wl;
    let pos_end = t.pos + (total + tau_end) * t.step;
    Some(Refined {
        pos_start: pos0,
        pos_end,
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

/// The delay profile of a region of known chips: power against lag, from
/// correlating `seg` chips coherently at a time at each lag and summing the
/// powers. `t` is the region's start on the reference grid.
pub fn profile_known(buf: &Buf, t: &Track, refs: &[Iq], seg: usize) -> DelayProfile {
    let seg = seg.min(refs.len()).max(1);
    let dphi = t.dphase();
    let mut power = vec![0.0f32; DelayProfile::LAGS];
    for (i, pw) in power.iter_mut().enumerate() {
        let lag = DelayProfile::lag(i);
        let mut k0 = 0;
        while k0 + seg <= refs.len() {
            let mut acc = Iq::new(0.0, 0.0);
            for (k, r) in refs.iter().enumerate().skip(k0).take(seg) {
                let (s, c) = (t.phase + k as f64 * dphi).sin_cos();
                let x = buf.at(t.pos + (k as f64 + lag) * t.step);
                acc += x * Iq::new(c as f32, -(s as f32)) * r.conj();
            }
            *pw += (acc / seg as f32).norm_sqr();
            k0 += seg;
        }
    }
    DelayProfile { power }
}

/// Fingers from a profile, with the reference moved onto the strongest path:
/// returns them (lags relative to the new reference) and how many chips the
/// reference moved.
pub fn fingers_from_profile(profile: &DelayProfile) -> (Vec<Finger>, f64) {
    let mut fingers = pick_fingers(profile, 3);
    let main = fingers
        .iter()
        .max_by(|a, b| a.power.total_cmp(&b.power))
        .map_or(0.0, |f| f.lag);
    for f in &mut fingers {
        f.lag -= main;
    }
    (fingers, main)
}

/// Fingers for what comes next: the delay profile of the last few segments of
/// a region of known chips, measured on the reference grid that ends at
/// `r.pos_end`, so the lags and the end position agree. Returns the fingers
/// and the chips the reference must move onto the strongest path.
pub fn tail_fingers(
    buf: &Buf,
    r: &Refined,
    t: &Track,
    refs: &[Iq],
    seg: usize,
) -> (Vec<Finger>, f64) {
    let tail = (8 * seg).min(refs.len());
    let tt = Track {
        pos: r.pos_end - tail as f64 * r.step,
        step: r.step,
        freq: r.freq,
        phase: 0.0,
        rate: t.rate,
    };
    let profile = profile_known(buf, &tt, &refs[refs.len() - tail..], seg);
    fingers_from_profile(&profile)
}
