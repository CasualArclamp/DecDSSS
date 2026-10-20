// made by CasualArclamp
//! Over framing: the header's bit layout and the geometry of a payload block
//! (how many symbols and chips a block takes, where the pilots sit).
//!
//! A *segment* is a preamble, a header and `nblocks` payload blocks. A
//! transmission is one or more segments (resync points repeat the preamble
//! and header so a receiver can join late). A *block* is `depth` LDPC
//! codewords interleaved together; a *frame* is one codeword's information.

use crate::codes::{CALL_BITS, pack_call, unpack_call};
use decdsss_core::mode::PILOT_INTERVAL;
use decdsss_core::{CodeRate, Family, Mode, Profile};
use decdsss_fec::crc::{bits_of, bytes_of, crc16, crc32};
use decdsss_fec::ldpc::Code;

/// Header information bits before the CRC-16 (the header code's k is 128).
pub const HEADER_INFO_BITS: usize = 112;
/// Header version this build writes for the payload types whose code fits in 3
/// bits, and reads.
pub const VERSION: u8 = 0;
/// The version for the payload types whose code needs 4 bits (8 and up): the type
/// field is one bit wider and the session one bit narrower (5 bits, 0 to 31). A
/// build that predates it skips such a header (its version is not 0), which is all
/// it could do with a type it does not know.
pub const VERSION_WIDE: u8 = 1;

/// Codeword depths a block can have (codewords interleaved together).
pub const DEPTHS: [usize; 4] = [1, 2, 4, 8];

/// What a transmission carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadType {
    /// Raw test data (the simulator and the CLI's `--raw`).
    Test = 0,
    File = 1,
    Picture = 2,
    /// Codec2 700C speech (see `decdsss-link`'s voice module).
    Voice = 3,
    /// Codec2 450 speech.
    Voice450 = 4,
    /// A text message (see `decdsss-link`'s text module).
    Text = 5,
    /// A CQ call: the header is the whole message (see `decdsss-link`'s cq module),
    /// so there are no payload blocks.
    Cq = 6,
    /// Codec2 1200 speech, for the fast modes (DS2 and DS1).
    Voice1200 = 7,
    /// Codec2 1600 speech. The first type with a code over 7, so its header is
    /// version 1 ([`VERSION_WIDE`]).
    Voice1600 = 8,
}

impl PayloadType {
    /// Every type, in code order.
    pub const ALL: [PayloadType; 9] = [
        PayloadType::Test,
        PayloadType::File,
        PayloadType::Picture,
        PayloadType::Voice,
        PayloadType::Voice450,
        PayloadType::Text,
        PayloadType::Cq,
        PayloadType::Voice1200,
        PayloadType::Voice1600,
    ];

    fn from_u8(v: u8) -> Option<PayloadType> {
        PayloadType::ALL.get(usize::from(v)).copied()
    }

    /// Speech, whichever codec.
    pub fn is_voice(self) -> bool {
        matches!(
            self,
            PayloadType::Voice
                | PayloadType::Voice450
                | PayloadType::Voice1200
                | PayloadType::Voice1600
        )
    }

    /// The header version that carries this type: 0, or [`VERSION_WIDE`] when its
    /// code does not fit in 3 bits.
    pub fn header_version(self) -> u8 {
        if (self as u8) < 8 {
            VERSION
        } else {
            VERSION_WIDE
        }
    }

    /// Bits of the header's session field next to this type: 6, or 5 in a wide header.
    pub fn session_bits(self) -> usize {
        if self.header_version() == VERSION {
            6
        } else {
            5
        }
    }

    /// `session` cut down to what this type's header can hold (a session number
    /// only tells one transmission from the next, so the low bits do).
    pub fn fit_session(self, session: u8) -> u8 {
        session & ((1u8 << self.session_bits()) - 1)
    }
}

/// The header of a segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub profile: Profile,
    pub mode: Mode,
    /// Codewords per block: one of `DEPTHS`.
    pub depth: usize,
    pub ptype: PayloadType,
    /// The sender's callsign, normalised (which also names its own code).
    pub call: String,
    /// 6 bits: tells one transmission from another.
    pub session: u8,
    /// Number of the first frame in this segment.
    pub frame0: u16,
    /// Blocks in this segment (12 bits).
    pub nblocks: u16,
    /// Frames in the whole transmission, 0 if unknown (15 bits).
    pub total_frames: u16,
}

/// Little helper: pushes `n` bits of `v`, most significant first.
struct BitWriter(Vec<u8>);

impl BitWriter {
    fn put(&mut self, v: u64, n: usize) {
        for i in (0..n).rev() {
            self.0.push(((v >> i) & 1) as u8);
        }
    }
}

struct BitReader<'a> {
    bits: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn get(&mut self, n: usize) -> u64 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bits[self.pos]);
            self.pos += 1;
        }
        v
    }
}

fn family_bit(f: Family) -> u64 {
    match f {
        Family::DsPsk => 0,
        Family::Csk => 1,
    }
}

fn profile_index(p: Profile) -> u64 {
    Profile::ALL.iter().position(|&q| q == p).unwrap() as u64
}

/// The spreading factors by their 3-bit code in the header. The first five
/// are the original ladder's order, kept in place so a header an older build
/// sent still reads the same; SF 2 and 1 took the two codes that were free. (The
/// ladder in `decdsss_core::mode` is in speed order, which is not this one.)
const DS_WIRE: [u16; 7] = [4, 8, 16, 32, 64, 2, 1];

/// The spreading factors or alphabet sizes of a family, in header-code order.
fn wire_ladder(f: Family) -> &'static [u16] {
    match f {
        Family::DsPsk => &DS_WIRE,
        Family::Csk => &decdsss_core::mode::CSK_SIZES,
    }
}

fn spread_index(m: Mode) -> u64 {
    wire_ladder(m.family)
        .iter()
        .position(|&s| s == m.spread)
        .unwrap() as u64
}

impl Header {
    /// The 128 information bits of the header codeword: 112 fields then
    /// CRC-16 over them. `None` if the callsign cannot be packed or a field
    /// is out of range.
    pub fn pack(&self) -> Option<Vec<u8>> {
        let depth_idx = DEPTHS.iter().position(|&d| d == self.depth)?;
        let session_bits = self.ptype.session_bits();
        if u32::from(self.session) >= 1 << session_bits
            || self.nblocks >= 1 << 12
            || self.total_frames >= 1 << 15
        {
            return None;
        }
        let version = self.ptype.header_version();
        let mut w = BitWriter(Vec::with_capacity(128));
        w.put(u64::from(version), 2);
        w.put(profile_index(self.profile), 2);
        w.put(family_bit(self.mode.family), 1);
        w.put(spread_index(self.mode), 3);
        w.put(
            CodeRate::ALL.iter().position(|&r| r == self.mode.rate)? as u64,
            2,
        );
        w.put(depth_idx as u64, 2);
        w.put(self.ptype as u64, 3 + usize::from(version));
        w.put(pack_call(&self.call)?, CALL_BITS as usize);
        w.put(u64::from(self.session), session_bits);
        w.put(u64::from(self.frame0), 16);
        w.put(u64::from(self.nblocks), 12);
        w.put(u64::from(self.total_frames), 15);
        debug_assert_eq!(w.0.len(), HEADER_INFO_BITS);
        let crc = crc16(&bytes_of(&w.0));
        w.put(u64::from(crc), 16);
        Some(w.0)
    }

    /// Reads 128 information bits; `None` if the CRC fails or a field is not
    /// a valid value.
    pub fn unpack(bits: &[u8]) -> Option<Header> {
        if bits.len() != HEADER_INFO_BITS + 16 {
            return None;
        }
        let crc = crc16(&bytes_of(&bits[..HEADER_INFO_BITS]));
        let mut r = BitReader { bits, pos: 0 };
        let version = r.get(2) as u8;
        if version != VERSION && version != VERSION_WIDE {
            return None;
        }
        let profile = *Profile::ALL.get(r.get(2) as usize)?;
        let family = if r.get(1) == 0 {
            Family::DsPsk
        } else {
            Family::Csk
        };
        let spread = *wire_ladder(family).get(r.get(3) as usize)?;
        let rate = *CodeRate::ALL.get(r.get(2) as usize)?;
        let mode = Mode::new(family, spread, rate)?;
        let depth = *DEPTHS.get(r.get(2) as usize)?;
        let ptype = PayloadType::from_u8(r.get(3 + usize::from(version)) as u8)?;
        // One spelling per type: a wide header for a type that fits in 3 bits (or the
        // other way round) is not something this build writes.
        if ptype.header_version() != version {
            return None;
        }
        let call = unpack_call(r.get(CALL_BITS as usize))?;
        let session = r.get(ptype.session_bits()) as u8;
        let frame0 = r.get(16) as u16;
        let nblocks = r.get(12) as u16;
        let total_frames = r.get(15) as u16;
        let sent_crc = r.get(16) as u16;
        if sent_crc != crc {
            return None;
        }
        Some(Header {
            profile,
            mode,
            depth,
            ptype,
            call,
            session,
            frame0,
            nblocks,
            total_frames,
        })
    }
}

/// The geometry of a payload block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub profile: Profile,
    pub mode: Mode,
    pub depth: usize,
}

/// Bits of the codeword a modem frame carries.
const N: usize = 1152;

impl Layout {
    pub fn new(profile: Profile, mode: Mode, depth: usize) -> Layout {
        Layout {
            profile,
            mode,
            depth,
        }
    }

    pub fn code(&self) -> &'static Code {
        Code::payload(self.mode.rate)
    }

    /// Coded bits in a block (before padding to whole symbols).
    pub fn coded_bits(&self) -> usize {
        self.depth * N
    }

    /// Data bytes a frame carries: the codeword's k bits less the CRC-32.
    pub fn frame_bytes(&self) -> usize {
        self.code().k() / 8 - 4
    }

    /// Coded bits one data symbol carries: 2 for DS-PSK (QPSK), log2 L for CSK.
    pub fn bits_per_data_symbol(&self) -> usize {
        match self.mode.family {
            Family::DsPsk => 2,
            Family::Csk => self.mode.spread.trailing_zeros() as usize,
        }
    }

    /// Data symbols in a block, padded up so that DS-PSK fills whole pilot
    /// groups.
    pub fn data_symbols(&self) -> usize {
        let per_group = (PILOT_INTERVAL as usize - 1) * self.bits_per_data_symbol();
        match self.mode.family {
            Family::DsPsk => self.coded_bits().div_ceil(per_group) * (PILOT_INTERVAL as usize - 1),
            Family::Csk => self.coded_bits().div_ceil(self.bits_per_data_symbol()),
        }
    }

    /// Symbols in a block: the data symbols plus, for DS-PSK, a pilot ahead
    /// of every group of seven and one closing pilot.
    pub fn symbols(&self) -> usize {
        match self.mode.family {
            Family::DsPsk => {
                let groups = self.data_symbols() / (PILOT_INTERVAL as usize - 1);
                groups * PILOT_INTERVAL as usize + 1
            }
            Family::Csk => self.data_symbols(),
        }
    }

    pub fn chips(&self) -> usize {
        self.symbols() * usize::from(self.mode.spread)
    }

    /// Block duration, seconds.
    pub fn seconds(&self) -> f64 {
        self.chips() as f64 / f64::from(self.profile.chip_rate())
    }

    /// Is symbol `n` of a DS-PSK block a pilot?
    pub fn is_pilot(&self, n: usize) -> bool {
        n.is_multiple_of(PILOT_INTERVAL as usize) || n == self.symbols() - 1
    }
}

/// The frame's information bits: its data bytes then their CRC-32.
pub fn frame_bits(data: &[u8]) -> Vec<u8> {
    let mut b = data.to_vec();
    b.extend_from_slice(&crc32(data).to_be_bytes());
    bits_of(&b)
}

/// Checks a decoded frame's information bits; the data bytes if the CRC-32
/// holds.
pub fn check_frame(info: &[u8]) -> Option<Vec<u8>> {
    let bytes = bytes_of(info);
    let (data, crc) = bytes.split_at(bytes.len() - 4);
    (crc32(data).to_be_bytes() == crc).then(|| data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            profile: Profile::W2400,
            mode: Mode::from_name("csk64-2/3").unwrap(),
            depth: 4,
            ptype: PayloadType::File,
            call: "VK2ABC/P".into(),
            session: 37,
            frame0: 1234,
            nblocks: 99,
            total_frames: 20_000,
        }
    }

    #[test]
    fn spreading_factors_keep_their_header_codes() {
        // The 3-bit codes of the original ladder are what older builds sent; SF 2
        // and 1 took the two that were free.
        assert_eq!(DS_WIRE[..5], [4, 8, 16, 32, 64]);
        assert_eq!(DS_WIRE[5..], [2, 1]);
        let mut codes: Vec<u16> = DS_WIRE.to_vec();
        codes.sort_unstable();
        let mut ladder = decdsss_core::mode::DS_SPREADS.to_vec();
        ladder.sort_unstable();
        assert_eq!(
            codes, ladder,
            "the header can name every mode on the ladder"
        );
    }

    #[test]
    fn header_round_trips_for_every_mode() {
        for profile in Profile::ALL {
            for mode in Mode::all() {
                for depth in DEPTHS {
                    let h = Header {
                        profile,
                        mode,
                        depth,
                        ..header()
                    };
                    let bits = h.pack().unwrap();
                    assert_eq!(bits.len(), 128);
                    assert_eq!(Header::unpack(&bits), Some(h));
                }
            }
        }
    }

    #[test]
    fn every_payload_type_round_trips_in_the_header_that_carries_it() {
        for ptype in PayloadType::ALL {
            let top = (1u8 << ptype.session_bits()) - 1;
            for session in [0, 1, top] {
                let h = Header {
                    ptype,
                    session,
                    ..header()
                };
                let bits = h.pack().unwrap();
                // The version sits in the first two bits.
                assert_eq!(bits[0] == 0 && bits[1] == 1, ptype as u8 >= 8, "{ptype:?}");
                assert_eq!(
                    Header::unpack(&bits),
                    Some(h),
                    "{ptype:?} session {session}"
                );
            }
            // One past the session the type's header can hold.
            let h = Header {
                ptype,
                session: top + 1,
                ..header()
            };
            assert_eq!(h.pack(), None, "{ptype:?}");
        }
        // The codes of the first seven are the ones older builds wrote.
        for (code, ptype) in PayloadType::ALL.iter().take(7).enumerate() {
            assert_eq!(*ptype as usize, code);
        }
    }

    #[test]
    fn header_crc_catches_every_single_bit_error() {
        let bits = header().pack().unwrap();
        for i in 0..bits.len() {
            let mut b = bits.clone();
            b[i] ^= 1;
            assert_eq!(Header::unpack(&b), None, "bit {i}");
        }
    }

    #[test]
    fn out_of_range_fields_are_refused() {
        assert_eq!(
            Header {
                session: 64,
                ..header()
            }
            .pack(),
            None
        );
        assert_eq!(
            Header {
                depth: 3,
                ..header()
            }
            .pack(),
            None
        );
        assert_eq!(
            Header {
                call: "NOT A CALL".into(),
                ..header()
            }
            .pack(),
            None
        );
    }

    #[test]
    fn block_geometry() {
        let ds = Layout::new(Profile::W2400, Mode::from_name("ds4-1/2").unwrap(), 1);
        // 1152 bits / 14 per group = 82.3 → 83 groups of 8, plus a closing pilot.
        assert_eq!(ds.data_symbols(), 83 * 7);
        assert_eq!(ds.symbols(), 83 * 8 + 1);
        assert!(ds.is_pilot(0) && ds.is_pilot(8) && ds.is_pilot(ds.symbols() - 1));
        assert!(!ds.is_pilot(1) && !ds.is_pilot(7));
        assert_eq!(ds.frame_bytes(), 576 / 8 - 4);
        let csk = Layout::new(Profile::W2400, Mode::from_name("csk256-1/2").unwrap(), 2);
        assert_eq!(csk.symbols(), 2 * 1152 / 8);
        assert_eq!(csk.chips(), 288 * 256);
        // 288 symbols at 7.5/s.
        assert!((csk.seconds() - 38.4).abs() < 1e-9);
    }

    #[test]
    fn frames_round_trip_and_catch_corruption() {
        let data: Vec<u8> = (0..68).collect();
        let bits = frame_bits(&data);
        assert_eq!(bits.len(), 576);
        assert_eq!(check_frame(&bits), Some(data));
        let mut bad = bits;
        bad[100] ^= 1;
        assert_eq!(check_frame(&bad), None);
    }
}
