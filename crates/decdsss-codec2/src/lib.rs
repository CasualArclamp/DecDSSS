// made by CasualArclamp
//! Codec2, David Rowe's low-bit-rate speech codec, from the vendored C core in `c/`
//! (LGPL-2.1; see the crate's README for what was changed to build it on MSVC).
//! Speech is 8 kHz, 16-bit, mono; a frame of `samples_per_frame` samples becomes
//! `bits_per_frame` bits.
//!
//! Voice on these channels needs the slowest modes: 700C (700 bit/s) and 450
//! (450 bit/s). [`pack_frames`] and [`unpack_frames`] put the codec's frames
//! back to back in a bit stream, without the padding to whole bytes that the
//! codec's own byte interface leaves (3 of 28 bits at 700C, 6 of 24 at 450).

use std::ffi::c_int;

#[allow(non_camel_case_types)]
#[repr(C)]
struct CODEC2 {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn codec2_create(mode: c_int) -> *mut CODEC2;
    fn codec2_destroy(state: *mut CODEC2);
    fn codec2_encode(state: *mut CODEC2, bytes: *mut u8, speech: *mut i16);
    fn codec2_decode(state: *mut CODEC2, speech: *mut i16, bytes: *const u8);
    fn codec2_samples_per_frame(state: *mut CODEC2) -> c_int;
    fn codec2_bits_per_frame(state: *mut CODEC2) -> c_int;
    fn codec2_bytes_per_frame(state: *mut CODEC2) -> c_int;
}

/// Sample rate of the speech, Hz.
pub const SPEECH_RATE: u32 = 8000;

/// The codec modes this crate offers (all 8 kHz speech).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// 3200 bit/s.
    C3200,
    /// 2400 bit/s.
    C2400,
    /// 1600 bit/s.
    C1600,
    /// 1400 bit/s.
    C1400,
    /// 1300 bit/s.
    C1300,
    /// 1200 bit/s.
    C1200,
    /// 700 bit/s, the "newamp1" rate-K codec.
    C700C,
    /// 450 bit/s ("newamp2"), the lowest rate; removed from Codec2 after 1.2.0 but
    /// kept here because the 2.4 kHz channel cannot carry 700.
    C450,
}

impl Mode {
    pub const ALL: [Mode; 8] = [
        Mode::C3200,
        Mode::C2400,
        Mode::C1600,
        Mode::C1400,
        Mode::C1300,
        Mode::C1200,
        Mode::C700C,
        Mode::C450,
    ];

    fn id(self) -> c_int {
        match self {
            Mode::C3200 => 0,
            Mode::C2400 => 1,
            Mode::C1600 => 2,
            Mode::C1400 => 3,
            Mode::C1300 => 4,
            Mode::C1200 => 5,
            Mode::C700C => 8,
            Mode::C450 => 10,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::C3200 => "3200",
            Mode::C2400 => "2400",
            Mode::C1600 => "1600",
            Mode::C1400 => "1400",
            Mode::C1300 => "1300",
            Mode::C1200 => "1200",
            Mode::C700C => "700C",
            Mode::C450 => "450",
        }
    }

    pub fn from_name(s: &str) -> Option<Mode> {
        let s = s.trim().to_ascii_lowercase();
        Mode::ALL
            .into_iter()
            .find(|m| m.name().to_ascii_lowercase() == s)
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One direction's codec state. Encoding and decoding each keep history, so use
/// one `Codec2` to encode a stream and another to decode one.
pub struct Codec2 {
    raw: *mut CODEC2,
    mode: Mode,
    samples: usize,
    bits: usize,
    bytes: usize,
}

// The state is owned by this struct alone and the C code keeps no globals that
// are written after creation, so it may move between threads (it is not Sync).
unsafe impl Send for Codec2 {}

impl Codec2 {
    pub fn new(mode: Mode) -> Option<Codec2> {
        // SAFETY: codec2_create allocates a state or returns null.
        let raw = unsafe { codec2_create(mode.id()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a live state.
        let (samples, bits, bytes) = unsafe {
            (
                codec2_samples_per_frame(raw),
                codec2_bits_per_frame(raw),
                codec2_bytes_per_frame(raw),
            )
        };
        Some(Codec2 {
            raw,
            mode,
            samples: samples as usize,
            bits: bits as usize,
            bytes: bytes as usize,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Speech samples (8 kHz) in a frame.
    pub fn samples_per_frame(&self) -> usize {
        self.samples
    }

    /// Bits a frame compresses to.
    pub fn bits_per_frame(&self) -> usize {
        self.bits
    }

    /// Duration of a frame, seconds.
    pub fn frame_seconds(&self) -> f64 {
        self.samples as f64 / f64::from(SPEECH_RATE)
    }

    /// Compresses one frame of speech. `speech` must hold exactly
    /// `samples_per_frame` samples. The result is the codec's own byte form
    /// (`bits_per_frame` bits, the last byte padded); see [`pack_frames`].
    pub fn encode(&mut self, speech: &[i16]) -> Vec<u8> {
        assert_eq!(speech.len(), self.samples, "one whole frame of speech");
        let mut input = speech.to_vec();
        let mut out = vec![0u8; self.bytes];
        // SAFETY: `input` holds samples_per_frame samples and `out` bytes_per_frame
        // bytes, which is what the codec reads and writes for this mode.
        unsafe { codec2_encode(self.raw, out.as_mut_ptr(), input.as_mut_ptr()) };
        out
    }

    /// Expands one frame (`bytes_per_frame` bytes, as [`Codec2::encode`] gives
    /// them) into speech.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<i16> {
        assert_eq!(bytes.len(), self.bytes, "one whole frame of bytes");
        let mut out = vec![0i16; self.samples];
        // SAFETY: as in `encode`, with the roles of the buffers swapped.
        unsafe { codec2_decode(self.raw, out.as_mut_ptr(), bytes.as_ptr()) };
        out
    }
}

impl Drop for Codec2 {
    fn drop(&mut self) {
        // SAFETY: `raw` came from codec2_create and is destroyed once.
        unsafe { codec2_destroy(self.raw) };
    }
}

/// Bit `i` (0 = most significant of byte 0) of `bytes`.
fn bit(bytes: &[u8], i: usize) -> u8 {
    (bytes[i / 8] >> (7 - i % 8)) & 1
}

/// Puts encoded frames end to end as a bit stream (MSB first), each contributing
/// only its `bits` bits. The last byte is zero-padded.
pub fn pack_frames(frames: &[Vec<u8>], bits: usize) -> Vec<u8> {
    let mut out = vec![0u8; (frames.len() * bits).div_ceil(8)];
    let mut n = 0;
    for f in frames {
        for i in 0..bits {
            out[n / 8] |= bit(f, i) << (7 - n % 8);
            n += 1;
        }
    }
    out
}

/// The inverse of [`pack_frames`]: `count` frames of `bits` bits, each padded to
/// `bytes_per_frame` bytes as the codec wants them. A stream too short for
/// `count` frames gives fewer.
pub fn unpack_frames(
    stream: &[u8],
    bits: usize,
    bytes_per_frame: usize,
    count: usize,
) -> Vec<Vec<u8>> {
    let have = (stream.len() * 8) / bits.max(1);
    (0..count.min(have))
        .map(|k| {
            let mut f = vec![0u8; bytes_per_frame];
            for i in 0..bits {
                f[i / 8] |= bit(stream, k * bits + i) << (7 - i % 8);
            }
            f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Voiced speech-like audio: a glottal pulse train at `f0` through three
    /// formant resonators, with the pitch drifting a little.
    fn vowel(seconds: f64, f0: f64) -> Vec<i16> {
        let n = (seconds * f64::from(SPEECH_RATE)) as usize;
        let mut x = vec![0.0f64; n];
        let mut phase = 0.0;
        for (i, s) in x.iter_mut().enumerate() {
            let f = f0 * (1.0 + 0.05 * (i as f64 / 8000.0 * 2.0).sin());
            phase += f / 8000.0;
            if phase >= 1.0 {
                phase -= 1.0;
                *s = 1.0;
            }
        }
        // Three two-pole resonators: formants of an "ah".
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
        x.iter().map(|v| (v / peak * 9000.0) as i16).collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Fundamental frequency from the autocorrelation peak in 60–400 Hz.
    fn pitch(x: &[i16]) -> f64 {
        let (lo, hi) = (20, 133);
        let mut best = (0.0, lo);
        for lag in lo..=hi {
            let c: f64 = x
                .iter()
                .zip(&x[lag..])
                .map(|(&a, &b)| f64::from(a) * f64::from(b))
                .sum();
            if c > best.0 {
                best = (c, lag);
            }
        }
        8000.0 / best.1 as f64
    }

    #[test]
    fn frame_sizes_of_the_slow_modes() {
        let c = Codec2::new(Mode::C700C).unwrap();
        assert_eq!((c.samples_per_frame(), c.bits_per_frame()), (320, 28));
        let c = Codec2::new(Mode::C450).unwrap();
        assert_eq!(c.bits_per_frame(), 18);
        for m in Mode::ALL {
            let c = Codec2::new(m).unwrap();
            // Bit rate = bits per frame over the frame's duration.
            let rate = c.bits_per_frame() as f64 / c.frame_seconds();
            let want: f64 = m
                .name()
                .trim_end_matches('C')
                .parse()
                .expect("the mode's name is its rate");
            assert!((rate - want).abs() / want < 0.05, "{m}: {rate} vs {want}");
        }
        assert_eq!(Mode::from_name("700c"), Some(Mode::C700C));
        assert_eq!(Mode::from_name("12"), None);
    }

    #[test]
    fn frames_pack_to_a_bit_stream_and_back() {
        for m in [Mode::C700C, Mode::C450] {
            let mut enc = Codec2::new(m).unwrap();
            let speech = vowel(1.0, 120.0);
            let frames: Vec<Vec<u8>> = speech
                .chunks_exact(enc.samples_per_frame())
                .map(|s| enc.encode(s))
                .collect();
            let bits = enc.bits_per_frame();
            let stream = pack_frames(&frames, bits);
            assert_eq!(stream.len(), (frames.len() * bits).div_ceil(8));
            let back = unpack_frames(&stream, bits, frames[0].len(), frames.len());
            assert_eq!(back.len(), frames.len());
            // Padding bits of the codec's own bytes are not guaranteed zero, so
            // compare the bits that count.
            for (a, b) in frames.iter().zip(&back) {
                assert!((0..bits).all(|i| bit(a, i) == bit(b, i)), "{m}");
            }
        }
    }

    #[test]
    fn speech_survives_the_slow_modes() {
        let speech = vowel(2.0, 120.0);
        for m in [Mode::C700C, Mode::C450] {
            let mut enc = Codec2::new(m).unwrap();
            let mut dec = Codec2::new(m).unwrap();
            let n = enc.samples_per_frame();
            let mut out = Vec::new();
            for s in speech.chunks_exact(n) {
                let bytes = enc.encode(s);
                out.extend(dec.decode(&bytes));
            }
            assert_eq!(out.len(), speech.len() / n * n);
            // Skip the first half second (the codec's start-up), compare the rest.
            let (a, b) = (&speech[4000..], &out[4000..]);
            let ratio = rms(b) / rms(a);
            assert!(
                (0.3..3.0).contains(&ratio),
                "{m}: output level {ratio:.2} of input"
            );
            let (pa, pb) = (pitch(a), pitch(b));
            assert!(
                (pb / pa - 1.0).abs() < 0.08,
                "{m}: pitch {pa:.0} Hz in, {pb:.0} Hz out"
            );
        }
    }

    #[test]
    fn the_same_speech_gives_the_same_bits() {
        let speech = vowel(0.8, 140.0);
        let run = || {
            let mut e = Codec2::new(Mode::C700C).unwrap();
            let n = e.samples_per_frame();
            speech
                .chunks_exact(n)
                .flat_map(|s| e.encode(s))
                .collect::<Vec<u8>>()
        };
        assert_eq!(run(), run());
    }
}
