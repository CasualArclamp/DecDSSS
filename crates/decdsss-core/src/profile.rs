// made by CasualArclamp
//! Bandwidth profiles: where in the SSB audio passband the spread signal sits.

use std::fmt;

/// Root-raised-cosine roll-off for every profile. With α = 0.25 the chip
/// rates below give occupied widths of exactly 500, 2400 and 3000 Hz, and
/// they divide 48 kHz (120, 25 and 20 samples a chip).
pub const ROLLOFF: f64 = 0.25;

/// A bandwidth profile. `Copy` makes it a plain value like an integer: it is
/// passed around by copying, never borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Profile {
    /// 500 Hz around 1500 Hz: fits a narrow data filter; no voice.
    N500,
    /// 2.4 kHz, 300–2700 Hz: a standard SSB passband.
    W2400,
    /// 3 kHz, 300–3300 Hz: a wide SSB passband.
    W3000,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::N500, Profile::W2400, Profile::W3000];

    /// Chips a second.
    pub const fn chip_rate(self) -> u32 {
        match self {
            Profile::N500 => 400,
            Profile::W2400 => 1920,
            Profile::W3000 => 2400,
        }
    }

    /// Centre of the occupied band in the audio passband, Hz.
    pub const fn centre_hz(self) -> f64 {
        match self {
            Profile::N500 => 1500.0,
            Profile::W2400 => 1500.0,
            Profile::W3000 => 1800.0,
        }
    }

    /// Occupied width, Hz: chip rate × (1 + α).
    pub fn bandwidth_hz(self) -> f64 {
        f64::from(self.chip_rate()) * (1.0 + ROLLOFF)
    }

    /// Lower and upper band edges in the audio passband, Hz.
    pub fn edges_hz(self) -> (f64, f64) {
        let half = self.bandwidth_hz() / 2.0;
        (self.centre_hz() - half, self.centre_hz() + half)
    }

    /// Chip duration, seconds.
    pub fn chip_s(self) -> f64 {
        1.0 / f64::from(self.chip_rate())
    }

    /// Short name used on the command line and in headers' text form.
    pub const fn name(self) -> &'static str {
        match self {
            Profile::N500 => "500",
            Profile::W2400 => "2k4",
            Profile::W3000 => "3k",
        }
    }

    /// Human label.
    pub const fn label(self) -> &'static str {
        match self {
            Profile::N500 => "500 Hz",
            Profile::W2400 => "2.4 kHz",
            Profile::W3000 => "3 kHz",
        }
    }

    /// Parses `500`, `2k4`/`2400`/`2.4k`, `3k`/`3000`.
    pub fn from_name(s: &str) -> Option<Profile> {
        match s.trim().to_ascii_lowercase().as_str() {
            "500" | "n500" => Some(Profile::N500),
            "2k4" | "2400" | "2.4k" | "2.4" => Some(Profile::W2400),
            "3k" | "3000" | "3.0k" | "3" => Some(Profile::W3000),
            _ => None,
        }
    }
}

// `Display` lets a profile go straight into `format!("{p}")`.
impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_exact() {
        assert_eq!(Profile::N500.bandwidth_hz(), 500.0);
        assert_eq!(Profile::W2400.bandwidth_hz(), 2400.0);
        assert_eq!(Profile::W3000.bandwidth_hz(), 3000.0);
    }

    #[test]
    fn bands_fit_the_ssb_passband() {
        assert_eq!(Profile::W2400.edges_hz(), (300.0, 2700.0));
        assert_eq!(Profile::W3000.edges_hz(), (300.0, 3300.0));
        assert_eq!(Profile::N500.edges_hz(), (1250.0, 1750.0));
    }

    #[test]
    fn chip_rates_divide_48k() {
        for p in Profile::ALL {
            assert_eq!(48_000 % p.chip_rate(), 0, "{p}");
        }
    }

    #[test]
    fn names_round_trip() {
        for p in Profile::ALL {
            assert_eq!(Profile::from_name(p.name()), Some(p));
        }
        assert_eq!(Profile::from_name("2.4k"), Some(Profile::W2400));
        assert_eq!(Profile::from_name("wide"), None);
    }
}
