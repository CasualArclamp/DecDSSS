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

/// Where a 500 Hz signal sits in the audio passband. Three of them fit side by
/// side, 500 Hz apart (FDMA, as narrow-band HF modems do), so up to three stations
/// can share a 2.4 kHz filter. It is not signalled: a receiver listens on all
/// three, and a transmitter just chooses one. The wider profiles have no slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    /// 500 Hz below the middle: 750–1250 Hz.
    Low,
    /// The profile's own place: 1250–1750 Hz.
    #[default]
    Mid,
    /// 500 Hz above the middle: 1750–2250 Hz.
    High,
}

impl Slot {
    pub const ALL: [Slot; 3] = [Slot::Low, Slot::Mid, Slot::High];

    /// Shift from the profile's centre, Hz: exactly one occupied width of the 500
    /// Hz profile, so neighbouring slots touch with no guard band.
    pub const fn offset_hz(self) -> f64 {
        match self {
            Slot::Low => -500.0,
            Slot::Mid => 0.0,
            Slot::High => 500.0,
        }
    }

    /// Short name for the command line: `-500`, `0` or `+500`.
    pub const fn name(self) -> &'static str {
        match self {
            Slot::Low => "-500",
            Slot::Mid => "0",
            Slot::High => "+500",
        }
    }

    /// Parses `-500`/`low`/`l`, `0`/`mid`/`centre`/`center`, `+500`/`500`/`high`/`h`.
    pub fn from_name(s: &str) -> Option<Slot> {
        match s.trim().to_ascii_lowercase().as_str() {
            "-500" | "low" | "l" | "lower" => Some(Slot::Low),
            "0" | "mid" | "m" | "centre" | "center" | "" => Some(Slot::Mid),
            "+500" | "500" | "high" | "h" | "upper" => Some(Slot::High),
            _ => None,
        }
    }
}

/// A place in the passband a receiver listens at and a transmitter sends from: a
/// profile and, for the 500 Hz profile, one of its slots. (The wider profiles are
/// always in the middle.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Band {
    pub profile: Profile,
    pub slot: Slot,
}

impl Band {
    /// Every band, narrowest and lowest first.
    pub const ALL: [Band; 5] = [
        Band::new(Profile::N500, Slot::Low),
        Band::new(Profile::N500, Slot::Mid),
        Band::new(Profile::N500, Slot::High),
        Band::new(Profile::W2400, Slot::Mid),
        Band::new(Profile::W3000, Slot::Mid),
    ];

    pub const fn new(profile: Profile, slot: Slot) -> Band {
        Band { profile, slot }
    }

    /// The profile's own place (what a profile alone meant before there were slots).
    pub const fn mid(profile: Profile) -> Band {
        Band::new(profile, Slot::Mid)
    }

    /// Centre of the occupied band in the audio passband, Hz.
    pub const fn centre_hz(self) -> f64 {
        match self.profile {
            Profile::N500 => self.profile.centre_hz() + self.slot.offset_hz(),
            _ => self.profile.centre_hz(),
        }
    }

    /// Lower and upper band edges, Hz.
    pub fn edges_hz(self) -> (f64, f64) {
        let half = self.profile.bandwidth_hz() / 2.0;
        (self.centre_hz() - half, self.centre_hz() + half)
    }

    /// Human label: the profile's, and for a 500 Hz slot off the middle its shift.
    pub fn label(self) -> String {
        match (self.profile, self.slot) {
            (Profile::N500, Slot::Mid) | (Profile::W2400 | Profile::W3000, _) => {
                self.profile.label().to_string()
            }
            (Profile::N500, s) => format!("{} {} Hz", self.profile.label(), s.name()),
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
    fn slots_sit_side_by_side_in_the_passband() {
        let edges: Vec<(f64, f64)> = Slot::ALL
            .iter()
            .map(|&s| Band::new(Profile::N500, s).edges_hz())
            .collect();
        assert_eq!(edges, [(750.0, 1250.0), (1250.0, 1750.0), (1750.0, 2250.0)]);
        // Inside the 2.4 kHz filter, and the wide profiles have only a middle.
        assert!(edges[0].0 >= 300.0 && edges[2].1 <= 2700.0);
        assert_eq!(
            Band::new(Profile::W2400, Slot::High).centre_hz(),
            Profile::W2400.centre_hz()
        );
        assert_eq!(Band::mid(Profile::N500), Band::ALL[1]);
        assert_eq!(Band::ALL[0].label(), "500 Hz -500 Hz");
        assert_eq!(Band::ALL[1].label(), "500 Hz");
        for s in Slot::ALL {
            assert_eq!(Slot::from_name(s.name()), Some(s));
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
