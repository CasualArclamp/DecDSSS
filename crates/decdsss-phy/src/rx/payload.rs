// made by CasualArclamp
//! The payload demodulator: one station's blocks, decoded in order.
//!
//! A block is processed in two passes, and the first runs as the samples
//! arrive. It walks the symbols once, despreading each rake finger and keeping
//! what the second needs: for CSK the Walsh correlations, for DS-PSK the
//! despread symbols and the pilot gains. At the end of the block the second
//! turns that into bit LLRs (a noncoherent Bessel metric for CSK; for DS-PSK a
//! G-RAKE, MMSE combining on a half-chip grid of delays, with pilot-aided
//! maximal-ratio combining on the fingers as the fallback for any codeword it
//! does not decode), de-interleaves, and LDPC-decodes each codeword. While it works it publishes [`Live`], the data behind the
//! scopes: constellation, correlator bank, early/prompt/late, strips.
//!
//! Timing is held by an early/late loop driven by the sum over all fingers
//! (one finger's late arm picks up the next path's correlation tail and the
//! loop settles between the two paths), backed by a *searcher*: at the start
//! of every block, and conservatively every few seconds inside it, the delay
//! profile is measured again from what is known (the pilots, or the symbols
//! just decided), the reference moves onto the strongest path and the fingers
//! are placed on the peaks by deconvolution.
//!
//! A watch on the energy the fingers collect catches the signal lost (a slip
//! of the audio, or a fade); a wide search on the samples kept tells a slip from
//! a fade, and after a slip the tracking state goes back to a snapshot from
//! before it and the symbols are despread again from there on the moved track
//! ([`rewind`]).

use super::despread::{
    Track, csk_bit_llrs, despread, ln_i0, sign_llr, sum, walsh_correlate, walsh_one,
};
use super::frontend::Buf;
use super::grake;
use super::header::{Finger, HeaderResult};
use super::preamble::DelayProfile;
use super::rewind::{self, Judged, Watch};
use crate::codes::{Scrambler, own_code};
use crate::eot;
use crate::framing::{Header, Layout, check_frame};
use crate::tx::{block_chips, pilot_symbol};
use decdsss_core::Family;
use decdsss_core::mode::PILOT_INTERVAL;
use decdsss_dsp::Iq;
use decdsss_fec::interleave::Interleaver;
use decdsss_fec::ldpc::Decoder;
use num_complex::Complex64;
use std::f64::consts::TAU;

/// Rake fingers kept (the header's pick is limited the same way).
const SLOTS: usize = 3;
/// Timing loop noise bandwidth, Hz: wide for the first seconds of a segment, so
/// a clock error left over from the start-up estimates is pulled in at once,
/// then narrow.
const DLL_BW_WIDE_HZ: f64 = 1.0;
const DLL_BW_HZ: f64 = 0.25;
/// Seconds of payload the wide bandwidth lasts.
const DLL_WIDE_S: f64 = 12.0;
/// Seconds of symbols the early/late powers are averaged over per update.
const DLL_GROUP_S: f64 = 0.05;
/// The frequency estimate is revised about every this many seconds.
const FREQ_UPDATE_S: f64 = 1.5;
/// LDPC iterations.
const MAX_ITER: usize = 50;
/// The block-start pre-pass looks at about this many seconds of the block's
/// start, or at least this many pilots / symbols.
const PREPASS_S: f64 = 3.0;
const PREPASS_MIN: usize = 24;
/// The searcher measures the profile again about this often (seconds), over at
/// least this many pilots / symbols so the profile has something to stand on.
const SEARCH_S: f64 = 6.0;
const SEARCH_MIN_PILOTS: usize = 48;
const SEARCH_MIN_SYMBOLS: usize = 24;
/// Mid-block the reference moves only to a path at least this far (chips) from
/// it and this many times stronger than what is at the reference now, so near
/// equal paths cannot take turns pulling it about.
const MOVE_MIN_CHIPS: f64 = 0.3;
const MOVE_POWER_RATIO: f32 = 1.5;
/// A profile is believed when its peak stands this far over its median.
const PROFILE_PEAK_OVER_MEDIAN: f32 = 6.0;
/// The clock error the searcher may learn, as a fraction.
const MAX_CLOCK: f64 = 300e-6;
/// A carrier update needs the phase steps it is built from to agree: the sum
/// of products must be at least this fraction of the sum of their magnitudes.
/// In a fade the steps are random and the ratio is small.
const FREQ_COHERENCE: f32 = 0.4;
/// One update moves the carrier by at most this fraction of the frequency at
/// which one measurement interval is a half turn.
const FREQ_MAX_STEP: f64 = 0.3;
/// The carrier is kept within this many Hz of where the header found it.
const FREQ_MAX_EXCURSION: f64 = 10.0;
/// A path that has dropped out of the measured profile (a fade, as on HF) stays a
/// finger for this many blocks, so it is there when it comes back instead of
/// being lost to the block that follows.
const GHOST_BLOCKS: u8 = 3;
/// Samples ahead of a symbol the buffer must hold: the longest finger lag the
/// profile grid allows, the late arm and the interpolation.
const AHEAD_CHIPS: f64 = 24.0;

/// Points the live scatter plots keep.
pub const LIVE_POINTS: usize = 384;
/// Points in the SNR and carrier strips.
pub const LIVE_STRIP: usize = 192;
/// Rows of the correlator-bank display.
pub const LIVE_ROWS: usize = 40;

/// Test-only switch (DEBUG_FREEZE): keep the fingers and reference as the start
/// gave them, to tell finger management from the channel itself.
fn debug_frozen(what: &str) -> bool {
    cfg!(test)
        && std::env::var("DEBUG_FREEZE")
            .is_ok_and(|v| v == "all" || v.split(',').any(|w| w == what))
}

/// Measurement switch (environment `DECDSSS_RAKE_ONLY`, any value): DS-PSK
/// blocks are combined by the rake alone, as before the G-RAKE, so the two can
/// be compared in one build (`decdsss sweep`). Read once.
fn rake_only() -> bool {
    // OnceLock: a value computed on first use and then shared, thread-safe.
    static RAKE_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RAKE_ONLY.get_or_init(|| std::env::var_os("DECDSSS_RAKE_ONLY").is_some())
}

/// The spreading factor whose pilot interval the gain smoothing and the carrier
/// measurement are timed by: SF 4's pilots are 8 symbols of 4 chips apart, 16.7 ms
/// on `2k4`, and its smoothing is the triangle [1 2 1]/4, which reaches two
/// intervals either side before it is zero.
const SMOOTH_REF_SF: usize = 4;

/// Pilots in one of SF 4's pilot intervals: 1 at SF 4 and above, 2 at SF 2, 4 at
/// SF 1. A pilot is one symbol in eight, so at a smaller SF the pilots come
/// faster, and each carries less: its gain is a sum over L chips, and the thermal
/// noise and the other paths' chips both average down as 1/L, so a raw gain is
/// 1/L as good (derived). The pilot energy a second is the same at every SF, so
/// smoothing over the same *time* gives an estimate as good as SF 4's, where
/// smoothing over the same number of pilots (SF 1: 8 ms) leaves it about 6 dB
/// worse (10·log10 4, derived).
fn pilots_per_ref_interval(sf: usize) -> usize {
    (SMOOTH_REF_SF / sf).max(1)
}

/// How many times SF 4's span in time the smoothing covers at SF 2 and 1. The
/// same span would give an estimate as good as SF 4's, but at SF 2 and 1 that is
/// not enough: a symbol is two chips or one, so the combined symbol SINR is 3 to
/// 6 dB under SF 4's and the channel estimate's error counts for more. Measured
/// (`decdsss sweep --fading moderate --offset 5 --clock-ppm 10`, 64 transmissions,
/// DS1 3/4 at 30 dB Eb/N0, frames lost): 13.7 % at ×1, 6.3 % at ×2, 6.1 % at ×3,
/// 5.7 % at ×4. A longer span follows a fast fade less well: DS1 1/2 on flutter
/// fading at 26 dB lost 35 % at ×1, 45 % at ×2, 77 % at ×3, 96 % at ×4 (DS2 1/2:
/// 2.3, 3.1, 18, 54 %). Two is the compromise.
const SMOOTH_STRETCH_FAST: usize = 2;

/// Pilots either side that the smoothing's triangle reaches (weights `half + 1 -
/// |i|`): [1 2 1] for `half` 1 (SF 4 and above), and at SF 2 and 1 the span in
/// time of `SMOOTH_STRETCH_FAST` times SF 4's (SF 2: 7, SF 1: 15).
fn smooth_half_width(sf: usize) -> usize {
    let stretch = if sf < SMOOTH_REF_SF {
        SMOOTH_STRETCH_FAST
    } else {
        1
    };
    2 * pilots_per_ref_interval(sf) * stretch - 1
}

/// The noise a smoothed pilot gain carries, as a fraction of a raw gain's: the
/// triangle's Σw² / (Σw)², 0.375 for `half` = 1 and 0.042 for SF 1's 15.
fn smooth_noise_gain(half: usize) -> f64 {
    let n = half as i64;
    let (mut s, mut s2) = (0.0, 0.0);
    for i in -n..=n {
        let w = (n + 1 - i.abs()) as f64;
        s += w;
        s2 += w * w;
    }
    s2 / (s * s)
}

/// The pilot gains smoothed by a triangle of `half` pilots either side (weights
/// `half + 1 - |i|`, so [1 2 1]/4 for 1), shorter at the ends where the weights
/// that remain are scaled to one.
fn smooth_gains(g: &[[Iq; SLOTS]], half: usize) -> Vec<[Iq; SLOTS]> {
    (0..g.len())
        .map(|k| {
            let (lo, hi) = (k.saturating_sub(half), (k + half).min(g.len() - 1));
            let mut out = [Iq::new(0.0, 0.0); SLOTS];
            let mut wsum = 0.0f32;
            // Rust note: `take(hi + 1).skip(lo)` is the slice `lo..=hi` as an
            // iterator that still carries the index (`enumerate` first).
            for (j, gj) in g.iter().enumerate().take(hi + 1).skip(lo) {
                let w = (half + 1 - j.abs_diff(k)) as f32;
                for (o, &z) in out.iter_mut().zip(gj) {
                    *o += z * w;
                }
                wsum += w;
            }
            out.map(|z| z / wsum)
        })
        .collect()
}

/// Where a block's known-chip refinement starts: after a slip the block's
/// timing is one line only from the slip on, so there, on the track that symbol
/// was taken on (moved onto the end's finger lags); otherwise the block's start.
fn refine_from(
    ts: &[(Track, f64)],
    shifted: f64,
    slipped_at: Option<usize>,
    start: Track,
) -> (Track, usize) {
    match slipped_at {
        Some(k) => {
            let (mut t, moved) = ts[k];
            t.pos += (shifted - moved) * t.step;
            (t, k)
        }
        None => (start, 0),
    }
}

/// What a slip search measures of one symbol at one lag: (buffer, the track
/// the symbol was taken on, its scrambler chips, lag in chips, a scratch
/// buffer) → power. A trait object (`dyn Fn`), so one search serves DS-PSK
/// and CSK.
type EnergyAt = dyn Fn(&Buf, &Track, &[Iq], f64, &mut [Iq]) -> f64;

/// Harmonic number H_n: the mean of the largest of n exponential variables of
/// unit mean (the largest of a CSK symbol's n code powers when only noise is
/// there).
fn harmonic(n: usize) -> f64 {
    (1..=n).map(|k| 1.0 / k as f64).sum()
}

/// Measurement switch (environment `DECDSSS_NO_REWIND`, any value): no watch,
/// no rewind, as before them. Read once.
fn no_rewind() -> bool {
    static NO_REWIND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_REWIND.get_or_init(|| std::env::var_os("DECDSSS_NO_REWIND").is_some())
}

/// The carrier correction from `acc`, a sum of successive-measurement
/// products (`norm` the sum of their magnitudes) taken `interval_s` apart;
/// zero when the products do not agree (a fade).
fn freq_correction(acc: Iq, norm: f32, interval_s: f64) -> f64 {
    if norm <= 0.0 || acc.norm() < FREQ_COHERENCE * norm {
        return 0.0;
    }
    let raw = 0.5 * f64::from(acc.arg()) / (TAU * interval_s);
    let limit = FREQ_MAX_STEP / (2.0 * interval_s);
    raw.clamp(-limit, limit)
}

/// What one block gave.
#[derive(Clone, Debug)]
pub struct BlockResult {
    /// Block number within the segment.
    pub index: usize,
    /// Number of the first frame (the segment's `frame0` plus the codewords
    /// before this block).
    pub first_frame: usize,
    /// Data of each codeword, `None` if it did not decode.
    pub frames: Vec<Option<Vec<u8>>>,
    pub iterations: Vec<usize>,
    pub fingers: Vec<Finger>,
    pub freq_hz: f64,
    /// Symbol-rate SNR estimate, dB (Es/N0 of a data symbol).
    pub snr_db: f64,
}

/// The data behind the instrument panels, updated as the demodulator works.
#[derive(Clone, Debug, Default)]
pub struct Live {
    /// Slips of the signal the rewind found and followed.
    pub slips: usize,
    /// DS-PSK: recent data symbols after rake combining, scaled to unit
    /// amplitude (the constellation).
    pub symbols: Vec<Iq>,
    /// Each symbol times the conjugate of the one before: tight when the
    /// channel and the carrier are steady.
    pub diffs: Vec<Iq>,
    /// The timing loop's early, prompt and late magnitudes, prompt = 1.
    pub epl: [f32; 3],
    /// Symbol-rate SNR (dB), about one point every half second.
    pub snr_db: Vec<f32>,
    /// The carrier offset (Hz) at the same points.
    pub freq_hz: Vec<f32>,
    /// CSK: the correlator bank, one row per recent symbol, power of the main
    /// finger against Walsh code number, each row scaled to its maximum.
    pub bank: Vec<Vec<f32>>,
    /// The last symbol's chips as received (carrier off, scrambler still on) and
    /// after the scrambler is taken off.
    pub raw_chips: Vec<Iq>,
    pub despread_chips: Vec<Iq>,
    /// The latest delay profile, power against lag, and the fingers it gave.
    pub profile: Vec<f32>,
    pub fingers: Vec<Finger>,
    /// LLRs of the last codeword that went to the decoder.
    pub soft: Vec<f32>,
    pub block: usize,
    pub symbol: usize,
    pub symbols_in_block: usize,
}

fn push_cap<T>(v: &mut Vec<T>, x: T, cap: usize) {
    if v.len() >= cap {
        v.remove(0);
    }
    v.push(x);
}

/// Loop gains of a second-order loop with ζ = 0.707.
fn loop_gains(bn_hz: f64, update_s: f64) -> (f64, f64) {
    let zeta = 0.707;
    let wn = 2.0 * bn_hz / (zeta + 1.0 / (4.0 * zeta));
    let w = wn * update_s;
    (2.0 * zeta * w, w * w)
}

/// The timing loop. Early and late magnitudes, summed over every rake finger
/// (each finger's own bias towards a neighbouring path points the other way
/// for the neighbour, so the sum is far better behaved than one finger's),
/// are averaged over a group of symbols, then one update.
#[derive(Clone)]
struct Dll {
    kp: f64,
    ki: f64,
    wide: (f64, f64),
    narrow: (f64, f64),
    group: usize,
    e: f64,
    l: f64,
    n: usize,
    /// Integrator: chips of correction per update.
    rate: f64,
    /// The last group's early and late sums, for the display.
    last: (f64, f64),
}

impl Dll {
    fn new(symbol_s: f64) -> Dll {
        let group = ((DLL_GROUP_S / symbol_s).ceil() as usize).max(1);
        let update_s = group as f64 * symbol_s;
        let narrow = loop_gains(DLL_BW_HZ, update_s);
        let wide = loop_gains(DLL_BW_WIDE_HZ, update_s);
        Dll {
            kp: wide.0,
            ki: wide.1,
            wide,
            narrow,
            group,
            e: 0.0,
            l: 0.0,
            n: 0,
            rate: 0.0,
            last: (0.0, 0.0),
        }
    }

    /// Wide gains while the segment is young, narrow after.
    fn set_wide(&mut self, wide: bool) {
        (self.kp, self.ki) = if wide { self.wide } else { self.narrow };
    }

    fn reset(&mut self) {
        self.e = 0.0;
        self.l = 0.0;
        self.n = 0;
        self.rate = 0.0;
    }

    /// Adds one symbol's early and late magnitudes; returns the correction in
    /// chips when a group is complete.
    fn push(&mut self, e: f64, l: f64) -> Option<f64> {
        self.e += e;
        self.l += l;
        self.n += 1;
        if self.n < self.group {
            return None;
        }
        // (L − E)/(L + E) is positive when the true timing is later than
        // ours; for a raised-cosine correlation with the arms ±¼ chip apart
        // it is about 1.33 times the error in chips.
        let d = (self.l - self.e) / (self.l + self.e).max(1e-30);
        let err = d / 1.33;
        self.last = (self.e, self.l);
        self.e = 0.0;
        self.l = 0.0;
        self.n = 0;
        self.rate += self.ki * err;
        Some(self.kp * err + self.rate)
    }
}

/// A CSK block in progress.
struct CskWork {
    scr: Vec<Iq>,
    start: Track,
    step0: f64,
    t: Track,
    s: usize,
    s_count: usize,
    /// W rows for every symbol and finger, flat, with where each symbol's rows
    /// start and how many fingers it had.
    w: Vec<Iq>,
    index: Vec<(usize, usize)>,
    decisions: Vec<usize>,
    peak_prev: Option<Iq>,
    freq_acc: Iq,
    freq_norm: f32,
    /// Track at the start of the searcher's chunk, and its first symbol.
    tc: Track,
    s_lo: usize,
    u: Vec<Iq>,
    ue: Vec<Iq>,
    ul: Vec<Iq>,
    /// As in [`DsWork`]: the track each symbol was taken on and the fingers'
    /// moves, snapshots, the search, the replay and the slip.
    ts: Vec<(Track, f64)>,
    shifted: f64,
    snaps: Vec<CskSnap>,
    search: bool,
    replay_until: usize,
    slip: Option<(usize, f64)>,
    slipped_at: Option<usize>,
}

/// What a CSK block's tracking was at a symbol.
#[derive(Clone)]
struct CskSnap {
    s: usize,
    t: Track,
    w_len: usize,
    peak_prev: Option<Iq>,
    freq_acc: Iq,
    freq_norm: f32,
    tc: Track,
    s_lo: usize,
    shifted: f64,
    dll: Dll,
    fingers: Vec<Finger>,
    finger_age: Vec<u8>,
}

/// A DS-PSK block in progress.
struct DsWork {
    scr: Vec<Iq>,
    start: Track,
    step0: f64,
    t: Track,
    s: usize,
    s_count: usize,
    /// y[s][f]: the despread symbol per finger slot; zero for unused slots.
    y: Vec<[Iq; SLOTS]>,
    /// g[k][f]: pilot gain estimates.
    g: Vec<[Iq; SLOTS]>,
    pilot_no: usize,
    freq_acc: Iq,
    freq_norm: f32,
    tc: Track,
    s_lo: usize,
    u: Vec<Iq>,
    ue: Vec<Iq>,
    ul: Vec<Iq>,
    /// The track each symbol was despread on, and the chips the fingers had
    /// been moved by then (`shifted` at the time): the G-RAKE despreads the
    /// block again from these at the end.
    ts: Vec<(Track, f64)>,
    /// Chips the reference has moved inside the block (the fingers moved back
    /// as far, so a path stays where it is).
    shifted: f64,
    /// The previous displayed symbol, for the differential plot.
    prev_z: Option<Iq>,
    /// Running mean of the squared distance of a displayed symbol from its
    /// nearest QPSK point: 1/this is the symbol-rate SNR.
    err2: f32,
    /// Snapshots of the tracking state at the watch's windows, the block's
    /// start first.
    snaps: Vec<DsSnap>,
    /// The watch asked for a search before the next symbol.
    search: bool,
    /// After a rewind, the symbols up to here come again and the watch does
    /// not judge them a second time.
    replay_until: usize,
    /// A slip the search found: from this symbol the signal is this many chips
    /// away.
    slip: Option<(usize, f64)>,
    /// The symbol of the last slip followed: the block's timing is one line
    /// only from there, so the known-chip refinement starts there.
    slipped_at: Option<usize>,
}

/// What a DS-PSK block's tracking was at a symbol: enough to go on from there
/// as though the symbols after it had not come yet.
#[derive(Clone)]
struct DsSnap {
    s: usize,
    t: Track,
    pilot_no: usize,
    freq_acc: Iq,
    freq_norm: f32,
    tc: Track,
    s_lo: usize,
    shifted: f64,
    prev_z: Option<Iq>,
    err2: f32,
    dll: Dll,
    fingers: Vec<Finger>,
    finger_age: Vec<u8>,
}

enum Work {
    Csk(Box<CskWork>),
    Ds(Box<DsWork>),
}

pub struct PayloadRx {
    pub header: Header,
    layout: Layout,
    scr: Scrambler,
    pub track: Track,
    pub fingers: Vec<Finger>,
    /// Blocks each finger has gone unmeasured (0: measured), parallel to
    /// `fingers`.
    finger_age: Vec<u8>,
    /// The carrier the header found; the loop stays near it.
    f0: f64,
    chip0: usize,
    next_block: usize,
    dll: Dll,
    rate: f64,
    work: Option<Work>,
    live: Live,
    /// Symbols since the last strip point, and their signal and noise powers
    /// summed.
    strip_count: usize,
    strip_signal: f64,
    strip_noise: f64,
    /// Set when the end-of-transmission postamble was heard: the peak-over-median
    /// ratio of its delay profile.
    eot: Option<f32>,
    /// The postamble was looked for at the start of the block in progress.
    eot_probed: bool,
    /// A live stream whose blocks are shorter than the postamble's second: the block
    /// ended before the samples for the question "was what came the postamble?"
    /// were in, so it is asked later, of this block's start and chips-before.
    eot_pending: Option<(Track, usize)>,
    /// The fingers and the reference were measured from known chips just before:
    /// from the header at the start, and after every block that decoded whole
    /// (the known-chip refinement). Then the block starts at once; otherwise it
    /// waits for the seconds of pilots its own pre-pass measures from.
    fresh: bool,
    /// The watch on the energy the fingers collect (see [`rewind`]); it runs on
    /// from block to block.
    watch: Watch,
    /// The last block's interleaved LLRs (kept for the tests).
    #[cfg(test)]
    pub last_llrs: Vec<f32>,
}

impl PayloadRx {
    pub fn new(h: &HeaderResult, rate: f64) -> Option<PayloadRx> {
        let header = h.header.clone();
        let layout = Layout::new(header.profile, header.mode, header.depth);
        let symbol_s = f64::from(header.mode.spread) / f64::from(header.profile.chip_rate());
        Some(PayloadRx {
            scr: Scrambler::new(own_code(&header.call)?),
            layout,
            header,
            track: h.track,
            fingers: h.fingers.clone(),
            finger_age: Vec::new(),
            f0: h.track.freq,
            chip0: 0,
            next_block: 0,
            dll: Dll::new(symbol_s),
            rate,
            work: None,
            live: Live {
                fingers: h.fingers.clone(),
                ..Live::default()
            },
            strip_count: 0,
            strip_signal: 0.0,
            strip_noise: 0.0,
            eot: None,
            eot_probed: false,
            eot_pending: None,
            fresh: true,
            watch: Watch::default(),
            #[cfg(test)]
            last_llrs: Vec::new(),
        })
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn done(&self) -> bool {
        self.next_block >= usize::from(self.header.nblocks)
    }

    /// A live voice stream: speech with no frame total, so no block count that
    /// says where it ends.
    pub fn is_live(&self) -> bool {
        self.header.total_frames == 0 && self.header.ptype.is_voice()
    }

    /// Moves past `n` blocks without decoding them (tests that measure the
    /// postamble from a genie start, whose track is exact).
    #[cfg(test)]
    pub fn skip_blocks(&mut self, n: usize) {
        self.track.advance(n * self.layout.chips());
        self.chip0 += n * self.layout.chips();
        self.next_block += n;
    }

    /// Whether the end-of-transmission postamble was heard (its delay profile's
    /// peak over its median).
    pub fn eot(&self) -> Option<f32> {
        self.eot
    }

    /// Looks for the postamble at the next block's place: `Some(ratio)` once the
    /// samples for it are in (the ratio is the delay profile's peak over its
    /// median; [`eot::EOT_PEAK_OVER_MEDIAN`] or more is a yes), `None` while they
    /// are not or while a block is in progress. The chips are the station's
    /// scrambler on from the last block times the B sequence, so data after the
    /// last block, which is the scrambler times symbols, does not correlate.
    pub fn probe_eot(&self, buf: &Buf) -> Option<f32> {
        if self.work.is_some() {
            return None;
        }
        self.probe_eot_at(buf, &self.track, self.chip0)
    }

    /// Chips the postamble question needs after a block's start: the delay profile's
    /// longest lag and the postamble itself.
    fn probe_chips(&self) -> f64 {
        DelayProfile::lag(DelayProfile::LAGS) + eot::len(self.header.profile) as f64
    }

    /// The postamble's place is where the block that starts at `at` would be (its
    /// scrambler position is `chip0`, the chips sent before that block).
    fn probe_eot_at(&self, buf: &Buf, at: &Track, chip0: usize) -> Option<f32> {
        let profile = self.header.profile;
        let reach = self.probe_chips();
        if at.pos + reach * at.step + 16.0 > buf.end() as f64 {
            return None;
        }
        let refs = eot::chips(profile, &self.scr, chip0);
        let p = super::refine::profile_known(buf, at, &refs, eot::segment_chips(profile));
        let mut sorted = p.power.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2].max(1e-30);
        Some(sorted[sorted.len() - 1] / median)
    }

    pub fn blocks_done(&self) -> usize {
        self.next_block
    }

    /// What the instrument panels show.
    pub fn live(&self) -> &Live {
        &self.live
    }

    /// Earliest sample still needed: the block in progress needs its start for
    /// the known-chip refinement at its end.
    pub fn oldest_needed(&self) -> i64 {
        let min_lag = self.fingers.iter().map(|f| f.lag).fold(0.0f64, f64::min);
        let mut from = match &self.work {
            Some(Work::Csk(w)) => w.start,
            Some(Work::Ds(w)) => w.start,
            None => self.track,
        };
        // An open question about the postamble needs the samples from its block's start.
        if let Some((at, _)) = &self.eot_pending
            && at.pos < from.pos
        {
            from = *at;
        }
        let reach = rewind::SLIP_REACH_S * f64::from(self.header.profile.chip_rate());
        (from.pos + (min_lag - reach - 10.0) * from.step) as i64
    }

    /// Works on the block in progress with the samples there are; returns it
    /// when it is complete.
    pub fn step(&mut self, buf: &Buf) -> Option<BlockResult> {
        if self.done() || self.eot.is_some() {
            return None;
        }
        // The question a short block left open: has the postamble's second come in?
        if let Some((at, chip0)) = self.eot_pending
            && let Some(r) = self.probe_eot_at(buf, &at, chip0)
        {
            self.eot_pending = None;
            if r >= eot::EOT_PEAK_OVER_MEDIAN {
                self.eot = Some(r);
                self.work = None;
                return None;
            }
        }
        if self.work.is_none() {
            // A block starts at once when the fingers are fresh (the display then
            // follows the signal all the way through, with no wait between
            // blocks); otherwise once its pre-pass has its first seconds.
            let prepass = !self.fresh;
            let l = f64::from(self.header.mode.spread);
            let cr = f64::from(self.header.profile.chip_rate());
            let span = if prepass {
                (PREPASS_S + 1.0)
                    .max(PREPASS_MIN as f64 * 8.0 * l / cr)
                    .min(self.layout.seconds())
            } else {
                0.0
            };
            let need = self.track.pos + (span * cr + AHEAD_CHIPS) * self.track.step + 16.0;
            if need > buf.end() as f64 {
                return None;
            }
            self.begin_block(buf, prepass);
        }
        // A live stream has no block count: a second into each block, ask whether
        // what came was the postamble (the block just begun is then not a block).
        if self.is_live() && self.next_block > 0 && !self.eot_probed {
            let start = match &self.work {
                Some(Work::Csk(w)) => w.start,
                Some(Work::Ds(w)) => w.start,
                None => return None,
            };
            if let Some(r) = self.probe_eot_at(buf, &start, self.chip0) {
                self.eot_probed = true;
                if r >= eot::EOT_PEAK_OVER_MEDIAN {
                    self.eot = Some(r);
                    self.work = None;
                    return None;
                }
            }
        }
        let work = self.work.take()?;
        let start = match &work {
            Work::Csk(w) => w.start,
            Work::Ds(w) => w.start,
        };
        let chip0 = self.chip0;
        match work {
            Work::Csk(mut w) => {
                if self.advance_csk(buf, &mut w) {
                    let r = self.finish_csk(buf, *w);
                    self.end_block(start, chip0);
                    Some(r)
                } else {
                    self.work = Some(Work::Csk(w));
                    None
                }
            }
            Work::Ds(mut w) => {
                if self.advance_ds(buf, &mut w) {
                    let r = self.finish_ds(buf, *w);
                    self.end_block(start, chip0);
                    Some(r)
                } else {
                    self.work = Some(Work::Ds(w));
                    None
                }
            }
        }
    }

    /// `start` and `chip0` are the block's own (they are read before it is worked on).
    fn end_block(&mut self, start: Track, chip0: usize) {
        // A live stream's block that ended before the postamble question could be
        // asked of it (the block is shorter than the second the postamble takes, as
        // DS1 and DS2 blocks are): keep the question for when the samples are in.
        if self.is_live()
            && self.next_block > 0
            && !self.eot_probed
            && (self.layout.chips() as f64) < self.probe_chips()
        {
            self.eot_pending = Some((start, chip0));
        }
        self.chip0 += self.layout.chips();
        self.next_block += 1;
    }

    fn begin_block(&mut self, buf: &Buf, prepass: bool) {
        self.eot_probed = false;
        // A loss carried over from the last block: its symbols are gone, so
        // there is nothing to rewind to before this block's start.
        self.watch.restart_window();
        if let Some(l) = self.watch.lost.as_mut() {
            l.since = 0;
        }
        let cr = f64::from(self.header.profile.chip_rate());
        self.dll.set_wide((self.chip0 as f64) / cr < DLL_WIDE_S);
        let l = usize::from(self.header.mode.spread);
        let s_count = self.layout.symbols();
        let scr = self.scr.chips(self.chip0, s_count * l);
        self.live.block = self.next_block;
        self.live.symbol = 0;
        self.live.symbols_in_block = s_count;
        let blank = || vec![Iq::new(0.0, 0.0); l];
        match self.layout.mode.family {
            Family::Csk | Family::BiCsk => {
                if prepass {
                    self.csk_prepass(buf, &scr);
                }
                let start = self.track;
                self.work = Some(Work::Csk(Box::new(CskWork {
                    scr,
                    start,
                    step0: start.step,
                    t: start,
                    s: 0,
                    s_count,
                    w: Vec::with_capacity(s_count * SLOTS * l),
                    index: Vec::with_capacity(s_count),
                    decisions: Vec::with_capacity(s_count),
                    peak_prev: None,
                    freq_acc: Iq::new(0.0, 0.0),
                    freq_norm: 0.0,
                    tc: start,
                    s_lo: 0,
                    u: blank(),
                    ue: blank(),
                    ul: blank(),
                    ts: Vec::with_capacity(s_count),
                    shifted: 0.0,
                    snaps: Vec::new(),
                    search: false,
                    replay_until: 0,
                    slip: None,
                    slipped_at: None,
                })));
                if let Some(Work::Csk(mut w)) = self.work.take() {
                    let first = self.csk_snap(&w);
                    w.snaps.push(first);
                    self.work = Some(Work::Csk(w));
                }
            }
            Family::DsPsk => {
                if prepass {
                    self.ds_prepass(buf, &scr);
                }
                let start = self.track;
                let pilots = s_count / PILOT_INTERVAL as usize + 1;
                self.work = Some(Work::Ds(Box::new(DsWork {
                    scr,
                    start,
                    step0: start.step,
                    t: start,
                    s: 0,
                    s_count,
                    y: vec![[Iq::new(0.0, 0.0); SLOTS]; s_count],
                    g: vec![[Iq::new(0.0, 0.0); SLOTS]; pilots],
                    pilot_no: 0,
                    freq_acc: Iq::new(0.0, 0.0),
                    freq_norm: 0.0,
                    tc: start,
                    s_lo: 0,
                    u: blank(),
                    ue: blank(),
                    ul: blank(),
                    ts: Vec::with_capacity(s_count),
                    shifted: 0.0,
                    prev_z: None,
                    err2: 0.5,
                    snaps: Vec::new(),
                    search: false,
                    replay_until: 0,
                    slip: None,
                    slipped_at: None,
                })));
                // The block's start is the first snapshot. (`take` moves the
                // work out of `self` so `ds_snap` can borrow `self` meanwhile.)
                if let Some(Work::Ds(mut w)) = self.work.take() {
                    let first = self.ds_snap(&w);
                    w.snaps.push(first);
                    self.work = Some(Work::Ds(w));
                }
            }
        }
        self.live.fingers = self.fingers.clone();
    }

    /// LDPC-decodes the interleaved block LLRs into frames.
    fn decode(&self, llrs: &[f32]) -> (Vec<Option<Vec<u8>>>, Vec<usize>) {
        let bits = self.layout.coded_bits();
        let il = Interleaver::new(bits);
        let coded = il.invert(&llrs[..bits]);
        let code = self.layout.code();
        let mut dec = Decoder::new(code);
        let mut frames = Vec::new();
        let mut iters = Vec::new();
        for cw in coded.chunks(code.n()) {
            let o = dec.decode(cw, MAX_ITER);
            iters.push(o.iterations);
            frames.push(if o.converged {
                check_frame(&dec.bits()[..code.k()])
            } else {
                None
            });
        }
        (frames, iters)
    }

    /// Decodes a block's LLRs and finishes it. A codeword `llrs` does not decode
    /// is tried again from `fallback` (the rake's, behind the G-RAKE's).
    fn finish(
        &mut self,
        buf: &Buf,
        llrs: &[f32],
        fallback: Option<&[f32]>,
        snr_db: f64,
        block_start: Track,
        from_symbol: usize,
    ) -> BlockResult {
        #[cfg(test)]
        {
            self.last_llrs = llrs.to_vec();
        }
        // The last codeword's soft bits, for the display.
        let n = self.layout.code().n();
        let coded =
            Interleaver::new(self.layout.coded_bits()).invert(&llrs[..self.layout.coded_bits()]);
        self.live.soft = coded[coded.len() - n..].to_vec();
        let (mut frames, mut iterations) = self.decode(llrs);
        if let Some(fb) = fallback.filter(|_| frames.iter().any(Option::is_none)) {
            let (frames2, iterations2) = self.decode(fb);
            for (i, f2) in frames2.into_iter().enumerate() {
                if frames[i].is_none() && f2.is_some() {
                    frames[i] = f2;
                    iterations[i] = iterations2[i];
                }
            }
        }
        let first_frame = usize::from(self.header.frame0) + self.next_block * self.layout.depth;
        self.fresh = false;
        if frames.iter().all(Option::is_some) && !debug_frozen("refresh") {
            self.refresh_fingers(buf, &frames, block_start, from_symbol);
            self.fresh = true;
        }
        self.live.fingers = self.fingers.clone();
        BlockResult {
            index: self.next_block,
            first_frame,
            frames,
            iterations,
            fingers: self.fingers.clone(),
            freq_hz: self.track.freq,
            snr_db,
        }
    }

    /// With every codeword of the block decoded, the transmitted chips are
    /// known: refine timing, clock and carrier against them, then correlate at
    /// each lag for the block's delay profile and re-pick the fingers from it.
    ///
    /// `start` is the track at symbol `from` (0 unless the block slipped: its
    /// timing is one line only after the slip).
    fn refresh_fingers(
        &mut self,
        buf: &Buf,
        frames: &[Option<Vec<u8>>],
        start: Track,
        from: usize,
    ) {
        let data: Vec<Vec<u8>> = frames.iter().map(|f| f.clone().unwrap()).collect();
        let chips = block_chips(&self.layout, &data, &self.scr, self.chip0);
        let l = usize::from(self.header.mode.spread);
        let chips = &chips[(from * l).min(chips.len())..];
        let main_lag = self.fingers.first().map_or(0.0, |f| f.lag);
        let cr = self.rate / super::frontend::SPS as f64;
        let seg = super::refine::segment_chips(cr);
        let refined = super::refine::refine(buf, &start, chips, main_lag, seg, 60e-6);
        let (fingers, shift) = match &refined {
            Some(r) => super::refine::tail_fingers(buf, r, &start, chips, seg),
            None => {
                let p = super::refine::profile_known(buf, &start, chips, seg);
                super::refine::fingers_from_profile(&p)
            }
        };
        if !debug_frozen("track") {
            match refined {
                Some(r) => {
                    self.track.pos = r.pos_end + shift * r.step;
                    self.track.step = r.step;
                    self.track.freq = r.freq;
                    self.track.phase = 0.0;
                    self.dll.reset();
                }
                None => self.track.pos += shift * self.track.step,
            }
        }
        if !debug_frozen("fingers") {
            self.set_fingers(fingers, shift);
        }
    }

    /// Takes the fingers just measured (lags relative to the reference after it
    /// has moved `shift` chips), and keeps a finger that was there before but is
    /// not now (its lag moved to the new reference) for a few blocks, as long as
    /// there is a slot: a path in a fade is likely to come back, and the rake
    /// cannot use it if it was dropped.
    fn set_fingers(&mut self, measured: Vec<Finger>, shift: f64) {
        let mut next = measured;
        let mut ages = vec![0u8; next.len()];
        for (i, old) in self.fingers.iter().enumerate() {
            let lag = old.lag - shift;
            if next.iter().any(|f| (f.lag - lag).abs() < 0.6) {
                continue;
            }
            let age = self.finger_age.get(i).copied().unwrap_or(0) + 1;
            if age <= GHOST_BLOCKS && next.len() < SLOTS && !debug_frozen("ghosts") {
                next.push(Finger { lag, power: 0.3 });
                ages.push(age);
            }
        }
        self.fingers = next;
        self.finger_age = ages;
    }

    /// Re-places the reference and the fingers from a measured profile, if the
    /// profile has a clear peak; returns the chips the reference moved.
    fn adopt(&mut self, profile: &DelayProfile) -> Option<f64> {
        self.live.profile = profile.power.clone();
        let mut p = profile.power.clone();
        p.sort_by(f32::total_cmp);
        let median = p[p.len() / 2].max(1e-30);
        let peak = p[p.len() - 1];
        if peak < PROFILE_PEAK_OVER_MEDIAN * median {
            return None;
        }
        let (fingers, shift) = super::refine::fingers_from_profile(profile);
        self.set_fingers(fingers, shift);
        self.live.fingers = self.fingers.clone();
        Some(shift)
    }

    /// Mid-block, a measured profile only moves the reference, and only to a
    /// clearly better path; the fingers keep their places relative to it.
    /// Returns the chips moved.
    fn adopt_track(&mut self, profile: &DelayProfile) -> Option<f64> {
        self.live.profile = profile.power.clone();
        let mut sorted = profile.power.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2].max(1e-30);
        let (lag, power) = profile.fingers(1, 99.0, 0.0, 0.85).first().copied()?;
        if power < PROFILE_PEAK_OVER_MEDIAN * median || lag.abs() < MOVE_MIN_CHIPS {
            return None;
        }
        // What is at the reference now.
        let i0 = ((0.0 - DelayProfile::LAG0) / DelayProfile::LAG_STEP).round() as usize;
        if power < MOVE_POWER_RATIO * profile.power[i0] {
            return None;
        }
        for f in &mut self.fingers {
            f.lag -= lag;
        }
        self.live.fingers = self.fingers.clone();
        Some(lag)
    }

    /// The delay profile of the DS-PSK pilots in symbols `s_lo..s_hi`, where
    /// `tc` is the track at symbol `s_lo` and `scr` the block's scrambler chips.
    fn profile_ds(
        &self,
        buf: &Buf,
        tc: &Track,
        scr: &[Iq],
        s_lo: usize,
        s_hi: usize,
    ) -> DelayProfile {
        let l = usize::from(self.header.mode.spread);
        let per = PILOT_INTERVAL as usize;
        let dphi = tc.dphase();
        let mut power = vec![0.0f32; DelayProfile::LAGS];
        for (i, pw) in power.iter_mut().enumerate() {
            let lag = DelayProfile::lag(i);
            for n in (s_lo..s_hi).filter(|n| n % per == 0) {
                let p = pilot_symbol(n / per).conj();
                let mut acc = Iq::new(0.0, 0.0);
                for k in 0..l {
                    let rel = (n - s_lo) * l + k;
                    let (s, c) = (tc.phase + rel as f64 * dphi).sin_cos();
                    let x = buf.at(tc.pos + (rel as f64 + lag) * tc.step);
                    acc += x * Iq::new(c as f32, -(s as f32)) * scr[n * l + k].conj() * p;
                }
                *pw += (acc / l as f32).norm_sqr();
            }
        }
        DelayProfile { power }
    }

    /// The delay profile from CSK symbols `s_lo..s_lo + codes.len()` whose
    /// Walsh codes are known or decided.
    fn profile_csk(
        &self,
        buf: &Buf,
        tc: &Track,
        scr: &[Iq],
        s_lo: usize,
        codes: &[usize],
    ) -> DelayProfile {
        let l = usize::from(self.header.mode.spread);
        let dphi = tc.dphase();
        let mut u = vec![Iq::new(0.0, 0.0); l];
        let mut power = vec![0.0f32; DelayProfile::LAGS];
        for (i, pw) in power.iter_mut().enumerate() {
            for (j, &m) in codes.iter().enumerate() {
                let mut tt = *tc;
                tt.pos = tc.pos + (j * l) as f64 * tc.step;
                tt.phase = tc.phase + (j * l) as f64 * dphi;
                let s = s_lo + j;
                despread(
                    buf,
                    &tt,
                    &scr[s * l..(s + 1) * l],
                    DelayProfile::lag(i),
                    &mut u,
                );
                *pw += walsh_one(&u, m).norm_sqr();
            }
        }
        DelayProfile { power }
    }

    /// Applies a chunk's measured shift: the reference moves, and the clock
    /// estimate learns from how far it had to.
    fn apply_shift(&self, t: &mut Track, shift: f64, chunk_chips: f64, step0: f64) {
        t.pos += shift * t.step;
        let learn = 0.3 * shift / chunk_chips.max(1.0);
        t.step =
            (t.step * (1.0 + learn)).clamp(step0 * (1.0 - MAX_CLOCK), step0 * (1.0 + MAX_CLOCK));
    }

    /// The CSK block-start pre-pass: over the first symbols, find the lag at
    /// which the Walsh peaks are strongest, then the profile from the codes
    /// decided there, and set the fingers and the reference from it.
    fn csk_prepass(&mut self, buf: &Buf, scr: &[Iq]) {
        let l = usize::from(self.header.mode.spread);
        let t = self.track;
        let dphi = t.dphase();
        let sym_s = l as f64 / f64::from(self.header.profile.chip_rate());
        let n = self
            .layout
            .symbols()
            .min(((PREPASS_S / sym_s).ceil() as usize).max(PREPASS_MIN));
        let mut u = vec![Iq::new(0.0, 0.0); l];
        // The Walsh peak of symbol s at a lag: its power and its code.
        let at_lag = |s: usize, lag: f64, u: &mut [Iq]| -> (f32, usize) {
            let mut tt = t;
            tt.pos = t.pos + (s * l) as f64 * t.step;
            tt.phase = t.phase + (s * l) as f64 * dphi;
            despread(buf, &tt, &scr[s * l..(s + 1) * l], lag, u);
            let w = walsh_correlate(u);
            w.iter()
                .enumerate()
                .map(|(m, z)| (z.norm_sqr(), m))
                .fold((0.0f32, 0usize), |a, b| if b.0 > a.0 { b } else { a })
        };
        let mut score = vec![0.0f32; DelayProfile::LAGS];
        for (i, sc) in score.iter_mut().enumerate() {
            for s in 0..n {
                *sc += at_lag(s, DelayProfile::lag(i), &mut u).0;
            }
        }
        let best = score
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map_or(0, |(i, _)| i);
        let codes: Vec<usize> = (0..n)
            .map(|s| at_lag(s, DelayProfile::lag(best), &mut u).1)
            .collect();
        let profile = self.profile_csk(buf, &t, scr, 0, &codes);
        if let Some(shift) = self.adopt(&profile) {
            self.track.pos += shift * self.track.step;
        }
    }

    /// The DS-PSK block-start pre-pass: the profile of the first pilots.
    fn ds_prepass(&mut self, buf: &Buf, scr: &[Iq]) {
        let l = usize::from(self.header.mode.spread);
        let per = PILOT_INTERVAL as usize;
        let all = self.layout.symbols() / per + 1;
        // Only the start of the block: a clock error left over from the start-up
        // estimates moves the later pilots by chips.
        let pilot_s = per as f64 * l as f64 / f64::from(self.header.profile.chip_rate());
        let pilots = all.min(((PREPASS_S / pilot_s).ceil() as usize).max(PREPASS_MIN));
        let t = self.track;
        let profile = self.profile_ds(buf, &t, scr, 0, pilots * per);
        if let Some(shift) = self.adopt(&profile) {
            self.track.pos += shift * self.track.step;
        }
    }

    /// Takes one symbol's signal and noise powers and adds a strip point (SNR
    /// and carrier) about twice a second. The SNR point is the ratio of the
    /// sums over the symbols since the last point, so it is a half-second
    /// average rather than one symbol's draw (a CSK symbol's alone wanders by
    /// decibels).
    fn strip(&mut self, signal: f32, noise: f32, freq: f64, sym_s: f64) {
        self.strip_count += 1;
        self.strip_signal += f64::from(signal);
        self.strip_noise += f64::from(noise);
        if self.strip_count as f64 * sym_s >= 0.5 {
            let snr_db =
                10.0 * (self.strip_signal.max(1e-30) / self.strip_noise.max(1e-30)).log10();
            self.strip_count = 0;
            self.strip_signal = 0.0;
            self.strip_noise = 0.0;
            push_cap(&mut self.live.snr_db, snr_db as f32, LIVE_STRIP);
            push_cap(&mut self.live.freq_hz, freq as f32, LIVE_STRIP);
        }
    }

    /// Processes CSK symbols while the buffer holds them; true when the block
    /// is done.
    fn advance_csk(&mut self, buf: &Buf, wk: &mut CskWork) -> bool {
        let l = usize::from(self.header.mode.spread);
        let t_sym = l as f64 * wk.t.step / wk.t.rate;
        let freq_every = ((FREQ_UPDATE_S / t_sym).ceil() as usize).max(4);
        let chunk = (((SEARCH_S / t_sym).ceil() as usize).max(SEARCH_MIN_SYMBOLS)).min(wk.s_count);
        let win = ((rewind::WATCH_S / t_sym).ceil() as usize).max(rewind::WATCH_MIN_SYMBOLS);
        let max_windows = ((rewind::LOST_MAX_S / (win as f64 * t_sym)).ceil() as usize).max(2);
        let reach = rewind::SLIP_REACH_S * f64::from(self.header.profile.chip_rate());
        let h_l = harmonic(l);
        let bicsk = self.header.mode.family == Family::BiCsk;
        while wk.s < wk.s_count {
            if wk.t.pos + (l as f64 + AHEAD_CHIPS) * wk.t.step + 16.0 > buf.end() as f64 {
                return false;
            }
            if wk.search {
                let ahead = reach + self.fingers.iter().map(|f| f.lag).fold(0.0, f64::max);
                if wk.t.pos + (l as f64 + ahead + AHEAD_CHIPS) * wk.t.step + 16.0 > buf.end() as f64
                {
                    return false;
                }
                wk.search = false;
                if self.resync_csk(buf, wk, win) {
                    continue;
                }
            }
            if let Some((at, d)) = wk.slip
                && at == wk.s
            {
                wk.t.pos += d * wk.t.step;
                wk.slip = None;
                wk.slipped_at = Some(wk.s);
            }
            let s = wk.s;
            wk.ts.push((wk.t, wk.shifted));
            let sc = &wk.scr[s * l..(s + 1) * l];
            let nf = self.fingers.len();
            let at = wk.w.len();
            wk.index.push((at, nf));
            for (fi, f) in self.fingers.iter().enumerate() {
                despread(buf, &wk.t, sc, f.lag, &mut wk.u);
                if fi == 0 {
                    self.live.despread_chips = wk.u.clone();
                }
                wk.w.extend(walsh_correlate(&wk.u));
            }
            // Decision: the code with the most weighted power over fingers.
            let mut best = (0usize, -1.0f32);
            for m in 0..l {
                let p: f32 = (0..nf)
                    .map(|f| self.fingers[f].power * wk.w[at + f * l + m].norm_sqr())
                    .sum();
                if p > best.1 {
                    best = (m, p);
                }
            }
            let m = best.0;
            wk.decisions.push(m);
            // Early and late arms of every finger, at the decided code.
            let (mut e_sum, mut l_sum, mut p_sum) = (0.0f64, 0.0f64, 0.0f64);
            for f in &self.fingers {
                despread(buf, &wk.t, sc, f.lag - 0.25, &mut wk.ue);
                despread(buf, &wk.t, sc, f.lag + 0.25, &mut wk.ul);
                e_sum += f64::from(walsh_one(&wk.ue, m).norm());
                l_sum += f64::from(walsh_one(&wk.ul, m).norm());
            }
            for (fi, _) in self.fingers.iter().enumerate() {
                p_sum += f64::from(wk.w[at + fi * l + m].norm());
            }
            if let Some(c) = self.dll.push(e_sum, l_sum) {
                wk.t.pos += c * wk.t.step;
                let p = p_sum.max(1e-30) as f32 * (self.dll.group as f32);
                self.live.epl = [
                    (self.dll.last.0 as f32 / p).min(2.0),
                    1.0,
                    (self.dll.last.1 as f32 / p).min(2.0),
                ];
            }
            if bicsk {
                // Bi-orthogonal CSK: the sign turns about half the steps by pi, so
                // the products are taken with the sign wiped, by squaring (the
                // phase of z² is twice the step, and a sign cannot change it). The
                // product is summed over the fingers: a finger's own channel phase
                // cancels in y[n]·conj(y[n-1]), so the fingers add coherently. A
                // wrong code in either symbol makes z noise, whose square has a
                // random phase, so the coherence test in `freq_correction` (which
                // would pass a sum of sign-decided noise products, all turned into
                // the right half-plane) turns such a run away.
                if s > 0 && !debug_frozen("bicsk_loop") {
                    let (pat, pnf) = wk.index[s - 1];
                    let pm = wk.decisions[s - 1];
                    let z: Iq = (0..nf.min(pnf))
                        .map(|f| wk.w[at + f * l + m] * wk.w[pat + f * l + pm].conj())
                        .sum();
                    let mag = z.norm();
                    if mag > 0.0 {
                        wk.freq_acc += z * z / mag;
                        wk.freq_norm += mag;
                    }
                }
            } else {
                // The main finger's peak carries the carrier measurement.
                let peak = wk.w[at + m];
                if let Some(p) = wk.peak_prev {
                    wk.freq_acc += peak * p.conj();
                    wk.freq_norm += peak.norm() * p.norm();
                }
                wk.peak_prev = Some(peak);
            }
            // Live: the correlator bank row, the chips, the SNR strip.
            let row: Vec<f32> = wk.w[at..at + l].iter().map(|z| z.norm_sqr()).collect();
            let mx = row.iter().copied().fold(1e-30f32, f32::max);
            let noise = (row.iter().sum::<f32>() - row[m]) / (l as f32 - 1.0);
            push_cap(
                &mut self.live.bank,
                row.iter().map(|v| v / mx).collect(),
                LIVE_ROWS,
            );
            // Signal: the winner less the mean of the others (its noise share);
            // summed over the half second, so it is not clipped here.
            self.strip(row[m] - noise, noise, wk.t.freq, t_sym);
            // The chips as received, carrier off, scrambler on.
            if s.is_multiple_of(4) {
                let dphi = wk.t.dphase();
                self.live.raw_chips = (0..l)
                    .map(|k| {
                        let (sn, c) = (wk.t.phase + k as f64 * dphi).sin_cos();
                        buf.at(wk.t.pos + k as f64 * wk.t.step) * Iq::new(c as f32, -(sn as f32))
                    })
                    .collect();
            }
            wk.t.advance(l);
            if (s + 1).is_multiple_of(freq_every) {
                // Bi-orthogonal: the sum is of squares; half its angle is the step.
                let acc = if bicsk {
                    Iq::from_polar(wk.freq_acc.norm(), 0.5 * wk.freq_acc.arg())
                } else {
                    wk.freq_acc
                };
                wk.t.freq += freq_correction(acc, wk.freq_norm, t_sym);
                wk.t.freq =
                    wk.t.freq
                        .clamp(self.f0 - FREQ_MAX_EXCURSION, self.f0 + FREQ_MAX_EXCURSION);
                wk.freq_acc = Iq::new(0.0, 0.0);
                wk.freq_norm = 0.0;
            }
            // The searcher: at the end of each chunk (not the last: the
            // known-chip refinement after the block does that better).
            if (s + 1 - wk.s_lo) >= chunk && s + 1 < wk.s_count {
                let profile =
                    self.profile_csk(buf, &wk.tc, &wk.scr, wk.s_lo, &wk.decisions[wk.s_lo..=s]);
                if let Some(shift) = self.adopt_track(&profile) {
                    let (step0, chips) = (wk.step0, ((s + 1 - wk.s_lo) * l) as f64);
                    self.apply_shift(&mut wk.t, shift, chips, step0);
                    wk.shifted += shift;
                }
                wk.tc = wk.t;
                wk.s_lo = s + 1;
            }
            wk.s += 1;
            self.live.symbol = wk.s;
            if wk.s > wk.replay_until && !no_rewind() {
                // The decided code's power at the fingers; the noise from the
                // other codes, times H_L: when the signal is gone the decided
                // code is the largest of L noise powers.
                let (mut e, mut others) = (0.0f64, 0.0f64);
                for f in 0..nf {
                    let row = &wk.w[at + f * l..at + (f + 1) * l];
                    let p = f64::from(row[m].norm_sqr());
                    e += p;
                    others += (row.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() - p)
                        / (l as f64 - 1.0);
                }
                match self.watch.push(e, h_l * others, win, wk.s, max_windows) {
                    Some(Judged::Locked) => {
                        let snap = self.csk_snap(wk);
                        if wk.snaps.len() > rewind::SNAPS {
                            wk.snaps.remove(1);
                        }
                        wk.snaps.push(snap);
                    }
                    Some(Judged::Lost | Judged::StillLost) => wk.search = true,
                    Some(Judged::Recovered | Judged::GiveUp) | None => {}
                }
            }
        }
        true
    }

    /// The tracking state of a CSK block as it stands, for a rewind.
    fn csk_snap(&self, wk: &CskWork) -> CskSnap {
        CskSnap {
            s: wk.s,
            t: wk.t,
            w_len: wk.w.len(),
            peak_prev: wk.peak_prev,
            freq_acc: wk.freq_acc,
            freq_norm: wk.freq_norm,
            tc: wk.tc,
            s_lo: wk.s_lo,
            shifted: wk.shifted,
            dll: self.dll.clone(),
            fingers: self.fingers.clone(),
            finger_age: self.finger_age.clone(),
        }
    }

    /// Puts a CSK block back to the newest snapshot at or before symbol `at`.
    fn csk_rewind(&mut self, wk: &mut CskWork, at: usize) {
        let i = wk.snaps.iter().rposition(|n| n.s <= at).unwrap_or(0);
        wk.snaps.truncate(i + 1);
        let n = wk.snaps[i].clone();
        wk.replay_until = wk.replay_until.max(wk.s);
        wk.s = n.s;
        wk.t = n.t;
        wk.w.truncate(n.w_len);
        wk.index.truncate(n.s);
        wk.decisions.truncate(n.s);
        wk.peak_prev = n.peak_prev;
        wk.freq_acc = n.freq_acc;
        wk.freq_norm = n.freq_norm;
        wk.tc = n.tc;
        wk.s_lo = n.s_lo;
        wk.shifted = n.shifted;
        wk.ts.truncate(n.s);
        wk.slip = None;
        if wk.slipped_at.is_some_and(|k| k >= n.s) {
            wk.slipped_at = None;
        }
        self.dll = n.dll;
        self.fingers = n.fingers;
        self.finger_age = n.finger_age;
        self.watch.restart_window();
    }

    /// The search for a CSK block (see [`PayloadRx::find_slip`]); the energy
    /// at a lag is the largest Walsh correlation power. True when it rewound.
    fn resync_csk(&mut self, buf: &Buf, wk: &mut CskWork, win: usize) -> bool {
        let Some(lost) = self.watch.lost else {
            return false;
        };
        if wk.s == 0 {
            return false;
        }
        let csk = |buf: &Buf, t: &Track, sc: &[Iq], lag: f64, u: &mut [Iq]| {
            despread(buf, t, sc, lag, u);
            walsh_correlate(u)
                .iter()
                .map(|z| f64::from(z.norm_sqr()))
                .fold(0.0, f64::max)
        };
        let span = (lost.since.min(wk.s - 1), wk.s);
        let Some((k, d)) = self.find_slip(
            buf,
            &wk.ts,
            wk.shifted,
            &wk.scr,
            wk.snaps[0].s,
            span,
            win,
            &csk,
        ) else {
            return false;
        };
        self.csk_rewind(wk, k);
        wk.slip = Some((k, d));
        self.watch.lost = None;
        self.live.slips += 1;
        true
    }

    /// Pass two of a CSK block: per-finger noise and amplitude, then the
    /// metric, then the decoders.
    fn finish_csk(&mut self, buf: &Buf, wk: CskWork) -> BlockResult {
        let l = usize::from(self.header.mode.spread);
        let bits = l.trailing_zeros() as usize;
        self.track = wk.t;
        let (mut sigma2, mut amp2, mut cnt) = ([0.0f64; SLOTS], [0.0f64; SLOTS], [0usize; SLOTS]);
        for (s, &m) in wk.decisions.iter().enumerate() {
            let (off, nf) = wk.index[s];
            for f in 0..nf {
                let row = &wk.w[off + f * l..off + (f + 1) * l];
                let total: f64 = row.iter().map(|z| f64::from(z.norm_sqr())).sum();
                let pk = f64::from(row[m].norm_sqr());
                sigma2[f] += (total - pk) / (l as f64 - 1.0);
                amp2[f] += pk;
                cnt[f] += 1;
            }
        }
        let mut kappa = [0.0f64; SLOTS];
        for f in 0..SLOTS {
            if cnt[f] > 0 {
                sigma2[f] /= cnt[f] as f64;
                amp2[f] = (amp2[f] / cnt[f] as f64 - sigma2[f]).max(0.01 * sigma2[f]);
                kappa[f] = 2.0 * amp2[f].sqrt() / sigma2[f].max(1e-30);
            }
        }
        let bicsk = self.header.mode.family == Family::BiCsk;
        let mut llrs = Vec::with_capacity(wk.s_count * (bits + usize::from(bicsk)));
        let mut metric = vec![0.0f64; l];
        let mut prev_metric = vec![0.0f64; l];
        for (s, &(off, nf)) in wk.index.iter().enumerate() {
            for (m, v) in metric.iter_mut().enumerate() {
                *v = (0..nf)
                    .map(|f| ln_i0(kappa[f] * f64::from(wk.w[off + f * l + m].norm())))
                    .sum();
            }
            csk_bit_llrs(&metric, bits, &mut llrs);
            if bicsk {
                // The sign bit: the change from the symbol before (the first symbol
                // is the reference and carries none).
                if s > 0 {
                    let (poff, pnf) = wk.index[s - 1];
                    let nf = nf.min(pnf);
                    llrs.push(sign_llr(
                        &wk.w[poff..poff + nf * l],
                        &wk.w[off..off + nf * l],
                        &prev_metric,
                        &metric,
                        l,
                        &kappa,
                    ));
                }
                std::mem::swap(&mut prev_metric, &mut metric);
            }
        }
        let snr = 10.0 * (amp2[0] / sigma2[0].max(1e-30)).log10();
        let (start, from) = refine_from(&wk.ts, wk.shifted, wk.slipped_at, wk.start);
        self.finish(buf, &llrs, None, snr, start, from)
    }

    /// Processes DS-PSK symbols while the buffer holds them; true when the
    /// block is done.
    fn advance_ds(&mut self, buf: &Buf, wk: &mut DsWork) -> bool {
        let l = usize::from(self.header.mode.spread);
        let per = PILOT_INTERVAL as usize;
        let t_sym = l as f64 * wk.t.step / wk.t.rate;
        let t_pilot = per as f64 * t_sym;
        let freq_every = ((FREQ_UPDATE_S / t_pilot).ceil() as usize).max(3);
        let chunk_pilots = ((SEARCH_S / t_pilot).ceil() as usize).max(SEARCH_MIN_PILOTS);
        let chunk = chunk_pilots * per;
        let win = ((rewind::WATCH_S / t_sym).ceil() as usize).max(rewind::WATCH_MIN_SYMBOLS);
        let max_windows = ((rewind::LOST_MAX_S / (win as f64 * t_sym)).ceil() as usize).max(2);
        let reach = rewind::SLIP_REACH_S * f64::from(self.header.profile.chip_rate());
        while wk.s < wk.s_count {
            if wk.t.pos + (l as f64 + AHEAD_CHIPS) * wk.t.step + 16.0 > buf.end() as f64 {
                return false;
            }
            if wk.search {
                // The search reaches ahead of the newest symbol too.
                let ahead = reach + self.fingers.iter().map(|f| f.lag).fold(0.0, f64::max);
                if wk.t.pos + (l as f64 + ahead + AHEAD_CHIPS) * wk.t.step + 16.0 > buf.end() as f64
                {
                    return false;
                }
                wk.search = false;
                if self.resync_ds(buf, wk, win) {
                    // Rewound: on from the snapshot.
                    continue;
                }
            }
            if let Some((at, d)) = wk.slip
                && at == wk.s
            {
                // The signal goes on `d` chips away from here (a slip of the
                // audio): the track moves, the fingers with it.
                wk.t.pos += d * wk.t.step;
                wk.slip = None;
                wk.slipped_at = Some(wk.s);
            }
            let s = wk.s;
            wk.ts.push((wk.t, wk.shifted));
            let sc = &wk.scr[s * l..(s + 1) * l];
            let (mut e_sum, mut l_sum, mut p_sum) = (0.0f64, 0.0f64, 0.0f64);
            for (fi, f) in self.fingers.iter().enumerate().take(SLOTS) {
                despread(buf, &wk.t, sc, f.lag, &mut wk.u);
                wk.y[s][fi] = sum(&wk.u);
                if fi == 0 {
                    self.live.despread_chips = wk.u.clone();
                }
                p_sum += f64::from(wk.y[s][fi].norm());
                despread(buf, &wk.t, sc, f.lag - 0.25, &mut wk.ue);
                despread(buf, &wk.t, sc, f.lag + 0.25, &mut wk.ul);
                e_sum += f64::from(sum(&wk.ue).norm());
                l_sum += f64::from(sum(&wk.ul).norm());
            }
            if let Some(c) = self.dll.push(e_sum, l_sum) {
                wk.t.pos += c * wk.t.step;
                let p = p_sum.max(1e-30) as f32 * (self.dll.group as f32);
                self.live.epl = [
                    (self.dll.last.0 as f32 / p).min(2.0),
                    1.0,
                    (self.dll.last.1 as f32 / p).min(2.0),
                ];
            }
            if self.layout.is_pilot(s) {
                let p = pilot_symbol(wk.pilot_no).conj();
                for fi in 0..SLOTS {
                    wk.g[wk.pilot_no][fi] = wk.y[s][fi] * p;
                }
                // The carrier is measured from pilots `lag` apart, so the interval
                // is the same (SF 4's) at every SF: adjacent pilots at SF 1 are
                // 4 ms apart, which gives the turn between them four times less to
                // show above the noise.
                let lag = pilots_per_ref_interval(l);
                if wk.pilot_no >= lag {
                    let (a, b) = (wk.g[wk.pilot_no][0], wk.g[wk.pilot_no - lag][0]);
                    wk.freq_acc += a * b.conj();
                    wk.freq_norm += a.norm() * b.norm();
                }
                wk.pilot_no += 1;
                if wk.pilot_no.is_multiple_of(freq_every) {
                    wk.t.freq += freq_correction(wk.freq_acc, wk.freq_norm, lag as f64 * t_pilot);
                    wk.t.freq =
                        wk.t.freq
                            .clamp(self.f0 - FREQ_MAX_EXCURSION, self.f0 + FREQ_MAX_EXCURSION);
                    wk.freq_acc = Iq::new(0.0, 0.0);
                    wk.freq_norm = 0.0;
                }
            } else if wk.pilot_no > 0 {
                // A data symbol for the display, rotated by the latest pilot's
                // gain (the estimate the decoder will refine) and scaled to unit
                // amplitude.
                let mut z = Iq::new(0.0, 0.0);
                let mut pw = 0.0f32;
                for fi in 0..SLOTS {
                    let h = wk.g[wk.pilot_no - 1][fi];
                    z += h.conj() * wk.y[s][fi];
                    pw += h.norm_sqr();
                }
                if pw > 0.0 {
                    let z = z / pw;
                    push_cap(&mut self.live.symbols, z, LIVE_POINTS);
                    if let Some(p) = wk.prev_z {
                        push_cap(&mut self.live.diffs, z * p.conj(), LIVE_POINTS);
                    }
                    wk.prev_z = Some(z);
                    // The error against the nearest QPSK point, averaged: its
                    // inverse is the SNR of a symbol after combining.
                    let k = std::f32::consts::FRAC_1_SQRT_2;
                    let e = (z.re.abs() - k).powi(2) + (z.im.abs() - k).powi(2);
                    wk.err2 += 0.02 * (e - wk.err2);
                    // Unit-amplitude symbols: signal 1, noise the mean error.
                    self.strip(1.0, wk.err2.max(1e-4), wk.t.freq, t_sym);
                }
            }
            if s.is_multiple_of(8) {
                let dphi = wk.t.dphase();
                self.live.raw_chips = (0..l)
                    .map(|k| {
                        let (sn, c) = (wk.t.phase + k as f64 * dphi).sin_cos();
                        buf.at(wk.t.pos + k as f64 * wk.t.step) * Iq::new(c as f32, -(sn as f32))
                    })
                    .collect();
            }
            wk.t.advance(l);
            if (s + 1 - wk.s_lo) >= chunk && s + 1 < wk.s_count {
                let profile = self.profile_ds(buf, &wk.tc, &wk.scr, wk.s_lo, s + 1);
                if let Some(shift) = self
                    .adopt_track(&profile)
                    .filter(|_| !debug_frozen("adopt"))
                {
                    let (step0, chips) = (wk.step0, ((s + 1 - wk.s_lo) * l) as f64);
                    self.apply_shift(&mut wk.t, shift, chips, step0);
                    wk.shifted += shift;
                }
                wk.tc = wk.t;
                wk.s_lo = s + 1;
            }
            wk.s += 1;
            self.live.symbol = wk.s;
            if wk.s > wk.replay_until && !no_rewind() {
                let e: f64 = wk.y[s].iter().map(|y| f64::from(y.norm_sqr())).sum();
                // Noise: the same number of fingers where there is no path, eight
                // chips past the last (beyond an HF delay spread at any profile).
                let nf = self.fingers.len().min(SLOTS);
                let off = self.fingers.iter().map(|f| f.lag).fold(0.0, f64::max) + 8.0;
                let (t0, _) = wk.ts[s];
                despread(buf, &t0, &wk.scr[s * l..(s + 1) * l], off, &mut wk.u);
                let noise = nf as f64 * f64::from(sum(&wk.u).norm_sqr());
                match self.watch.push(e, noise, win, wk.s, max_windows) {
                    Some(Judged::Locked) => {
                        let snap = self.ds_snap(wk);
                        // The block's start stays; then the newest few.
                        if wk.snaps.len() > rewind::SNAPS {
                            wk.snaps.remove(1);
                        }
                        wk.snaps.push(snap);
                    }
                    Some(Judged::Lost | Judged::StillLost) => wk.search = true,
                    Some(Judged::Recovered | Judged::GiveUp) | None => {}
                }
            }
        }
        true
    }

    /// The tracking state of a DS-PSK block as it stands, for a rewind.
    fn ds_snap(&self, wk: &DsWork) -> DsSnap {
        DsSnap {
            s: wk.s,
            t: wk.t,
            pilot_no: wk.pilot_no,
            freq_acc: wk.freq_acc,
            freq_norm: wk.freq_norm,
            tc: wk.tc,
            s_lo: wk.s_lo,
            shifted: wk.shifted,
            prev_z: wk.prev_z,
            err2: wk.err2,
            dll: self.dll.clone(),
            fingers: self.fingers.clone(),
            finger_age: self.finger_age.clone(),
        }
    }

    /// Puts a DS-PSK block back to the newest snapshot at or before symbol
    /// `at`; the symbols after it come again. Returns the symbol it went to.
    fn ds_rewind(&mut self, wk: &mut DsWork, at: usize) -> usize {
        let i = wk.snaps.iter().rposition(|n| n.s <= at).unwrap_or(0);
        wk.snaps.truncate(i + 1);
        let n = wk.snaps[i].clone();
        wk.replay_until = wk.replay_until.max(wk.s);
        wk.s = n.s;
        wk.t = n.t;
        wk.pilot_no = n.pilot_no;
        wk.freq_acc = n.freq_acc;
        wk.freq_norm = n.freq_norm;
        wk.tc = n.tc;
        wk.s_lo = n.s_lo;
        wk.shifted = n.shifted;
        wk.prev_z = n.prev_z;
        wk.err2 = n.err2;
        wk.ts.truncate(n.s);
        wk.slip = None;
        if wk.slipped_at.is_some_and(|k| k >= n.s) {
            wk.slipped_at = None;
        }
        self.dll = n.dll;
        self.fingers = n.fingers;
        self.finger_age = n.finger_age;
        self.watch.restart_window();
        n.s
    }

    /// The watch's search (see [`rewind`]) on a block's symbols `from..to`:
    /// what `energy_at` collects at every lag within the reach, summed. If the
    /// fingers' pattern moved by some shift collects clearly more than where it
    /// is, and the energy left the old lag and reached the new one in one clean
    /// step, the signal slipped: returns the symbol it slipped at (looked for
    /// from `lo` on) and the shift, chips. `ts` holds the track each symbol was
    /// taken on and the chips the fingers had moved by then, `shifted` the
    /// chips they have moved by now.
    #[allow(clippy::too_many_arguments)]
    fn find_slip(
        &self,
        buf: &Buf,
        ts: &[(Track, f64)],
        shifted: f64,
        scr: &[Iq],
        lo: usize,
        (from, to): (usize, usize),
        win: usize,
        energy_at: &EnergyAt,
    ) -> Option<(usize, f64)> {
        let l = usize::from(self.header.mode.spread);
        let nf = self.fingers.len().min(SLOTS);
        if nf == 0 || from >= to {
            return None;
        }
        let lags: Vec<f64> = self.fingers[..nf].iter().map(|f| f.lag).collect();
        let reach = rewind::SLIP_REACH_S * f64::from(self.header.profile.chip_rate());
        let step = DelayProfile::LAG_STEP;
        let (lmin, lmax) = lags
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        let lag0 = lmin - reach;
        let n = ((lmax + reach - lag0) / step).round() as usize + 1;
        let mut u = vec![Iq::new(0.0, 0.0); l];
        // Symbol `s` at `lag` on the fingers' present scale.
        let mut at = |s: usize, lag: f64| {
            let (t, moved) = ts[s];
            energy_at(
                buf,
                &t,
                &scr[s * l..(s + 1) * l],
                lag + shifted - moved,
                &mut u,
            )
        };
        let mut energy = vec![0.0f64; n];
        for s in from..to {
            for (i, e) in energy.iter_mut().enumerate() {
                *e += at(s, lag0 + i as f64 * step);
            }
        }
        let symbols = (to - from) as f64;
        let settled = self.watch.settled.unwrap_or(0.0);
        let (d, got, here) = rewind::best_shift(&energy, lag0, step, &lags, reach)?;
        if got < rewind::SLIP_RATIO * here
            || got / symbols - self.watch.noise.unwrap_or(0.0) < rewind::SLIP_MIN * settled
        {
            return None;
        }
        // Where it slipped, from `lo` on.
        let (mut old, mut new) = (Vec::new(), Vec::new());
        for s in lo..to {
            old.push(lags.iter().map(|&g| at(s, g)).sum::<f64>());
            new.push(lags.iter().map(|&g| at(s, g + d)).sum::<f64>());
        }
        let split = rewind::slip_at(&old, &new);
        // The noise a symbol collects at the fingers: most lags searched hold
        // none of the signal, so the median is noise.
        let mut sorted = energy.clone();
        sorted.sort_by(f64::total_cmp);
        let noise = nf as f64 * sorted[sorted.len() / 2] / symbols;
        rewind::is_clean_slip(&old, &new, split, win, noise).then_some((lo + split, d))
    }

    /// The search for a DS-PSK block; on a slip, rewinds to before it and goes
    /// on with the shift from there. True when it rewound. (In a fade the loops
    /// ride it out as they always have. Holding them, as BinModem does for its
    /// decision-directed loops, cost DS4 whole transmissions on moderate
    /// fading: the early/late loop and the carrier were held at the start,
    /// before they had settled.)
    fn resync_ds(&mut self, buf: &Buf, wk: &mut DsWork, win: usize) -> bool {
        let Some(lost) = self.watch.lost else {
            return false;
        };
        if wk.s == 0 {
            return false;
        }
        let ds = |buf: &Buf, t: &Track, sc: &[Iq], lag: f64, u: &mut [Iq]| {
            despread(buf, t, sc, lag, u);
            f64::from(sum(u).norm_sqr())
        };
        let span = (lost.since.min(wk.s - 1), wk.s);
        let Some((k, d)) = self.find_slip(
            buf,
            &wk.ts,
            wk.shifted,
            &wk.scr,
            wk.snaps[0].s,
            span,
            win,
            &ds,
        ) else {
            return false;
        };
        self.ds_rewind(wk, k);
        wk.slip = Some((k, d));
        self.watch.lost = None;
        self.live.slips += 1;
        true
    }

    /// Pass two of a DS-PSK block: smoothed, shrunk and interpolated pilot
    /// gains, maximal-ratio combining, then the decoders.
    fn finish_ds(&mut self, buf: &Buf, wk: DsWork) -> BlockResult {
        let per = PILOT_INTERVAL as usize;
        self.track = wk.t;
        let (y, g) = (&wk.y, &wk.g);
        let (s_count, pilots) = (wk.s_count, g.len());
        debug_assert_eq!(wk.pilot_no, pilots);

        // Smooth the pilot gains (a triangle of `half` pilots either side, [1 2 1]/4
        // at SF 4; shorter at the ends) and interpolate linearly to the data symbols.
        let half = smooth_half_width(usize::from(self.header.mode.spread));
        let mut hs = smooth_gains(g, half);
        // Per finger, the noise in a pilot gain from the second difference (the
        // smooth channel cancels): var(g_k − (g_{k−1}+g_{k+1})/2) = 1.5 σ². A
        // smoothed estimate carries `smooth_noise_gain(half)` of σ² (0.375 for
        // SF 4's kernel). Where a finger's estimate
        // is not clearly above that (a noise peak taken for a path, a path in a
        // fade) its weight is cut by power subtraction, so it stops adding noise.
        for fi in 0..SLOTS {
            let (mut acc, mut cnt) = (0.0f64, 0usize);
            for k in 1..pilots.saturating_sub(1) {
                let d = g[k][fi] - (g[k - 1][fi] + g[k + 1][fi]) * 0.5;
                acc += f64::from(d.norm_sqr()) / 1.5;
                cnt += 1;
            }
            if cnt == 0 {
                continue;
            }
            let var = (smooth_noise_gain(half) * acc / cnt as f64) as f32;
            for h in hs.iter_mut() {
                let p = h[fi].norm_sqr();
                if p > 0.0 {
                    h[fi] *= ((p - var).max(0.0) / p).sqrt();
                }
            }
        }
        let (mut n_acc, mut n_cnt, mut p_acc) = (0.0f64, 0usize, 0.0f64);
        for k in 1..pilots.saturating_sub(1) {
            let d = g[k][0] - (g[k - 1][0] + g[k + 1][0]) * 0.5;
            n_acc += f64::from(d.norm_sqr()) / 1.5;
            p_acc += f64::from(hs[k][0].norm_sqr());
            n_cnt += 1;
        }
        let snr = if n_cnt > 0 && n_acc > 0.0 {
            10.0 * (p_acc / n_acc).log10()
        } else {
            f64::NAN
        };

        let mut llrs = Vec::with_capacity(self.layout.data_symbols() * 2);
        for s in (0..s_count).filter(|&s| !self.layout.is_pilot(s)) {
            let (k, a) = (s / per, (s % per) as f32 / per as f32);
            let mut z = Iq::new(0.0, 0.0);
            for fi in 0..SLOTS {
                let h = hs[k][fi] * (1.0 - a) + hs[k + 1][fi] * a;
                z += h.conj() * y[s][fi];
            }
            // z = Σ conj(ĥ)·y: the strong paths weigh most, which is the
            // reliability the decoder needs. The factor is arbitrary (the
            // decoder is scale-free) but keeps the numbers readable.
            llrs.push(4.0 * z.re);
            llrs.push(4.0 * z.im);
        }
        let var_g0 = if n_cnt > 0 {
            n_acc / n_cnt as f64
        } else {
            f64::NAN
        };
        let grake = if debug_frozen("grake") || rake_only() {
            None
        } else {
            self.grake_llrs(buf, &wk, &hs, var_g0)
        };
        let (start, from) = refine_from(&wk.ts, wk.shifted, wk.slipped_at, wk.start);
        match grake {
            Some(g) => self.finish(buf, &g, Some(&llrs), snr, start, from),
            None => self.finish(buf, &llrs, None, snr, start, from),
        }
    }

    /// The G-RAKE's LLRs for a DS-PSK block (see [`grake`]): at every pilot, the
    /// paths from the smoothed gains and the weights on a half-chip grid around
    /// the fingers; every data symbol despread on that grid along the track it
    /// was first taken on, combined with the weights interpolated between its
    /// pilots. `var_g0` is the impairment of one pilot gain at the main finger.
    /// None when there is nothing to model.
    fn grake_llrs(
        &self,
        buf: &Buf,
        wk: &DsWork,
        hs: &[[Iq; SLOTS]],
        var_g0: f64,
    ) -> Option<Vec<f32>> {
        let l = usize::from(self.header.mode.spread);
        let per = PILOT_INTERVAL as usize;
        let nf = self.fingers.len().min(SLOTS);
        if nf == 0 || !(var_g0.is_finite() && var_g0 > 0.0) || wk.ts.len() < wk.s_count {
            return None;
        }
        let lags: Vec<f64> = self.fingers[..nf].iter().map(|f| f.lag).collect();
        let grid = grake::grid(&lags);
        let c64 = |z: Iq| Complex64::new(f64::from(z.re), f64::from(z.im));
        let a: Vec<Vec<Complex64>> = hs
            .iter()
            .map(|h| grake::paths(&lags, &h[..nf].iter().map(|&z| c64(z)).collect::<Vec<_>>()))
            .collect();
        // The matched filter's noise per chip: a pilot gain at the main finger
        // carries the neighbours' chips and the noise, over L chips. What the
        // model says the neighbours are is taken off (not below a tenth, so an
        // over-estimate of the paths cannot leave no noise at all).
        let isi = a
            .iter()
            .map(|a| grake::isi_power(lags[0], &lags, a))
            .sum::<f64>()
            / a.len() as f64;
        let total = l as f64 * var_g0;
        let sigma2 = (total - isi).max(0.1 * total);
        // `collect` into Option<Vec<_>>: None as soon as one pilot's model is
        // degenerate (then the rake decodes the block).
        let w: Vec<Vec<Complex64>> = a
            .iter()
            .map(|a| grake::weights(&grid, &lags, a, sigma2).map(|(w, _)| w))
            .collect::<Option<_>>()?;
        let scale = 2.0 * std::f64::consts::SQRT_2 * l as f64;
        let mut u = vec![Iq::new(0.0, 0.0); l];
        let mut y = vec![Complex64::new(0.0, 0.0); grid.len()];
        let mut llrs = Vec::with_capacity(self.layout.data_symbols() * 2);
        for s in (0..wk.s_count).filter(|&s| !self.layout.is_pilot(s)) {
            let (t, moved) = &wk.ts[s];
            // The grid is on the fingers' lags at the end of the block; a symbol
            // taken before the reference last moved sits that much later.
            let back = wk.shifted - moved;
            let sc = &wk.scr[s * l..(s + 1) * l];
            for (yj, &d) in y.iter_mut().zip(&grid) {
                despread(buf, t, sc, d + back, &mut u);
                *yj = c64(sum(&u));
            }
            let (k, f) = (s / per, (s % per) as f64 / per as f64);
            let z: Complex64 = w[k]
                .iter()
                .zip(&w[k + 1])
                .zip(&y)
                .map(|((w0, w1), y)| (w0 * (1.0 - f) + w1 * f).conj() * y)
                .sum();
            llrs.push((scale * z.re) as f32);
            llrs.push((scale * z.im) as f32);
        }
        Some(llrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rx::testrig::*;
    use decdsss_chan::Watterson;
    use decdsss_core::Profile;

    /// Decodes every block of a clean-ish transmission and returns the
    /// fraction of frames recovered exactly.
    fn recover(
        profile: Profile,
        mode: &str,
        depth: usize,
        nblocks: u16,
        ch: &decdsss_chan::Channel,
    ) -> (usize, usize, Option<String>) {
        let seg = segment(profile, mode, depth, nblocks);
        let audio = through(&seg, ch);
        let mut run = match to_header(&audio, profile) {
            Ok(r) => r,
            Err(e) => return (0, seg.frames.len(), Some(e)),
        };
        assert_eq!(run.header.header, seg.header);
        // Feed the rest of the audio so the buffer holds everything.
        let mut rx = PayloadRx::new(&run.header, run.fe.out_rate()).unwrap();
        let mut rest = Vec::new();
        // Re-run the front end over the whole audio into a fresh buffer: the
        // front end is deterministic, so this equals what streaming gives.
        let mut fe = crate::rx::frontend::Frontend::new(profile, FS);
        fe.push(&audio, &mut rest);
        let mut buf = crate::rx::frontend::Buf::default();
        buf.push(&rest);
        run.buf = buf;
        let (mut ok, mut total) = (0, 0);
        let mut note = None;
        while let Some(r) = rx.step(&run.buf) {
            for (i, f) in r.frames.iter().enumerate() {
                total += 1;
                let want = &seg.frames[r.first_frame + i];
                if f.as_ref() == Some(want) {
                    ok += 1;
                } else if note.is_none() {
                    note = Some(format!(
                        "block {} frame {i} failed (iters {:?}, snr {:.1})",
                        r.index, r.iterations, r.snr_db
                    ));
                }
            }
        }
        (ok, total, note)
    }

    /// Frame error rate of one mode at one Eb/N0 with a genie start; `trials`
    /// independent transmissions are run on all cores.
    fn genie_fer(
        profile: Profile,
        mode: &str,
        depth: usize,
        nblocks: u16,
        ebn0_db: f64,
        trials: usize,
        fading: Option<Watterson>,
    ) -> (usize, usize) {
        let m = decdsss_core::Mode::from_name(mode).unwrap();
        let snr = ebn0_db + m.snr_offset_db(profile);
        let workers = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(trials);
        let results = std::sync::Mutex::new((0usize, 0usize));
        std::thread::scope(|sc| {
            for w in 0..workers {
                let results = &results;
                sc.spawn(move || {
                    for trial in (w..trials).step_by(workers) {
                        let seg = segment(profile, mode, depth, nblocks);
                        let mut ch = channel(profile, snr);
                        ch.seed = 9000 + trial as u64;
                        ch.fading = fading;
                        ch.offset_hz = -10.0 + (trial % 7) as f64 * 3.0;
                        ch.clock_ppm = -30.0 + (trial % 5) as f64 * 15.0;
                        let audio = through(&seg, &ch);
                        let (mut rx, buf) = genie(&seg, &ch, &audio);
                        let (mut ok, mut total) = (0, 0);
                        while let Some(r) = rx.step(&buf) {
                            for (i, f) in r.frames.iter().enumerate() {
                                total += 1;
                                if f.as_ref() == Some(&seg.frames[r.first_frame + i]) {
                                    ok += 1;
                                }
                            }
                        }
                        let mut g = results.lock().unwrap();
                        g.0 += total - ok;
                        g.1 += total;
                    }
                });
            }
        });
        results.into_inner().unwrap()
    }

    fn seg_block_seconds(seg: &crate::tx::Segment) -> f64 {
        Layout::new(seg.header.profile, seg.header.mode, seg.header.depth).seconds()
    }

    /// Hunts transmissions that lose frames at a high Eb/N0 with a genie start:
    /// fading and the demodulator, with the preamble and header out of it.
    #[test]
    #[ignore]
    fn debug_genie_floor() {
        let mode = std::env::var("DEBUG_MODE").unwrap_or_else(|_| "ds4-2/3".into());
        let profile = Profile::W2400;
        let m = decdsss_core::Mode::from_name(&mode).unwrap();
        let (mut lost, mut total, mut shown) = (0, 0, 0);
        let only: Option<usize> = std::env::var("DEBUG_TRIAL")
            .ok()
            .and_then(|v| v.parse().ok());
        for trial in 0..120usize {
            if only.is_some_and(|t| t != trial) {
                continue;
            }
            let seg = segment(profile, &mode, 2, 4);
            let mut ch = channel(profile, 30.0 + m.snr_offset_db(profile));
            if std::env::var("DEBUG_NONOISE").is_ok() {
                ch.snr_db = None;
            }
            ch.seed = 9000 + trial as u64;
            ch.fading = Some(Watterson::MODERATE);
            ch.offset_hz = -10.0 + (trial % 7) as f64 * 3.0;
            ch.clock_ppm = -30.0 + (trial % 5) as f64 * 15.0;
            let audio = through(&seg, &ch);
            if only.is_some() && std::env::var("DEBUG_FADE").is_ok() {
                // The channel's own fading, regenerated from its seed: total path
                // power every 0.25 s, with the block boundaries.
                use decdsss_dsp::rng::Rng;
                let n = audio.len() as f64 / (1.0 + ch.clock_ppm * 1e-6);
                let n = n as usize;
                let w = Watterson::MODERATE;
                let mut rng = Rng::new(ch.seed);
                let g0 = decdsss_chan::tap_process(n, 48_000.0, w.spread_hz, &mut rng);
                let g1 = decdsss_chan::tap_process(n, 48_000.0, w.spread_hz, &mut rng);
                let t0 = 1.0 + crate::tx::lead_chips_for(profile, m) as f64 / 1920.0;
                let blk = seg_block_seconds(&seg);
                let mut t = t0 - 0.5;
                while t < t0 + 4.0 * blk {
                    let i = (t * 48_000.0) as usize;
                    let (a, b) = (g0[i].norm_sqr(), g1[i].norm_sqr());
                    let tot = 0.5 * (a + b);
                    println!(
                        "t {:6.2} block {:5.2}  path0 {:6.1} dB  path1 {:6.1} dB  total {:6.1} dB",
                        t,
                        (t - t0) / blk,
                        10.0 * a.max(1e-9).log10(),
                        10.0 * b.max(1e-9).log10(),
                        10.0 * tot.max(1e-9).log10()
                    );
                    t += 0.25;
                }
            }
            let (mut rx, buf) = genie(&seg, &ch, &audio);
            let t0 = rx.track;
            let block_chips =
                Layout::new(seg.header.profile, seg.header.mode, seg.header.depth).chips() as f64;
            let mut line = String::new();
            let mut bad = 0;
            while let Some(r) = rx.step(&buf) {
                let ok = r
                    .frames
                    .iter()
                    .enumerate()
                    .filter(|(i, f)| f.as_ref() == Some(&seg.frames[r.first_frame + i]))
                    .count();
                if only.is_some() && std::env::var("DEBUG_ERRS").is_ok() {
                    let layout = Layout::new(seg.header.profile, seg.header.mode, seg.header.depth);
                    let from = r.first_frame - usize::from(seg.header.frame0);
                    let bits =
                        crate::tx::block_bits(&layout, &seg.frames[from..from + layout.depth]);
                    let llr = &rx.last_llrs;
                    let n = llr.len().min(bits.len());
                    let wrong: Vec<bool> =
                        (0..n).map(|j| (llr[j] < 0.0) != (bits[j] == 1)).collect();
                    let flip = wrong.iter().filter(|w| **w).count() > n / 2;
                    let slices = 12;
                    let mut row = String::new();
                    for q in 0..slices {
                        let (a, b) = (q * n / slices, (q + 1) * n / slices);
                        let e = (a..b).filter(|&j| wrong[j] != flip).count();
                        let mag = llr[a..b].iter().map(|v| v.abs()).sum::<f32>() / (b - a) as f32;
                        row += &format!(" {:3}%/{:5.1}", 100 * e / (b - a), mag);
                    }
                    println!(
                        "  block {} bit errors per twelfth (err%/mean|llr|):{row}",
                        r.index
                    );
                }
                if only.is_some() {
                    // How far the refined track is from the truth the genie started with.
                    let want = t0.pos + (r.index + 1) as f64 * block_chips * t0.step;
                    println!(
                        "  after block {}: position {:+.2} chips, clock {:+.0} ppm, carrier {:+.2} Hz off",
                        r.index,
                        (rx.track.pos - want) / t0.step,
                        (rx.track.step / t0.step - 1.0) * 1e6,
                        rx.track.freq - ch.offset_hz
                    );
                }
                total += r.frames.len();
                lost += r.frames.len() - ok;
                bad += r.frames.len() - ok;
                line += &format!(
                    " b{}:{}/{} fingers {:?}",
                    r.index,
                    ok,
                    r.frames.len(),
                    r.fingers
                        .iter()
                        .map(|f| (f.lag * 100.0).round() / 100.0)
                        .collect::<Vec<_>>()
                );
            }
            if bad > 0 && shown < 6 {
                println!("trial {trial} seed {}:{line}", ch.seed);
                shown += 1;
            }
        }
        println!("{mode}: {lost} of {total} frames lost");
    }

    /// The same hunt through the whole receiver: preamble search, header, then
    /// the payload from what the header gave. Prints, for transmissions that lose
    /// frames, how far the header's timing and carrier were from the truth and
    /// what each block did. `DEBUG_MODE`, `DEBUG_EBN0` (default 30), `DEBUG_TRIALS`
    /// (default 48), `DEBUG_DEPTH` (4), `DEBUG_BLOCKS` (2), `DEBUG_FADE` (moderate,
    /// good, poor, none).
    #[test]
    #[ignore]
    fn debug_full_floor() {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
        let mode = env("DEBUG_MODE", "ds1-3/4");
        let ebn0: f64 = env("DEBUG_EBN0", "30").parse().unwrap();
        let trials: usize = env("DEBUG_TRIALS", "48").parse().unwrap();
        let depth: usize = env("DEBUG_DEPTH", "4").parse().unwrap();
        let blocks: u16 = env("DEBUG_BLOCKS", "2").parse().unwrap();
        let fading = match env("DEBUG_FADE", "moderate").as_str() {
            "good" => Some(Watterson::GOOD),
            "poor" => Some(Watterson::POOR),
            "none" => None,
            _ => Some(Watterson::MODERATE),
        };
        let profile = Profile::W2400;
        let m = decdsss_core::Mode::from_name(&mode).unwrap();
        let cr = f64::from(profile.chip_rate());
        let (mut lost, mut total, mut no_header) = (0, 0, 0);
        for trial in 0..trials {
            let seg = segment(profile, &mode, depth, blocks);
            let mut ch = channel(profile, ebn0 + m.snr_offset_db(profile));
            ch.seed = 9000 + trial as u64;
            ch.fading = fading;
            ch.offset_hz = -10.0 + (trial % 7) as f64 * 3.0;
            ch.clock_ppm = -30.0 + (trial % 5) as f64 * 15.0;
            let audio = through(&seg, &ch);
            let (mut rx, buf, head) = if std::env::var("DEBUG_GENIE").is_ok() {
                let (rx, buf) = genie(&seg, &ch, &audio);
                (rx, buf, "genie start".to_string())
            } else {
                let Ok(run) = to_header(&audio, profile) else {
                    no_header += 1;
                    continue;
                };
                // Where the first payload chip really is (as `genie` has it).
                let mut fe = crate::rx::frontend::Frontend::new(profile, FS);
                let rate = fe.out_rate();
                let r = 1.0 + ch.clock_ppm * 1e-6;
                let first_chip = ((1.0 + 10.0 / cr) * rate + fe.delay()) / r;
                let lead = crate::tx::lead_chips_for(profile, seg.header.mode);
                let truth = first_chip + lead as f64 * crate::rx::frontend::SPS as f64 / r;
                let t = run.header.track;
                let head = format!(
                    "pos {:+.2} chips, carrier {:+.2} Hz, clock {:+.0} ppm, fingers {:?}",
                    (t.pos - truth) / t.step,
                    t.freq - ch.offset_hz,
                    (t.step / (crate::rx::frontend::SPS as f64 / r) - 1.0) * 1e6,
                    run.header
                        .fingers
                        .iter()
                        .map(|f| (f.lag * 100.0).round() / 100.0)
                        .collect::<Vec<_>>()
                );
                let rx = PayloadRx::new(&run.header, rate).unwrap();
                let mut samples = Vec::new();
                fe.push(&audio, &mut samples);
                let mut buf = crate::rx::frontend::Buf::default();
                buf.push(&samples);
                (rx, buf, head)
            };
            let (mut line, mut bad) = (String::new(), 0);
            while let Some(b) = rx.step(&buf) {
                let ok = b
                    .frames
                    .iter()
                    .enumerate()
                    .filter(|(i, f)| f.as_ref() == Some(&seg.frames[b.first_frame + i]))
                    .count();
                total += b.frames.len();
                lost += b.frames.len() - ok;
                bad += b.frames.len() - ok;
                line += &format!(" b{}:{}/{}", b.index, ok, b.frames.len());
            }
            if bad > 0 {
                println!("trial {trial} seed {}: {head};{line}", ch.seed);
            }
        }
        println!("{mode}: {lost} of {total} frames lost, {no_header} without a header");
    }

    /// A stretch of audio cut out of (negative) or silence put into (positive)
    /// the middle of block 2, as a sound card that drops or repeats a buffer
    /// does; which blocks decode after it, on a clean channel and on moderate
    /// fading. `cargo test --release -p decdsss-phy sync_slip_curve -- --ignored
    /// --nocapture`; `SLIP_ONLY=ds4`, `SLIP_TRIAL=0` and `SLIP_MS=-50` narrow it
    /// to one mode, trial and cut, `SLIP_MODES` picks the modes. A number in brackets is the
    /// slips followed.
    #[test]
    #[ignore]
    fn sync_slip_curve() {
        let profile = Profile::W2400;
        let only = std::env::var("SLIP_ONLY").ok();
        // `SLIP_MODES=ds2-1/2,ds1-1/2` replaces the modes tried.
        let modes: Vec<String> = std::env::var("SLIP_MODES").map_or_else(
            |_| {
                ["ds4-2/3", "ds8-1/2", "csk64-1/2"]
                    .map(String::from)
                    .to_vec()
            },
            |v| v.split(',').map(String::from).collect(),
        );
        for mode in modes.iter().map(String::as_str) {
            if only.as_ref().is_some_and(|o| !mode.starts_with(o.as_str())) {
                continue;
            }
            let m = decdsss_core::Mode::from_name(mode).unwrap();
            let layout = Layout::new(profile, m, 2);
            let nblocks = 6u16;
            for fading in [None, Some(Watterson::MODERATE)] {
                for ms in [-50.0f64, -20.0, -5.0, -1.0, 0.0, 1.0, 5.0, 20.0, 50.0] {
                    let mut row = String::new();
                    let mut ok_all = 0;
                    for trial in 0..4u64 {
                        if std::env::var("SLIP_TRIAL").is_ok_and(|v| v != trial.to_string())
                            || std::env::var("SLIP_MS")
                                .is_ok_and(|v| v.parse::<f64>().ok() != Some(ms))
                        {
                            continue;
                        }
                        let seg = segment(profile, mode, 2, nblocks);
                        let ebn0 = if fading.is_some() { 20.0 } else { 10.0 };
                        let mut ch = channel(profile, ebn0 + m.snr_offset_db(profile));
                        ch.seed = 500 + trial;
                        ch.fading = fading;
                        ch.offset_hz = 3.0;
                        ch.clock_ppm = 20.0;
                        let mut audio = through(&seg, &ch);
                        // Middle of block 2: the payload ends a second before the end.
                        let end = audio.len() as f64 - FS;
                        let at = end - (f64::from(nblocks) - 2.5) * layout.seconds() * FS;
                        let at = at as usize;
                        let n = (ms.abs() * 1e-3 * FS) as usize;
                        if ms < 0.0 {
                            audio.drain(at..at + n);
                        } else {
                            audio.splice(at..at, std::iter::repeat_n(0.0, n));
                        }
                        let Ok(run) = to_header(&audio, profile) else {
                            row += " no header";
                            continue;
                        };
                        let mut rx = PayloadRx::new(&run.header, run.fe.out_rate()).unwrap();
                        let mut fe = crate::rx::frontend::Frontend::new(profile, FS);
                        let mut rest = Vec::new();
                        fe.push(&audio, &mut rest);
                        let mut buf = crate::rx::frontend::Buf::default();
                        buf.push(&rest);
                        row.push(' ');
                        while let Some(r) = rx.step(&buf) {
                            let good = r
                                .frames
                                .iter()
                                .enumerate()
                                .filter(|(i, f)| f.as_ref() == Some(&seg.frames[r.first_frame + i]))
                                .count();
                            ok_all += good;
                            row.push(match good {
                                0 => 'x',
                                g if g == r.frames.len() => 'o',
                                _ => '/',
                            });
                        }
                        if rx.live().slips > 0 {
                            row += &format!("({})", rx.live().slips);
                        }
                    }
                    println!(
                        "{mode:10} {:8} {ms:+5.0} ms: {ok_all:3}/{:3}  {row}",
                        if fading.is_some() { "moderate" } else { "awgn" },
                        4 * usize::from(nblocks) * 2
                    );
                }
            }
        }
    }

    /// Frame error rate against Eb/N0 for every mode; run with
    /// `cargo test -p decdsss-phy --release -- --ignored --nocapture payload_curve`.
    #[test]
    #[ignore]
    fn payload_curve() {
        let profile = Profile::W2400;
        let modes = [
            "ds4-1/2",
            "ds16-1/2",
            "ds64-1/2",
            "csk32-1/2",
            "csk64-1/2",
            "csk128-1/2",
            "csk256-1/2",
        ];
        for mode in modes {
            let mut line = format!("{mode:11}");
            for ebn0 in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0] {
                let (bad, total) = genie_fer(profile, mode, 4, 1, ebn0, 12, None);
                line += &format!("  {ebn0:.0}dB:{:.2}", bad as f64 / total as f64);
            }
            println!("{line}");
        }
    }

    #[test]
    fn csk_blocks_decode_on_a_clean_channel() {
        for mode in ["csk32-1/2", "csk64-2/3", "csk128-1/2", "csk256-3/4"] {
            let ch = channel(Profile::W2400, 0.0);
            let (ok, total, note) = recover(Profile::W2400, mode, 1, 2, &ch);
            assert_eq!(ok, total, "{mode}: {ok}/{total} {note:?}");
        }
    }

    /// Frame loss of bi-orthogonal CSK against plain CSK through the payload
    /// demodulator alone (a genie start), AWGN and fading. `cargo test --release -p
    /// decdsss-phy bicsk_genie_curve -- --ignored --nocapture`; `BICSK_MODES`
    /// (comma-separated), `BICSK_EBN0` (from,to,step), `BICSK_TRIALS`, `BICSK_FADING`
    /// (good, moderate or poor), `BICSK_DEPTH` and `BICSK_BLOCKS` change the run.
    #[test]
    #[ignore]
    fn bicsk_genie_curve() {
        let env = |k: &str| std::env::var(k).ok();
        let modes: Vec<String> = env("BICSK_MODES").map_or_else(
            || {
                ["csk64-1/2", "bcsk64-1/2", "csk128-1/2"]
                    .map(String::from)
                    .to_vec()
            },
            |v| v.split(',').map(String::from).collect(),
        );
        let [from, to, step]: [f64; 3] = env("BICSK_EBN0")
            .map(|v| {
                v.split(',')
                    .map(|x| x.parse().unwrap())
                    .collect::<Vec<f64>>()
            })
            .map_or([2.0, 8.0, 1.0], |v| [v[0], v[1], v[2]]);
        let trials = env("BICSK_TRIALS").map_or(24, |v| v.parse().unwrap());
        let depth = env("BICSK_DEPTH").map_or(4, |v| v.parse().unwrap());
        let blocks = env("BICSK_BLOCKS").map_or(1, |v| v.parse().unwrap());
        let fading = env("BICSK_FADING").map(|n| Watterson::from_name(&n).unwrap());
        for mode in &modes {
            let mut line = format!("{mode:11}");
            let mut x = from;
            while x <= to + 1e-9 {
                let (bad, total) =
                    genie_fer(Profile::W2400, mode, depth, blocks, x, trials, fading);
                line += &format!("  {x:.1}dB:{:5.1}%", 100.0 * bad as f64 / total as f64);
                x += step;
            }
            println!("{line}");
        }
    }

    /// What a wrong code does to the sign bit: a bi-orthogonal block's hard bit errors
    /// (the LLR's sign against the bit sent) split by whether the symbol's code and the
    /// one before it were right. `cargo test --release -p decdsss-phy
    /// bicsk_sign_diagnostics -- --ignored --nocapture`; `BICSK_MODES` (first one),
    /// `BICSK_EBN0` (from,to,step), `BICSK_TRIALS`, `BICSK_FADING` as for the curve.
    #[test]
    #[ignore]
    fn bicsk_sign_diagnostics() {
        let env = |k: &str| std::env::var(k).ok();
        let mode = env("BICSK_MODES")
            .and_then(|v| v.split(',').next().map(String::from))
            .unwrap_or_else(|| "bcsk64-1/2".into());
        let [from, to, step]: [f64; 3] = env("BICSK_EBN0")
            .map(|v| {
                v.split(',')
                    .map(|x| x.parse().unwrap())
                    .collect::<Vec<f64>>()
            })
            .map_or([2.0, 8.0, 2.0], |v| [v[0], v[1], v[2]]);
        let trials: usize = env("BICSK_TRIALS").map_or(8, |v| v.parse().unwrap());
        let fading = env("BICSK_FADING").map(|n| Watterson::from_name(&n).unwrap());
        let profile = Profile::W2400;
        let m = decdsss_core::Mode::from_name(&mode).unwrap();
        let bits_idx = m.spread.trailing_zeros() as usize;
        let mut x = from;
        while x <= to + 1e-9 {
            // [prev ok, cur ok] -> (sign bits, sign errors, sum |llr|); all symbols' codes.
            let mut cls = [(0usize, 0usize, 0.0f64); 4];
            let (mut syms, mut idx_ok_n) = (0usize, 0usize);
            for trial in 0..trials {
                let seg = segment(profile, &mode, 4, 1);
                let mut ch = channel(profile, x + m.snr_offset_db(profile));
                ch.seed = 7000 + trial as u64;
                ch.fading = fading;
                ch.offset_hz = -10.0 + (trial % 7) as f64 * 3.0;
                ch.clock_ppm = -30.0 + (trial % 5) as f64 * 15.0;
                let audio = through(&seg, &ch);
                let (mut rx, buf) = genie(&seg, &ch, &audio);
                let layout = *rx.layout();
                while let Some(r) = rx.step(&buf) {
                    let frames = &seg.frames[r.first_frame..r.first_frame + layout.depth];
                    let bits = crate::tx::block_bits(&layout, frames);
                    let llrs = &rx.last_llrs;
                    let (mut pos, mut prev_ok) = (0usize, true);
                    for n in 0..layout.symbols() {
                        let ok = (0..bits_idx).all(|j| {
                            pos + j >= bits.len() || u8::from(llrs[pos + j] < 0.0) == bits[pos + j]
                        });
                        pos += bits_idx;
                        if n > 0 {
                            if pos < bits.len() {
                                let err = u8::from(llrs[pos] < 0.0) != bits[pos];
                                let c = &mut cls[usize::from(prev_ok) * 2 + usize::from(ok)];
                                c.0 += 1;
                                c.1 += usize::from(err);
                                c.2 += f64::from(llrs[pos].abs());
                            }
                            pos += 1;
                        }
                        syms += 1;
                        idx_ok_n += usize::from(ok);
                        prev_ok = ok;
                    }
                }
            }
            let mut line = format!(
                "{mode} {x:4.1} dB: codes right {:5.1}%",
                100.0 * idx_ok_n as f64 / syms as f64
            );
            for (name, i) in [
                ("both ok", 3),
                ("prev wrong", 1),
                ("cur wrong", 2),
                ("both wrong", 0),
            ] {
                let (n, e, a) = cls[i];
                line += &format!(
                    " | {name}: n {n} err {:5.1}% |llr| {:.1}",
                    100.0 * e as f64 / n.max(1) as f64,
                    a / n.max(1) as f64
                );
            }
            println!("{line}");
            x += step;
        }
    }

    /// How far off the carrier the demodulator may start and still decode: the
    /// block's first frames start `BICSK_ERR` Hz off (the loop has to pull it in),
    /// AWGN at `BICSK_EBN0` dB, two blocks. `BICSK_MODES`, `BICSK_TRIALS`, `BICSK_ERRS`
    /// (comma-separated Hz) as for the curves; `BICSK_DRIFT` drifts the carrier (Hz a second).
    #[test]
    #[ignore]
    fn bicsk_carrier_error_curve() {
        let env = |k: &str| std::env::var(k).ok();
        let modes: Vec<String> = env("BICSK_MODES").map_or_else(
            || ["csk256-1/2", "bcsk256-1/2"].map(String::from).to_vec(),
            |v| v.split(',').map(String::from).collect(),
        );
        let errs: Vec<f64> = env("BICSK_ERRS").map_or_else(
            || vec![0.0, 0.5, 1.0, 1.5, 2.0, 3.0],
            |v| v.split(',').map(|x| x.parse().unwrap()).collect(),
        );
        let trials: usize = env("BICSK_TRIALS").map_or(6, |v| v.parse().unwrap());
        let ebn0: f64 = env("BICSK_EBN0").map_or(8.0, |v| v.parse().unwrap());
        let drift: f64 = env("BICSK_DRIFT").map_or(0.0, |v| v.parse().unwrap());
        let profile = Profile::W2400;
        for mode in &modes {
            let m = decdsss_core::Mode::from_name(mode).unwrap();
            let mut line = format!("{mode:11} {ebn0:.0} dB");
            for &err in &errs {
                let (mut lost, mut total) = (0, 0);
                for trial in 0..trials {
                    let seg = segment(profile, mode, 2, 2);
                    let mut ch = channel(profile, ebn0 + m.snr_offset_db(profile));
                    ch.seed = 4000 + trial as u64;
                    ch.offset_hz = -4.0 + trial as f64;
                    ch.drift_hz_per_s = drift;
                    let audio = through(&seg, &ch);
                    let (mut rx, buf) = genie(&seg, &ch, &audio);
                    rx.track.freq += if trial % 2 == 0 { err } else { -err };
                    while let Some(r) = rx.step(&buf) {
                        for (i, f) in r.frames.iter().enumerate() {
                            total += 1;
                            lost += usize::from(f.as_ref() != Some(&seg.frames[r.first_frame + i]));
                        }
                    }
                }
                line += &format!("  {err:.1}Hz:{:5.1}%", 100.0 * lost as f64 / total as f64);
            }
            println!("{line}");
        }
    }

    #[test]
    fn bi_orthogonal_csk_blocks_decode_on_a_clean_channel() {
        for mode in ["bcsk32-1/2", "bcsk64-2/3", "bcsk128-1/2", "bcsk256-3/4"] {
            let ch = channel(Profile::W2400, 0.0);
            let (ok, total, note) = recover(Profile::W2400, mode, 1, 2, &ch);
            assert_eq!(ok, total, "{mode}: {ok}/{total} {note:?}");
        }
    }

    #[test]
    fn ds_blocks_decode_on_a_clean_channel() {
        for mode in ["ds4-1/2", "ds8-2/3", "ds16-1/2", "ds32-3/4", "ds64-1/2"] {
            let ch = channel(Profile::W2400, 0.0);
            let (ok, total, note) = recover(Profile::W2400, mode, 1, 2, &ch);
            assert_eq!(ok, total, "{mode}: {ok}/{total} {note:?}");
        }
    }

    #[test]
    fn the_fast_end_decodes_with_noise_offset_and_clock_error() {
        // SF 2 and 1, 5 dB over what the AWGN sweeps found they need (3 to 4 dB).
        for mode in ["ds2-1/2", "ds2-3/4", "ds1-1/2", "ds1-3/4"] {
            let m = decdsss_core::Mode::from_name(mode).unwrap();
            let mut ch = channel(Profile::W2400, 9.0 + m.snr_offset_db(Profile::W2400));
            ch.offset_hz = 11.0;
            ch.clock_ppm = -40.0;
            let (ok, total, note) = recover(Profile::W2400, mode, 2, 2, &ch);
            assert_eq!(ok, total, "{mode}: {ok}/{total} {note:?}");
        }
    }

    #[test]
    fn the_fast_end_holds_up_on_moderate_fading() {
        // Before the pilot-gain smoothing was timed (three pilots at any SF, 8 ms
        // at SF 1) DS1 1/2 lost 34 of 128 frames here, and 43 % through the whole
        // receiver (`decdsss sweep`, 48 transmissions, 30 dB). With it, a few per
        // cent. A wide margin: this guards the cliff, not the floor.
        for mode in ["ds2-1/2", "ds1-1/2"] {
            let (bad, total) = genie_fer(
                Profile::W2400,
                mode,
                4,
                2,
                30.0,
                16,
                Some(Watterson::MODERATE),
            );
            assert!(
                (bad as f64) < 0.2 * total as f64,
                "{mode}: {bad} of {total} frames lost"
            );
        }
    }

    #[test]
    fn pilot_smoothing_spans_the_same_time_at_every_spreading_factor() {
        // SF 4: [1 2 1]/4, as it always was.
        assert_eq!(smooth_half_width(4), 1);
        assert_eq!(smooth_half_width(64), 1);
        assert!((smooth_noise_gain(1) - 0.375).abs() < 1e-12);
        // SF 2 and 1: twice SF 4's span in time. A pilot interval is 8 symbols, so
        // the triangle's zero is (half + 1) pilot intervals of 8·SF chips out:
        // 2 × 8 × 4 = 64 chips at SF 4, 128 at SF 2 and SF 1.
        for (sf, chips) in [(4usize, 64usize), (2, 128), (1, 128)] {
            assert_eq!((smooth_half_width(sf) + 1) * 8 * sf, chips, "SF {sf}");
        }
        // The carrier is measured over the same interval at every SF: 8·4 chips.
        for sf in [1usize, 2, 4, 8, 16] {
            let lag = pilots_per_ref_interval(sf);
            assert!(lag * 8 * sf == 32 || sf > 4, "SF {sf}");
        }
        // Σw²/(Σw)² for SF 1's 15: weights 1..16..1, Σw = 256, Σw² = 2736.
        assert!((smooth_noise_gain(15) - 2736.0 / 65536.0).abs() < 1e-12);
    }

    #[test]
    fn decodes_with_offset_and_clock_error() {
        let mut ch = channel(Profile::W3000, -2.0);
        ch.offset_hz = 47.0;
        ch.clock_ppm = 60.0;
        for mode in ["ds8-1/2", "csk64-1/2"] {
            let (ok, total, note) = recover(Profile::W3000, mode, 2, 2, &ch);
            assert_eq!(ok, total, "{mode}: {ok}/{total} {note:?}");
        }
    }

    #[test]
    fn decodes_through_fading() {
        // Slow Rayleigh fading takes whole transmissions out now and then
        // (the header alone is 8.5 s); with plenty of average SNR most get
        // through complete.
        for mode in ["ds16-1/2", "csk64-1/2"] {
            let mut complete = 0;
            let seeds = 6;
            for seed in 0..seeds {
                let mut ch = channel(Profile::W2400, 6.0);
                ch.fading = Some(Watterson::GOOD);
                ch.offset_hz = -15.0;
                ch.seed = 40 + seed;
                let (ok, total, _) = recover(Profile::W2400, mode, 2, 2, &ch);
                if ok == total {
                    complete += 1;
                }
            }
            assert!(complete >= 4, "{mode}: only {complete} of {seeds} complete");
        }
    }
}
