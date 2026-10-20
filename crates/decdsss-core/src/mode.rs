// made by CasualArclamp
//! The mode ladder: a spreading family, its spreading factor (DS-PSK) or
//! alphabet size (CSK), and an LDPC code rate.

use crate::Profile;
use std::fmt;

/// DS-PSK carries one known pilot symbol in every `PILOT_INTERVAL`.
pub const PILOT_INTERVAL: u32 = 8;

/// Spreading factors offered for DS-PSK, fastest first. SF 2 and 1 are the
/// fast end added on 2026-10-20: the symbol rate is the chip rate over SF, so
/// they double and quadruple DS4's, and there is little or no processing gain
/// left, so they want a clean channel (DESIGN.md, Verification).
pub const DS_SPREADS: [u16; 7] = [1, 2, 4, 8, 16, 32, 64];

/// Alphabet sizes (= chips a symbol) offered for CSK.
pub const CSK_SIZES: [u16; 4] = [32, 64, 128, 256];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    /// Direct sequence: a QPSK symbol held for SF chips, coherent, pilot-aided.
    DsPsk,
    /// Code-shift keying: one of L orthogonal Walsh codes of L chips, log2(L)
    /// bits a symbol, noncoherent.
    Csk,
    /// Bi-orthogonal CSK: the same L Walsh codes, and the code's sign (BPSK) as
    /// one more bit a symbol, log2(L) + 1 in all. The code is found noncoherently
    /// as in CSK; the sign is the change from the symbol before (differential),
    /// so no pilots are needed and the first symbol of a block is the sign
    /// reference.
    BiCsk,
}

impl Family {
    /// Does a symbol pick a Walsh code (CSK and bi-orthogonal CSK)?
    pub const fn is_walsh(self) -> bool {
        matches!(self, Family::Csk | Family::BiCsk)
    }
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
            Family::Csk | Family::BiCsk => &CSK_SIZES,
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
            (Family::BiCsk, &CSK_SIZES[..]),
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
    /// pilot in `PILOT_INTERVAL` for DS-PSK, log2(L) for CSK, log2(L) + 1 for
    /// bi-orthogonal CSK (the block's first symbol has no sign bit to send, a
    /// share of 1 in a few hundred that this leaves out).
    pub fn coded_bits_per_symbol(self) -> f64 {
        match self.family {
            Family::DsPsk => 2.0 * f64::from(PILOT_INTERVAL - 1) / f64::from(PILOT_INTERVAL),
            Family::Csk => f64::from(self.spread.trailing_zeros()),
            Family::BiCsk => f64::from(self.spread.trailing_zeros()) + 1.0,
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

    /// Information bits a modulation symbol, the pilots counted as air time
    /// (the convention the measured thresholds use; padding is left out).
    pub fn info_bits_per_symbol(self) -> f64 {
        self.coded_bits_per_symbol() * self.rate.value()
    }

    /// Eb/N0 (dB) from the Es/N0 of a data symbol: pilots carry the same
    /// energy as data symbols, so Eb/N0 = Es/N0 − 10·log10(info bits a symbol).
    pub fn ebn0_from_es_db(self, es_n0_db: f64) -> f64 {
        es_n0_db - 10.0 * self.info_bits_per_symbol().log10()
    }

    /// The lowest Eb/N0 (dB, in 0.5 dB steps) at which every frame arrived
    /// over AWGN through the whole receiver: measured with `decdsss sweep`
    /// on 2026-10-19 (DESIGN.md, Verification). `None` where that profile and
    /// mode were not measured.
    pub fn ebn0_measured_db(self, p: Profile) -> Option<f64> {
        // Rust note: a `use` inside a function brings the enum's variants into
        // scope here only, so the table reads `DsPsk` rather than `Family::DsPsk`.
        use CodeRate::*;
        use Family::*;
        let db = match (p, self.family, self.spread, self.rate) {
            // SF 2 and 1, measured 2026-10-20 (24 transmissions a point, 0.5 dB steps).
            (Profile::W2400, DsPsk, 1 | 2, R1_2) => 3.0,
            (Profile::W2400, DsPsk, 1 | 2, R2_3) => 3.5,
            (Profile::W2400, DsPsk, 1 | 2, R3_4) => 4.0,
            (Profile::W2400, DsPsk, 4, R1_2) => 4.0,
            (Profile::W2400, DsPsk, 8, R1_2) => 4.5,
            (Profile::W2400, DsPsk, 16, R1_2) => 4.0,
            (Profile::W2400, DsPsk, 32, R1_2) => 4.5,
            (Profile::W2400, DsPsk, 64, R1_2) => 4.0,
            (Profile::W2400, DsPsk, 4, R3_4) => 5.0,
            (Profile::W2400, DsPsk, 16, R3_4) => 4.5,
            (Profile::W2400, Csk, 32 | 64 | 128 | 256, R1_2) => 4.0,
            (Profile::W2400, Csk, 32, R3_4) => 4.0,
            (Profile::W2400, Csk, 64, R3_4) => 3.5,
            (Profile::W3000, DsPsk, 8, R1_2) => 4.0,
            (Profile::W3000, Csk, 64 | 256, R1_2) => 4.0,
            (Profile::N500, DsPsk, 8, R1_2) => 4.0,
            (Profile::N500, Csk, 64, R1_2) => 6.0,
            _ => return None,
        };
        Some(db)
    }

    /// The Eb/N0 (dB) a display takes this mode to need, and whether it was
    /// measured: where it was not, 4.0 dB, the figure most measured modes
    /// came out at.
    pub fn ebn0_needed_db(self, p: Profile) -> (f64, bool) {
        match self.ebn0_measured_db(p) {
            Some(db) => (db, true),
            None => (4.0, false),
        }
    }

    /// Parses `ds4-2/3`, `DS16 1/2`, `csk64-1/2`, `bcsk64-1/2`. The rate defaults to 1/2.
    pub fn from_name(s: &str) -> Option<Mode> {
        let s = s.trim().to_ascii_lowercase();
        let (head, rate) = match s.split_once(['-', ' ', '@']) {
            Some((h, r)) => (h.to_string(), CodeRate::from_name(r)?),
            None => (s.clone(), CodeRate::R1_2),
        };
        // `?` on an Option returns None from the function when it is None.
        let (family, digits) = if let Some(d) = head.strip_prefix("bcsk") {
            (Family::BiCsk, d)
        } else if let Some(d) = head.strip_prefix("csk") {
            (Family::Csk, d)
        } else {
            (Family::DsPsk, head.strip_prefix("ds")?)
        };
        Mode::new(family, digits.parse().ok()?, rate)
    }

    /// `ds4-2/3`: the form `from_name` reads back.
    pub fn name(self) -> String {
        let f = match self.family {
            Family::DsPsk => "ds",
            Family::Csk => "csk",
            Family::BiCsk => "bcsk",
        };
        format!("{f}{}-{}", self.spread, self.rate)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fam = match self.family {
            Family::DsPsk => "DS",
            Family::Csk => "CSK",
            Family::BiCsk => "BCSK",
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
    fn the_fast_end_doubles_and_quadruples_ds4() {
        // Halving SF doubles the symbol rate; the pilots stay one in eight.
        for rate in ["1/2", "2/3", "3/4"] {
            let r4 = m(&format!("ds4-{rate}")).info_bit_rate(Profile::W2400);
            assert_eq!(
                m(&format!("ds2-{rate}")).info_bit_rate(Profile::W2400),
                2.0 * r4
            );
            assert_eq!(
                m(&format!("ds1-{rate}")).info_bit_rate(Profile::W2400),
                4.0 * r4
            );
        }
        // DS1 3/4: 1920 sym/s × 2 bits × 7/8 × 3/4 = 2520 bit/s, a little over the
        // 2400 Hz the signal occupies (+0.2 dB).
        let ds1 = m("ds1-3/4");
        assert_eq!(ds1.info_bit_rate(Profile::W2400), 2520.0);
        assert!((ds1.snr_offset_db(Profile::W2400) - 0.212).abs() < 1e-3);
        // Fastest first.
        assert_eq!(Mode::all()[0], ds1);
    }

    #[test]
    fn csk_rates_on_2k4() {
        // 1920 / L × log2 L.
        assert_eq!(m("csk32").coded_bit_rate(Profile::W2400), 300.0);
        assert_eq!(m("csk256").info_bit_rate(Profile::W2400), 30.0);
    }

    #[test]
    fn bi_orthogonal_csk_adds_a_bit_a_symbol_at_the_same_symbol_rate() {
        // 1920 / L symbols/s × (log2 L + 1) bits × 1/2.
        for (name, plain, bi) in [
            ("32", 150.0, 180.0),
            ("64", 90.0, 105.0),
            ("128", 52.5, 60.0),
            ("256", 30.0, 33.75),
        ] {
            let p = m(&format!("csk{name}-1/2"));
            let b = m(&format!("bcsk{name}-1/2"));
            assert_eq!(p.info_bit_rate(Profile::W2400), plain);
            assert_eq!(b.info_bit_rate(Profile::W2400), bi, "bcsk{name}");
            assert_eq!(b.symbol_rate(Profile::W2400), p.symbol_rate(Profile::W2400));
        }
        // Same bits a symbol as the next larger plain alphabet, twice its rate.
        assert_eq!(
            m("bcsk64-1/2").coded_bits_per_symbol(),
            m("csk128-1/2").coded_bits_per_symbol()
        );
        assert_eq!(
            m("bcsk64-1/2").info_bit_rate(Profile::W2400),
            2.0 * m("csk128-1/2").info_bit_rate(Profile::W2400)
        );
        assert_eq!(m("bcsk64-2/3").name(), "bcsk64-2/3");
        assert_eq!(m("BCSK128 3/4").family, Family::BiCsk);
        assert_eq!(Mode::from_name("bcsk16"), None);
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
    fn ebn0_from_es_by_hand() {
        // DS8 1/2: 2 bits × 7/8 × 1/2 = 0.875 bit a symbol → +0.58 dB.
        let e = m("ds8-1/2").ebn0_from_es_db(10.0);
        assert!((e - 10.580).abs() < 1e-3, "{e}");
        // CSK32 3/4: 5 bits × 3/4 = 3.75 bits a symbol → −5.74 dB.
        let e = m("csk32-3/4").ebn0_from_es_db(10.0);
        assert!((e - 4.260).abs() < 1e-3, "{e}");
    }

    #[test]
    fn thresholds_where_measured() {
        assert_eq!(m("ds8-1/2").ebn0_measured_db(Profile::W2400), Some(4.5));
        assert_eq!(m("csk64-1/2").ebn0_measured_db(Profile::N500), Some(6.0));
        assert_eq!(m("ds2-1/2").ebn0_measured_db(Profile::W2400), Some(3.0));
        assert_eq!(m("ds1-3/4").ebn0_measured_db(Profile::W2400), Some(4.0));
        assert_eq!(m("ds1-3/4").ebn0_measured_db(Profile::W3000), None);
        assert_eq!(m("ds4-2/3").ebn0_measured_db(Profile::W2400), None);
        assert_eq!(m("ds4-2/3").ebn0_needed_db(Profile::W2400), (4.0, false));
    }

    #[test]
    fn snr_offset_matches_hand_calc() {
        // CSK256 1/2 on 2k4: 30 bit/s in 2400 Hz → 10·log10(1/80) ≈ −19.03 dB.
        let o = m("csk256-1/2").snr_offset_db(Profile::W2400);
        assert!((o - (-19.031)).abs() < 1e-3, "{o}");
    }
}
