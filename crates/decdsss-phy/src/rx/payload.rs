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

use super::despread::{Track, csk_bit_llrs, despread, ln_i0, sum, walsh_correlate, walsh_one};
use super::frontend::Buf;
use super::grake;
use super::header::{Finger, HeaderResult};
use super::preamble::DelayProfile;
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
    /// The fingers and the reference were measured from known chips just before:
    /// from the header at the start, and after every block that decoded whole
    /// (the known-chip refinement). Then the block starts at once; otherwise it
    /// waits for the seconds of pilots its own pre-pass measures from.
    fresh: bool,
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
            fresh: true,
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
        self.header.total_frames == 0
            && matches!(
                self.header.ptype,
                crate::framing::PayloadType::Voice | crate::framing::PayloadType::Voice450
            )
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
        self.probe_eot_at(buf, &self.track)
    }

    /// The postamble's place is where the block that starts at `at` would be (its
    /// scrambler position is `chip0`, the chips sent before that block).
    fn probe_eot_at(&self, buf: &Buf, at: &Track) -> Option<f32> {
        let profile = self.header.profile;
        let n = eot::len(profile);
        let reach = DelayProfile::lag(DelayProfile::LAGS) + n as f64;
        if at.pos + reach * at.step + 16.0 > buf.end() as f64 {
            return None;
        }
        let refs = eot::chips(profile, &self.scr, self.chip0);
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
        let from = match &self.work {
            Some(Work::Csk(w)) => w.start,
            Some(Work::Ds(w)) => w.start,
            None => self.track,
        };
        (from.pos + (min_lag - 10.0) * from.step) as i64
    }

    /// Works on the block in progress with the samples there are; returns it
    /// when it is complete.
    pub fn step(&mut self, buf: &Buf) -> Option<BlockResult> {
        if self.done() || self.eot.is_some() {
            return None;
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
            if let Some(r) = self.probe_eot_at(buf, &start) {
                self.eot_probed = true;
                if r >= eot::EOT_PEAK_OVER_MEDIAN {
                    self.eot = Some(r);
                    self.work = None;
                    return None;
                }
            }
        }
        let work = self.work.take()?;
        match work {
            Work::Csk(mut w) => {
                if self.advance_csk(buf, &mut w) {
                    let r = self.finish_csk(buf, *w);
                    self.end_block();
                    Some(r)
                } else {
                    self.work = Some(Work::Csk(w));
                    None
                }
            }
            Work::Ds(mut w) => {
                if self.advance_ds(buf, &mut w) {
                    let r = self.finish_ds(buf, *w);
                    self.end_block();
                    Some(r)
                } else {
                    self.work = Some(Work::Ds(w));
                    None
                }
            }
        }
    }

    fn end_block(&mut self) {
        self.chip0 += self.layout.chips();
        self.next_block += 1;
    }

    fn begin_block(&mut self, buf: &Buf, prepass: bool) {
        self.eot_probed = false;
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
            Family::Csk => {
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
                })));
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
                })));
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
            self.refresh_fingers(buf, &frames, block_start);
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
    fn refresh_fingers(&mut self, buf: &Buf, frames: &[Option<Vec<u8>>], start: Track) {
        let data: Vec<Vec<u8>> = frames.iter().map(|f| f.clone().unwrap()).collect();
        let chips = block_chips(&self.layout, &data, &self.scr, self.chip0);
        let main_lag = self.fingers.first().map_or(0.0, |f| f.lag);
        let cr = self.rate / super::frontend::SPS as f64;
        let seg = super::refine::segment_chips(cr);
        let refined = super::refine::refine(buf, &start, &chips, main_lag, seg, 60e-6);
        let (fingers, shift) = match &refined {
            Some(r) => super::refine::tail_fingers(buf, r, &start, &chips, seg),
            None => {
                let p = super::refine::profile_known(buf, &start, &chips, seg);
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
        while wk.s < wk.s_count {
            if wk.t.pos + (l as f64 + AHEAD_CHIPS) * wk.t.step + 16.0 > buf.end() as f64 {
                return false;
            }
            let s = wk.s;
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
            // The main finger's peak carries the carrier measurement.
            let peak = wk.w[at + m];
            if let Some(p) = wk.peak_prev {
                wk.freq_acc += peak * p.conj();
                wk.freq_norm += peak.norm() * p.norm();
            }
            wk.peak_prev = Some(peak);
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
                wk.t.freq += freq_correction(wk.freq_acc, wk.freq_norm, t_sym);
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
                }
                wk.tc = wk.t;
                wk.s_lo = s + 1;
            }
            wk.s += 1;
            self.live.symbol = wk.s;
        }
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
        let mut llrs = Vec::with_capacity(wk.s_count * bits);
        let mut metric = vec![0.0f64; l];
        for &(off, nf) in &wk.index {
            for (m, v) in metric.iter_mut().enumerate() {
                *v = (0..nf)
                    .map(|f| ln_i0(kappa[f] * f64::from(wk.w[off + f * l + m].norm())))
                    .sum();
            }
            csk_bit_llrs(&metric, bits, &mut llrs);
        }
        let snr = 10.0 * (amp2[0] / sigma2[0].max(1e-30)).log10();
        self.finish(buf, &llrs, None, snr, wk.start)
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
        while wk.s < wk.s_count {
            if wk.t.pos + (l as f64 + AHEAD_CHIPS) * wk.t.step + 16.0 > buf.end() as f64 {
                return false;
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
                if wk.pilot_no > 0 {
                    let (a, b) = (wk.g[wk.pilot_no][0], wk.g[wk.pilot_no - 1][0]);
                    wk.freq_acc += a * b.conj();
                    wk.freq_norm += a.norm() * b.norm();
                }
                wk.pilot_no += 1;
                if wk.pilot_no.is_multiple_of(freq_every) {
                    wk.t.freq += freq_correction(wk.freq_acc, wk.freq_norm, t_pilot);
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
        }
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

        // Smooth the pilot gains ([1 2 1]/4, shorter at the ends) and
        // interpolate linearly to the data symbols.
        let mut hs = vec![[Iq::new(0.0, 0.0); SLOTS]; pilots];
        for k in 0..pilots {
            for fi in 0..SLOTS {
                hs[k][fi] = match (k.checked_sub(1), (k + 1 < pilots).then_some(k + 1)) {
                    (Some(a), Some(b)) => (g[a][fi] + g[k][fi] * 2.0 + g[b][fi]) / 4.0,
                    (None, Some(b)) => (g[k][fi] * 2.0 + g[b][fi]) / 3.0,
                    (Some(a), None) => (g[a][fi] + g[k][fi] * 2.0) / 3.0,
                    (None, None) => g[k][fi],
                };
            }
        }
        // Per finger, the noise in a pilot gain from the second difference (the
        // smooth channel cancels): var(g_k − (g_{k−1}+g_{k+1})/2) = 1.5 σ². A
        // smoothed estimate carries 0.375 σ² of it. Where a finger's estimate
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
            let var = (0.375 * acc / cnt as f64) as f32;
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
        let var_g0 = if n_cnt > 0 { n_acc / n_cnt as f64 } else { f64::NAN };
        let grake = if debug_frozen("grake") || rake_only() {
            None
        } else {
            self.grake_llrs(buf, &wk, &hs, var_g0)
        };
        match grake {
            Some(g) => self.finish(buf, &g, Some(&llrs), snr, wk.start),
            None => self.finish(buf, &llrs, None, snr, wk.start),
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
        let isi = a.iter().map(|a| grake::isi_power(lags[0], &lags, a)).sum::<f64>() / a.len() as f64;
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
                let t0 = 1.0 + crate::tx::lead_chips(profile) as f64 / 1920.0;
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

    /// A stretch of audio cut out of (negative) or silence put into (positive)
    /// the middle of block 2, as a sound card that drops or repeats a buffer
    /// does; which blocks decode after it, on a clean channel and on moderate
    /// fading. `cargo test --release -p decdsss-phy sync_slip_curve -- --ignored
    /// --nocapture`
    #[test]
    #[ignore]
    fn sync_slip_curve() {
        let profile = Profile::W2400;
        for mode in ["ds4-2/3", "ds8-1/2", "csk64-1/2"] {
            let m = decdsss_core::Mode::from_name(mode).unwrap();
            let layout = Layout::new(profile, m, 2);
            let nblocks = 6u16;
            for fading in [None, Some(Watterson::MODERATE)] {
                for ms in [-50.0f64, -20.0, -5.0, -1.0, 0.0, 1.0, 5.0, 20.0, 50.0] {
                    let mut row = String::new();
                    let mut ok_all = 0;
                    for trial in 0..4u64 {
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

    #[test]
    fn ds_blocks_decode_on_a_clean_channel() {
        for mode in ["ds4-1/2", "ds8-2/3", "ds16-1/2", "ds32-3/4", "ds64-1/2"] {
            let ch = channel(Profile::W2400, 0.0);
            let (ok, total, note) = recover(Profile::W2400, mode, 1, 2, &ch);
            assert_eq!(ok, total, "{mode}: {ok}/{total} {note:?}");
        }
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
