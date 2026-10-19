// made by CasualArclamp
//! The header demodulator: CSK symbols on the call code (one of the alphabets of
//! `tx::HEADER_ALPHABETS`), rake-combined noncoherently, de-interleaved and
//! LDPC-decoded.

use super::despread::{Track, csk_bit_llrs, despread, ln_i0, walsh_correlate, walsh_one};
use super::frontend::Buf;
use super::preamble::PreambleLock;
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

/// Demodulates the header, in CSK alphabet `alphabet`, that starts at
/// `lock.header_start`.
pub fn demod_header(buf: &Buf, lock: &PreambleLock, rate: f64, alphabet: usize) -> HeaderOutcome {
    let (hl, nsym, bits) = (alphabet, header_symbols(alphabet), header_bits(alphabet));
    let fingers = pick_fingers(&lock.profile, 3);
    let max_lag = fingers.iter().map(|f| f.lag).fold(0.0f64, f64::max);
    let total_chips = header_len(alphabet);
    let need = lock.header_start + (total_chips as f64 + max_lag + 6.0) * lock.step + 16.0;
    if need > buf.end() as f64 {
        return HeaderOutcome::NeedMore(need.ceil() as i64);
    }
    let scr = Scrambler::new(CALL_CODE).chips(0, total_chips);
    let main = fingers
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.power.total_cmp(&b.1.power))
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut t = Track {
        pos: lock.header_start,
        step: lock.step,
        freq: lock.freq_hz,
        phase: 0.0,
        rate,
    };
    let mut u = vec![Iq::new(0.0, 0.0); hl];
    // W[symbol][finger] = the `hl` correlations.
    let mut w: Vec<Vec<Vec<Iq>>> = Vec::with_capacity(nsym);
    let mut decisions = Vec::with_capacity(nsym);
    let mut peak_prev: Option<Iq> = None;
    let mut freq_acc = Iq::new(0.0, 0.0);
    let (mut err_int, kp, ki) = (0.0f64, 0.15, 0.01);
    for s in 0..nsym {
        let sc = &scr[s * hl..(s + 1) * hl];
        let mut per_finger = Vec::with_capacity(fingers.len());
        for f in &fingers {
            despread(buf, &t, sc, f.lag, &mut u);
            per_finger.push(walsh_correlate(&u));
        }
        // The decision: the code with the most weighted power.
        let mut power = vec![0.0f32; hl];
        for (f, wf) in fingers.iter().zip(&per_finger) {
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
        decisions.push(m);
        // Early/late on the main finger, with the decided code.
        let lag = fingers[main].lag;
        let mut ue = vec![Iq::new(0.0, 0.0); hl];
        let mut ul = vec![Iq::new(0.0, 0.0); hl];
        despread(buf, &t, sc, lag - 0.25, &mut ue);
        despread(buf, &t, sc, lag + 0.25, &mut ul);
        let (e, l) = (walsh_one(&ue, m).norm(), walsh_one(&ul, m).norm());
        let d = f64::from((l - e) / (l + e).max(1e-30));
        let err = d / 1.33;
        err_int += ki * err;
        t.pos += (kp * err + err_int) * t.step;
        // Frequency from successive peak phases.
        let peak = per_finger[main][m];
        if let Some(p) = peak_prev {
            freq_acc += peak * p.conj();
        }
        peak_prev = Some(peak);
        w.push(per_finger);
        t.advance(hl);
    }
    let t_sym = hl as f64 * t.step / rate;
    let f_res = f64::from(freq_acc.arg()) / (std::f64::consts::TAU * t_sym);
    t.freq += 0.5 * f_res;

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
            pos: lock.header_start,
            step: lock.step,
            freq: lock.freq_hz,
            phase: 0.0,
            rate,
        };
        let cr = rate / super::frontend::SPS as f64;
        let seg = super::refine::segment_chips(cr);
        if let Some(r) = super::refine::refine(buf, &t0, &known, fingers[main].lag, seg, 300e-6) {
            let (f, shift) = super::refine::tail_fingers(buf, &r, &t0, &known, seg);
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
    /// channel; run with
    /// `cargo test -p decdsss-phy --release -- --ignored --nocapture header_alphabet_curve`.
    #[test]
    #[ignore]
    fn header_alphabet_curve() {
        use crate::tx::{HEADER_ALPHABETS, chips_to_audio, header_chips_l, preamble_chips};
        let profile = Profile::W2400;
        let seg = segment(profile, "ds8-1/2", 1, 1);
        let chans: [(&str, Option<Watterson>); 2] =
            [("awgn", None), ("moderate", Some(Watterson::MODERATE))];
        for (name, fading) in chans {
            for &l in &HEADER_ALPHABETS {
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
                        if let Ok(r) = to_header(&heard, profile)
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
