// made by CasualArclamp
//! Finding a lost station again: a search over lag and carrier on what is known
//! of its signal, run while the watch ([`super::rewind`]) says the channel is lost
//! and the slip search has found no clean step.
//!
//! The slip search looks for the energy at the fingers' pattern moved, in one clean
//! step, within ±60 ms and four seconds. A deep fade defeats it when it lasts longer,
//! and so does anything that moves the carrier or the timing while the signal is
//! gone (a slip of the audio inside the fade, the early/late loop walking on noise,
//! a rig's drift): the station comes back where the loops are not. This search takes
//! what is known of the signal and correlates it at every lag of the pattern's reach
//! and at a few carrier offsets, a watch window at a time:
//!
//! - DS-PSK: the pilot symbols, each coherently;
//! - `dsc`: the pilot code under every symbol, coherently over a quarter second;
//! - CSK and bi-orthogonal CSK: the largest Walsh correlation of each symbol (the
//!   data is not known, so noncoherently, as the block pre-pass does).
//!
//! The windows' powers are summed over the last few (about two seconds), each window
//! first scaled by its own median over the cells, as the preamble search does (a
//! fading signal changes the level from one window to the next). With known chips a
//! cell's power in noise is exponential, so the sum is gamma-distributed and the
//! threshold comes from its tail at a false-alarm rate of [`PFA`] a test over all the
//! cells; with the Walsh maxima the threshold is a robust z-score. A cell over the
//! threshold is where the station is: if that is where the loops already are, the
//! channel was only weaker and the watch starts again from its new level; otherwise
//! the tracking state goes back to before the loss and the symbols are despread
//! again with the track moved, from the window where the station came back (the same
//! rewind as for a slip).

/// Lag step of the search, chips.
pub const STEP_CHIPS: f64 = 0.5;
/// The search looks this far either side of the fingers' pattern, seconds: the slip
/// search's reach.
pub const REACH_S: f64 = super::rewind::SLIP_REACH_S;
/// Carrier offsets tried, Hz either side of the track's.
pub const REACH_HZ: f64 = 4.0;
/// Longest coherent stretch of known chips, seconds (`dsc`'s unit; a DS-PSK pilot
/// and a CSK symbol are each one unit whatever their length).
pub const COHERENT_S: f64 = 0.25;
/// The windows summed: as many as make this many seconds, at most `MAX_WINDOWS`.
pub const SPAN_S: f64 = 2.0;
pub const MAX_WINDOWS: usize = 8;
/// False alarms a test, over all the cells.
pub const PFA: f64 = 1e-4;
/// The search goes on this long before the channel gives up and takes what there is
/// as its new level (the watch's own limit is four seconds without it).
pub const LOST_MAX_S: f64 = 60.0;
/// Margin added to the normal quantile for the noncoherent (Walsh maximum) statistic,
/// whose tail is heavier than a normal one.
const ROBUST_MARGIN: f64 = 1.0;

/// Measurement switch (environment `DECDSSS_NO_REACQ`, any value): no reacquisition
/// search, the watch and the slip search as before it. Read once.
pub fn disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("DECDSSS_NO_REACQ").is_some())
}

/// How the statistic of a cell is distributed in noise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A sum of exponential powers (correlations with known chips).
    Coherent,
    /// A sum of the largest of L powers (CSK's unknown codes).
    Walsh,
}

/// The grid searched: shifts of the fingers' pattern by `deltas[d]` chips, at
/// carrier offsets `freqs[b]` Hz from the track.
#[derive(Clone, Debug)]
pub struct Grid {
    pub deltas: Vec<f64>,
    pub freqs: Vec<f64>,
}

impl Grid {
    /// Shifts within `reach` chips at [`STEP_CHIPS`], and carrier offsets a bin of
    /// `1/(2·coherent_s)` apart within [`REACH_HZ`] (just the track's own when a
    /// unit is too short for a bin to be narrower than the reach), kept within
    /// `freq_room` (Hz either way the carrier may still go).
    pub fn new(reach: f64, coherent_s: f64, freq_room: (f64, f64)) -> Grid {
        let nd = (reach / STEP_CHIPS).floor() as i64;
        let deltas = (-nd..=nd).map(|k| k as f64 * STEP_CHIPS).collect();
        let bin = 1.0 / (2.0 * coherent_s.max(1e-6));
        let nb = (REACH_HZ / bin).floor() as i64;
        let freqs = (-nb..=nb)
            .map(|k| k as f64 * bin)
            .filter(|&f| f >= -freq_room.0 && f <= freq_room.1)
            .collect();
        Grid { deltas, freqs }
    }

    pub fn cells(&self) -> usize {
        self.deltas.len() * self.freqs.len()
    }

    /// The cell of shift index `d` and offset index `b`.
    pub fn cell(&self, d: usize, b: usize) -> usize {
        d * self.freqs.len() + b
    }

    /// The cell nearest no shift and no offset (where the loops already are).
    pub fn here(&self) -> usize {
        let d = self.deltas.len() / 2;
        let b = (0..self.freqs.len())
            .min_by(|&i, &j| self.freqs[i].abs().total_cmp(&self.freqs[j].abs()))
            .unwrap_or(0);
        self.cell(d, b)
    }
}

/// One watch window's statistic over the grid.
#[derive(Clone, Debug)]
struct Window {
    /// The window's first block symbol.
    from: usize,
    /// Per cell, scaled so the noise has the distribution's own median.
    cells: Vec<f64>,
    /// The gamma shape of a cell in noise ([`Kind::Coherent`]), as measured.
    shape: f64,
}

/// Where the search found the station.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    /// Chips the fingers' pattern moves.
    pub delta: f64,
    /// Hz the carrier moves.
    pub freq: f64,
    /// It is where the loops are (no move).
    pub here: bool,
    /// The block symbol from which the station is there: the first symbol of the
    /// window where it came back.
    pub from: usize,
    /// The statistic over its threshold.
    pub score: f64,
}

/// A search in progress, from the loss on.
#[derive(Clone, Debug)]
pub struct Search {
    pub grid: Grid,
    pub kind: Kind,
    /// The next symbol a window starts at.
    pub next: usize,
    /// Windows summed.
    pub span: usize,
    windows: Vec<Window>,
}

impl Search {
    /// A search from block symbol `since`, over `span` windows at a time.
    pub fn new(grid: Grid, kind: Kind, since: usize, span: usize) -> Search {
        Search {
            grid,
            kind,
            next: since,
            span: span.clamp(1, MAX_WINDOWS),
            windows: Vec::new(),
        }
    }

    /// Adds a window's raw cell statistics, scaled by their median. For the coherent
    /// kind `terms` is how many exponential powers a cell sums (units × fingers), but
    /// the fingers' terms are not independent when two fingers sit within a chip or
    /// so of each other (the pulse makes their noise alike), and a sum of correlated
    /// exponentials has a heavier tail than the gamma of that many: so the shape is
    /// measured from the cells instead, at most `terms` ([`gamma_shape`]).
    pub fn push(&mut self, from: usize, mut cells: Vec<f64>, terms: usize) {
        debug_assert_eq!(cells.len(), self.grid.cells());
        let med = median(&cells);
        let shape = match self.kind {
            Kind::Coherent => gamma_shape(&cells, terms.max(1) as f64),
            Kind::Walsh => 1.0,
        };
        if med > 0.0 {
            let target = match self.kind {
                Kind::Coherent => gamma_median(shape),
                Kind::Walsh => 1.0,
            };
            for c in &mut cells {
                *c *= target / med;
            }
        }
        self.windows.push(Window { from, cells, shape });
    }

    /// The best cell of the last `span` windows if it clears the threshold.
    pub fn detect(&self) -> Option<Found> {
        if self.windows.len() < self.span || self.grid.cells() < 8 {
            return None;
        }
        let recent = &self.windows[self.windows.len() - self.span..];
        let n = self.grid.cells();
        let mut sum = vec![0.0f64; n];
        for w in recent {
            for (s, c) in sum.iter_mut().zip(&w.cells) {
                *s += c;
            }
        }
        let (best, peak) = sum
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        let p = PFA / n as f64;
        let score = match self.kind {
            Kind::Coherent => {
                let k: f64 = recent.iter().map(|w| w.shape).sum();
                peak / gamma_quantile(k, p)
            }
            Kind::Walsh => {
                let med = median(&sum);
                let dev: Vec<f64> = sum.iter().map(|v| (v - med).abs()).collect();
                let mad = 1.4826 * median(&dev);
                if mad <= 0.0 {
                    return None;
                }
                ((peak - med) / mad) / (normal_upper(p) + ROBUST_MARGIN)
            }
        };
        if score <= 1.0 {
            return None;
        }
        let nb = self.grid.freqs.len();
        let (d, b) = (best / nb, best % nb);
        let here_cell = self.grid.here();
        let delta = self.grid.deltas[d];
        let freq = self.grid.freqs[b];
        // Somewhere else only if it holds clearly more than where the loops are, by
        // the slip search's ratio, the noise taken off both: when one path fades and
        // another carries the signal, the pattern moved onto the other path collects
        // about what the fingers already do, and the track must stay.
        let noise = match self.kind {
            Kind::Coherent => recent.iter().map(|w| w.shape).sum::<f64>(),
            Kind::Walsh => median(&sum),
        };
        let moved = delta.abs() >= super::rewind::SLIP_MIN_CHIPS || b != here_cell % nb;
        let clearly = peak - noise >= super::rewind::SLIP_RATIO * (sum[here_cell] - noise).max(0.0);
        let here = !(moved && clearly);
        // Where it came back: the window split that best gives the cells before it to
        // where the loops are and after it to the station's new place.
        let old: Vec<f64> = self.windows.iter().map(|w| w.cells[here_cell]).collect();
        let new: Vec<f64> = self.windows.iter().map(|w| w.cells[best]).collect();
        // In a fade both hold noise and the split may fall anywhere in it; the
        // station is back from the first window after it that holds half its level
        // ([`super::rewind::back_at`], window by window).
        let split = super::rewind::slip_at(&old, &new).min(self.windows.len() - 1);
        let mean_noise = noise / self.span as f64;
        let k = super::rewind::back_at(&new, split, 1, mean_noise);
        Some(Found {
            delta,
            freq,
            here,
            from: self.windows[k].from,
            score,
        })
    }
}

/// The median of `v` (the upper one of an even count); 0 when empty.
pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    let mid = s.len() / 2;
    // Rust note: `select_nth_unstable_by` puts the `mid`-th smallest in place
    // without sorting the rest, which is all a median needs.
    let (_, m, _) = s.select_nth_unstable_by(mid, f64::total_cmp);
    *m
}

/// The shape of the gamma distribution the noise cells of a window follow, from the
/// variance of their logarithms: for a gamma of shape k, Var(ln X) is the trigamma
/// function ψ₁(k), whatever the scale, and the logarithm keeps the few cells that
/// hold signal from pulling it far (derived). Between 1 (the terms all alike) and
/// `most` (all independent).
pub fn gamma_shape(cells: &[f64], most: f64) -> f64 {
    let logs: Vec<f64> = cells.iter().filter(|&&c| c > 0.0).map(|c| c.ln()).collect();
    if logs.len() < 8 {
        return most.max(1.0);
    }
    let n = logs.len() as f64;
    let mean = logs.iter().sum::<f64>() / n;
    let var = logs.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / (n - 1.0);
    // ψ₁ falls from 1.645 at k = 1: bisect for ψ₁(k) = var.
    let (mut lo, mut hi) = (1.0f64, most.max(1.0));
    if trigamma(hi) >= var {
        return hi;
    }
    if trigamma(lo) <= var {
        return lo;
    }
    for _ in 0..50 {
        let mid = 0.5 * (lo + hi);
        if trigamma(mid) > var {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The trigamma function ψ₁(k) for k ≥ 1: the recurrence ψ₁(k) = ψ₁(k + 1) + 1/k² up
/// to 6, then the asymptotic series 1/k + 1/2k² + 1/6k³ − 1/30k⁵ + 1/42k⁷.
pub fn trigamma(mut k: f64) -> f64 {
    let mut acc = 0.0;
    while k < 6.0 {
        acc += 1.0 / (k * k);
        k += 1.0;
    }
    let (k2, k3) = (k * k, k * k * k);
    acc + 1.0 / k + 1.0 / (2.0 * k2) + 1.0 / (6.0 * k3) - 1.0 / (30.0 * k3 * k2)
        + 1.0 / (42.0 * k3 * k3 * k)
}

/// The median of a gamma distribution of shape `k` and unit scale: about
/// k − 1/3 + 8/(405k) (Choi's expansion; ln 2 = 0.693 at k = 1, which this gives as
/// 0.686).
pub fn gamma_median(k: f64) -> f64 {
    if k <= 1.0 {
        return std::f64::consts::LN_2;
    }
    k - 1.0 / 3.0 + 8.0 / (405.0 * k)
}

/// The upper `p` quantile of a gamma distribution of shape `k` and unit scale (a sum
/// of k unit exponentials) by the Wilson–Hilferty cube-root normal approximation.
pub fn gamma_quantile(k: f64, p: f64) -> f64 {
    let z = normal_upper(p);
    let c = 1.0 / (9.0 * k);
    k * (1.0 - c + z * c.sqrt()).powi(3)
}

/// The upper `p` quantile of the standard normal (Abramowitz and Stegun 26.2.23,
/// error under 4.5·10⁻⁴), for p ≤ 0.5.
pub fn normal_upper(p: f64) -> f64 {
    let t = (-2.0 * p.clamp(1e-300, 0.5).ln()).sqrt();
    t - (2.515517 + 0.802853 * t + 0.010328 * t * t)
        / (1.0 + 1.432788 * t + 0.189269 * t * t + 0.001308 * t * t * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_dsp::rng::Rng;

    fn exp(rng: &mut Rng) -> f64 {
        -(1.0 - rng.uniform()).ln()
    }

    #[test]
    fn the_quantiles_match_known_values() {
        // z at 1e-4 is 3.719; a gamma of shape 1 is an exponential, whose upper
        // 1e-4 point is ln 1e4 = 9.21 (the approximation is a little high there).
        assert!((normal_upper(1e-4) - 3.719).abs() < 2e-3);
        let q1 = gamma_quantile(1.0, 1e-4);
        assert!(q1 > 9.0 && q1 < 10.5, "{q1}");
        // Shape 16, 1e-3: 31.24 (chi-square of 32 degrees, 62.49, halved).
        let q = gamma_quantile(16.0, 1e-3);
        assert!((q - 31.24).abs() < 0.2, "{q}");
        assert!((gamma_median(8.0) - 7.67).abs() < 0.01);
    }

    #[test]
    fn noise_alone_seldom_clears_the_threshold() {
        // 400 searches of 8 windows of noise on 461 × 5 cells, 3 terms a cell: at
        // 1e-4 a test, the chance of more than two false alarms is under 1e-3.
        let mut rng = Rng::new(3);
        let grid = Grid {
            deltas: (0..461).map(|k| k as f64 * 0.5).collect(),
            freqs: vec![-4.0, -2.0, 0.0, 2.0, 4.0],
        };
        let mut alarms = 0;
        for _ in 0..400 {
            let mut s = Search::new(grid.clone(), Kind::Coherent, 0, 8);
            for w in 0..8 {
                let cells = (0..grid.cells())
                    .map(|_| 7.0 * (exp(&mut rng) + exp(&mut rng) + exp(&mut rng)))
                    .collect();
                s.push(w * 10, cells, 3);
            }
            alarms += usize::from(s.detect().is_some());
        }
        assert!(alarms <= 2, "{alarms} false alarms in 400");
    }

    #[test]
    fn a_station_back_at_another_lag_is_found_and_where_it_came_back() {
        let mut rng = Rng::new(9);
        let grid = Grid::new(20.0, 0.25, (10.0, 10.0));
        let target = grid.cell(grid.deltas.len() / 2 + 14, 3);
        let mut s = Search::new(grid.clone(), Kind::Coherent, 0, 8);
        // Four windows of fade (noise only), then the station 7 chips and +2 Hz away,
        // a window's pilot energy 3 dB over the noise of a cell.
        for w in 0..12 {
            let cells = (0..grid.cells())
                .map(|c| {
                    let sig = if w >= 4 && c == target {
                        2.0 * 8.0
                    } else {
                        0.0
                    };
                    (0..8).map(|_| exp(&mut rng)).sum::<f64>() + sig
                })
                .collect();
            s.push(w * 60, cells, 8);
        }
        let f = s.detect().expect("found");
        assert_eq!((f.delta, f.freq, f.here), (7.0, 2.0, false));
        // In the fade both cells hold noise, so the split can fall anywhere there
        // (moving the track for symbols that are lost anyway costs nothing); never
        // after the station came back.
        assert!(f.from <= 240, "{}", f.from);
    }

    #[test]
    fn the_shape_is_measured_and_correlated_terms_do_not_raise_false_alarms() {
        let mut rng = Rng::new(21);
        assert!((trigamma(1.0) - 1.644934).abs() < 1e-5);
        assert!((trigamma(10.0) - 0.105166).abs() < 1e-5);
        let three: Vec<f64> = (0..4000)
            .map(|_| exp(&mut rng) + exp(&mut rng) + exp(&mut rng))
            .collect();
        let k = gamma_shape(&three, 3.0);
        assert!(k > 2.6, "{k}");
        // Three fingers that see the same noise: one exponential, three times over.
        let alike: Vec<f64> = (0..4000).map(|_| 3.0 * exp(&mut rng)).collect();
        assert!(gamma_shape(&alike, 3.0) < 1.2);
        let grid = Grid {
            deltas: (0..461).map(|k| k as f64 * 0.5).collect(),
            freqs: vec![-4.0, -2.0, 0.0, 2.0, 4.0],
        };
        let mut alarms = 0;
        for _ in 0..200 {
            let mut s = Search::new(grid.clone(), Kind::Coherent, 0, 8);
            for w in 0..8 {
                let cells = (0..grid.cells()).map(|_| 3.0 * exp(&mut rng)).collect();
                s.push(w * 10, cells, 3);
            }
            alarms += usize::from(s.detect().is_some_and(|f| !f.here));
        }
        assert!(alarms <= 2, "{alarms} false alarms in 200");
    }

    #[test]
    fn a_path_that_takes_over_in_a_fade_does_not_move_the_track() {
        // Fingers at 0 and 2 chips; the first path faded, the second carries the
        // signal. The pattern moved +2 puts finger 0 on it (and finger 1 on nothing):
        // the same energy as where the loops are, so the station is here.
        let mut rng = Rng::new(12);
        let grid = Grid::new(10.0, 0.25, (10.0, 10.0));
        let here = grid.here();
        let moved = grid.cell(grid.deltas.len() / 2 + 4, grid.here() % grid.freqs.len());
        let mut s = Search::new(grid.clone(), Kind::Coherent, 0, 8);
        for w in 0..8 {
            let cells = (0..grid.cells())
                .map(|c| {
                    let sig = if c == here || c == moved { 20.0 } else { 0.0 };
                    (0..8).map(|_| exp(&mut rng)).sum::<f64>() + sig
                })
                .collect();
            s.push(w * 60, cells, 8);
        }
        assert!(s.detect().expect("found").here);
    }

    #[test]
    fn the_walsh_statistic_finds_a_peak_too() {
        let mut rng = Rng::new(4);
        let grid = Grid::new(30.0, 0.03, (10.0, 10.0));
        assert_eq!(grid.freqs, [0.0]);
        let target = grid.cell(5, 0);
        let mut s = Search::new(grid.clone(), Kind::Walsh, 0, 4);
        // The largest of 64 noise powers is about H_64 = 4.7 a symbol, 8 symbols a
        // window; the station adds 3 a symbol.
        for w in 0..4 {
            let cells = (0..grid.cells())
                .map(|c| {
                    let mut v = 0.0;
                    for _ in 0..8 {
                        let m = (0..64).map(|_| exp(&mut rng)).fold(0.0, f64::max);
                        v += m + if c == target { 3.0 } else { 0.0 };
                    }
                    v
                })
                .collect();
            s.push(w * 8, cells, 8);
        }
        let f = s.detect().expect("found");
        assert_eq!(f.delta, grid.deltas[5]);
        // And noise alone is not.
        let mut quiet = Search::new(grid.clone(), Kind::Walsh, 0, 4);
        for w in 0..4 {
            let cells = (0..grid.cells())
                .map(|_| {
                    (0..8)
                        .map(|_| (0..64).map(|_| exp(&mut rng)).fold(0.0, f64::max))
                        .sum()
                })
                .collect();
            quiet.push(w * 8, cells, 8);
        }
        assert_eq!(quiet.detect(), None);
    }

    #[test]
    fn the_grid_keeps_the_carrier_in_its_room_and_finds_here() {
        let g = Grid::new(2.0, 0.25, (1.0, 10.0));
        assert_eq!(g.deltas, [-2.0, -1.5, -1.0, -0.5, 0.0, 0.5, 1.0, 1.5, 2.0]);
        assert_eq!(g.freqs, [0.0, 2.0, 4.0]);
        assert_eq!(g.here(), g.cell(4, 0));
    }
}
