// made by CasualArclamp
//! Voice: Codec2 speech in modem frames.
//!
//! Speech (8 kHz, mono) is compressed by Codec2 — 450 bit/s on the 2.4 kHz
//! profile, 700C on 3 kHz, or 1200 and 1600 on the fast modes (DS2 and DS1) —
//! and the codec frames are packed end to end into the
//! data of the modem's frames, a whole number of codec frames to a modem frame
//! (a few bits of padding at the end of each). Each modem frame is then
//! self-contained: one that does not decode is a gap of known length in the
//! speech (silence), and the next one starts on a codec frame boundary. There
//! is no erasure coding or ARQ for voice; the LDPC code and the interleaver are
//! all it has, as a live voice link would.
//!
//! The frame number in the header says where a frame belongs in time, so a
//! listener who joins at a later segment still hears the rest.

use crate::send::{Frames, cut_segments};
use decdsss_codec2::{Codec2, Mode as Codec, SPEECH_RATE, pack_frames, unpack_frames};
use decdsss_core::{Mode, Profile};
use decdsss_phy::framing::{Layout, PayloadType};
use decdsss_phy::tx::Segment;

/// The payload type that says which codec a voice transmission uses.
pub fn ptype_of(codec: Codec) -> Option<PayloadType> {
    match codec {
        Codec::C700C => Some(PayloadType::Voice),
        Codec::C450 => Some(PayloadType::Voice450),
        Codec::C1200 => Some(PayloadType::Voice1200),
        Codec::C1600 => Some(PayloadType::Voice1600),
        _ => None,
    }
}

/// The codecs a voice transmission can use, slowest first.
pub const CODECS: [Codec; 4] = [Codec::C450, Codec::C700C, Codec::C1200, Codec::C1600];

/// The codec a voice payload type means.
pub fn codec_of(ptype: PayloadType) -> Option<Codec> {
    match ptype {
        PayloadType::Voice => Some(Codec::C700C),
        PayloadType::Voice450 => Some(Codec::C450),
        PayloadType::Voice1200 => Some(Codec::C1200),
        PayloadType::Voice1600 => Some(Codec::C1600),
        _ => None,
    }
}

/// The codec that goes with a profile (the mode is [`Mode::voice`]): 450 on
/// 2.4 kHz, 700C on 3 kHz, none on 500 Hz.
pub fn default_codec(profile: Profile) -> Option<Codec> {
    match profile {
        Profile::N500 => None,
        Profile::W2400 => Some(Codec::C450),
        Profile::W3000 => Some(Codec::C700C),
    }
}

/// The mode that carries `codec` on `profile` with the speech running ahead of
/// the air time (see [`VoicePlan::throughput_ratio`]), or `None` where there is
/// none to suggest. 450 on 2.4 kHz and 700C on 3 kHz are [`Mode::voice`]; the
/// faster codecs ride DS2 and DS1, which need a clean, strong channel (DESIGN.md,
/// *DS2 and DS1*). The ratios are at depth 1 (`modes_for_the_faster_codecs_keep_up`):
///
/// | codec | 2.4 kHz | ratio | 3 kHz | ratio |
/// |---|---|---|---|---|
/// | 1200 | DS1 1/2 | 1.27 | DS2 3/4 | 1.23 |
/// | 1600 | DS1 2/3 | 1.27 | DS1 1/2 | 1.15 |
///
/// (Measured by the plan, whole codec frames to a modem frame: 11 of 48 or 64 bits in
/// 68 or 92 bytes, 17 and 8 at 3 kHz. DS2 3/4 on 2.4 kHz would carry 1200 at 0.99 and
/// DS1 1/2 on 2.4 kHz 1600 at 0.93, below 1, so those are not suggested.)
pub fn voice_mode(profile: Profile, codec: Codec) -> Option<Mode> {
    let ds = |sf, rate| Mode::new(decdsss_core::Family::DsPsk, sf, rate);
    use decdsss_core::CodeRate::{R1_2, R2_3, R3_4};
    match (profile, codec) {
        (Profile::W2400, Codec::C450) | (Profile::W3000, Codec::C700C) => Mode::voice(profile),
        (Profile::W2400, Codec::C1200) => ds(1, R1_2),
        (Profile::W2400, Codec::C1600) => ds(1, R2_3),
        (Profile::W3000, Codec::C1200) => ds(2, R3_4),
        (Profile::W3000, Codec::C1600) => ds(1, R1_2),
        _ => None,
    }
}

/// 8 kHz mono speech from audio at `rate` (mono, ±1): resampled, and its peak brought
/// to 80 % of full scale (at most 30 dB of gain, so a near-silent recording is
/// refused rather than amplified into noise).
pub fn speech_from_audio(audio: &[f32], rate: f64) -> Result<Vec<i16>, String> {
    if audio.is_empty() {
        return Err("no audio".into());
    }
    let x = decdsss_dsp::resample::resample_real(audio, rate, 8000.0, 3500.0, 4000.0);
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak < 1e-4 {
        return Err("it is silent".into());
    }
    let gain = (0.8 / peak).min(31.6);
    Ok(x.iter().map(|&v| (v * gain * 32767.0) as i16).collect())
}

/// How a speech transmission is sent.
#[derive(Clone, Debug)]
pub struct VoiceSpec {
    pub profile: Profile,
    pub mode: Mode,
    pub depth: usize,
    pub call: String,
    pub session: u8,
    pub codec: Codec,
    /// Seconds between preambles.
    pub resync_s: f64,
}

/// A planned speech transmission.
#[derive(Clone, Debug)]
pub struct VoicePlan {
    pub segments: Vec<Segment>,
    /// Modem frames in all.
    pub frames: usize,
    /// Codec frames in a modem frame.
    pub per_frame: usize,
    /// Seconds of speech sent (rounded up to whole modem frames).
    pub speech_seconds: f64,
    /// Seconds of signal, preambles and headers included.
    pub air_seconds: f64,
    /// Speech seconds one second of signal carries in the payload (below 1,
    /// the receiver is always catching up): the mode's throughput over the
    /// codec's rate.
    pub throughput_ratio: f64,
}

/// Codec frames in a modem frame of `frame_bytes` data bytes.
fn per_frame(frame_bytes: usize, bits: usize) -> usize {
    frame_bytes * 8 / bits
}

/// Compresses `speech` and cuts it into segments of modem frames.
pub fn plan_voice(spec: &VoiceSpec, speech: &[i16]) -> Result<VoicePlan, String> {
    let ptype = ptype_of(spec.codec).ok_or_else(|| {
        format!(
            "Codec2 {} is not a voice mode here (use 450, 700C, 1200 or 1600)",
            spec.codec
        )
    })?;
    if speech.is_empty() {
        return Err("no speech to send".into());
    }
    let layout = Layout::new(spec.profile, spec.mode, spec.depth);
    let fb = layout.frame_bytes();
    let mut enc = Codec2::new(spec.codec).ok_or("cannot start Codec2")?;
    let (n, bits) = (enc.samples_per_frame(), enc.bits_per_frame());
    let per = per_frame(fb, bits);
    if per == 0 {
        return Err("a modem frame is smaller than a codec frame".into());
    }
    // Whole modem frames of speech: the end is padded with silence.
    let group = n * per;
    let mut pcm = speech.to_vec();
    pcm.resize(pcm.len().div_ceil(group) * group, 0);
    let coded: Vec<Vec<u8>> = pcm.chunks_exact(n).map(|s| enc.encode(s)).collect();
    let frames: Vec<Vec<u8>> = coded
        .chunks(per)
        .map(|g| {
            let mut data = pack_frames(g, bits);
            data.resize(fb, 0);
            data
        })
        .collect();
    if frames.len() > (1 << 15) - 1 {
        return Err(format!(
            "{} frames is more than one transmission can number",
            frames.len()
        ));
    }
    let frames_hdr = Frames {
        profile: spec.profile,
        mode: spec.mode,
        depth: spec.depth,
        ptype,
        call: &spec.call,
        session: ptype.fit_session(spec.session),
        resync_s: spec.resync_s,
    };
    let segments = cut_segments(&frames_hdr, &frames);
    let speech_seconds = pcm.len() as f64 / f64::from(SPEECH_RATE);
    let air_seconds = crate::send::air_time(&frames_hdr, frames.len());
    let payload_rate = (per * bits) as f64 / layout.seconds() * spec.depth as f64;
    let codec_rate = bits as f64 / enc.frame_seconds();
    Ok(VoicePlan {
        frames: frames.len(),
        per_frame: per,
        speech_seconds,
        air_seconds,
        throughput_ratio: payload_rate / codec_rate,
        segments,
    })
}

/// Speech from the data of modem frames in order (`None` for one that did not
/// arrive, which becomes silence of the same length). Also the number of frames
/// missing.
pub fn render_speech(codec: Codec, frames: &[Option<&[u8]>]) -> (Vec<i16>, usize) {
    let Some(mut dec) = Codec2::new(codec) else {
        return (Vec::new(), frames.len());
    };
    let (n, bits) = (dec.samples_per_frame(), dec.bits_per_frame());
    let bytes = bits.div_ceil(8);
    let mut out = Vec::new();
    let mut lost = 0;
    // The frame size is the length of the first frame that is there.
    let per = frames
        .iter()
        .flatten()
        .next()
        .map(|f| per_frame(f.len(), bits));
    let Some(per) = per else {
        return (Vec::new(), frames.len());
    };
    for f in frames {
        match f {
            Some(data) => {
                for c in unpack_frames(data, bits, bytes, per) {
                    out.extend(dec.decode(&c));
                }
            }
            None => {
                lost += 1;
                out.extend(std::iter::repeat_n(0i16, n * per));
            }
        }
    }
    (out, lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voiced-speech-like test signal: a pulse train through formants.
    fn speech(seconds: f64) -> Vec<i16> {
        let n = (seconds * 8000.0) as usize;
        let mut x = vec![0.0f64; n];
        let mut phase = 0.0;
        for (i, s) in x.iter_mut().enumerate() {
            phase += (110.0 + 20.0 * (i as f64 / 4000.0).sin()) / 8000.0;
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

    fn spec(profile: Profile, codec: Codec) -> VoiceSpec {
        VoiceSpec {
            profile,
            mode: voice_mode(profile, codec)
                .or_else(|| Mode::voice(profile))
                .unwrap(),
            depth: 1,
            call: "VK2ABC".into(),
            session: 2,
            codec,
            resync_s: 60.0,
        }
    }

    #[test]
    fn speech_from_audio_resamples_and_levels() {
        // A 1 kHz tone at 48 kHz, quiet: comes out at 8 kHz, peak 80 %.
        let tone: Vec<f32> = (0..48_000)
            .map(|i| 0.1 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let s = speech_from_audio(&tone, 48_000.0).unwrap();
        assert_eq!(s.len(), 8000);
        let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!((26_000..26_400).contains(&peak), "{peak}");
        assert!(speech_from_audio(&vec![0.0; 1000], 8000.0).is_err());
        assert!(speech_from_audio(&[], 8000.0).is_err());
    }

    #[test]
    fn codecs_and_payload_types_go_together() {
        for c in CODECS {
            assert_eq!(codec_of(ptype_of(c).unwrap()), Some(c));
        }
        // The other Codec2 rates are in the vendored core but not offered.
        for c in [Codec::C3200, Codec::C2400, Codec::C1400, Codec::C1300] {
            assert_eq!(ptype_of(c), None);
        }
        assert_eq!(codec_of(PayloadType::File), None);
        assert_eq!(default_codec(Profile::W2400), Some(Codec::C450));
        assert_eq!(default_codec(Profile::W3000), Some(Codec::C700C));
        assert_eq!(default_codec(Profile::N500), None);
    }

    #[test]
    fn speech_becomes_whole_frames_and_comes_back() {
        for (profile, codec) in [
            (Profile::W2400, Codec::C450),
            (Profile::W3000, Codec::C700C),
            (Profile::W2400, Codec::C1200),
            (Profile::W2400, Codec::C1600),
            (Profile::W3000, Codec::C1200),
            (Profile::W3000, Codec::C1600),
        ] {
            let s = speech(6.0);
            let p = plan_voice(&spec(profile, codec), &s).unwrap();
            assert!(
                p.speech_seconds >= 6.0 && p.speech_seconds < 6.0 + 2.5,
                "{}",
                p.speech_seconds
            );
            assert!(p.per_frame >= 8, "{}", p.per_frame);
            let frames: Vec<Option<Vec<u8>>> = p
                .segments
                .iter()
                .flat_map(|s| s.frames.iter().cloned().map(Some))
                .collect();
            assert_eq!(frames.len(), p.frames);
            let refs: Vec<Option<&[u8]>> = frames.iter().map(|f| f.as_deref()).collect();
            let (back, lost) = render_speech(codec, &refs);
            assert_eq!(lost, 0);
            assert_eq!(back.len() as f64 / 8000.0, p.speech_seconds);
            // The same as running the codec straight through, up to the codec's
            // own random phases for noise-like parts (it draws on the C library's
            // rand(), so two runs are not sample for sample the same): frame by
            // frame the level agrees.
            let mut e = Codec2::new(codec).unwrap();
            let mut d = Codec2::new(codec).unwrap();
            let n = e.samples_per_frame();
            let mut pcm = s.clone();
            pcm.resize(back.len(), 0);
            let direct: Vec<i16> = pcm
                .chunks_exact(n)
                .flat_map(|c| d.decode(&e.encode(c)))
                .collect();
            assert_eq!(back.len(), direct.len());
            let level = |x: &[i16]| {
                (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
            };
            // Only the speech itself: the padding after it is silence, which the faster
            // codecs decode to a few counts of noise drawn from the C library's rand()
            // (shared by every test thread), so its level differs from run to run.
            let speech_frames = s.len() / n;
            for (a, b) in back
                .chunks_exact(n)
                .zip(direct.chunks_exact(n))
                .take(speech_frames)
                .skip(10)
            {
                let (la, lb) = (level(a), level(b));
                assert!(
                    la > 0.0 && (0.8..1.25).contains(&(lb / la)),
                    "{codec}: frame level {la:.0} against {lb:.0}"
                );
            }
        }
    }

    #[test]
    fn a_lost_frame_is_a_silent_gap_of_its_length() {
        let s = speech(8.0);
        let p = plan_voice(&spec(Profile::W2400, Codec::C450), &s).unwrap();
        let all: Vec<Vec<u8>> = p.segments.iter().flat_map(|s| s.frames.clone()).collect();
        assert!(all.len() >= 4);
        let refs: Vec<Option<&[u8]>> = all
            .iter()
            .enumerate()
            .map(|(i, f)| (i != 2).then_some(f.as_slice()))
            .collect();
        let (out, lost) = render_speech(Codec::C450, &refs);
        assert_eq!(lost, 1);
        assert_eq!(out.len() as f64 / 8000.0, p.speech_seconds);
        let per_s = (p.speech_seconds / p.frames as f64 * 8000.0) as usize;
        assert!(out[2 * per_s..3 * per_s].iter().all(|&v| v == 0));
        assert!(out[per_s..2 * per_s].iter().any(|&v| v != 0));
    }

    #[test]
    fn the_voice_modes_keep_up_with_their_codecs_only_just() {
        // Throughput of the payload over the codec's rate: just above 1 on the
        // 2.4 kHz mode at depth 1, which is why a listener waits for the
        // preamble and the first blocks before the speech can run.
        for (profile, codec) in [
            (Profile::W2400, Codec::C450),
            (Profile::W3000, Codec::C700C),
        ] {
            let p = plan_voice(&spec(profile, codec), &speech(10.0)).unwrap();
            assert!(
                p.throughput_ratio > 1.0 && p.throughput_ratio < 1.4,
                "{profile}: {}",
                p.throughput_ratio
            );
            assert!(p.air_seconds > p.speech_seconds / p.throughput_ratio);
        }
        assert!(plan_voice(&spec(Profile::W2400, Codec::C1300), &speech(1.0)).is_err());
        assert!(plan_voice(&spec(Profile::W2400, Codec::C450), &[]).is_err());
    }

    #[test]
    fn modes_for_the_faster_codecs_keep_up() {
        // The table in `voice_mode`'s documentation, from the plan itself: the
        // payload runs at least 1.15 times the codec's rate (the older pairs, 450
        // on 2.4 kHz and 700C on 3 kHz, 1.17 and 1.05).
        for (profile, codec, mode, ratio) in [
            (Profile::W2400, Codec::C1200, "ds1-1/2", 1.27),
            (Profile::W2400, Codec::C1600, "ds1-2/3", 1.27),
            (Profile::W3000, Codec::C1200, "ds2-3/4", 1.23),
            (Profile::W3000, Codec::C1600, "ds1-1/2", 1.155),
        ] {
            let m = voice_mode(profile, codec).unwrap();
            assert_eq!(m, Mode::from_name(mode).unwrap(), "{profile} {codec}");
            let p = plan_voice(&spec(profile, codec), &speech(10.0)).unwrap();
            assert!(
                (p.throughput_ratio - ratio).abs() < 0.005,
                "{profile} {codec} {m}: {}",
                p.throughput_ratio
            );
            assert!(p.throughput_ratio >= 1.15, "{}", p.throughput_ratio);
        }
        // The pairs that would not keep up are not suggested.
        for (profile, codec, mode) in [
            (Profile::W2400, Codec::C1200, "ds2-3/4"),
            (Profile::W2400, Codec::C1600, "ds1-1/2"),
            (Profile::W3000, Codec::C1600, "ds2-3/4"),
        ] {
            let mut sp = spec(profile, codec);
            sp.mode = Mode::from_name(mode).unwrap();
            let p = plan_voice(&sp, &speech(10.0)).unwrap();
            assert!(
                p.throughput_ratio < 1.0,
                "{profile} {codec} {mode}: {}",
                p.throughput_ratio
            );
            assert_ne!(voice_mode(profile, codec), Some(sp.mode));
        }
        assert_eq!(
            voice_mode(Profile::W2400, Codec::C450),
            Mode::voice(Profile::W2400)
        );
        assert_eq!(voice_mode(Profile::N500, Codec::C1200), None);
    }

    #[test]
    fn a_wide_header_gets_a_session_it_can_hold() {
        // Codec2 1600's payload type needs the wide header: sessions 0 to 31.
        let mut sp = spec(Profile::W2400, Codec::C1600);
        sp.session = 45;
        let p = plan_voice(&sp, &speech(3.0)).unwrap();
        assert_eq!(p.segments[0].header.session, 45 & 31);
        assert!(p.segments[0].header.pack().is_some());
    }
}
