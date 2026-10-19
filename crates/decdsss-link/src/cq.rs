// made by CasualArclamp
//! The CQ packet: a call that is only a header.
//!
//! A CQ says one thing, who is calling, and the header already carries that: the
//! callsign, in a rate-1/4 LDPC codeword with a CRC-16, sent as CSK symbols on the call
//! code. So a CQ is a preamble and a header and nothing after them: no payload blocks
//! (`nblocks` 0), no frames, no container. It is the best protected thing the modem
//! sends, because the header is decoded wherever the weakest payload mode is (the
//! long CSK256 header, which every weak mode already uses), and nothing has to survive
//! after it, where a text CQ also needs its data frame to decode.
//!
//! The packet is sent in the weakest mode's header (DS64 1/2, which picks the 256-symbol
//! alphabet) whatever mode the sender has set, and in `copies` back-to-back segments,
//! each decodable on its own. The copies see different noise and different fading, so
//! the call gets through if any one of them does. The header's own spare fields count
//! them: `frame0` is the copy number and `total_frames` how many were sent.

use decdsss_core::{CodeRate, Family, Mode, Profile};
use decdsss_phy::framing::{Header, PayloadType};
use decdsss_phy::tx::{Segment, lead_chips_for};

/// Copies sent unless asked otherwise.
pub const DEFAULT_COPIES: usize = 2;
/// Most copies one transmission may have (kept short: each is a preamble and a header).
pub const MAX_COPIES: usize = 8;

/// The mode a CQ is sent in: the one whose header is the long, most sensitive kind
/// (`decdsss_phy::tx::header_alphabet_for`).
pub fn mode() -> Mode {
    Mode::new(Family::DsPsk, 64, CodeRate::R1_2).expect("DS64 1/2 is a mode")
}

/// The segments of a CQ from `call`: `copies` header-only segments.
pub fn plan_cq(
    profile: Profile,
    call: &str,
    session: u8,
    copies: usize,
) -> Result<Vec<Segment>, String> {
    let copies = copies.clamp(1, MAX_COPIES);
    (0..copies)
        .map(|i| {
            let header = Header {
                profile,
                mode: mode(),
                depth: 1,
                ptype: PayloadType::Cq,
                call: call.to_string(),
                session: session & 63,
                frame0: i as u16,
                nblocks: 0,
                total_frames: copies as u16,
            };
            header
                .pack()
                .map(|_| Segment {
                    header,
                    frames: Vec::new(),
                })
                .ok_or_else(|| format!("{call:?} cannot be sent as a callsign"))
        })
        .collect()
}

/// Seconds of signal a CQ of `copies` copies takes, the postamble after the last
/// included (the same arithmetic as the sender's; checked against the real chips).
pub fn air_seconds(profile: Profile, copies: usize) -> f64 {
    let copies = copies.clamp(1, MAX_COPIES);
    let lead = lead_chips_for(profile, mode()) as f64 / f64::from(profile.chip_rate());
    copies as f64 * lead + decdsss_phy::eot::seconds(profile)
}

/// Is this header a CQ?
pub fn is_cq(h: &Header) -> bool {
    h.ptype == PayloadType::Cq
}

/// The line a receiver shows for a CQ it heard.
pub fn describe(h: &Header, snr_db: f64) -> String {
    format!(
        "CQ from {} (CQ packet, copy {} of {}, symbol SNR {snr_db:.0} dB)",
        h.call,
        u32::from(h.frame0) + 1,
        h.total_frames.max(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_phy::tx::transmission_chips;

    #[test]
    fn a_cq_is_header_only_copies_with_the_long_header() {
        let segs = plan_cq(Profile::W2400, "VK2ABC", 70, 3).unwrap();
        assert_eq!(segs.len(), 3);
        for (i, s) in segs.iter().enumerate() {
            assert!(is_cq(&s.header));
            assert_eq!((s.header.nblocks, s.frames.len()), (0, 0));
            assert_eq!(usize::from(s.header.frame0), i);
            assert_eq!(s.header.total_frames, 3);
            assert_eq!(s.header.session, 70 & 63);
            assert_eq!(decdsss_phy::tx::header_alphabet_for(s.header.mode), 256);
        }
        assert!(plan_cq(Profile::W2400, "NOT A CALL", 1, 1).is_err());
        assert_eq!(plan_cq(Profile::W2400, "VK2ABC", 1, 0).unwrap().len(), 1);
    }

    #[test]
    fn air_time_matches_the_real_chips() {
        for profile in Profile::ALL {
            for copies in [1, 2, 5] {
                let segs = plan_cq(profile, "VK2ABC", 1, copies).unwrap();
                let chips = transmission_chips(&segs).unwrap().len();
                let secs = chips as f64 / f64::from(profile.chip_rate());
                assert!((secs - air_seconds(profile, copies)).abs() < 1e-9);
            }
        }
    }
}
