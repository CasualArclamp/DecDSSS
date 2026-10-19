// made by CasualArclamp
//! The CQ packet the whole way: header-only copies through a noisy channel with a
//! carrier offset and clock error, heard as CQ headers carrying the callsign.
//! `cq_curve` (ignored; it takes minutes) measures how weak a CQ can be and still get
//! through, against the same call sent as text.

use decdsss_chan::{Channel, Watterson};
use decdsss_core::{Band, Mode, Profile, Slot};
use decdsss_link::cq::{self, plan_cq};
use decdsss_link::text::cq as cq_text;
use decdsss_link::{SendSpec, plan_text};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{chips_to_audio_at, transmission_chips};

const RATE: f64 = 48_000.0;

/// CQ copies heard (copy number, total) and whether the postamble ended it.
fn hear(audio: &[f32], bands: &[Band]) -> Vec<(Band, String, u16, u16)> {
    let mut rx = MultiReceiver::with_bands(bands, RATE);
    let mut got = Vec::new();
    for chunk in audio.chunks(4800) {
        for t in rx.push(chunk) {
            if let Event::Header { header, .. } = t.event
                && cq::is_cq(&header)
            {
                got.push((
                    t.band,
                    header.call.clone(),
                    header.frame0,
                    header.total_frames,
                ));
            }
        }
    }
    got
}

fn channel(band: Band, snr_db: f64, seed: u64, fading: Option<Watterson>) -> Channel {
    Channel {
        snr_db: Some(snr_db),
        offset_hz: 6.0,
        clock_ppm: 12.0,
        fading,
        seed,
        ..Channel::clean(0.2, band.profile.bandwidth_hz())
    }
}

fn cq_audio(band: Band, copies: usize) -> Vec<f32> {
    let segs = plan_cq(band.profile, "VK2ABC", 9, copies).unwrap();
    let chips = transmission_chips(&segs).unwrap();
    chips_to_audio_at(&chips, band, RATE, 0.2, 1.0, 3.0)
}

#[test]
fn a_cq_is_heard_with_its_call_and_copy_numbers() {
    let band = Band::mid(Profile::W2400);
    let heard = channel(band, 0.0, 5, None).apply(&cq_audio(band, 2), RATE);
    let got = hear(&heard, &Band::ALL);
    assert_eq!(
        got,
        vec![
            (band, "VK2ABC".to_string(), 0, 2),
            (band, "VK2ABC".to_string(), 1, 2)
        ]
    );
}

#[test]
fn a_cq_in_a_500_hz_slot_is_heard_in_that_slot() {
    let band = Band::new(Profile::N500, Slot::High);
    let heard = channel(band, 0.0, 6, None).apply(&cq_audio(band, 1), RATE);
    let got = hear(&heard, &Band::ALL);
    assert_eq!(got, vec![(band, "VK2ABC".to_string(), 0, 1)]);
}

#[test]
fn a_cq_far_below_the_noise_still_gets_through() {
    // 8 dB under the noise in the 2.4 kHz band, two copies.
    let band = Band::mid(Profile::W2400);
    let audio = cq_audio(band, 2);
    let heard_any = (0..4).filter(|&seed| {
        let heard = channel(band, -8.0, 100 + seed, None).apply(&audio, RATE);
        !hear(&heard, &[band]).is_empty()
    });
    assert!(heard_any.count() >= 3, "heard in at least 3 of 4 runs");
}

/// Fraction of runs in which at least one CQ header decoded, text CQ against packet.
/// `cargo test -p decdsss-link --release -- --ignored --nocapture cq_curve`
#[test]
#[ignore = "measurement"]
fn cq_curve() {
    let band = Band::mid(Profile::W2400);
    let trials = 24u64;
    let text_audio = {
        let spec = SendSpec {
            profile: band.profile,
            mode: Mode::from_name("ds8-1/2").unwrap(),
            depth: 1,
            call: "VK2ABC".into(),
            session: 9,
            ptype: PayloadType::Text,
            resync_s: 60.0,
            parity: 0.25,
            passes: 1,
        };
        let plan = plan_text(&spec, &cq_text("VK2ABC")).unwrap();
        let chips = transmission_chips(&plan.segments).unwrap();
        chips_to_audio_at(&chips, band, RATE, 0.2, 1.0, 3.0)
    };
    let packet1 = cq_audio(band, 1);
    let packet2 = cq_audio(band, 2);
    for (label, fading) in [
        ("AWGN", None),
        ("moderate fading", Some(Watterson::MODERATE)),
    ] {
        println!("{label}: CQ heard (of {trials}) — text CQ DS8 1/2, packet x1, packet x2");
        for snr in [-4.0, -8.0, -10.0, -12.0, -14.0, -16.0] {
            let run = |audio: &[f32], packet: bool| {
                (0..trials)
                    .filter(|&seed| {
                        let heard = channel(band, snr, 1000 + seed, fading).apply(audio, RATE);
                        let mut rx = MultiReceiver::with_bands(&[band], RATE);
                        let mut ok = false;
                        for chunk in heard.chunks(4800) {
                            for t in rx.push(chunk) {
                                match t.event {
                                    Event::Header { header, .. } if packet => {
                                        ok |= cq::is_cq(&header);
                                    }
                                    Event::Header { .. } if !packet => ok = true,
                                    _ => {}
                                }
                            }
                        }
                        ok
                    })
                    .count()
            };
            println!(
                "  {snr:6.1} dB: {:2} {:2} {:2}",
                run(&text_audio, false),
                run(&packet1, true),
                run(&packet2, true)
            );
        }
    }
}
