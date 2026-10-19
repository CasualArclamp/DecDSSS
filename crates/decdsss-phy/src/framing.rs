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
/// Header version this build writes and reads.
pub const VERSION: u8 = 0;

/// Codeword depths a block can have (codewords interleaved together).
pub const DEPTHS: [usize; 4] = [1, 2, 4, 8];

/// What a transmission carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadType {
    /// Raw test data (the simulator and the CLI's `--raw`).
    Test = 0,
    File = 1,
    Picture = 2,
    Voice = 3,
}

impl PayloadType {
    fn from_u8(v: u8) -> Option<PayloadType> {
        match v {
            0 => Some(PayloadType::Test),
            1 => Some(PayloadType::File),
            2 => Some(PayloadType::Picture),
            3 => Some(PayloadType::Voice),
            _ => None,
        }
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

fn spread_index(m: Mode) -> u64 {
    let ladder: &[u16] = match m.family {
        Family::DsPsk => &decdsss_core::mode::DS_SPREADS,
        Family::Csk => &decdsss_core::mode::CSK_SIZES,
    };
    ladder.iter().position(|&s| s == m.spread).unwrap() as u64
}

impl Header {
    /// The 128 information bits of the header codeword: 112 fields then
    /// CRC-16 over them. `None` if the callsign cannot be packed or a field
    /// is out of range.
    pub fn pack(&self) -> Option<Vec<u8>> {
        let depth_idx = DEPTHS.iter().position(|&d| d == self.depth)?;
        if self.session >= 64 || self.nblocks >= 1 << 12 || self.total_frames >= 1 << 15 {
            return None;
        }
        let mut w = BitWriter(Vec::with_capacity(128));
        w.put(u64::from(VERSION), 2);
        w.put(profile_index(self.profile), 2);
        w.put(family_bit(self.mode.family), 1);
        w.put(spread_index(self.mode), 3);
        w.put(
            CodeRate::ALL.iter().position(|&r| r == self.mode.rate)? as u64,
            2,
        );
        w.put(depth_idx as u64, 2);
        w.put(self.ptype as u64, 3);
        w.put(pack_call(&self.call)?, CALL_BITS as usize);
        w.put(u64::from(self.session), 6);
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
        if r.get(2) != u64::from(VERSION) {
            return None;
        }
        let profile = *Profile::ALL.get(r.get(2) as usize)?;
        let family = if r.get(1) == 0 {
            Family::DsPsk
        } else {
            Family::Csk
        };
        let ladder: &[u16] = match family {
            Family::DsPsk => &decdsss_core::mode::DS_SPREADS,
            Family::Csk => &decdsss_core::mode::CSK_SIZES,
        };
        let spread = *ladder.get(r.get(3) as usize)?;
        let rate = *CodeRate::ALL.get(r.get(2) as usize)?;
        let mode = Mode::new(family, spread, rate)?;
        let depth = *DEPTHS.get(r.get(2) as usize)?;
        let ptype = PayloadType::from_u8(r.get(3) as u8)?;
        let call = unpack_call(r.get(CALL_BITS as usize))?;
        let session = r.get(6) as u8;
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
