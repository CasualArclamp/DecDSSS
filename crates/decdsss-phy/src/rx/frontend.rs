// made by CasualArclamp
//! The receiver's front end: audio at the sound card's rate to complex
//! baseband at four samples a chip, matched-filtered.
//!
//! The real audio is mixed with e^{-jωt} at the profile's centre (the signal
//! lands at 0 Hz, its mirror image at −2·centre), a polyphase resampler picks
//! the signal band and rejects the image, and a root-raised-cosine filter
//! matches the transmitter's pulse. One front end runs per profile; they all
//! read the same audio.

use decdsss_core::Profile;
use decdsss_core::profile::ROLLOFF;
use decdsss_dsp::Iq;
use decdsss_dsp::fir::Fir;
use decdsss_dsp::interp::cubic;
use decdsss_dsp::resample::Resampler;
use decdsss_dsp::rrc::rrc_taps;

/// Samples a chip after the front end.
pub const SPS: usize = 4;

/// Largest carrier offset the front end keeps in band, Hz.
pub const MAX_OFFSET_HZ: f64 = 100.0;

pub struct Frontend {
    pub profile: Profile,
    sample_rate: f64,
    phase: f64,
    dphase: f64,
    resampler: Resampler,
    mf: Fir,
    /// Delay from the audio instant to the matched-filter output, output
    /// samples. (The resampler also needs about `half` input samples of
    /// lookahead before it can emit, which delays when output appears, not
    /// where the signal sits in it.)
    delay: f64,
}

impl Frontend {
    pub fn new(profile: Profile, sample_rate: f64) -> Frontend {
        let cr = f64::from(profile.chip_rate());
        let out_rate = SPS as f64 * cr;
        let centre = profile.centre_hz();
        // The signal is ±(1 + α)/2 chip rates wide; keep the offset margin too.
        let pass = (1.0 + ROLLOFF) / 2.0 * cr + MAX_OFFSET_HZ;
        // Stop at the nearer of the mixing image's edge and the output's alias.
        let image = 2.0 * centre - (1.0 + ROLLOFF) / 2.0 * cr;
        let stop = image
            .min(out_rate / 2.0 - pass)
            .min(sample_rate / 2.0 - 1.0);
        let resampler = Resampler::new(sample_rate, out_rate, pass, stop);
        // Unit-energy taps give the matched filter a peak gain of √SPS on a
        // chip; dividing by that makes a chip come out with its own amplitude.
        let norm = (SPS as f32).sqrt();
        let mf = Fir::new(
            rrc_taps(SPS as f64, ROLLOFF, 8)
                .into_iter()
                .map(|t| t / norm)
                .collect(),
        );
        // The resampler is centred (an output at time t reads the input around
        // t), so only the matched filter delays the signal.
        let delay = mf.delay() as f64;
        Frontend {
            profile,
            sample_rate,
            phase: 0.0,
            dphase: centre / sample_rate,
            resampler,
            mf,
            delay,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Output rate, Hz.
    pub fn out_rate(&self) -> f64 {
        SPS as f64 * f64::from(self.profile.chip_rate())
    }

    /// Where the signal sits in the output: its delay, output samples.
    pub fn delay(&self) -> f64 {
        self.delay
    }

    pub fn push(&mut self, audio: &[f32], out: &mut Vec<Iq>) {
        let mixed: Vec<Iq> = audio
            .iter()
            .map(|&a| {
                let (s, c) = (std::f64::consts::TAU * self.phase).sin_cos();
                self.phase = (self.phase + self.dphase).fract();
                // ×2: the positive-frequency half of a real signal carries half
                // its amplitude, so this keeps the chip amplitude.
                Iq::new(
                    (2.0 * f64::from(a) * c) as f32,
                    (-2.0 * f64::from(a) * s) as f32,
                )
            })
            .collect();
        let mut rs = Vec::with_capacity(mixed.len() * SPS * 2 / 25 + 8);
        self.resampler.process(&mixed, &mut rs);
        self.mf.process(&rs, out);
    }
}

/// Baseband samples with absolute indices, so everything downstream can speak
/// in "sample n of the stream" while the front of the buffer is released.
#[derive(Default)]
pub struct Buf {
    data: Vec<Iq>,
    /// Absolute index of `data[0]`.
    pub base: i64,
}

impl Buf {
    pub fn push(&mut self, x: &[Iq]) {
        self.data.extend_from_slice(x);
    }

    /// One past the last absolute index.
    pub fn end(&self) -> i64 {
        self.base + self.data.len() as i64
    }

    /// Releases everything before absolute index `i`.
    pub fn trim(&mut self, i: i64) {
        let n = (i - self.base).clamp(0, self.data.len() as i64) as usize;
        if n > 0 {
            self.data.drain(..n);
            self.base += n as i64;
        }
    }

    pub fn get(&self, i: i64) -> Iq {
        let k = i - self.base;
        if k < 0 || k >= self.data.len() as i64 {
            Iq::new(0.0, 0.0)
        } else {
            self.data[k as usize]
        }
    }

    /// The signal at fractional absolute position `pos` (cubic interpolation).
    pub fn at(&self, pos: f64) -> Iq {
        cubic(&self.data, pos - self.base as f64)
    }

    pub fn slice(&self, from: i64, to: i64) -> &[Iq] {
        let a = (from - self.base).clamp(0, self.data.len() as i64) as usize;
        let b = (to - self.base).clamp(0, self.data.len() as i64) as usize;
        &self.data[a..b]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes::{Scrambler, own_code};
    use crate::framing::{Header, PayloadType};
    use crate::tx::{Segment, chips_to_audio, segment_chips};
    use decdsss_core::Mode;

    /// The matched-filter output sampled at the chip instants reproduces the
    /// transmitted chips: find the delay by correlation with the preamble,
    /// then check the chips come back with the right amplitude and phase.
    #[test]
    fn matched_filter_output_reproduces_the_chips() {
        for profile in Profile::ALL {
            let header = Header {
                profile,
                mode: Mode::from_name("ds8-1/2").unwrap(),
                depth: 1,
                ptype: PayloadType::Test,
                call: "VK2ABC".into(),
                session: 0,
                frame0: 0,
                nblocks: 0,
                total_frames: 0,
            };
            let seg = Segment {
                header,
                frames: vec![],
            };
            let chips = segment_chips(&seg).unwrap();
            let audio = chips_to_audio(&chips, profile, 48_000.0, 0.2, 0.05, 0.05);
            let mut fe = Frontend::new(profile, 48_000.0);
            let mut bb = Vec::new();
            fe.push(&audio, &mut bb);
            let buf = {
                let mut b = Buf::default();
                b.push(&bb);
                b
            };
            // Chip k of the segment peaks near sample (lead + span/chip_rate +
            // k/chip_rate)·out_rate plus the front-end delay; scan the offset
            // in quarter-chip steps for the best match on 400 preamble chips.
            let cr = f64::from(profile.chip_rate());
            let nominal = (0.05 + 10.0 / cr) * fe.out_rate() + fe.delay();
            let mut best = (0.0f32, 0.0f64);
            for q in -40..=40 {
                let off = nominal + f64::from(q) * 0.25;
                let c: Iq = (0..400)
                    .map(|k| buf.at(off + k as f64 * SPS as f64) * chips[k].conj())
                    .sum();
                if c.norm() > best.0 {
                    best = (c.norm(), off);
                }
            }
            // Amplitude: unit chips come back with |.| ≈ 1·scale (√2·level·…).
            let off = best.1;
            let c: Iq = (0..400)
                .map(|k| buf.at(off + k as f64 * SPS as f64) * chips[k].conj())
                .sum::<Iq>()
                / 400.0;
            let level_gain = (std::f32::consts::SQRT_2 * 0.2) * 1.0; // shaper
            // The mix+filter chain restores the shaper's √2·level amplitude.
            assert!(
                (c.norm() - level_gain).abs() < 0.03,
                "{profile}: gain {} want {level_gain}, offset {}",
                c.norm(),
                off - nominal
            );
            assert!(c.im.abs() < 0.03 * c.re.abs(), "{profile}: phase {c}");
            assert!(
                (off - nominal).abs() < 6.0,
                "{profile}: offset {}",
                off - nominal
            );
            let _ = Scrambler::new(own_code("VK2ABC").unwrap());
        }
    }
}
