// made by CasualArclamp
//! The end-of-transmission postamble: the last second of a transmission.
//!
//! After the last block of the last segment (and after the last block of a live
//! stream) the transmitter sends [`EOT_SEGMENTS`] quarter-seconds of the
//! station's own scrambler multiplied by the preamble's B sequence, real chips,
//! full power. It carries no data. A receiver that is following the station
//! already has its timing, carrier and rake fingers, so it correlates the chips
//! that come next with that known sequence at the lags of its delay profile and
//! asks whether one lag stands clear of the rest. A yes ends the transmission
//! on the spot (a live stream does not have to wait for blocks that never
//! come); a no changes nothing, so a missed postamble costs only that.
//!
//! Why the B sequence on top of the scrambler, and not the scrambler alone: the
//! receiver tests for it after *every* block of a live stream, where the next
//! chips are data. Data is the scrambler times a symbol held for SF chips, so
//! correlating with the bare scrambler would add the symbols up coherently and
//! show a strong peak at the main path. The B chips change every chip; against
//! them the data sums like noise and the delay profile stays flat. The station's
//! own scrambler keeps one station's postamble from ending another's channel.

use crate::codes::{Scrambler, preamble_b};
use decdsss_core::Profile;
use decdsss_dsp::Iq;

/// Coherent segments in the postamble. Four quarter-seconds: the delay profile
/// is a sum of this many powers, and four makes a peak 7 times the median of
/// 105 lags a false alarm about once in 10⁵ tests (derived: Gamma(4) tail
/// P(X > 7 · 3.67) = 1.5·10⁻⁶ a lag, about 26 independent lags).
pub const EOT_SEGMENTS: usize = 4;

/// How many times the median of the delay profile its peak must reach.
pub const EOT_PEAK_OVER_MEDIAN: f32 = 7.0;

/// Chips correlated coherently at a time: about a quarter of a second, the same
/// as the refinement uses, so a fade has little time to turn the phase.
pub fn segment_chips(profile: Profile) -> usize {
    crate::rx::refine::segment_chips(f64::from(profile.chip_rate()))
}

/// Chips in the postamble.
pub fn len(profile: Profile) -> usize {
    EOT_SEGMENTS * segment_chips(profile)
}

/// Seconds the postamble takes.
pub fn seconds(profile: Profile) -> f64 {
    len(profile) as f64 / f64::from(profile.chip_rate())
}

/// The postamble's chips for a station whose scrambler is at chip `chip0` (the
/// number of payload chips already sent): `scr.chip(chip0 + i) · B[i mod |B|]`.
pub fn chips(profile: Profile, scr: &Scrambler, chip0: usize) -> Vec<Iq> {
    let b = preamble_b(profile);
    (0..len(profile))
        .map(|i| scr.chip(chip0 + i) * b[i % b.len()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_a_second_long_and_full_power() {
        for p in [Profile::N500, Profile::W2400, Profile::W3000] {
            assert!((seconds(p) - 1.0).abs() < 0.02, "{p}: {}", seconds(p));
            let scr = Scrambler::new(5);
            let c = chips(p, &scr, 12345);
            let pw = c.iter().map(|z| z.norm_sqr()).sum::<f32>() / c.len() as f32;
            assert!((pw - 1.0).abs() < 1e-3, "{p}: {pw}");
        }
    }
}
