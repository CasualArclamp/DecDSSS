// made by CasualArclamp
//! Text the whole way: a CQ call and a few lines of chat, sent as the text payload
//! through a noisy channel with a carrier offset, received, assembled and read back
//! as the same text. Once on the 2.4 kHz profile and once in the upper slot of the
//! 500 Hz profile, where two more stations could be on the air beside it.

use decdsss_chan::Channel;
use decdsss_core::{Band, Mode, Profile, Slot};
use decdsss_link::text::{cq, read, tidy};
use decdsss_link::{Assembler, SendSpec, plan_text};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{chips_to_audio_at, transmission_chips};

const RATE: f64 = 48_000.0;

fn send_and_receive(band: Band, mode: &str, text: &str, snr_db: f64) -> (Vec<String>, bool) {
    let spec = SendSpec {
        profile: band.profile,
        mode: Mode::from_name(mode).unwrap(),
        depth: 4,
        call: "VK2ABC".into(),
        session: 9,
        ptype: PayloadType::File,
        resync_s: 60.0,
        parity: 0.5,
        passes: 1,
    };
    let plan = plan_text(&spec, text).unwrap();
    let chips = transmission_chips(&plan.segments).unwrap();
    let audio = chips_to_audio_at(&chips, band, RATE, 0.2, 1.0, 3.0);
    let channel = Channel {
        snr_db: Some(snr_db),
        offset_hz: 6.0,
        clock_ppm: 12.0,
        seed: 5,
        ..Channel::clean(0.2, band.profile.bandwidth_hz())
    };
    let heard = channel.apply(&audio, RATE);

    let mut rx = MultiReceiver::with_bands(&Band::ALL, RATE);
    let mut asm = Assembler::new();
    let (mut got, mut finished) = (Vec::new(), false);
    for chunk in heard.chunks(4800) {
        for t in rx.push(chunk) {
            match t.event {
                Event::Block { header, result, .. } => {
                    assert_eq!(header.ptype, PayloadType::Text);
                    assert_eq!(t.band, band, "heard where it was sent");
                    if let Some(f) = asm.push(&header, &result) {
                        got.push(read(&f).expect("a text file"));
                    }
                }
                Event::EndOfTransmission { .. } => finished = true,
                _ => {}
            }
        }
    }
    (got, finished)
}

#[test]
fn a_cq_call_arrives_as_text() {
    let msg = cq("VK2ABC");
    let (got, finished) = send_and_receive(Band::mid(Profile::W2400), "ds8-1/2", &msg, 4.0);
    assert_eq!(got, vec![msg]);
    assert!(finished, "the postamble was heard");
}

#[test]
fn chat_in_a_500_hz_slot_arrives_as_text() {
    let msg =
        "Hello Rory, copy you 5 and 7 here in Brisbane.\nBack to you, 73 de VK2ABC \u{2014} K";
    let (got, finished) =
        send_and_receive(Band::new(Profile::N500, Slot::High), "ds4-1/2", msg, 8.0);
    assert_eq!(got, vec![tidy(msg)]);
    assert!(finished, "the postamble was heard");
}
