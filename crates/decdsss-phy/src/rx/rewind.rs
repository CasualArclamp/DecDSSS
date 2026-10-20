// made by CasualArclamp
//! Holding on to a payload channel through a slip: a watch on what the
//! receiver collects, snapshots of the tracking state, a wide search on the
//! samples kept, and a rewind.
//!
//! The idea is BinModem's (F:\dialupmodem2, `crates/dsp/src/qam/track.rs`, same
//! author): keep a copy of the loops every so often; call the signal lost when
//! it falls well below what it had settled to; put the loops back as they were
//! before the symbols that showed it, since every loop has been learning from
//! them as though they were right; and look again on stored samples. A DecDSSS
//! channel keeps its whole block's samples, so it can do one thing more: despread
//! the symbols from the rewind point again on the corrected track, and a slip
//! found within the block costs little more than the symbols the audio lost.
//!
//! A slip is the signal going on at another lag: a sound card or a network
//! stream that dropped or repeated a buffer. Before this, one millisecond of
//! audio cut out of a DS4 transmission on a clean channel lost every block after
//! it (`payload::tests::sync_slip_curve`): the early/late loop cannot pull two
//! chips, and the block pre-pass searches −6 to +20 chips on a few pilots. A fade
//! looks the same to the watch, the energy gone from the fingers; the search
//! tells them apart, since after a slip the energy is at the fingers' pattern
//! moved, in one clean step, and in a fade it is nowhere. In a fade nothing is
//! done: the loops ride it out as they did before. (BinModem also holds its
//! loops while the signal is lost. Here that cost DS4 whole transmissions on
//! moderate fading, the early/late loop and the carrier held at the start
//! before they had settled, so it was taken out.)

/// The watch judges the energy in windows of this long, or of
/// `WATCH_MIN_SYMBOLS` symbols if that is longer.
pub const WATCH_S: f64 = 0.25;
pub const WATCH_MIN_SYMBOLS: usize = 12;
/// A window whose signal (energy over the noise) is below this share of the
/// settled signal (−5 dB) is a loss. The noise comes off first: at SF 4 a
/// despread symbol is only a few dB over it, and losing the signal takes the
/// raw energy from S + N to N, under 5 dB down.
pub const LOSS: f64 = 0.316;
/// A lost channel is back when a window reaches half the settled signal.
pub const RECOVER: f64 = 0.5;
/// How fast the settled energy follows the windows while locked.
const SETTLE: f64 = 0.1;
/// The search looks this far either way, seconds (a 2400-sample buffer at
/// 48 kHz is 50 ms).
pub const SLIP_REACH_S: f64 = 0.06;
/// A shift is taken when the fingers' pattern moved by it collects this many
/// times what it collects where it is, and at least `SLIP_MIN` of the settled
/// energy a symbol.
pub const SLIP_RATIO: f64 = 2.0;
pub const SLIP_MIN: f64 = 0.4;
/// Shifts under this (chips) are the early/late loop's business.
pub const SLIP_MIN_CHIPS: f64 = 0.5;
/// Across a slip, the lag left and the lag not yet reached hold at most this
/// share of the signal's energy.
pub const SLIP_CLEAN: f64 = 0.3;
/// Lost longer than this with no slip found, the channel takes what it has as
/// the new settled level (and stops searching).
pub const LOST_MAX_S: f64 = 4.0;
/// Snapshots kept besides the block's start: two seconds of windows.
pub const SNAPS: usize = 8;

/// What a window said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Judged {
    /// Locked: take a snapshot here.
    Locked,
    /// Just lost: search, and rewind.
    Lost,
    /// Still lost: search again.
    StillLost,
    /// Back: let the loops learn again.
    Recovered,
    /// Lost too long: carry on with what there is.
    GiveUp,
}

/// The loss, while it lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lost {
    /// Block symbol where the window that showed it began.
    pub since: usize,
    /// Windows lost.
    pub windows: usize,
}

/// The per-symbol signal the fingers collect (energy less the noise), judged
/// a window at a time against what it settled to.
#[derive(Clone, Debug, Default)]
pub struct Watch {
    acc: f64,
    acc_noise: f64,
    n: usize,
    /// Mean signal a symbol while locked.
    pub settled: Option<f64>,
    /// Mean noise a symbol collects at the fingers.
    pub noise: Option<f64>,
    pub lost: Option<Lost>,
}

impl Watch {
    /// Adds one symbol's energy at the fingers, `e`, and what the same fingers
    /// collect where there is no signal, `noise`; `s_next` is the block symbol
    /// after it. When `win` symbols have come, judges them. `max_windows` lost
    /// windows give up.
    pub fn push(
        &mut self,
        e: f64,
        noise: f64,
        win: usize,
        s_next: usize,
        max_windows: usize,
    ) -> Option<Judged> {
        self.acc += e;
        self.acc_noise += noise;
        self.n += 1;
        if self.n < win {
            return None;
        }
        let window_noise = self.acc_noise / self.n as f64;
        let noise = match self.noise {
            Some(n) => n + SETTLE * (window_noise - n),
            None => window_noise,
        };
        self.noise = Some(noise);
        let mean = (self.acc / self.n as f64 - noise).max(0.0);
        self.acc = 0.0;
        self.acc_noise = 0.0;
        self.n = 0;
        let Some(settled) = self.settled else {
            self.settled = Some(mean);
            return Some(Judged::Locked);
        };
        let follow = settled + SETTLE * (mean - settled);
        match self.lost.as_mut() {
            None if mean < LOSS * settled => {
                self.lost = Some(Lost {
                    since: s_next.saturating_sub(win),
                    windows: 1,
                });
                Some(Judged::Lost)
            }
            None => {
                self.settled = Some(follow);
                Some(Judged::Locked)
            }
            Some(_) if mean >= RECOVER * settled => {
                self.lost = None;
                self.settled = Some(follow);
                Some(Judged::Recovered)
            }
            Some(l) => {
                l.windows += 1;
                if l.windows > max_windows {
                    self.lost = None;
                    self.settled = Some(mean);
                    Some(Judged::GiveUp)
                } else {
                    Some(Judged::StillLost)
                }
            }
        }
    }

    /// Forgets a part-window (after a rewind, the symbols come again).
    pub fn restart_window(&mut self) {
        self.acc = 0.0;
        self.acc_noise = 0.0;
        self.n = 0;
    }
}

/// The shift that moves every finger onto the signal. `energy[i]` is the
/// despread power summed over the searched symbols at lag `lag0 + i·step`
/// (chips); a shift δ collects Σ_f energy(lag_f + δ). Looks within `reach`
/// chips, skipping shifts under [`SLIP_MIN_CHIPS`]. Returns (δ, what δ
/// collects, what no shift collects).
pub fn best_shift(
    energy: &[f64],
    lag0: f64,
    step: f64,
    fingers: &[f64],
    reach: f64,
) -> Option<(f64, f64, f64)> {
    let at = |lag: f64| -> f64 {
        let i = ((lag - lag0) / step).round();
        if i < 0.0 || i as usize >= energy.len() {
            0.0
        } else {
            energy[i as usize]
        }
    };
    let collect = |d: f64| fingers.iter().map(|&f| at(f + d)).sum::<f64>();
    let here = collect(0.0);
    let j = (reach / step).floor() as i64;
    (-j..=j)
        .map(|k| k as f64 * step)
        .filter(|d| d.abs() >= SLIP_MIN_CHIPS)
        .map(|d| (d, collect(d)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(d, e)| (d, e, here))
}

/// Where the signal moved: the k that makes Σ_{s<k} old[s] + Σ_{s≥k} new[s]
/// largest (`old` the energy at the fingers, `new` at the fingers shifted).
pub fn slip_at(old: &[f64], new: &[f64]) -> usize {
    debug_assert_eq!(old.len(), new.len());
    // Start with everything on the new lag, then move symbols over one by one.
    let mut best = (0usize, new.iter().sum::<f64>());
    let mut run = best.1;
    for k in 0..old.len() {
        run += old[k] - new[k];
        if run > best.1 {
            best = (k + 1, run);
        }
    }
    best.0
}

/// A slip is a step: the energy leaves the old lag and turns up at the new
/// one at the same symbol, and the new lag held nothing before. A fade of one
/// path with another path at the new lag fails the second test. With fewer than
/// half a window of symbols before the step there is nothing to test it on.
/// `noise` is what a symbol collects at the fingers from noise alone; it is
/// taken off first (at SF 4 a despread symbol is only a few dB over it).
pub fn is_clean_slip(old: &[f64], new: &[f64], split: usize, win: usize, noise: f64) -> bool {
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            (v.iter().sum::<f64>() / v.len() as f64 - noise).max(0.0)
        }
    };
    let (old_before, new_before) = (mean(&old[..split]), mean(&new[..split]));
    let (old_after, new_after) = (mean(&old[split..]), mean(&new[split..]));
    new_after > 0.0
        && old_after <= SLIP_CLEAN * new_after
        && (split < win / 2 || new_before <= SLIP_CLEAN * old_before)
}

/// How far above the noise, in robust standard deviations, what a shift
/// collects must stand. The search tries about a thousand lags, so a few sigma
/// would be passed by chance near threshold; and a sum over a few CSK symbols
/// has a longer tail than a Gaussian.
pub const SLIP_SIGMAS: f64 = 8.0;

/// Whether `got`, what a shift collects over `fingers` lags of `energy` (the
/// search's energy at each lag), stands clear of what noise collects: most lags
/// hold no signal, so the median and the median absolute deviation of `energy`
/// are the noise's level and spread (1.4826 MAD is one standard deviation of a
/// Gaussian). Near threshold a window of the watch dips by chance, and without
/// this the best of a thousand noise lags was taken for the signal.
pub fn significant(energy: &[f64], fingers: usize, got: f64) -> bool {
    if energy.len() < 8 {
        return false;
    }
    let mut v = energy.to_vec();
    v.sort_by(f64::total_cmp);
    let med = v[v.len() / 2];
    let mut dev: Vec<f64> = v.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(f64::total_cmp);
    let sigma = 1.4826 * dev[dev.len() / 2];
    let n = fingers.max(1) as f64;
    got > n * med + SLIP_SIGMAS * sigma * n.sqrt()
}

/// The decided code's power, less what noise alone would give the largest of
/// the codes (H_L times the mean of the others): the signal a CSK symbol holds
/// at one lag, about zero where there is none.
pub fn csk_excess(w: &[decdsss_dsp::Iq], h_l: f64) -> f64 {
    let (mut top, mut total) = (0.0f64, 0.0f64);
    for z in w {
        let p = f64::from(z.norm_sqr());
        top = top.max(p);
        total += p;
    }
    let others = (total - top) / (w.len() as f64 - 1.0).max(1.0);
    top - h_l * others
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_well_down_is_a_loss_and_half_back_recovers() {
        let mut w = Watch::default();
        let mut judged = Vec::new();
        let mut s = 0;
        for e in [1.0, 1.0, 1.0, 0.2, 0.2, 0.6, 1.0] {
            for _ in 0..10 {
                s += 1;
                if let Some(j) = w.push(e, 0.0, 10, s, 4) {
                    judged.push(j);
                }
            }
        }
        use Judged::*;
        assert_eq!(
            judged,
            [Locked, Locked, Locked, Lost, StillLost, Recovered, Locked]
        );
    }

    #[test]
    fn the_noise_comes_off_before_judging() {
        // Signal 1 over noise 2: the raw energy drops from 3 to 2 (−1.8 dB) when
        // the signal goes, which is the whole signal.
        let mut w = Watch::default();
        assert_eq!(w.push(3.0, 2.0, 1, 1, 4), Some(Judged::Locked));
        assert_eq!(w.push(2.0, 2.0, 1, 2, 4), Some(Judged::Lost));
    }

    #[test]
    fn held_too_long_it_gives_up_and_settles_lower() {
        let mut w = Watch::default();
        let mut last = None;
        for (i, e) in [1.0, 0.1, 0.1, 0.1, 0.1].into_iter().enumerate() {
            last = w.push(e, 0.0, 1, i + 1, 3);
        }
        assert_eq!(last, Some(Judged::GiveUp));
        assert_eq!(w.settled, Some(0.1));
        assert_eq!(w.lost, None);
    }

    #[test]
    fn the_shift_moves_the_whole_pattern() {
        // Two paths, at 0 and 2 chips; the signal slipped 10 chips earlier.
        let step = 0.25;
        let lag0 = -20.0;
        let mut energy = vec![0.01; 200];
        let put = |e: &mut Vec<f64>, lag: f64, v: f64| e[((lag - lag0) / step) as usize] = v;
        put(&mut energy, -10.0, 1.0);
        put(&mut energy, -8.0, 0.6);
        let (d, got, here) = best_shift(&energy, lag0, step, &[0.0, 2.0], 15.0).unwrap();
        assert_eq!(d, -10.0);
        assert!(got > 1.5 && here < 0.1, "{got} {here}");
    }

    #[test]
    fn a_path_that_takes_over_is_not_a_slip() {
        // Paths at 0 and 2 chips, the second now the stronger: no shift
        // collects more than the fingers already do.
        let step = 0.25;
        let lag0 = -20.0;
        let mut energy = vec![0.01; 200];
        energy[((0.0 - lag0) / step) as usize] = 0.3;
        energy[((2.0 - lag0) / step) as usize] = 1.0;
        let (_, got, here) = best_shift(&energy, lag0, step, &[0.0, 2.0], 15.0).unwrap();
        assert!(got < SLIP_RATIO * here, "{got} {here}");
    }

    #[test]
    fn a_fade_onto_another_path_is_not_a_clean_slip() {
        // The new lag held energy all along (a second path); the old faded.
        let old: Vec<f64> = (0..60).map(|s| if s < 30 { 1.0 } else { 0.1 }).collect();
        let new = vec![0.6; 60];
        assert!(!is_clean_slip(&old, &new, slip_at(&old, &new), 12, 0.0));
        let step: Vec<f64> = (0..60).map(|s| if s < 30 { 0.05 } else { 1.0 }).collect();
        assert!(is_clean_slip(&old, &step, slip_at(&old, &step), 12, 0.0));
        // Noise as strong as the signal: not clean on the raw numbers, clean
        // once it is taken off.
        let old: Vec<f64> = (0..60).map(|s| if s < 30 { 2.0 } else { 1.0 }).collect();
        let step: Vec<f64> = (0..60).map(|s| if s < 30 { 1.0 } else { 2.0 }).collect();
        assert!(!is_clean_slip(&old, &step, slip_at(&old, &step), 12, 0.0));
        assert!(is_clean_slip(&old, &step, slip_at(&old, &step), 12, 1.0));
    }

    #[test]
    fn the_slip_is_found_where_the_energy_moved() {
        let old: Vec<f64> = (0..50).map(|s| if s < 31 { 1.0 } else { 0.1 }).collect();
        let new: Vec<f64> = (0..50).map(|s| if s < 31 { 0.1 } else { 1.0 }).collect();
        assert_eq!(slip_at(&old, &new), 31);
        assert_eq!(slip_at(&[0.1; 5], &[1.0; 5]), 0);
        assert_eq!(slip_at(&[1.0; 5], &[0.1; 5]), 5);
    }
}
