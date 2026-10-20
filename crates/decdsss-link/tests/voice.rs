// made by CasualArclamp
//! Speech sent the whole way: Codec2 frames in modem frames, through a noisy
//! channel with a carrier offset, received, assembled and played back as speech
//! of the length that was sent, with the frames that were lost as gaps.

use decdsss_chan::Channel;
use decdsss_codec2::Mode as Codec;
use decdsss_core::Profile;
use decdsss_link::{Assembler, VoiceSpec, plan_voice};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{chips_to_audio, segment_chips};

const RATE: f64 = 48_000.0;

/// Vowel-like speech: a pitch-gliding pulse train through three formants.
fn speech(seconds: f64) -> Vec<i16> {
    let n = (seconds * 8000.0) as usize;
    let mut x = vec![0.0f64; n];
    let mut phase = 0.0;
    for (i, s) in x.iter_mut().enumerate() {
        phase += (105.0 + 25.0 * (i as f64 / 5000.0).sin()) / 8000.0;
        if phase >= 1.0 {
            phase -= 1.0;
            *s = 1.0;
        }
    }
    for (fc, bw) in [(700.0, 90.0), (1220.0, 110.0), (2600.0, 160.0)] {
        let r = (-std::f64::consts::PI * bw / 8000.0).exp();
        let (a1, a2) = (
            -2.0 * r * (std::f64::consts::TAU * fc / 8000.0).cos(),
            r * r,
        );
        let (mut y1, mut y2) = (0.0, 0.0);
        for s in x.iter_mut() {
            let y = *s - a1 * y1 - a2 * y2;
            y2 = y1;
            y1 = y;
            *s = y;
        }
    }
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    x.iter().map(|v| (v / peak * 8000.0) as i16).collect()
}

#[test]
fn speech_arrives_as_speech_of_the_same_length() {
    for (profile, codec, ptype) in [
        (Profile::W2400, Codec::C450, PayloadType::Voice450),
        (Profile::W3000, Codec::C700C, PayloadType::Voice),
        // The fast modes' codecs (DS1 and DS2; 1600's header is the wide one).
        (Profile::W2400, Codec::C1200, PayloadType::Voice1200),
        (Profile::W2400, Codec::C1600, PayloadType::Voice1600),
        (Profile::W3000, Codec::C1200, PayloadType::Voice1200),
        (Profile::W3000, Codec::C1600, PayloadType::Voice1600),
    ] {
        let spec = VoiceSpec {
            profile,
            mode: decdsss_link::voice::voice_mode(profile, codec).unwrap(),
            depth: 2,
            call: "VK2ABC".into(),
            session: 9,
            codec,
            resync_s: 300.0,
        };
        let input = speech(6.0);
        let plan = plan_voice(&spec, &input).unwrap();
        let mut chips = Vec::new();
        for s in &plan.segments {
            assert_eq!(s.header.ptype, ptype);
            chips.extend(segment_chips(s).unwrap());
        }
        let audio = chips_to_audio(&chips, profile, RATE, 0.2, 1.0, 1.0);
        let channel = Channel {
            snr_db: Some(12.0),
            offset_hz: 4.0,
            clock_ppm: 10.0,
            seed: 31,
            ..Channel::clean(0.2, profile.bandwidth_hz())
        };
        let heard = channel.apply(&audio, RATE);

        let mut rx = MultiReceiver::new(&[profile], RATE);
        let mut asm = Assembler::new();
        let mut partial_lengths = Vec::new();
        for chunk in heard.chunks(8192) {
            for tagged in rx.push(chunk) {
                if let Event::Block { header, result, .. } = tagged.event {
                    assert_eq!(header.ptype, ptype);
                    asm.push(&header, &result);
                    if let Some((so_far, _)) = asm.voice_speech("VK2ABC", 9) {
                        partial_lengths.push(so_far.len());
                    }
                }
            }
        }
        let done = asm.take_voice();
        assert_eq!(done.len(), 1, "{codec}: one speech transmission arrived");
        let v = &done[0];
        assert_eq!((v.call.as_str(), v.session, v.codec), ("VK2ABC", 9, codec));
        assert_eq!(v.lost, 0);
        assert_eq!(v.speech.len() as f64 / 8000.0, plan.speech_seconds);
        // Speech, not silence: its level is that of the input, within a factor of two.
        let rms = |x: &[i16]| {
            (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        };
        let ratio = rms(&v.speech[4000..input.len()]) / rms(&input[4000..]);
        assert!((0.5..2.0).contains(&ratio), "{codec}: level {ratio:.2}");
        // And the progress was there on the way: the speech so far is whole
        // length every time (silence for what has not come).
        assert!(partial_lengths.len() >= 2);
        assert!(partial_lengths.iter().all(|&n| n == v.speech.len()));
        assert!(asm.voice_progress("VK2ABC", 9).unwrap().have == plan.frames);
    }
}
