// made by CasualArclamp
//! A picture sent the whole way: compressed to a budget, framed, modulated,
//! through a fading channel with a carrier offset, received, assembled, and
//! drawn as far as it has come after every block.

use decdsss_chan::{Channel, Watterson};
use decdsss_core::{Mode, Profile};
use decdsss_link::{Assembler, SendSpec, container, plan};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{chips_to_audio, segment_chips};

const RATE: f64 = 48_000.0;

/// A picture with structure at several scales (gradients, edges, detail).
fn test_picture(w: u32, h: u32) -> decdsss_image::RgbImage {
    decdsss_image::RgbImage::from_fn(w, h, |x, y| {
        let (u, v) = (x as f32, y as f32);
        let r = 128.0 + 60.0 * (u * 0.13 + v * 0.05).sin() + 30.0 * (u * 0.5).sin();
        let g = 255.0 * v / h as f32 + 25.0 * (v * 0.6).sin();
        let b = if (x / 12 + y / 12) % 2 == 0 {
            210.0
        } else {
            70.0
        };
        decdsss_image::Rgb([r, g, b].map(|c| c.clamp(0.0, 255.0) as u8))
    })
}

#[test]
fn a_picture_arrives_through_fading_and_draws_as_it_comes() {
    let jpeg = decdsss_image::encode_to_fit(&test_picture(96, 72), 900)
        .unwrap()
        .bytes;
    let name = "test.jpg";
    let spec = SendSpec {
        profile: Profile::W2400,
        mode: Mode::from_name("ds8-1/2").unwrap(),
        depth: 2,
        call: "VK2ABC".into(),
        session: 5,
        ptype: PayloadType::Picture,
        resync_s: 300.0,
        parity: 0.25,
        passes: 1,
    };
    let p = plan(&spec, &container::pack(name, &jpeg)).unwrap();
    assert_eq!(p.segments.len(), 1, "one segment, so one preamble to find");
    let mut chips = Vec::new();
    for s in &p.segments {
        chips.extend(segment_chips(s).unwrap());
    }
    let audio = chips_to_audio(&chips, spec.profile, RATE, 0.2, 1.0, 1.0);
    let channel = Channel {
        snr_db: Some(12.0),
        offset_hz: 6.0,
        clock_ppm: -12.0,
        fading: Some(Watterson::MODERATE),
        seed: 21,
        ..Channel::clean(0.2, spec.profile.bandwidth_hz())
    };
    let heard = channel.apply(&audio, RATE);

    let mut rx = MultiReceiver::new(&[Profile::W2400], RATE);
    let mut asm = Assembler::new();
    let mut file = None;
    // What was drawn after each block: (bytes in, complete scans).
    let mut drawn: Vec<(usize, usize)> = Vec::new();
    for chunk in heard.chunks(8192) {
        for tagged in rx.push(chunk) {
            let Event::Block { header, result, .. } = tagged.event else {
                continue;
            };
            assert_eq!(header.ptype, PayloadType::Picture);
            let done = asm.push(&header, &result);
            if let Some(front) = asm.front("VK2ABC", 5)
                && let Some(part) = decdsss_image::decode_partial(&front.data)
            {
                assert_eq!((part.image.width(), part.image.height()), (96, 72));
                drawn.push((front.data.len(), part.scans));
            }
            file = file.or(done);
        }
    }
    let f = file.expect("the picture arrived");
    assert_eq!((f.name.as_str(), f.ptype), (name, PayloadType::Picture));
    assert_eq!(f.data, jpeg, "the JPEG is byte for byte what was sent");
    let whole = decdsss_image::decode(&f.data).expect("decodes");
    assert_eq!(whole.dimensions(), (96, 72));
    // It was drawn on the way: the front grew from block to block, and the
    // scans with it.
    assert!(drawn.len() >= 3, "{drawn:?}");
    assert!(
        drawn
            .windows(2)
            .all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1)
    );
    assert!(
        drawn.first().unwrap().1 < drawn.last().unwrap().1,
        "{drawn:?}"
    );
}
