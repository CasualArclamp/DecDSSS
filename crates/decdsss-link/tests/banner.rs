// made by CasualArclamp
//! The waterfall ID ahead of a transmission: the receiver still hears the call it
//! introduces, hears nothing in the banner itself, and a banner in one slot of the
//! 500 Hz profile is not heard in the next. (What the banner looks like and how loud
//! it is: `decdsss_phy::banner`'s own tests.)

use decdsss_chan::Channel;
use decdsss_core::{Band, Mode, Profile, Slot};
use decdsss_link::cq::{self, plan_cq};
use decdsss_link::text::{cq as cq_text, read};
use decdsss_link::{Assembler, SendSpec, plan_text};
use decdsss_phy::banner::banner;
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{chips_to_audio_at, transmission_chips};

const RATE: f64 = 48_000.0;

fn channel(band: Band, snr_db: f64, seed: u64) -> Channel {
    Channel {
        snr_db: Some(snr_db),
        offset_hz: 6.0,
        clock_ppm: 12.0,
        seed,
        ..Channel::clean(0.2, band.profile.bandwidth_hz())
    }
}

/// The audio a station sends: the waterfall ID if asked for, then `audio`.
fn with_banner(audio: Vec<f32>, band: Band, said: &str, wfid: bool) -> Vec<f32> {
    if !wfid {
        return audio;
    }
    let mut v = banner(&["VK2ABC", said], band, RATE, 0.2);
    v.extend(audio);
    v
}

fn cq_audio(band: Band, copies: usize) -> Vec<f32> {
    let segs = plan_cq(band.profile, "VK2ABC", 9, copies).unwrap();
    let chips = transmission_chips(&segs).unwrap();
    chips_to_audio_at(&chips, band, RATE, 0.2, 1.0, 3.0)
}

/// The bands and callsigns of every CQ header heard.
fn hear_cq(audio: &[f32]) -> Vec<(Band, String)> {
    let mut rx = MultiReceiver::with_bands(&Band::ALL, RATE);
    let mut got = Vec::new();
    for chunk in audio.chunks(4800) {
        for t in rx.push(chunk) {
            if let Event::Header { header, .. } = t.event
                && cq::is_cq(&header)
            {
                got.push((t.band, header.call));
            }
        }
    }
    got
}

#[test]
fn a_cq_with_a_waterfall_id_is_heard_as_before_in_its_own_band_only() {
    for band in [
        Band::mid(Profile::W2400),
        Band::mid(Profile::W3000),
        Band::new(Profile::N500, Slot::Low),
        Band::new(Profile::N500, Slot::High),
    ] {
        let audio = with_banner(cq_audio(band, 1), band, "CQ", true);
        let heard = channel(band, 0.0, 5).apply(&audio, RATE);
        assert_eq!(
            hear_cq(&heard),
            vec![(band, "VK2ABC".to_string())],
            "{band:?}"
        );
    }
}

#[test]
fn the_banner_itself_is_not_taken_for_a_signal() {
    for band in [
        Band::mid(Profile::W2400),
        Band::new(Profile::N500, Slot::Mid),
    ] {
        let audio = banner(&["VK2ABC", "DS8 1/2"], band, RATE, 0.2);
        for snr in [30.0, 0.0] {
            let heard = channel(band, snr, 11).apply(&audio, RATE);
            assert!(hear_cq(&heard).is_empty(), "{band:?} at {snr} dB");
        }
    }
}

#[test]
fn a_weak_cq_still_gets_through_behind_a_banner() {
    // As `cq`'s own weak-signal test (8 dB under the noise, two copies, 3 of 4 runs),
    // with the banner ahead: it raises nothing the receiver trips on.
    let band = Band::mid(Profile::W2400);
    let audio = with_banner(cq_audio(band, 2), band, "CQ", true);
    let heard = (0..4)
        .filter(|&seed| {
            let heard = channel(band, -8.0, 100 + seed).apply(&audio, RATE);
            !hear_cq(&heard).is_empty()
        })
        .count();
    assert!(heard >= 3, "heard in {heard} of 4 runs");
}

#[test]
fn text_behind_a_banner_arrives_intact() {
    let band = Band::new(Profile::N500, Slot::High);
    let mode = Mode::from_name("ds8-1/2").unwrap();
    let spec = SendSpec {
        profile: band.profile,
        mode,
        depth: 1,
        call: "VK2ABC".into(),
        session: 9,
        ptype: PayloadType::Text,
        resync_s: 60.0,
        parity: 0.5,
        passes: 1,
    };
    let message = cq_text("VK2ABC");
    let plan = plan_text(&spec, &message).unwrap();
    let chips = transmission_chips(&plan.segments).unwrap();
    let audio = chips_to_audio_at(&chips, band, RATE, 0.2, 1.0, 3.0);
    let audio = with_banner(audio, band, &mode.to_string(), true);
    let heard = channel(band, 6.0, 5).apply(&audio, RATE);

    let mut rx = MultiReceiver::with_bands(&Band::ALL, RATE);
    let mut asm = Assembler::new();
    let mut got = Vec::new();
    for chunk in heard.chunks(4800) {
        for t in rx.push(chunk) {
            if let Event::Block { header, result, .. } = t.event {
                assert_eq!(t.band, band, "heard where it was sent");
                if let Some(f) = asm.push(&header, &result) {
                    got.push(read(&f).expect("a text file"));
                }
            }
        }
    }
    assert_eq!(got, vec![message]);
}
