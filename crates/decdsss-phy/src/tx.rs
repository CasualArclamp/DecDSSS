// made by CasualArclamp
//! The transmitter: a segment's chips and the audio they become.
//!
//! Chips (complex, unit power) are built in this order: the preamble (real
//! BPSK, unscrambled), the header (CSK64, 128 or 256 on the call code) and the payload
//! blocks (DS-PSK or CSK on the sender's own code). A scrambler chip
//! (±1 ± j)/√2 multiplies every chip after the preamble; the header's
//! scrambler starts at chip 0 of the header, the payload's at chip 0 of the
//! first block and runs on across blocks.

use crate::codes::{
    CALL_CODE, PRE_A_PERIODS, PRE_B_PERIODS, Scrambler, own_code, preamble_a, preamble_b,
    walsh_chip,
};
use crate::framing::{Header, Layout, frame_bits};
use decdsss_core::mode::PILOT_INTERVAL;
use decdsss_core::{Family, Profile, profile::ROLLOFF};
use decdsss_dsp::Iq;
use decdsss_dsp::shaper::Shaper;
use decdsss_fec::interleave::Interleaver;
use decdsss_fec::ldpc::Code;

/// The long header's CSK alphabet and symbol count: 256 chips a symbol, 8 bits each.
pub const HEADER_CSK: usize = 256;
pub const HEADER_SYMBOLS: usize = 512 / 8;

/// The CSK alphabets a header is sent in (chips and codes to a symbol), shortest
/// first: exactly the ones [`header_alphabet_for`] picks. 64 is the short header
/// (86 symbols, 5504 chips, 2.9 s on `2k4`) and needs the strongest signal; 256 is
/// the long one (16384 chips, 8.5 s) for the weakest modes. The receiver tries
/// them in this order, as soon as each could be in, so it starts as early as the
/// transmitter allows. 16 and 32 were measured and never chosen
/// (`header_alphabet_curve`), so the receiver no longer tries them.
pub const HEADER_ALPHABETS: [usize; 3] = [64, 128, 256];

/// Bits in a header symbol of alphabet `l`.
pub fn header_bits(l: usize) -> usize {
    l.trailing_zeros() as usize
}

/// Symbols in a header of alphabet `l` (the 512 coded bits, the last symbol
/// padded with zeros).
pub fn header_symbols(l: usize) -> usize {
    512usize.div_ceil(header_bits(l))
}

/// Chips in a header of alphabet `l`.
pub fn header_len(l: usize) -> usize {
    header_symbols(l) * l
}

/// The alphabet a transmission in `mode` sends its header in: the shortest that
/// still decodes some way below the payload's own threshold and leaves the payload
/// as well started as the long header does. Measured at 2.4 kHz (`header_alphabet_curve`,
/// ten trials a point; in-band SNR from which the header decoded on the moderate
/// fading channel, AWGN 3-9 dB lower): 16 about +8 dB, 32 -1 dB, 64 about -4 dB
/// (8 of 10, 10 of 10 from +5), 128 -7 dB, 256 -4 dB (and 8 of 10 at -13). The
/// payload, over 256 transmissions a point: DS4 2/3 at 24 dB Eb/N0 lost 1.3 % of its
/// frames with the long header, 0.9 % with 64, 1.2 % with 128 and 2.8 % with 32 (a
/// short header gives the timing loop less to start from); DS8 1/2 at 10 dB 2.3 %,
/// 3.2 %, 3.2 % for 256, 128, 64. So the fast modes use 64 (DS4) and 128 (SF 8, 16);
/// the slow ones keep the 256 they need.
pub fn header_alphabet_for(mode: decdsss_core::Mode) -> usize {
    use decdsss_core::Family;
    match (mode.family, mode.spread) {
        (Family::DsPsk, 0..=4) => 64,
        (Family::DsPsk, 5..=16) => 128,
        _ => HEADER_CSK,
    }
}

/// The known value of the k-th pilot of a block: QPSK from an m-sequence.
pub fn pilot_symbol(k: usize) -> Iq {
    use crate::codes::{POLY7, m_sequence};
    use std::sync::OnceLock;
    static SEQ: OnceLock<Vec<u8>> = OnceLock::new();
    let s = SEQ.get_or_init(|| m_sequence(POLY7));
    let a = s[(2 * k) % s.len()];
    let b = s[(2 * k + 1) % s.len()];
    qpsk(a, b)
}

/// Two bits to a QPSK symbol of unit power; bit 0 maps to +.
pub fn qpsk(b0: u8, b1: u8) -> Iq {
    let k = std::f32::consts::FRAC_1_SQRT_2;
    Iq::new(
        k * (1.0 - 2.0 * f32::from(b0)),
        k * (1.0 - 2.0 * f32::from(b1)),
    )
}

/// The preamble's chips for a profile.
pub fn preamble_chips(profile: Profile) -> Vec<Iq> {
    let (a, b) = (preamble_a(profile), preamble_b(profile));
    let mut v = Vec::new();
    for _ in 0..PRE_A_PERIODS {
        v.extend(a.iter().map(|&c| Iq::new(c, 0.0)));
    }
    for _ in 0..PRE_B_PERIODS {
        v.extend(b.iter().map(|&c| Iq::new(c, 0.0)));
    }
    v
}

/// Chips of the long header: 128 information bits to the 512-bit header
/// codeword (rate 1/4), interleaved, 64 CSK256 symbols on the call code.
pub fn header_chips(h: &Header) -> Option<Vec<Iq>> {
    header_chips_l(h, HEADER_CSK)
}

/// Chips of the header in CSK alphabet `l` (one of [`HEADER_ALPHABETS`], or any
/// power of two from 16 for a measurement): the same codeword, `log2 l` bits to a
/// symbol.
pub fn header_chips_l(h: &Header, l: usize) -> Option<Vec<Iq>> {
    let info = h.pack()?;
    let cw = Code::header().encode(&info);
    let sent = Interleaver::new(cw.len()).apply(&cw);
    let scr = Scrambler::new(CALL_CODE);
    let mut chips = Vec::with_capacity(header_len(l));
    for (s, bits) in sent.chunks(header_bits(l)).enumerate() {
        // The last symbol may have fewer bits: pad with zeros.
        let m = (0..header_bits(l)).fold(0usize, |a, i| {
            (a << 1) | usize::from(bits.get(i).copied().unwrap_or(0))
        });
        for i in 0..l {
            chips.push(scr.chip(s * l + i) * walsh_chip(m, i));
        }
    }
    Some(chips)
}

/// The coded bits of one block, interleaved, in transmit order: `depth`
/// codewords from `depth` frames (a short last block is padded with zero
/// frames).
pub fn block_bits(layout: &Layout, frames: &[Vec<u8>]) -> Vec<u8> {
    let code = layout.code();
    let zero = vec![0u8; layout.frame_bytes()];
    let mut coded = Vec::with_capacity(layout.coded_bits());
    for f in 0..layout.depth {
        let data = frames.get(f).unwrap_or(&zero);
        assert_eq!(data.len(), layout.frame_bytes(), "frame has the wrong size");
        coded.extend(code.encode(&frame_bits(data)));
    }
    Interleaver::new(coded.len()).apply(&coded)
}

/// The block's symbols from its transmit-order bits: for DS-PSK QPSK data
/// symbols with pilots; for CSK the Walsh code numbers. Short groups are
/// padded with zero bits.
pub fn block_symbols(layout: &Layout, bits: &[u8]) -> Vec<Iq> {
    assert_eq!(layout.mode.family, Family::DsPsk);
    let mut out = Vec::with_capacity(layout.symbols());
    let mut it = bits.iter().copied().chain(std::iter::repeat(0));
    let mut pilot = 0;
    for n in 0..layout.symbols() {
        if layout.is_pilot(n) {
            out.push(pilot_symbol(pilot));
            pilot += 1;
        } else {
            let (b0, b1) = (it.next().unwrap(), it.next().unwrap());
            out.push(qpsk(b0, b1));
        }
    }
    debug_assert_eq!(PILOT_INTERVAL, 8);
    out
}

/// The Walsh code numbers of a CSK block.
pub fn block_walsh(layout: &Layout, bits: &[u8]) -> Vec<usize> {
    assert_eq!(layout.mode.family, Family::Csk);
    let m = layout.bits_per_data_symbol();
    let mut it = bits.iter().copied().chain(std::iter::repeat(0));
    (0..layout.symbols())
        .map(|_| (0..m).fold(0usize, |a, _| (a << 1) | usize::from(it.next().unwrap())))
        .collect()
}

/// Chips of a block starting at scrambler chip `chip0`.
pub fn block_chips(layout: &Layout, frames: &[Vec<u8>], scr: &Scrambler, chip0: usize) -> Vec<Iq> {
    let bits = block_bits(layout, frames);
    let l = usize::from(layout.mode.spread);
    let mut chips = Vec::with_capacity(layout.chips());
    match layout.mode.family {
        Family::DsPsk => {
            for (n, sym) in block_symbols(layout, &bits).into_iter().enumerate() {
                for i in 0..l {
                    chips.push(scr.chip(chip0 + n * l + i) * sym);
                }
            }
        }
        Family::Csk => {
            for (n, m) in block_walsh(layout, &bits).into_iter().enumerate() {
                for i in 0..l {
                    chips.push(scr.chip(chip0 + n * l + i) * walsh_chip(m, i));
                }
            }
        }
    }
    chips
}

/// A segment to send.
#[derive(Clone, Debug)]
pub struct Segment {
    pub header: Header,
    /// Frames of `Layout::frame_bytes` data bytes each, `nblocks × depth` of
    /// them at most (fewer: the last block is padded).
    pub frames: Vec<Vec<u8>>,
}

/// All chips of a segment: preamble, header, payload blocks. `None` if the
/// header cannot be packed.
pub fn segment_chips(seg: &Segment) -> Option<Vec<Iq>> {
    let h = &seg.header;
    let layout = Layout::new(h.profile, h.mode, h.depth);
    let mut chips = preamble_chips(h.profile);
    chips.extend(header_chips_l(h, header_alphabet_for(h.mode))?);
    let scr = Scrambler::new(own_code(&h.call)?);
    let mut chip0 = 0;
    for b in 0..usize::from(h.nblocks) {
        let from = (b * h.depth).min(seg.frames.len());
        let to = ((b + 1) * h.depth).min(seg.frames.len());
        let block = block_chips(&layout, &seg.frames[from..to], &scr, chip0);
        chip0 += block.len();
        chips.extend(block);
    }
    Some(chips)
}

/// The end-of-transmission postamble for the station that sent `seg` as its last
/// segment: its scrambler on from the last block, times the B sequence (see
/// [`crate::eot`]). `None` if the callsign has no code.
pub fn eot_chips(seg: &Segment) -> Option<Vec<Iq>> {
    let h = &seg.header;
    let layout = Layout::new(h.profile, h.mode, h.depth);
    let scr = Scrambler::new(own_code(&h.call)?);
    Some(crate::eot::chips(
        h.profile,
        &scr,
        usize::from(h.nblocks) * layout.chips(),
    ))
}

/// All the chips of a transmission: every segment back to back, then the
/// postamble that tells a receiver it is over. `None` if a header cannot be packed
/// or there are no segments.
pub fn transmission_chips(segs: &[Segment]) -> Option<Vec<Iq>> {
    let mut chips = Vec::new();
    for s in segs {
        chips.extend(segment_chips(s)?);
    }
    chips.extend(eot_chips(segs.last()?)?);
    Some(chips)
}

/// A lead for a radio that keys on VOX: a 1 kHz tone of `tone_s` seconds at RMS
/// `level`, then `gap_s` of silence, for the audio to follow.
pub fn vox_lead(rate: f64, level: f32, tone_s: f64, gap_s: f64) -> Vec<f32> {
    let n = (tone_s * rate) as usize;
    let amp = level * std::f32::consts::SQRT_2;
    let mut v: Vec<f32> = (0..n)
        .map(|i| {
            // Short ramps so the burst does not click.
            let ramp = (i.min(n - i) as f32 / (0.01 * rate as f32)).min(1.0);
            amp * ramp * (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin()
        })
        .collect();
    v.extend(std::iter::repeat_n(0.0, (gap_s * rate) as usize));
    v
}

/// A transmission built a block at a time, for live voice: the preamble and the
/// header first, then each block as its frames are ready. The pulse shaper carries
/// on from one call to the next, so the audio is continuous and is exactly what
/// [`chips_to_audio`] makes of all the chips at once.
pub struct LiveTx {
    header: Header,
    layout: Layout,
    scr: Scrambler,
    chip0: usize,
    shaper: Shaper,
}

impl LiveTx {
    /// `None` if the header cannot be packed or the callsign has no code.
    pub fn new(header: Header, sample_rate: f64, level: f32) -> Option<LiveTx> {
        header.pack()?;
        let layout = Layout::new(header.profile, header.mode, header.depth);
        let scr = Scrambler::new(own_code(&header.call)?);
        let shaper = Shaper::new(
            f64::from(header.profile.chip_rate()),
            sample_rate,
            header.profile.centre_hz(),
            ROLLOFF,
            10,
            level,
        );
        Some(LiveTx {
            header,
            layout,
            scr,
            chip0: 0,
            shaper,
        })
    }

    /// The audio of the preamble and the header, to be sent first.
    pub fn lead(&mut self) -> Vec<f32> {
        let mut chips = preamble_chips(self.header.profile);
        chips.extend(
            header_chips_l(&self.header, header_alphabet_for(self.header.mode))
                .expect("the header packed in new()"),
        );
        let mut audio = Vec::new();
        self.shaper.push(&chips, &mut audio);
        audio
    }

    /// The audio of the next block from `frames` (at most `depth`; fewer are
    /// padded with zero frames).
    pub fn block(&mut self, frames: &[Vec<u8>]) -> Vec<f32> {
        let chips = block_chips(&self.layout, frames, &self.scr, self.chip0);
        self.chip0 += chips.len();
        let mut audio = Vec::new();
        self.shaper.push(&chips, &mut audio);
        audio
    }

    /// The postamble, to be sent after the last block when the talker lets go
    /// (not when a transmission is cut short): it tells the receiver the stream is
    /// over without it having to wait for blocks that do not come.
    pub fn eot(&mut self) -> Vec<f32> {
        let chips = crate::eot::chips(self.header.profile, &self.scr, self.chip0);
        self.chip0 += chips.len();
        let mut audio = Vec::new();
        self.shaper.push(&chips, &mut audio);
        audio
    }

    /// The end of the shaped signal (the filter's tail), after the last block.
    pub fn finish(&mut self) -> Vec<f32> {
        let mut audio = Vec::new();
        self.shaper.flush(&mut audio);
        audio
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Seconds of signal in a block.
    pub fn block_seconds(&self) -> f64 {
        self.layout.seconds()
    }

    /// Seconds of signal before the first block (preamble and header).
    pub fn lead_seconds(&self) -> f64 {
        lead_chips_for(self.header.profile, self.header.mode) as f64
            / f64::from(self.header.profile.chip_rate())
    }
}

/// Chips up to the first payload block with the long header: preamble plus header.
pub fn lead_chips(profile: Profile) -> usize {
    lead_chips_with(profile, HEADER_CSK)
}

/// Chips up to the first payload block of a transmission in `mode`.
pub fn lead_chips_for(profile: Profile, mode: decdsss_core::Mode) -> usize {
    lead_chips_with(profile, header_alphabet_for(mode))
}

/// Chips up to the first payload block with a header of alphabet `l`.
pub fn lead_chips_with(profile: Profile, l: usize) -> usize {
    (PRE_A_PERIODS + PRE_B_PERIODS) * crate::codes::preamble_len(profile) + header_len(l)
}

/// Chips to audio at `sample_rate`: RRC-shaped, on the profile's audio
/// centre, `level` the RMS of the output. `lead_s` and `tail_s` seconds of
/// silence go before and after.
pub fn chips_to_audio(
    chips: &[Iq],
    profile: Profile,
    sample_rate: f64,
    level: f32,
    lead_s: f64,
    tail_s: f64,
) -> Vec<f32> {
    let mut shaper = Shaper::new(
        f64::from(profile.chip_rate()),
        sample_rate,
        profile.centre_hz(),
        ROLLOFF,
        10,
        level,
    );
    let mut audio = vec![0.0; (lead_s * sample_rate) as usize];
    shaper.push(chips, &mut audio);
    shaper.flush(&mut audio);
    audio.extend(std::iter::repeat_n(0.0, (tail_s * sample_rate) as usize));
    audio
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::PayloadType;
    use decdsss_core::Mode;

    fn seg(mode: &str, depth: usize, nblocks: u16) -> Segment {
        let header = Header {
            profile: Profile::W2400,
            mode: Mode::from_name(mode).unwrap(),
            depth,
            ptype: PayloadType::Test,
            call: "VK2ABC".into(),
            session: 1,
            frame0: 0,
            nblocks,
            total_frames: 0,
        };
        let layout = Layout::new(header.profile, header.mode, depth);
        let frames = (0..usize::from(nblocks) * depth)
            .map(|i| {
                (0..layout.frame_bytes())
                    .map(|b| (i * 7 + b) as u8)
                    .collect()
            })
            .collect();
        Segment { header, frames }
    }

    #[test]
    fn a_live_transmission_is_the_audio_of_the_whole_segment() {
        let s = seg("ds4-2/3", 2, 3);
        let whole = chips_to_audio(
            &segment_chips(&s).unwrap(),
            Profile::W2400,
            48_000.0,
            0.2,
            0.0,
            0.0,
        );
        let mut live = LiveTx::new(s.header.clone(), 48_000.0, 0.2).unwrap();
        let mut audio = live.lead();
        for b in s.frames.chunks(2) {
            audio.extend(live.block(b));
        }
        audio.extend(live.finish());
        assert_eq!(audio.len(), whole.len());
        let worst = audio
            .iter()
            .zip(&whole)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "{worst}");
        // 14 preamble periods of 511 chips and a 64-code header (86 symbols).
        assert!((live.lead_seconds() - (7154.0 + 5504.0) / 1920.0).abs() < 0.02);
        assert!(
            (live.block_seconds() - 2.75).abs() < 0.05,
            "{}",
            live.block_seconds()
        );
    }

    #[test]
    fn segment_length_follows_the_layout() {
        for mode in ["ds4-1/2", "ds64-3/4", "csk32-2/3", "csk256-1/2"] {
            let s = seg(mode, 2, 3);
            let layout = Layout::new(s.header.profile, s.header.mode, 2);
            let chips = segment_chips(&s).unwrap();
            assert_eq!(
                chips.len(),
                lead_chips_for(Profile::W2400, s.header.mode) + 3 * layout.chips(),
                "{mode}"
            );
        }
    }

    #[test]
    fn the_receiver_tries_exactly_the_alphabets_that_are_sent() {
        let sent: std::collections::BTreeSet<usize> =
            Mode::all().into_iter().map(header_alphabet_for).collect();
        let tried: std::collections::BTreeSet<usize> = HEADER_ALPHABETS.into_iter().collect();
        assert_eq!(sent, tried);
        assert!(HEADER_ALPHABETS.is_sorted(), "shortest first");
    }

    #[test]
    fn header_symbol_count_matches_the_code() {
        assert_eq!(HEADER_SYMBOLS * 8, Code::header().n());
    }

    #[test]
    fn chips_have_unit_power() {
        let chips = segment_chips(&seg("ds8-1/2", 1, 2)).unwrap();
        let p = chips.iter().map(|c| c.norm_sqr()).sum::<f32>() / chips.len() as f32;
        assert!((p - 1.0).abs() < 1e-3, "{p}");
        let chips = segment_chips(&seg("csk64-1/2", 1, 2)).unwrap();
        let p = chips.iter().map(|c| c.norm_sqr()).sum::<f32>() / chips.len() as f32;
        assert!((p - 1.0).abs() < 1e-3, "{p}");
    }

    #[test]
    fn pilots_sit_where_the_layout_says() {
        let s = seg("ds4-1/2", 1, 1);
        let layout = Layout::new(s.header.profile, s.header.mode, 1);
        let scr = Scrambler::new(own_code("VK2ABC").unwrap());
        let chips = block_chips(&layout, &s.frames, &scr, 0);
        // Despread the first symbol by hand: it is pilot 0.
        let y: Iq = (0..4).map(|i| chips[i] * scr.chip(i).conj()).sum::<Iq>() / 4.0;
        assert!((y - pilot_symbol(0)).norm() < 1e-5);
    }

    #[test]
    fn audio_has_the_requested_level_and_band() {
        let chips = segment_chips(&seg("csk64-1/2", 1, 1)).unwrap();
        let a = chips_to_audio(&chips, Profile::W2400, 48_000.0, 0.2, 0.0, 0.0);
        let mid = &a[a.len() / 4..a.len() * 3 / 4];
        let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.2).abs() < 0.02, "rms {rms}");
        assert!(a.iter().all(|v| v.abs() < 1.0));
    }
}
