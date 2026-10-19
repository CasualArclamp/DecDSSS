// made by CasualArclamp
//! The payload demodulator: one station's blocks, decoded in order.
//!
//! A block is processed in two passes. The first walks the symbols once,
//! despreading each rake finger and keeping what the second needs: for CSK the
//! Walsh correlations, for DS-PSK the despread symbols and the pilot gains.
//! The second turns that into bit LLRs (a noncoherent Bessel metric for CSK,
//! pilot-aided maximal-ratio combining for DS-PSK), de-interleaves, and
//! LDPC-decodes each codeword.
//!
//! Timing is kept by a *searcher*, not an early/late loop. Every few seconds
//! the delay profile is measured again from what is known (the pilots, or the
//! symbols just decided), the reference is moved onto the strongest path and
//! the fingers are placed on the peaks; the shifts also teach the clock error.
//! An early/late loop on one finger cannot be trusted with several paths a chip
//! or so apart: the late arm picks up the next path's correlation tail and the
//! loop settles between the two.

use super::despread::{Track, csk_bit_llrs, despread, ln_i0, sum, walsh_correlate, walsh_one};
use super::frontend::Buf;
use super::header::{Finger, HeaderResult};
use super::preamble::DelayProfile;
use crate::codes::{Scrambler, own_code};
use crate::framing::{Header, Layout, check_frame};
use crate::tx::{block_chips, pilot_symbol};
use decdsss_core::Family;
use decdsss_core::mode::PILOT_INTERVAL;
use decdsss_dsp::Iq;
use decdsss_fec::interleave::Interleaver;
use decdsss_fec::ldpc::Decoder;
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
        self.e = 0.0;
        self.l = 0.0;
        self.n = 0;
        self.rate += self.ki * err;
        Some(self.kp * err + self.rate)
    }
}

pub struct PayloadRx {
    pub header: Header,
    layout: Layout,
    scr: Scrambler,
    pub track: Track,
    pub fingers: Vec<Finger>,
    /// The carrier the header found; the loop stays near it.
    f0: f64,
    chip0: usize,
    next_block: usize,
    dll: Dll,
    rate: f64,
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
            f0: h.track.freq,
            chip0: 0,
            next_block: 0,
            dll: Dll::new(symbol_s),
            rate,
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

    pub fn blocks_done(&self) -> usize {
        self.next_block
    }

    /// Earliest sample still needed (the earliest finger's lag, with margin).
    pub fn oldest_needed(&self) -> i64 {
        let min_lag = self.fingers.iter().map(|f| f.lag).fold(0.0f64, f64::min);
        (self.track.pos + (min_lag - 8.0) * self.track.step) as i64
    }

    /// Decodes the next block if the buffer holds all of it.
    pub fn step(&mut self, buf: &Buf) -> Option<BlockResult> {
        if self.done() {
            return None;
        }
        // The searcher may reach 20 chips past the reference.
        let need = self.track.pos
            + (self.layout.chips() as f64 + DelayProfile::lag(DelayProfile::LAGS - 1) + 6.0)
                * self.track.step
            + 16.0;
        if need > buf.end() as f64 {
            return None;
        }
        let cr = f64::from(self.header.profile.chip_rate());
        self.dll.set_wide((self.chip0 as f64) / cr < DLL_WIDE_S);
        let result = match self.layout.mode.family {
            Family::Csk => self.block_csk(buf),
            Family::DsPsk => self.block_ds(buf),
        };
        self.chip0 += self.layout.chips();
        self.next_block += 1;
        Some(result)
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

    fn finish(&mut self, buf: &Buf, llrs: &[f32], snr_db: f64, block_start: Track) -> BlockResult {
        #[cfg(test)]
        {
            self.last_llrs = llrs.to_vec();
        }
        let (frames, iterations) = self.decode(llrs);
        let first_frame = usize::from(self.header.frame0) + self.next_block * self.layout.depth;
        if frames.iter().all(Option::is_some) {
            self.refresh_fingers(buf, &frames, block_start);
        }
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
        self.fingers = fingers;
    }

    /// Re-places the reference and the fingers from a measured profile, if the
    /// profile has a clear peak; returns the chips the reference moved.
    fn adopt(&mut self, profile: &DelayProfile) -> Option<f64> {
        let mut p = profile.power.clone();
        p.sort_by(f32::total_cmp);
        let median = p[p.len() / 2].max(1e-30);
        let peak = p[p.len() - 1];
        if peak < PROFILE_PEAK_OVER_MEDIAN * median {
            return None;
        }
        let (fingers, shift) = super::refine::fingers_from_profile(profile);
        self.fingers = fingers;
        Some(shift)
    }

    /// Mid-block, a measured profile only moves the reference, and only to a
    /// clearly better path; the fingers keep their places relative to it.
    /// Returns the chips moved.
    fn adopt_track(&mut self, profile: &DelayProfile) -> Option<f64> {
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

    fn block_csk(&mut self, buf: &Buf) -> BlockResult {
        let l = usize::from(self.header.mode.spread);
        let bits = l.trailing_zeros() as usize;
        let s_count = self.layout.symbols();
        let scr = self.scr.chips(self.chip0, s_count * l);
        self.csk_prepass(buf, &scr);
        let start = self.track;
        let step0 = start.step;
        let mut t = self.track;
        let mut u = vec![Iq::new(0.0, 0.0); l];
        let mut ue = u.clone();
        let mut ul = u.clone();
        // W rows for every symbol and finger, flat, with where each symbol's
        // rows start and how many fingers it had.
        let mut w: Vec<Iq> = Vec::with_capacity(s_count * SLOTS * l);
        let mut index: Vec<(usize, usize)> = Vec::with_capacity(s_count);
        let mut decisions = Vec::with_capacity(s_count);
        let t_sym = l as f64 * t.step / t.rate;
        let freq_every = ((FREQ_UPDATE_S / t_sym).ceil() as usize).max(4);
        let chunk = (((SEARCH_S / t_sym).ceil() as usize).max(SEARCH_MIN_SYMBOLS)).min(s_count);
        let mut peak_prev: Option<Iq> = None;
        let mut freq_acc = Iq::new(0.0, 0.0);
        let mut freq_norm = 0.0f32;
        let (mut tc, mut s_lo) = (t, 0usize);
        for s in 0..s_count {
            let sc = &scr[s * l..(s + 1) * l];
            let nf = self.fingers.len();
            let at = w.len();
            index.push((at, nf));
            for f in &self.fingers {
                despread(buf, &t, sc, f.lag, &mut u);
                w.extend(walsh_correlate(&u));
            }
            // Decision: the code with the most weighted power over fingers.
            let mut best = (0usize, -1.0f32);
            for m in 0..l {
                let p: f32 = (0..nf)
                    .map(|f| self.fingers[f].power * w[at + f * l + m].norm_sqr())
                    .sum();
                if p > best.1 {
                    best = (m, p);
                }
            }
            let m = best.0;
            decisions.push(m);
            // Early and late arms of every finger, at the decided code.
            let (mut e_sum, mut l_sum) = (0.0f64, 0.0f64);
            for f in &self.fingers {
                despread(buf, &t, sc, f.lag - 0.25, &mut ue);
                despread(buf, &t, sc, f.lag + 0.25, &mut ul);
                e_sum += f64::from(walsh_one(&ue, m).norm());
                l_sum += f64::from(walsh_one(&ul, m).norm());
            }
            if let Some(c) = self.dll.push(e_sum, l_sum) {
                t.pos += c * t.step;
            }
            // The strongest finger carries the carrier measurement.
            let peak = w[at + m];
            if let Some(p) = peak_prev {
                freq_acc += peak * p.conj();
                freq_norm += peak.norm() * p.norm();
            }
            peak_prev = Some(peak);
            t.advance(l);
            if (s + 1) % freq_every == 0 {
                t.freq += freq_correction(freq_acc, freq_norm, t_sym);
                t.freq = t
                    .freq
                    .clamp(self.f0 - FREQ_MAX_EXCURSION, self.f0 + FREQ_MAX_EXCURSION);
                freq_acc = Iq::new(0.0, 0.0);
                freq_norm = 0.0;
            }
            // The searcher: at the end of each chunk (not the last: the
            // known-chip refinement after the block does that better).
            if (s + 1 - s_lo) >= chunk && s + 1 < s_count {
                let profile = self.profile_csk(buf, &tc, &scr, s_lo, &decisions[s_lo..=s]);
                if let Some(shift) = self.adopt_track(&profile) {
                    self.apply_shift(&mut t, shift, ((s + 1 - s_lo) * l) as f64, step0);
                }
                tc = t;
                s_lo = s + 1;
            }
        }
        self.track = t;

        // Pass two: per-finger noise and amplitude, then the metric.
        let (mut sigma2, mut amp2, mut cnt) = ([0.0f64; SLOTS], [0.0f64; SLOTS], [0usize; SLOTS]);
        for (s, &m) in decisions.iter().enumerate() {
            let (off, nf) = index[s];
            for f in 0..nf {
                let row = &w[off + f * l..off + (f + 1) * l];
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
        let mut llrs = Vec::with_capacity(s_count * bits);
        let mut metric = vec![0.0f64; l];
        for &(off, nf) in &index {
            for (m, v) in metric.iter_mut().enumerate() {
                *v = (0..nf)
                    .map(|f| ln_i0(kappa[f] * f64::from(w[off + f * l + m].norm())))
                    .sum();
            }
            csk_bit_llrs(&metric, bits, &mut llrs);
        }
        let snr = 10.0 * (amp2[0] / sigma2[0].max(1e-30)).log10();
        self.finish(buf, &llrs, snr, start)
    }

    fn block_ds(&mut self, buf: &Buf) -> BlockResult {
        let l = usize::from(self.header.mode.spread);
        let s_count = self.layout.symbols();
        let per = PILOT_INTERVAL as usize;
        let scr = self.scr.chips(self.chip0, s_count * l);
        self.ds_prepass(buf, &scr);
        let start = self.track;
        let step0 = start.step;
        let mut t = self.track;
        let mut u = vec![Iq::new(0.0, 0.0); l];
        let mut ue = u.clone();
        let mut ul = u.clone();
        // y[s][f]: the despread symbol per finger slot; zero for unused slots.
        let mut y = vec![[Iq::new(0.0, 0.0); SLOTS]; s_count];
        let pilots = s_count / per + 1;
        // g[k][f]: pilot gain estimates.
        let mut g = vec![[Iq::new(0.0, 0.0); SLOTS]; pilots];
        let t_sym = l as f64 * t.step / t.rate;
        let t_pilot = per as f64 * t_sym;
        let freq_every = ((FREQ_UPDATE_S / t_pilot).ceil() as usize).max(3);
        let chunk_pilots = ((SEARCH_S / t_pilot).ceil() as usize).max(SEARCH_MIN_PILOTS);
        let chunk = chunk_pilots * per;
        let mut freq_acc = Iq::new(0.0, 0.0);
        let mut freq_norm = 0.0f32;
        let mut pilot_no = 0;
        let (mut tc, mut s_lo) = (t, 0usize);
        for s in 0..s_count {
            let sc = &scr[s * l..(s + 1) * l];
            let (mut e_sum, mut l_sum) = (0.0f64, 0.0f64);
            for (fi, f) in self.fingers.iter().enumerate().take(SLOTS) {
                despread(buf, &t, sc, f.lag, &mut u);
                y[s][fi] = sum(&u);
                despread(buf, &t, sc, f.lag - 0.25, &mut ue);
                despread(buf, &t, sc, f.lag + 0.25, &mut ul);
                e_sum += f64::from(sum(&ue).norm());
                l_sum += f64::from(sum(&ul).norm());
            }
            if let Some(c) = self.dll.push(e_sum, l_sum) {
                t.pos += c * t.step;
            }
            if self.layout.is_pilot(s) {
                let p = pilot_symbol(pilot_no).conj();
                for fi in 0..SLOTS {
                    g[pilot_no][fi] = y[s][fi] * p;
                }
                if pilot_no > 0 {
                    freq_acc += g[pilot_no][0] * g[pilot_no - 1][0].conj();
                    freq_norm += g[pilot_no][0].norm() * g[pilot_no - 1][0].norm();
                }
                pilot_no += 1;
                if pilot_no % freq_every == 0 {
                    t.freq += freq_correction(freq_acc, freq_norm, t_pilot);
                    t.freq = t
                        .freq
                        .clamp(self.f0 - FREQ_MAX_EXCURSION, self.f0 + FREQ_MAX_EXCURSION);
                    freq_acc = Iq::new(0.0, 0.0);
                    freq_norm = 0.0;
                }
            }
            t.advance(l);
            if (s + 1 - s_lo) >= chunk && s + 1 < s_count {
                let profile = self.profile_ds(buf, &tc, &scr, s_lo, s + 1);
                if let Some(shift) = self.adopt_track(&profile) {
                    self.apply_shift(&mut t, shift, ((s + 1 - s_lo) * l) as f64, step0);
                }
                tc = t;
                s_lo = s + 1;
            }
        }
        self.track = t;
        debug_assert_eq!(pilot_no, pilots);

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
        self.finish(buf, &llrs, snr, start)
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
