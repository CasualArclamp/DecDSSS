// made by CasualArclamp
//! The payload demodulator: one station's blocks, decoded in order.
//!
//! A block is processed in two passes. The first walks the symbols once,
//! despreading each rake finger, running the delay-locked loop (early/late
//! on the main finger) and the carrier loop, and keeping what the second
//! pass needs: for CSK the Walsh correlations, for DS-PSK the despread
//! symbols and the pilot gains. The second pass turns that into bit LLRs
//! (a noncoherent Bessel metric for CSK, pilot-aided maximal-ratio combining
//! for DS-PSK), de-interleaves, and LDPC-decodes each codeword.

use super::despread::{Track, csk_bit_llrs, despread, ln_i0, sum, walsh_correlate, walsh_one};
use super::frontend::Buf;
use super::header::{Finger, HeaderResult};
use crate::codes::{Scrambler, own_code};
use crate::framing::{Header, Layout, check_frame};
use crate::tx::{block_chips, pilot_symbol};
use decdsss_core::Family;
use decdsss_core::mode::PILOT_INTERVAL;
use decdsss_dsp::Iq;
use decdsss_fec::interleave::Interleaver;
use decdsss_fec::ldpc::Decoder;
use std::f64::consts::TAU;

/// Delay-locked loop noise bandwidth, Hz.
const DLL_BW_HZ: f64 = 0.25;
/// Seconds of symbols the early/late powers are averaged over per update.
const DLL_GROUP_S: f64 = 0.05;
/// The frequency estimate is revised about every this many seconds.
const FREQ_UPDATE_S: f64 = 1.5;
/// LDPC iterations.
const MAX_ITER: usize = 50;

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
    let wn = 2.0 * bn_hz / (zeta + 1.0 / (4.0 * zeta)) * 1.0;
    let w = wn * update_s;
    (2.0 * zeta * w, w * w)
}

/// The timing loop: early and late powers averaged over a group of symbols,
/// then one update.
struct Dll {
    kp: f64,
    ki: f64,
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
        let (kp, ki) = loop_gains(DLL_BW_HZ, group as f64 * symbol_s);
        Dll {
            kp,
            ki,
            group,
            e: 0.0,
            l: 0.0,
            n: 0,
            rate: 0.0,
        }
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
        (self.track.pos + (min_lag - 3.0) * self.track.step) as i64
    }

    /// Decodes the next block if the buffer holds all of it.
    pub fn step(&mut self, buf: &Buf) -> Option<BlockResult> {
        if self.done() {
            return None;
        }
        let max_lag = self.fingers.iter().map(|f| f.lag).fold(0.0f64, f64::max);
        let need =
            self.track.pos + (self.layout.chips() as f64 + max_lag + 6.0) * self.track.step + 16.0;
        if need > buf.end() as f64 {
            return None;
        }
        let result = match self.layout.mode.family {
            Family::Csk => self.block_csk(buf),
            Family::DsPsk => self.block_ds(buf),
        };
        self.chip0 += self.layout.chips();
        self.next_block += 1;
        Some(result)
    }

    fn main_finger(&self) -> usize {
        self.fingers
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.power.total_cmp(&b.1.power))
            .map(|(i, _)| i)
            .unwrap_or(0)
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
    /// known: correlate the received signal against them at each lag to get
    /// the block's delay profile, and re-pick the fingers from it.
    fn refresh_fingers(&mut self, buf: &Buf, frames: &[Option<Vec<u8>>], start: Track) {
        use super::preamble::DelayProfile;
        let data: Vec<Vec<u8>> = frames.iter().map(|f| f.clone().unwrap()).collect();
        let chips = block_chips(&self.layout, &data, &self.scr, self.chip0);
        // Timing, clock and carrier against the exact chips first.
        let main_lag = self
            .fingers
            .iter()
            .max_by(|a, b| a.power.total_cmp(&b.power))
            .map_or(0.0, |f| f.lag);
        let cr = self.rate / super::frontend::SPS as f64;
        if let Some(r) = super::refine::refine(
            buf,
            &start,
            &chips,
            main_lag,
            super::refine::segment_chips(cr),
        ) {
            self.track.pos = r.pos_end;
            self.track.step = r.step;
            self.track.freq = r.freq;
            self.track.phase = 0.0;
            self.dll.reset();
        }
        let seg = 1024.min(chips.len());
        let mut power = vec![0.0f32; DelayProfile::LAGS];
        let t = start;
        let dphi = t.dphase();
        for (i, pw) in power.iter_mut().enumerate() {
            let lag = DelayProfile::lag(i);
            let mut k0 = 0;
            while k0 + seg <= chips.len() {
                let mut acc = Iq::new(0.0, 0.0);
                for (k, chip) in chips.iter().enumerate().skip(k0).take(seg) {
                    let ph = t.phase + k as f64 * dphi;
                    let (s, c) = ph.sin_cos();
                    let r = buf.at(t.pos + (k as f64 + lag) * t.step);
                    acc += r * Iq::new(c as f32, -(s as f32)) * chip.conj();
                }
                *pw += (acc / seg as f32).norm_sqr();
                k0 += seg;
            }
        }
        let profile = DelayProfile { power };
        let mut fingers = super::header::pick_fingers(&profile, 3);
        // Keep the main finger at lag 0: move the reference onto it.
        if let Some(main) = fingers
            .iter()
            .max_by(|a, b| a.power.total_cmp(&b.power))
            .copied()
            && main.lag.abs() >= 0.5
        {
            self.track.pos += main.lag * self.track.step;
            for f in &mut fingers {
                f.lag -= main.lag;
            }
        }
        self.fingers = fingers;
    }

    fn block_csk(&mut self, buf: &Buf) -> BlockResult {
        let start = self.track;
        let l = usize::from(self.header.mode.spread);
        let bits = l.trailing_zeros() as usize;
        let s_count = self.layout.symbols();
        let scr = self.scr.chips(self.chip0, s_count * l);
        let nf = self.fingers.len();
        let main = self.main_finger();
        let mut t = self.track;
        let mut u = vec![Iq::new(0.0, 0.0); l];
        let mut ue = u.clone();
        let mut ul = u.clone();
        let mut w: Vec<Iq> = Vec::with_capacity(s_count * nf * l);
        let mut decisions = Vec::with_capacity(s_count);
        let t_sym = l as f64 * t.step / t.rate;
        let freq_every = ((FREQ_UPDATE_S / t_sym).ceil() as usize).max(4);
        let mut peak_prev: Option<Iq> = None;
        let mut freq_acc = Iq::new(0.0, 0.0);
        let mut weights = vec![0.0f32; nf];
        for (wt, f) in weights.iter_mut().zip(&self.fingers) {
            *wt = f.power;
        }
        for s in 0..s_count {
            let sc = &scr[s * l..(s + 1) * l];
            let at = w.len();
            for f in &self.fingers {
                despread(buf, &t, sc, f.lag, &mut u);
                w.extend(walsh_correlate(&u));
            }
            // Decision: the code with the most weighted power over fingers.
            let mut best = (0usize, -1.0f32);
            for m in 0..l {
                let p: f32 = (0..nf)
                    .map(|f| weights[f] * w[at + f * l + m].norm_sqr())
                    .sum();
                if p > best.1 {
                    best = (m, p);
                }
            }
            let m = best.0;
            decisions.push(m);
            let lag = self.fingers[main].lag;
            despread(buf, &t, sc, lag - 0.25, &mut ue);
            despread(buf, &t, sc, lag + 0.25, &mut ul);
            if let Some(c) = self.dll.push(
                f64::from(walsh_one(&ue, m).norm()),
                f64::from(walsh_one(&ul, m).norm()),
            ) {
                t.pos += c * t.step;
            }
            let peak = w[at + main * l + m];
            if let Some(p) = peak_prev {
                freq_acc += peak * p.conj();
            }
            peak_prev = Some(peak);
            t.advance(l);
            if (s + 1) % freq_every == 0 {
                if std::env::var("DSSS_DEBUG").is_ok() {
                    eprintln!(
                        "blk {} sym {s}: freq_acc {:?} -> {:+.3} Hz, pos {:.2}, peak |{:.0}| m={m}",
                        self.next_block,
                        freq_acc,
                        f64::from(freq_acc.arg()) / (TAU * t_sym),
                        t.pos,
                        peak.norm()
                    );
                }
                t.freq += 0.5 * f64::from(freq_acc.arg()) / (TAU * t_sym);
                freq_acc = Iq::new(0.0, 0.0);
            }
        }
        self.track = t;

        // Pass two: per-finger noise and amplitude, then the metric.
        let mut sigma2 = vec![0.0f64; nf];
        let mut amp2 = vec![0.0f64; nf];
        for (s, &m) in decisions.iter().enumerate() {
            for f in 0..nf {
                let row = &w[(s * nf + f) * l..(s * nf + f + 1) * l];
                let total: f64 = row.iter().map(|z| f64::from(z.norm_sqr())).sum();
                let pk = f64::from(row[m].norm_sqr());
                sigma2[f] += (total - pk) / (l as f64 - 1.0);
                amp2[f] += pk;
            }
        }
        for f in 0..nf {
            sigma2[f] /= s_count as f64;
            amp2[f] = (amp2[f] / s_count as f64 - sigma2[f]).max(0.01 * sigma2[f]);
        }
        let kappa: Vec<f64> = (0..nf)
            .map(|f| 2.0 * amp2[f].sqrt() / sigma2[f].max(1e-30))
            .collect();
        let mut llrs = Vec::with_capacity(s_count * bits);
        let mut metric = vec![0.0f64; l];
        for s in 0..s_count {
            for (m, v) in metric.iter_mut().enumerate() {
                *v = (0..nf)
                    .map(|f| ln_i0(kappa[f] * f64::from(w[(s * nf + f) * l + m].norm())))
                    .sum();
            }
            csk_bit_llrs(&metric, bits, &mut llrs);
        }
        let snr = 10.0 * (amp2[main] / sigma2[main]).log10();
        self.finish(buf, &llrs, snr, start)
    }

    fn block_ds(&mut self, buf: &Buf) -> BlockResult {
        let start = self.track;
        let l = usize::from(self.header.mode.spread);
        let s_count = self.layout.symbols();
        let scr = self.scr.chips(self.chip0, s_count * l);
        let nf = self.fingers.len();
        let main = self.main_finger();
        let mut t = self.track;
        let mut u = vec![Iq::new(0.0, 0.0); l];
        let mut ue = u.clone();
        let mut ul = u.clone();
        // y[f][s]
        let mut y = vec![vec![Iq::new(0.0, 0.0); s_count]; nf];
        let pilots = s_count / PILOT_INTERVAL as usize + 1;
        // g[f][k]: pilot gain estimates.
        let mut g = vec![vec![Iq::new(0.0, 0.0); pilots]; nf];
        let t_sym = l as f64 * t.step / t.rate;
        let t_pilot = PILOT_INTERVAL as f64 * t_sym;
        let freq_every = ((FREQ_UPDATE_S / t_pilot).ceil() as usize).max(3);
        let mut freq_acc = Iq::new(0.0, 0.0);
        let mut pilot_no = 0;
        for s in 0..s_count {
            let sc = &scr[s * l..(s + 1) * l];
            for (fi, f) in self.fingers.iter().enumerate() {
                despread(buf, &t, sc, f.lag, &mut u);
                y[fi][s] = sum(&u);
            }
            let lag = self.fingers[main].lag;
            despread(buf, &t, sc, lag - 0.25, &mut ue);
            despread(buf, &t, sc, lag + 0.25, &mut ul);
            if let Some(c) = self
                .dll
                .push(f64::from(sum(&ue).norm()), f64::from(sum(&ul).norm()))
            {
                t.pos += c * t.step;
            }
            if self.layout.is_pilot(s) {
                let p = pilot_symbol(pilot_no).conj();
                for fi in 0..nf {
                    g[fi][pilot_no] = y[fi][s] * p;
                }
                if pilot_no > 0 {
                    freq_acc += g[main][pilot_no] * g[main][pilot_no - 1].conj();
                }
                pilot_no += 1;
                if pilot_no % freq_every == 0 {
                    t.freq += 0.5 * f64::from(freq_acc.arg()) / (TAU * t_pilot);
                    freq_acc = Iq::new(0.0, 0.0);
                }
            }
            t.advance(l);
        }
        self.track = t;
        debug_assert_eq!(pilot_no, pilots);

        // Smooth the pilot gains ([1 2 1]/4, shorter at the ends) and
        // interpolate linearly to the data symbols.
        let mut hs = vec![vec![Iq::new(0.0, 0.0); pilots]; nf];
        for fi in 0..nf {
            for k in 0..pilots {
                hs[fi][k] = match (k.checked_sub(1), (k + 1 < pilots).then_some(k + 1)) {
                    (Some(a), Some(b)) => (g[fi][a] + g[fi][k] * 2.0 + g[fi][b]) / 4.0,
                    (None, Some(b)) => (g[fi][k] * 2.0 + g[fi][b]) / 3.0,
                    (Some(a), None) => (g[fi][a] + g[fi][k] * 2.0) / 3.0,
                    (None, None) => g[fi][k],
                };
            }
        }
        // Noise per pilot from the second difference (the smooth channel
        // cancels): var(g_k − (g_{k−1}+g_{k+1})/2) = 1.5 σ².
        let (mut n_acc, mut n_cnt, mut p_acc) = (0.0f64, 0usize, 0.0f64);
        for k in 1..pilots.saturating_sub(1) {
            let d = g[main][k] - (g[main][k - 1] + g[main][k + 1]) * 0.5;
            n_acc += f64::from(d.norm_sqr()) / 1.5;
            p_acc += f64::from(hs[main][k].norm_sqr());
            n_cnt += 1;
        }
        let snr = if n_cnt > 0 && n_acc > 0.0 {
            10.0 * (p_acc / n_acc).log10()
        } else {
            f64::NAN
        };

        let mut llrs = Vec::with_capacity(self.layout.data_symbols() * 2);
        let per = PILOT_INTERVAL as usize;
        for s in (0..s_count).filter(|&s| !self.layout.is_pilot(s)) {
            let (k, a) = (s / per, (s % per) as f32 / per as f32);
            let mut z = Iq::new(0.0, 0.0);
            for fi in 0..nf {
                let h = hs[fi][k] * (1.0 - a) + hs[fi][k + 1] * a;
                z += h.conj() * y[fi][s];
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
    #[ignore]
    fn debug_floor() {
        let profile = Profile::W2400;
        let mode = "ds16-1/2";
        let m = decdsss_core::Mode::from_name(mode).unwrap();
        for trial in 0..20usize {
            let seg = segment(profile, mode, 4, 1);
            let mut ch = channel(profile, 6.0 + m.snr_offset_db(profile));
            ch.seed = 9000 + trial as u64;
            ch.offset_hz = -10.0 + (trial % 7) as f64 * 3.0;
            ch.clock_ppm = -30.0 + (trial % 5) as f64 * 15.0;
            let audio = through(&seg, &ch);
            let (mut rx, buf) = genie(&seg, &ch, &audio);
            while let Some(r) = rx.step(&buf) {
                if trial == 7 {
                    // Truth coded bits of the block, in transmit order.
                    let layout = *rx.layout();
                    let truth = crate::tx::block_bits(
                        &layout,
                        &seg.frames[r.first_frame..r.first_frame + 4],
                    );
                    let wrong: Vec<usize> = (0..truth.len())
                        .filter(|&i| (rx.last_llrs[i] < 0.0) != (truth[i] == 1))
                        .collect();
                    println!("hard-decision errors in the block: {}", wrong.len());
                    let il = decdsss_fec::interleave::Interleaver::new(truth.len());
                    let idx: Vec<usize> = (0..truth.len()).collect();
                    let coding = il.invert(&idx);
                    let _ = coding;
                    let mut per_cw = [0usize; 4];
                    let sent = il.apply(&(0..truth.len()).collect::<Vec<_>>());
                    for &w in &wrong {
                        per_cw[sent[w] / 1152] += 1;
                    }
                    println!(
                        "per codeword: {per_cw:?}; first wrong transmit positions {:?}",
                        &wrong[..wrong.len().min(20)]
                    );
                    {
                        let il2 = decdsss_fec::interleave::Interleaver::new(truth.len());
                        let coded = il2.invert(&rx.last_llrs[..truth.len()]);
                        let code = layout.code();
                        let cw = &coded[1152..2304];
                        for alpha in [0.6f32, 0.7, 0.8, 0.9, 1.0] {
                            for iters in [50usize, 200] {
                                let mut d = decdsss_fec::ldpc::Decoder::new(code);
                                d.alpha = alpha;
                                let o = d.decode(cw, iters);
                                println!("cw1 alpha {alpha} iters {iters}: {o:?}");
                            }
                        }
                        // Scaled LLRs (min-sum should not care).
                        let big: Vec<f32> = cw.iter().map(|x| x * 40.0).collect();
                        let mut d = decdsss_fec::ldpc::Decoder::new(code);
                        println!("cw1 scaled x40: {:?}", d.decode(&big, 50));
                    }
                    println!("LLR magnitude median {:.1}", {
                        let mut m: Vec<f32> = rx.last_llrs.iter().map(|x| x.abs()).collect();
                        m.sort_by(f32::total_cmp);
                        m[m.len() / 2]
                    });
                }
                let bad: Vec<usize> = r
                    .frames
                    .iter()
                    .enumerate()
                    .filter(|(i, f)| f.as_ref() != Some(&seg.frames[r.first_frame + i]))
                    .map(|(i, _)| i)
                    .collect();
                if !bad.is_empty() {
                    println!(
                        "trial {trial} (offset {:.0}, clock {:.0}): bad frames {bad:?}, iters {:?}, snr {:.1}, freq {:.2}",
                        ch.offset_hz, ch.clock_ppm, r.iterations, r.snr_db, r.freq_hz
                    );
                }
            }
        }
    }

    #[test]
    #[ignore]
    fn debug_blocks() {
        let profile = Profile::W2400;
        let seg = segment(profile, "csk256-3/4", 1, 2);
        let audio = through(&seg, &channel(profile, 0.0));
        let run = to_header(&audio, profile).unwrap();
        let mut rx = PayloadRx::new(&run.header, run.fe.out_rate()).unwrap();
        let mut fe = crate::rx::frontend::Frontend::new(profile, FS);
        let mut rest = Vec::new();
        fe.push(&audio, &mut rest);
        let mut buf = crate::rx::frontend::Buf::default();
        buf.push(&rest);
        println!("start track {:?} fingers {:?}", rx.track, rx.fingers);
        while let Some(r) = rx.step(&buf) {
            println!(
                "block {} ok {:?} iters {:?} snr {:.1} fingers {:?} pos {:.2} freq {:.3} step {:.5}",
                r.index,
                r.frames.iter().map(|f| f.is_some()).collect::<Vec<_>>(),
                r.iterations,
                r.snr_db,
                r.fingers,
                rx.track.pos,
                rx.track.freq,
                rx.track.step
            );
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
