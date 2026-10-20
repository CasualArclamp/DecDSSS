// made by CasualArclamp
//! The header demodulator: CSK symbols on the call code (one of the alphabets of
//! `tx::HEADER_ALPHABETS`), rake-combined noncoherently, de-interleaved and
//! LDPC-decoded.

use super::despread::{Track, csk_bit_llrs, despread, ln_i0, walsh_correlate, walsh_one};
use super::frontend::Buf;
use super::preamble::{DelayProfile, PreambleLock};
use super::rewind::{self, Judged, Watch};
use crate::codes::{CALL_CODE, Scrambler};
use crate::framing::{HEADER_INFO_BITS, Header};
use crate::tx::{header_bits, header_chips_l, header_len, header_symbols};
use decdsss_dsp::Iq;
use decdsss_fec::interleave::Interleaver;
use decdsss_fec::ldpc::{Code, Decoder};

/// A rake finger: a path `lag` chips after the tracked reference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Finger {
    pub lag: f64,
    /// Relative power from the delay profile.
    pub power: f32,
}

/// Picks the fingers of a delay profile: local maxima at least 12 dB under
/// the peak is too weak to help, and 2.2 times the median is the noise
/// floor of a non-coherent sum.
pub fn pick_fingers(profile: &super::preamble::DelayProfile, max: usize) -> Vec<Finger> {
    let mut p = profile.power.clone();
    if p.is_empty() {
        return vec![Finger {
            lag: 0.0,
            power: 1.0,
        }];
    }
    p.sort_by(f32::total_cmp);
    let median = p[p.len() / 2];
    let found = profile.fingers(max, 10.0, 3.5 * median, 0.85);
    if found.is_empty() {
        return vec![Finger {
            lag: 0.0,
            power: 1.0,
        }];
    }
    let top = found[0].1;
    found
        .into_iter()
        .map(|(lag, pw)| Finger {
            lag,
            power: pw / top,
        })
        .collect()
}

#[derive(Debug)]
pub enum HeaderOutcome {
    /// More samples are needed: the absolute index the buffer must reach.
    NeedMore(i64),
    /// The header did not decode.
    Bad(&'static str),
    Decoded(Box<HeaderResult>),
}

#[derive(Debug)]
pub struct HeaderResult {
    pub header: Header,
    /// Where the first payload chip is, and the carrier at that point.
    pub track: Track,
    pub fingers: Vec<Finger>,
    /// LDPC iterations the header took.
    pub iterations: usize,
    /// Despread SNR of the strongest finger's peak, dB (Es/N0 of a symbol).
    pub snr_db: f64,
}

/// Chips of samples the demodulator needs past the header's own, and its
/// length in samples at the lock's chip length: where the buffer must reach.
pub fn header_need(lock: &PreambleLock, alphabet: usize) -> f64 {
    let fingers = pick_fingers(&lock.profile, 3);
    let max_lag = fingers.iter().map(|f| f.lag).fold(0.0f64, f64::max);
    lock.header_start + (header_len(alphabet) as f64 + max_lag + 6.0) * lock.step + 16.0
}

/// One header symbol as the demodulator saw it.
#[derive(Clone)]
struct Symbol {
    /// The Walsh correlations on each finger: `per_finger[finger][code]`.
    per_finger: Vec<Vec<Iq>>,
    /// The finger-weighted power of each code, and the decided code (the most).
    power: Vec<f32>,
    code: usize,
    /// Early and late magnitudes on the main finger, with the decided code.
    early: f32,
    late: f32,
}

/// The timing loop's proportional and integral gains, per symbol.
const KP: f64 = 0.15;
const KI: f64 = 0.01;
/// The slip watch judges at least this many header symbols at a time (with
/// [`rewind::WATCH_S`]). A header symbol is long (27 ms, CSK64 on 3k, to 640
/// ms, CSK256 on 500), so a quarter second may hold only one or two; four keep
/// a window's level steadier near threshold, and a slip is still found before
/// it has spoilt much of a header only 64 to 86 symbols long.
const WATCH_MIN_SYMBOLS: usize = 4;
/// Slips one header may follow.
const MAX_SLIPS: usize = 12;

/// Measurement switch (environment `DECDSSS_NO_HEADER_SLIP`): the header
/// follows no slips. Read once.
fn no_header_slip() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("DECDSSS_NO_HEADER_SLIP").is_some())
}

/// Where the loop stood before a symbol, for a rewind.
#[derive(Clone, Copy)]
struct Start {
    t: Track,
    err_int: f64,
    freq_acc: Iq,
}

/// The per-symbol part of the header demodulator: despreads a CSK symbol on
/// every finger, decides its code, steers the timing from the decision and
/// gathers the carrier's drift. [`demod_header`] runs it over the whole header
/// at once; [`HeaderWatch`] runs it as the chips come in, for a display.
///
/// It also follows slips of the audio (see [`rewind`]): a watch on the energy
/// the fingers collect, a search over ±[`rewind::SLIP_REACH_S`] on the symbols
/// since the loss, and a rewind to the symbol it slipped at, from which the
/// symbols are taken again on the moved track.
struct SymbolLoop {
    hl: usize,
    nsym: usize,
    fingers: Vec<Finger>,
    /// The strongest finger, which the timing and carrier follow.
    main: usize,
    max_lag: f64,
    scr: Vec<Iq>,
    t: Track,
    err_int: f64,
    peak_prev: Option<Iq>,
    /// Σ peak·conj(previous peak): its angle is the carrier's turn a symbol.
    freq_acc: Iq,
    /// Symbols so far, and where the loop stood before each.
    syms: Vec<Symbol>,
    starts: Vec<Start>,
    watch: Watch,
    win: usize,
    max_windows: usize,
    /// H_L: the mean of the largest of L unit exponentials, the noise the
    /// decided code collects when there is no signal.
    h_l: f64,
    chip_rate: f64,
    /// After a rewind, the symbols up to here come again and the watch has
    /// judged them already.
    replay_until: usize,
    /// Each slip followed: the symbol it slipped at and the move, chips.
    slips: Vec<(usize, f64)>,
}

impl SymbolLoop {
    fn new(lock: &PreambleLock, rate: f64, alphabet: usize) -> SymbolLoop {
        let fingers = pick_fingers(&lock.profile, 3);
        let max_lag = fingers.iter().map(|f| f.lag).fold(0.0f64, f64::max);
        let main = fingers
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.power.total_cmp(&b.1.power))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let nsym = header_symbols(alphabet);
        let chip_rate = rate / lock.step;
        let t_sym = alphabet as f64 / chip_rate;
        let win = ((rewind::WATCH_S / t_sym).ceil() as usize).max(WATCH_MIN_SYMBOLS);
        let max_windows = ((rewind::LOST_MAX_S / (win as f64 * t_sym)).ceil() as usize).max(2);
        SymbolLoop {
            hl: alphabet,
            nsym,
            fingers,
            main,
            max_lag,
            scr: Scrambler::new(CALL_CODE).chips(0, header_len(alphabet)),
            t: Track {
                pos: lock.header_start,
                step: lock.step,
                freq: lock.freq_hz,
                phase: 0.0,
                rate,
            },
            err_int: 0.0,
            peak_prev: None,
            freq_acc: Iq::new(0.0, 0.0),
            syms: Vec::with_capacity(nsym),
            starts: Vec::with_capacity(nsym),
            watch: Watch::default(),
            win,
            max_windows,
            h_l: (1..=alphabet).map(|k| 1.0 / k as f64).sum(),
            chip_rate,
            replay_until: 0,
            slips: Vec::new(),
        }
    }

    /// Symbols done.
    fn done(&self) -> usize {
        self.syms.len()
    }

    /// True if there is a symbol left and its chips (and the fingers' and the
    /// interpolator's reach past them) are all in `buf`: the margin
    /// [`header_need`] allows for the whole header, a symbol at a time.
    fn ready(&self, buf: &Buf) -> bool {
        self.done() < self.nsym
            && self.t.pos + (self.hl as f64 + self.max_lag + 6.0) * self.t.step + 16.0
                <= buf.end() as f64
    }

    /// Takes the next symbol and returns it. After it the loop may rewind to
    /// an earlier symbol (a slip was found), so [`SymbolLoop::done`] can go
    /// down; the symbols are then taken again.
    fn next(&mut self, buf: &Buf) -> Symbol {
        let (hl, s) = (self.hl, self.done());
        self.starts.push(Start {
            t: self.t,
            err_int: self.err_int,
            freq_acc: self.freq_acc,
        });
        let sc = &self.scr[s * hl..(s + 1) * hl];
        let mut u = vec![Iq::new(0.0, 0.0); hl];
        let mut per_finger = Vec::with_capacity(self.fingers.len());
        for f in &self.fingers {
            despread(buf, &self.t, sc, f.lag, &mut u);
            per_finger.push(walsh_correlate(&u));
        }
        // The decision: the code with the most weighted power.
        let mut power = vec![0.0f32; hl];
        for (f, wf) in self.fingers.iter().zip(&per_finger) {
            for (p, z) in power.iter_mut().zip(wf) {
                *p += f.power * z.norm_sqr();
            }
        }
        let m = power
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or(0);
        // Early/late on the main finger, with the decided code.
        let lag = self.fingers[self.main].lag;
        let mut ue = vec![Iq::new(0.0, 0.0); hl];
        let mut ul = vec![Iq::new(0.0, 0.0); hl];
        despread(buf, &self.t, sc, lag - 0.25, &mut ue);
        despread(buf, &self.t, sc, lag + 0.25, &mut ul);
        let (e, l) = (walsh_one(&ue, m).norm(), walsh_one(&ul, m).norm());
        let d = f64::from((l - e) / (l + e).max(1e-30));
        let err = d / 1.33;
        self.err_int += KI * err;
        self.t.pos += (KP * err + self.err_int) * self.t.step;
        // Frequency from successive peak phases.
        let peak = per_finger[self.main][m];
        if let Some(p) = self.peak_prev {
            self.freq_acc += peak * p.conj();
        }
        self.peak_prev = Some(peak);
        self.t.advance(hl);
        // The watch: the decided code's power at the fingers less the other
        // codes' mean times H_L (the largest of L noise powers), symbol by
        // symbol as in the payload's CSK watch.
        let (mut energy, mut others) = (0.0f64, 0.0f64);
        for wf in &per_finger {
            let p = f64::from(wf[m].norm_sqr());
            energy += p;
            others +=
                (wf.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() - p) / (hl as f64 - 1.0);
        }
        let sym = Symbol {
            per_finger,
            power,
            code: m,
            early: e,
            late: l,
        };
        self.syms.push(sym.clone());
        let n = self.done();
        if n > self.replay_until && !super::payload::no_rewind() && !no_header_slip() {
            let judged = self.watch.push(
                energy - self.h_l * others,
                0.0,
                self.win,
                n,
                self.max_windows,
            );
            if matches!(judged, Some(Judged::Lost | Judged::StillLost)) {
                self.resync(buf);
            }
        }
        sym
    }

    /// The watch saw the signal go: search the symbols since for a slip, and
    /// on one, rewind to it with the track moved.
    fn resync(&mut self, buf: &Buf) {
        let Some(lost) = self.watch.lost else {
            return;
        };
        let to = self.done();
        let from = lost.since.min(to - 1);
        let hl = self.hl;
        let lags: Vec<f64> = self.fingers.iter().map(|f| f.lag).collect();
        let reach = rewind::SLIP_REACH_S * self.chip_rate;
        let step = DelayProfile::LAG_STEP;
        let (lmin, lmax) = lags
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        let lag0 = lmin - reach;
        let nlags = ((lmax + reach - lag0) / step).round() as usize + 1;
        let mut u = vec![Iq::new(0.0, 0.0); hl];
        let (starts, scr, h_l) = (&self.starts, &self.scr, self.h_l);
        // Symbol `s` at `lag` chips from where it was taken.
        let mut at = |s: usize, lag: f64| {
            despread(buf, &starts[s].t, &scr[s * hl..(s + 1) * hl], lag, &mut u);
            rewind::csk_excess(&walsh_correlate(&u), h_l)
        };
        let mut energy = vec![0.0f64; nlags];
        for s in from..to {
            for (i, e) in energy.iter_mut().enumerate() {
                *e += at(s, lag0 + i as f64 * step);
            }
        }
        let Some((d, got, here)) = rewind::best_shift(&energy, lag0, step, &lags, reach) else {
            return;
        };
        let symbols = (to - from) as f64;
        let settled = self.watch.settled.unwrap_or(0.0);
        if got < rewind::SLIP_RATIO * here.max(0.0)
            || got / symbols < rewind::SLIP_MIN * settled
            || !rewind::significant(&energy, lags.len(), got)
        {
            return;
        }
        // Where it slipped: from a window before the loss on.
        let lo = from.saturating_sub(self.win);
        let (mut old, mut new) = (Vec::new(), Vec::new());
        for s in lo..to {
            old.push(lags.iter().map(|&g| at(s, g)).sum::<f64>());
            new.push(lags.iter().map(|&g| at(s, g + d)).sum::<f64>());
        }
        let split = rewind::slip_at(&old, &new);
        // The excess is already clear of the noise.
        if !rewind::is_clean_slip(&old, &new, split, self.win, 0.0) {
            return;
        }
        let k = lo + split;
        // Slips come in order, and a header that keeps slipping is not worth
        // following round in circles.
        if self.slips.last().is_some_and(|&(j, _)| k < j) || self.slips.len() >= MAX_SLIPS {
            return;
        }
        let st = self.starts[k];
        self.t = st.t;
        self.t.pos += d * self.t.step;
        self.err_int = st.err_int;
        self.peak_prev = None;
        self.freq_acc = st.freq_acc;
        self.syms.truncate(k);
        self.starts.truncate(k);
        self.replay_until = self.replay_until.max(to);
        self.slips.push((k, d));
        self.watch.lost = None;
        self.watch.restart_window();
    }

    /// The track at the end of the header, its carrier moved half way to the
    /// drift the symbols showed.
    fn end_track(&self) -> Track {
        // Rust note: `Track` is `Copy`, so this is a copy, not a move out of `self`.
        let mut t = self.t;
        let t_sym = self.hl as f64 * t.step / t.rate;
        let f_res = f64::from(self.freq_acc.arg()) / (std::f64::consts::TAU * t_sym);
        t.freq += 0.5 * f_res;
        t
    }
}

/// Demodulates the header, in CSK alphabet `alphabet`, that starts at
/// `lock.header_start`.
pub fn demod_header(buf: &Buf, lock: &PreambleLock, rate: f64, alphabet: usize) -> HeaderOutcome {
    let (hl, nsym, bits) = (alphabet, header_symbols(alphabet), header_bits(alphabet));
    let need = header_need(lock, alphabet);
    if need > buf.end() as f64 {
        return HeaderOutcome::NeedMore(need.ceil() as i64);
    }
    let mut lp = SymbolLoop::new(lock, rate, alphabet);
    while lp.done() < nsym {
        lp.next(buf);
    }
    let mut t = lp.end_track();
    // Where the known-chip refinement starts: after the last slip (it fits one
    // line), on the track the symbols were taken from there.
    let (from_sym, t_from) = match lp.slips.last() {
        Some(&(k, _)) => (k, lp.starts[k].t),
        None => (
            0,
            Track {
                pos: lock.header_start,
                step: lock.step,
                freq: lock.freq_hz,
                phase: 0.0,
                rate,
            },
        ),
    };
    // W[symbol][finger] = the `hl` correlations.
    let decisions: Vec<usize> = lp.syms.iter().map(|s| s.code).collect();
    let w: Vec<Vec<Vec<Iq>>> = lp.syms.into_iter().map(|s| s.per_finger).collect();
    let (fingers, main) = (lp.fingers, lp.main);

    // Per-finger noise and amplitude from the decided symbols.
    let nf = fingers.len();
    let mut sigma2 = vec![0.0f64; nf];
    let mut amp2 = vec![0.0f64; nf];
    for (s, &m) in decisions.iter().enumerate() {
        for f in 0..nf {
            let wf = &w[s][f];
            let total: f64 = wf.iter().map(|z| f64::from(z.norm_sqr())).sum();
            let pk = f64::from(wf[m].norm_sqr());
            sigma2[f] += (total - pk) / (hl as f64 - 1.0);
            amp2[f] += pk;
        }
    }
    for f in 0..nf {
        sigma2[f] /= nsym as f64;
        amp2[f] = (amp2[f] / nsym as f64 - sigma2[f]).max(0.01 * sigma2[f]);
    }
    let kappa: Vec<f64> = (0..nf)
        .map(|f| 2.0 * amp2[f].sqrt() / sigma2[f].max(1e-30))
        .collect();

    let mut llrs = Vec::with_capacity(nsym * bits);
    let mut metric = vec![0.0f64; hl];
    for sym in &w {
        for (m, v) in metric.iter_mut().enumerate() {
            *v = (0..nf)
                .map(|f| ln_i0(kappa[f] * f64::from(sym[f][m].norm())))
                .sum();
        }
        csk_bit_llrs(&metric, bits, &mut llrs);
    }
    // The last symbol may carry padding bits past the codeword.
    llrs.truncate(Code::header().n());
    let llrs = Interleaver::new(llrs.len()).invert(&llrs);
    let code = Code::header();
    let mut dec = Decoder::new(code);
    let outcome = dec.decode(&llrs, 60);
    if !outcome.converged {
        return HeaderOutcome::Bad("header LDPC did not converge");
    }
    let Some(header) = Header::unpack(&dec.bits()[..HEADER_INFO_BITS + 16]) else {
        return HeaderOutcome::Bad("header CRC failed");
    };
    // The header is now known: refine timing, clock and carrier against its
    // exact chips, from where the preamble put the start, and re-derive the
    // rake fingers from them, on the refined reference.
    let mut fingers = fingers;
    if let Some(known) = header_chips_l(&header, hl) {
        let t0 = Track {
            phase: 0.0,
            ..t_from
        };
        let known = &known[from_sym * hl..];
        let cr = rate / super::frontend::SPS as f64;
        let seg = super::refine::segment_chips(cr);
        if let Some(r) = super::refine::refine(buf, &t0, known, fingers[main].lag, seg, 300e-6) {
            let (f, shift) = super::refine::tail_fingers(buf, &r, &t0, known, seg);
            fingers = f;
            t.pos = r.pos_end + shift * r.step;
            t.step = r.step;
            t.freq = r.freq;
            t.phase = 0.0;
        }
    }
    HeaderOutcome::Decoded(Box::new(HeaderResult {
        header,
        track: t,
        fingers,
        iterations: outcome.iterations,
        snr_db: 10.0 * (amp2[main] / sigma2[main]).log10(),
    }))
}

/// The header so far, for a display: the symbols of the alphabet being tried,
/// as the decoding loop sees them.
#[derive(Clone, Debug, Default)]
pub struct HeaderView {
    /// The alphabet being tried, and the ones tried before it that did not decode.
    pub alphabet: usize,
    pub failed: Vec<usize>,
    /// Symbols of a header in this alphabet, and how many are in.
    pub symbols: usize,
    pub done: usize,
    /// Symbol × previous: each symbol's correlation with its decided code on the
    /// main finger times the conjugate of the one before (so from the second
    /// symbol on), turned back by the mean turn a symbol (taken modulo 180°) and
    /// scaled by the mean product magnitude. CSK puts nothing in the phase, so a
    /// header in this alphabet is one cluster at 1, and noise a ring. A longer
    /// header seen in this alphabet decides just as cleanly but is two clusters,
    /// at ±1, half the points each: a Walsh code of 2L chips is a code of L
    /// chips twice, the second time negated when the code's top bit is set, so
    /// neighbouring pieces differ in sign half the time. Neighbours, not a
    /// running reference, because a fade can turn a reference round by 180°.
    pub points: Vec<Iq>,
    /// Each symbol's decision margin: the decided code's finger-weighted power
    /// over the runner-up's, dB. About 1 dB for noise alone (derived for one
    /// finger: the code powers are then L equal exponentials, whose largest and
    /// second largest have means H_L and H_L − 1, harmonic numbers; their ratio
    /// is 1.03 dB at L = 64, 0.88 dB at 128, 0.77 dB at 256).
    pub margin_db: Vec<f32>,
    /// The correlator bank: main-finger power against code, one row a symbol
    /// (the last `payload::LIVE_ROWS`), each row scaled to its maximum.
    pub bank: Vec<Vec<f32>>,
    /// The last symbol's early, prompt and late magnitudes, prompt = 1.
    pub epl: [f32; 3],
    /// The preamble's delay profile (power against lag) and the fingers from it.
    pub profile: Vec<f32>,
    pub fingers: Vec<Finger>,
}

/// Follows a header as its chips come in, with the same loop that decodes it,
/// and keeps a [`HeaderView`]. Display only: the decision is [`demod_header`]'s.
pub struct HeaderWatch {
    lp: SymbolLoop,
    view: HeaderView,
    /// The last decided correlation, and Σ (z·conj(previous z))²: half its
    /// angle is the carrier's turn a symbol, blind to the 180° flips of a
    /// header seen in too short an alphabet.
    prev: Option<Iq>,
    turn_sq: Iq,
    /// Sum of the products' magnitudes so far.
    mag_sum: f32,
}

impl HeaderWatch {
    pub fn new(lock: &PreambleLock, rate: f64, alphabet: usize, failed: Vec<usize>) -> HeaderWatch {
        let lp = SymbolLoop::new(lock, rate, alphabet);
        let view = HeaderView {
            alphabet,
            failed,
            symbols: lp.nsym,
            profile: lock.profile.power.clone(),
            fingers: lp.fingers.clone(),
            ..HeaderView::default()
        };
        HeaderWatch {
            lp,
            view,
            prev: None,
            turn_sq: Iq::new(0.0, 0.0),
            mag_sum: 0.0,
        }
    }

    /// Runs every symbol whose chips are in.
    pub fn advance(&mut self, buf: &Buf) {
        while self.lp.ready(buf) {
            let s = self.lp.next(buf);
            let v = &mut self.view;
            v.done = self.lp.done();
            let z = s.per_finger[self.lp.main][s.code];
            let a = z.norm();
            // Rust note: `if let Some(p) = self.prev` copies the `Iq` out (it is
            // `Copy`), so `self.prev` can be overwritten below.
            if let Some(p) = self.prev {
                let d = z * p.conj();
                self.turn_sq += d * d;
                self.mag_sum += d.norm();
                // The carrier's mean turn a symbol so far, taken back off so a
                // small offset left by the preamble does not move the cluster.
                let (sn, cs) = (0.5 * self.turn_sq.arg()).sin_cos();
                let products = v.points.len() + 1;
                let mean = (self.mag_sum / products as f32).max(1e-30);
                v.points.push(d * Iq::new(cs, -sn) / mean);
            }
            self.prev = Some(z);
            let mut sorted = s.power.clone();
            sorted.sort_by(|a, b| b.total_cmp(a));
            let (best, second) = (sorted[0], sorted.get(1).copied().unwrap_or(0.0));
            v.margin_db
                .push(10.0 * (best.max(1e-30) / second.max(1e-30)).log10());
            let row: Vec<f32> = s.per_finger[self.lp.main]
                .iter()
                .map(|z| z.norm_sqr())
                .collect();
            let top = row.iter().copied().fold(1e-30f32, f32::max);
            if v.bank.len() == super::payload::LIVE_ROWS {
                v.bank.remove(0);
            }
            v.bank.push(row.into_iter().map(|p| p / top).collect());
            let p = a.max(1e-30);
            v.epl = [s.early / p, 1.0, s.late / p];
        }
    }

    pub fn view(&self) -> &HeaderView {
        &self.view
    }
}

impl HeaderView {
    /// Symbols turned round from the one before: points at −1.
    pub fn flipped(&self) -> usize {
        self.points.iter().filter(|z| z.re < 0.0).count()
    }
}

#[cfg(test)]
mod tests {
    use crate::rx::testrig::*;
    use decdsss_chan::Watterson;
    use decdsss_core::Profile;

    #[test]
    fn decodes_the_header_on_every_profile() {
        for profile in Profile::ALL {
            let seg = segment(profile, "csk64-1/2", 1, 1);
            let audio = through(&seg, &channel(profile, -8.0));
            let run = to_header(&audio, profile).unwrap_or_else(|e| panic!("{profile}: {e}"));
            assert_eq!(run.header.header, seg.header, "{profile}");
            eprintln!(
                "{profile}: header ok, {} iterations, symbol SNR {:.1} dB, fingers {:?}",
                run.header.iterations, run.header.snr_db, run.header.fingers
            );
        }
    }

    /// Header success rate at the weakest payload mode's SNR.
    #[test]
    fn header_survives_the_weakest_snr() {
        let profile = Profile::W2400;
        let mut ok = 0;
        let runs = 6;
        for seed in 0..runs {
            let seg = segment(profile, "csk256-1/2", 1, 1);
            let mut ch = channel(profile, -16.5);
            ch.seed = 100 + seed;
            ch.offset_hz = 33.0;
            ch.clock_ppm = 40.0;
            let audio = through(&seg, &ch);
            match to_header(&audio, profile) {
                Ok(r) if r.header.header == seg.header => ok += 1,
                Ok(_) => eprintln!("seed {seed}: wrong header"),
                Err(e) => eprintln!("seed {seed}: {e}"),
            }
        }
        eprintln!("header decoded in {ok} of {runs} runs at -16.5 dB");
        assert!(ok >= runs - 1);
    }

    /// Header success rate by SNR and channel; run with
    /// `cargo test -p decdsss-phy --release -- --ignored --nocapture header_curve`.
    #[test]
    #[ignore]
    fn header_curve() {
        let profile = Profile::W2400;
        let chans: [(&str, Option<Watterson>); 3] = [
            ("awgn", None),
            ("good", Some(Watterson::GOOD)),
            ("moderate", Some(Watterson::MODERATE)),
        ];
        for (name, fading) in chans {
            for snr in [-17.0, -15.0, -13.0, -10.0, -5.0, 0.0, 5.0] {
                let trials = 12;
                let mut ok = 0;
                let mut why = std::collections::BTreeMap::new();
                for seed in 0..trials {
                    let seg = segment(profile, "csk256-1/2", 1, 1);
                    let mut ch = channel(profile, snr);
                    ch.seed = 500 + seed;
                    ch.fading = fading;
                    ch.offset_hz = -15.0 + seed as f64 * 2.0;
                    ch.clock_ppm = -30.0 + seed as f64 * 5.0;
                    let audio = through(&seg, &ch);
                    match to_header(&audio, profile) {
                        Ok(r) if r.header.header == seg.header => ok += 1,
                        Ok(_) => *why.entry("wrong header".to_string()).or_insert(0) += 1,
                        Err(e) => {
                            if snr >= -5.0 && fading.is_none() {
                                println!(
                                    "  fail seed {seed}: offset {:.1} clock {:.1}: {e}",
                                    ch.offset_hz, ch.clock_ppm
                                );
                            }
                            *why.entry(e).or_insert(0) += 1
                        }
                    }
                }
                println!("{name:9} {snr:6.1} dB: header ok {ok}/{trials} {why:?}");
            }
        }
    }

    /// Header success against SNR for each alphabet, on AWGN and the moderate
    /// channel, each alphabet tried alone; 16 and 32 too, which no mode sends,
    /// so the choice in `tx::header_alphabet_for` can be checked again. Run with
    /// `cargo test -p decdsss-phy --release -- --ignored --nocapture header_alphabet_curve`.
    #[test]
    #[ignore]
    fn header_alphabet_curve() {
        use crate::tx::{chips_to_audio, header_chips_l, preamble_chips};
        let profile = Profile::W2400;
        let seg = segment(profile, "ds8-1/2", 1, 1);
        let chans: [(&str, Option<Watterson>); 2] =
            [("awgn", None), ("moderate", Some(Watterson::MODERATE))];
        for (name, fading) in chans {
            for l in [16, 32, 64, 128, 256] {
                let mut chips = preamble_chips(profile);
                chips.extend(header_chips_l(&seg.header, l).unwrap());
                chips.extend(std::iter::repeat_n(decdsss_dsp::Iq::new(0.0, 0.0), 4000));
                let audio = chips_to_audio(&chips, profile, 48_000.0, 0.2, 1.0, 1.0);
                let mut row = format!("{name:9} L={l:3}:");
                for snr in [-16.0, -13.0, -10.0, -7.0, -4.0, -1.0, 2.0, 5.0, 8.0] {
                    let trials = 10;
                    let mut ok = 0;
                    for seed in 0..trials {
                        let mut ch = channel(profile, snr);
                        ch.seed = 700 + seed;
                        ch.fading = fading;
                        ch.offset_hz = -15.0 + seed as f64 * 3.0;
                        ch.clock_ppm = -30.0 + seed as f64 * 6.0;
                        let heard = ch.apply(&audio, 48_000.0);
                        if let Ok(r) = to_header_in(&heard, profile, &[l])
                            && r.header.header == seg.header
                        {
                            ok += 1;
                        }
                    }
                    row += &format!(" {snr:+.0}:{ok:2}");
                }
                println!("{row}");
            }
        }
    }

    /// The watch on headers in AWGN and in moderate fading. In the alphabet sent:
    /// every symbol in, one cluster at 1. In a shorter one (as the receiver tries
    /// first): margins just as clear, since a longer Walsh code is a shorter one
    /// repeated with signs, but half the points at −1.
    #[test]
    fn the_watch_tells_the_alphabet_sent_by_its_phase() {
        use super::HeaderWatch;
        use crate::tx::{HEADER_ALPHABETS, header_alphabet_for};
        let profile = Profile::W2400;
        let median = |v: &[f32]| {
            let mut v = v.to_vec();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        for (fading, snr) in [(None, 0.0), (Some(Watterson::MODERATE), 2.0)] {
            for (k, mode) in ["ds4-1/2", "ds8-1/2", "csk64-1/2"].into_iter().enumerate() {
                let seg = segment(profile, mode, 1, 1);
                let sent = header_alphabet_for(seg.header.mode);
                let mut ch = channel(profile, snr);
                ch.fading = fading;
                ch.seed = 40 + k as u64;
                let audio = through(&seg, &ch);
                let Ok(run) = to_header(&audio, profile) else {
                    eprintln!("{mode}: no header this time");
                    continue;
                };
                let rate = run.fe.out_rate();
                for l in HEADER_ALPHABETS.into_iter().filter(|&l| l <= sent) {
                    let mut w = HeaderWatch::new(&run.lock, rate, l, Vec::new());
                    w.advance(&run.buf);
                    let v = w.view();
                    assert_eq!(v.done, v.symbols, "{mode} as {l}");
                    let m = median(&v.margin_db);
                    let flips = v.flipped() as f32 / v.points.len() as f32;
                    eprintln!(
                        "{} {mode} (CSK{sent}) as CSK{l}: median margin {m:.1} dB, \
                         {:.0} % at -1",
                        if fading.is_some() { "fading" } else { "awgn  " },
                        100.0 * flips
                    );
                    if l == sent {
                        assert!(flips < 0.15, "{mode} as {l}: flips {flips}");
                    } else {
                        assert!(m > 6.0, "{mode} as {l}: margin {m}");
                        assert!((0.3..0.7).contains(&flips), "{mode} as {l}: {flips}");
                    }
                }
            }
        }
    }

    #[test]
    fn header_through_fading() {
        let profile = Profile::W3000;
        let seg = segment(profile, "ds8-1/2", 1, 1);
        let mut ch = channel(profile, -4.0);
        ch.fading = Some(Watterson::MODERATE);
        ch.offset_hz = -20.0;
        let audio = through(&seg, &ch);
        let run = to_header(&audio, profile).expect("header through fading");
        assert_eq!(run.header.header, seg.header);
    }
}
