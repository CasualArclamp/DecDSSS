// made by CasualArclamp
//! The mode ladder: a spreading family, its spreading factor (DS-PSK) or
//! alphabet size (CSK), and an LDPC code rate.

use crate::Profile;
use std::fmt;

/// DS-PSK carries one known pilot symbol in every `PILOT_INTERVAL`.
pub const PILOT_INTERVAL: u32 = 8;

/// Spreading factors offered for DS-PSK.
pub const DS_SPREADS: [u16; 5] = [4, 8, 16, 32, 64];

/// Alphabet sizes (= chips a symbol) offered for CSK.
pub const CSK_SIZES: [u16; 4] = [32, 64, 128, 256];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    /// Direct sequence: a QPSK symbol held for SF chips, coherent, pilot-aided.
    DsPsk,
    /// Code-shift keying: one of L orthogonal Walsh codes of L chips, log2(L)
    /// bits a symbol, noncoherent.
    Csk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CodeRate {
    R1_2,
    R2_3,
    R3_4,
}

impl CodeRate {
    pub const ALL: [CodeRate; 3] = [CodeRate::R1_2, CodeRate::R2_3, CodeRate::R3_4];

    /// (numerator, denominator).
    pub const fn ratio(self) -> (u32, u32) {
        match self {
            CodeRate::R1_2 => (1, 2),
            CodeRate::R2_3 => (2, 3),
            CodeRate::R3_4 => (3, 4),
        }
    }

    pub fn value(self) -> f64 {
        let (n, d) = self.ratio();
        f64::from(n) / f64::from(d)
    }

    pub fn from_name(s: &str) -> Option<CodeRate> {
        match s.trim() {
            "1/2" => Some(CodeRate::R1_2),
            "2/3" => Some(CodeRate::R2_3),
            "3/4" => Some(CodeRate::R3_4),
            _ => None,
        }
    }
}

impl fmt::Display for CodeRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (n, d) = self.ratio();
        write!(f, "{n}/{d}")
    }
}

/// A mode. Only the spreads in `DS_SPREADS` / `CSK_SIZES` are valid; build
/// one with [`Mode::new`] to have that checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mode {
    pub family: Family,
    /// Spreading factor (DS-PSK) or alphabet size L (CSK), in chips a symbol.
    pub spread: u16,
    pub rate: CodeRate,
}

impl Mode {
    /// `None` if `spread` is not on the family's ladder.
    pub fn new(family: Family, spread: u16, rate: CodeRate) -> Option<Mode> {
        let ladder: &[u16] = match family {
            Family::DsPsk => &DS_SPREADS,
            Family::Csk => &CSK_SIZES,
        };
        ladder.contains(&spread).then_some(Mode {
            family,
            spread,
            rate,
        })
    }

    /// Every valid mode, DS-PSK first, fastest first within a family.
    pub fn all() -> Vec<Mode> {
        let mut v = Vec::new();
        for (family, ladder) in [
            (Family::DsPsk, &DS_SPREADS[..]),
            (Family::Csk, &CSK_SIZES[..]),
        ] {
            for &spread in ladder {
                for rate in CodeRate::ALL.into_iter().rev() {
                    v.push(Mode {
                        family,
                        spread,
                        rate,
                    });
                }
            }
        }
        v
    }

    /// The voice mode for a profile (Codec2 450 on 2k4, 700C on 3k); `None`
    /// on 500 Hz, which is too slow for any codec.
    pub fn voice(profile: Profile) -> Option<Mode> {
        match profile {
            Profile::N500 => None,
            Profile::W2400 => Mode::new(Family::DsPsk, 4, CodeRate::R2_3),
            Profile::W3000 => Mode::new(Family::DsPsk, 4, CodeRate::R3_4),
        }
    }

    /// Modulation symbols a second (pilots included).
    pub fn symbol_rate(self, p: Profile) -> f64 {
        f64::from(p.chip_rate()) / f64::from(self.spread)
    }

    /// Coded bits a modulation symbol, averaged over the pilots: QPSK less one
    /// pilot in `PILOT_INTERVAL` for DS-PSK, log2(L) for CSK.
    pub fn coded_bits_per_symbol(self) -> f64 {
        match self.family {
            Family::DsPsk => 2.0 * f64::from(PILOT_INTERVAL - 1) / f64::from(PILOT_INTERVAL),
            Family::Csk => f64::from(self.spread.trailing_zeros()),
        }
    }

    /// Coded bits a second.
    pub fn coded_bit_rate(self, p: Profile) -> f64 {
        self.symbol_rate(p) * self.coded_bits_per_symbol()
    }

    /// Information bits a second (before framing overheads: headers, resync
    /// blocks and CRCs come off this).
    pub fn info_bit_rate(self, p: Profile) -> f64 {
        self.coded_bit_rate(p) * self.rate.value()
    }

    /// Chips an information bit.
    pub fn chips_per_bit(self, p: Profile) -> f64 {
        f64::from(p.chip_rate()) / self.info_bit_rate(p)
    }

    /// In-band SNR (in the profile's occupied width) minus Eb/N0, dB:
    /// SNR = Eb/N0 + 10·log10(Rb / B). Negative: the signal sits under the
    /// noise by that much more than its Eb/N0.
    pub fn snr_offset_db(self, p: Profile) -> f64 {
        10.0 * (self.info_bit_rate(p) / p.bandwidth_hz()).log10()
    }

    /// Parses `ds4-2/3`, `DS16 1/2`, `csk64-1/2`. The rate defaults to 1/2.
    pub fn from_name(s: &str) -> Option<Mode> {
        let s = s.trim().to_ascii_lowercase();
        let (head, rate) = match s.split_once(['-', ' ', '@']) {
            Some((h, r)) => (h.to_string(), CodeRate::from_name(r)?),
            None => (s.clone(), CodeRate::R1_2),
        };
        // `?` on an Option returns None from the function when it is None.
        let (family, digits) = match head.strip_prefix("csk") {
            Some(d) => (Family::Csk, d),
            None => (Family::DsPsk, head.strip_prefix("ds")?),
        };
        Mode::new(family, digits.parse().ok()?, rate)
    }

    /// `ds4-2/3`: the form `from_name` reads back.
    pub fn name(self) -> String {
        let f = match self.family {
            Family::DsPsk => "ds",
            Family::Csk => "csk",
        };
        format!("{f}{}-{}", self.spread, self.rate)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fam = match self.family {
            Family::DsPsk => "DS",
            Family::Csk => "CSK",
        };
        write!(f, "{fam}{} {}", self.spread, self.rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> Mode {
        Mode::from_name(s).unwrap()
    }

    #[test]
    fn ds_rates_on_2k4() {
        // 1920 chip/s / SF × 2 bits × 7/8 (pilots).
        assert_eq!(m("ds4-1/2").coded_bit_rate(Profile::W2400), 840.0);
        assert_eq!(m("ds64-1/2").info_bit_rate(Profile::W2400), 26.25);
    }

    #[test]
    fn csk_rates_on_2k4() {
        // 1920 / L × log2 L.
        assert_eq!(m("csk32").coded_bit_rate(Profile::W2400), 300.0);
        assert_eq!(m("csk256").info_bit_rate(Profile::W2400), 30.0);
    }

    #[test]
    fn voice_modes_fit_their_codecs() {
        let v24 = Mode::voice(Profile::W2400).unwrap();
        let v30 = Mode::voice(Profile::W3000).unwrap();
        assert!(v24.info_bit_rate(Profile::W2400) >= 450.0 * 1.2);
        assert!(v30.info_bit_rate(Profile::W3000) >= 700.0 * 1.1);
        assert_eq!(Mode::voice(Profile::N500), None);
    }

    #[test]
    fn names_round_trip() {
        for mode in Mode::all() {
            assert_eq!(Mode::from_name(&mode.name()), Some(mode), "{mode}");
        }
        assert_eq!(
            m("DS16 2/3"),
            Mode::new(Family::DsPsk, 16, CodeRate::R2_3).unwrap()
        );
        assert_eq!(Mode::from_name("ds5"), None);
        assert_eq!(Mode::from_name("csk64-5/6"), None);
    }

    #[test]
    fn snr_offset_matches_hand_calc() {
        // CSK256 1/2 on 2k4: 30 bit/s in 2400 Hz → 10·log10(1/80) ≈ −19.03 dB.
        let o = m("csk256-1/2").snr_offset_db(Profile::W2400);
        assert!((o - (-19.031)).abs() < 1e-3, "{o}");
    }
}
