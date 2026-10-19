// made by CasualArclamp
//! Live voice: speech streamed while it is spoken, played as it arrives.
//!
//! A live transmission is one very long segment (the header's block count at its
//! most, no frame total) sent *continuously*: the transmitter never has a gap to
//! hold speech back in, so when no speech frame is ready for a block it sends a
//! filler. Every modem frame starts with a byte:
//!
//! - `0 ..= 253`: a speech frame, this number (wrapping) in the stream;
//! - [`END`]: the talker let go, no more speech;
//! - [`FILLER`]: nothing, to keep the signal going.
//!
//! After the byte, [`LiveEncoder`] packs as many Codec2 frames as fit (40 of 450,
//! 29 of 700C). The numbers let the receiver put silence where frames were lost
//! and ignore the fillers, so the speech runs at its own rate whatever the modem
//! does: [`LiveDecoder`] gives out PCM as frames arrive.
//!
//! The modem carries about 1.17 times the codec's rate (1.05 at 700C), so a
//! transmitter that starts with a backlog (the speech spoken during the preamble)
//! catches up with the talker and then sends a filler now and then.

use decdsss_codec2::{Codec2, Mode as Codec, SPEECH_RATE, pack_frames, unpack_frames};

/// The first byte of the frame that ends a live transmission.
pub const END: u8 = 0xFE;
/// The first byte of a frame with nothing in it.
pub const FILLER: u8 = 0xFF;
/// Speech frame numbers run 0 … `SEQ_MOD - 1` and wrap.
pub const SEQ_MOD: u16 = 254;

/// Codec frames in a live modem frame of `frame_bytes` bytes.
pub fn per_frame(frame_bytes: usize, bits: usize) -> usize {
    (frame_bytes.saturating_sub(1)) * 8 / bits
}

/// Turns a stream of speech into live modem frames.
pub struct LiveEncoder {
    codec: Codec2,
    frame_bytes: usize,
    per: usize,
    samples: Vec<i16>,
    /// Encoded codec frames not yet filling a modem frame.
    coded: Vec<Vec<u8>>,
    seq: u16,
}

impl LiveEncoder {
    pub fn new(codec: Codec, frame_bytes: usize) -> Option<LiveEncoder> {
        let c = Codec2::new(codec)?;
        let per = per_frame(frame_bytes, c.bits_per_frame());
        (per > 0).then_some(LiveEncoder {
            codec: c,
            frame_bytes,
            per,
            samples: Vec::new(),
            coded: Vec::new(),
            seq: 0,
        })
    }

    /// Seconds of speech in a modem frame.
    pub fn frame_seconds(&self) -> f64 {
        self.per as f64 * self.codec.frame_seconds()
    }

    fn frame(&mut self, kind: u8, codec_frames: &[Vec<u8>]) -> Vec<u8> {
        let mut data = vec![kind];
        data.extend(pack_frames(codec_frames, self.codec.bits_per_frame()));
        data.resize(self.frame_bytes, 0);
        data
    }

    /// Takes speech (8 kHz); returns the modem frames it completes.
    pub fn push(&mut self, pcm: &[i16]) -> Vec<Vec<u8>> {
        self.samples.extend_from_slice(pcm);
        let n = self.codec.samples_per_frame();
        let mut out = Vec::new();
        while self.samples.len() >= n {
            let chunk: Vec<i16> = self.samples.drain(..n).collect();
            self.coded.push(self.codec.encode(&chunk));
            if self.coded.len() == self.per {
                let group = std::mem::take(&mut self.coded);
                let kind = self.seq as u8;
                self.seq = (self.seq + 1) % SEQ_MOD;
                out.push(self.frame(kind, &group));
            }
        }
        out
    }

    /// The talker has let go: whatever speech is left (the last frame padded with
    /// silence) and then the end frame.
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let n = self.codec.samples_per_frame();
        if !self.samples.is_empty() || !self.coded.is_empty() {
            // Pad to a whole modem frame with silence.
            let have = self.samples.len() + self.coded.len() * n;
            let group = n * self.per;
            let pad = (group - have % group) % group;
            let mut tail = std::mem::take(&mut self.samples);
            tail.extend(std::iter::repeat_n(0i16, pad));
            out.extend(self.push(&tail));
        }
        let end = self.frame(END, &[]);
        out.push(end);
        out
    }

    /// A frame with nothing in it, for when no speech is ready.
    pub fn filler(&self) -> Vec<u8> {
        let mut data = vec![0u8; self.frame_bytes];
        data[0] = FILLER;
        data
    }
}

/// What a live frame gave.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Played {
    /// Speech (8 kHz), with silence for the frames that were lost before this one.
    pub pcm: Vec<i16>,
    /// Speech frames lost before this one (seen from the gap in the numbers).
    pub lost: usize,
    /// This was the end frame.
    pub ended: bool,
}

/// Turns live modem frames into speech, in order.
pub struct LiveDecoder {
    codec: Codec2,
    per: usize,
    /// The next speech frame number expected.
    next: Option<u16>,
    ended: bool,
}

impl LiveDecoder {
    pub fn new(codec: Codec) -> Option<LiveDecoder> {
        Some(LiveDecoder {
            codec: Codec2::new(codec)?,
            per: 0,
            next: None,
            ended: false,
        })
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Takes one frame's data (as the modem gave it). Fillers give nothing; a gap
    /// in the numbers gives silence of the lost frames' length first.
    pub fn push(&mut self, data: &[u8]) -> Played {
        let mut out = Played::default();
        let Some(&kind) = data.first() else {
            return out;
        };
        let bits = self.codec.bits_per_frame();
        if self.per == 0 {
            self.per = per_frame(data.len(), bits);
        }
        match kind {
            FILLER => {}
            END => {
                self.ended = true;
                out.ended = true;
            }
            k if u16::from(k) < SEQ_MOD => {
                let seq = u16::from(k);
                let gap = match self.next {
                    // The first frame heard is where we are; what came before
                    // it was before we joined.
                    None => 0,
                    Some(n) => ((seq + SEQ_MOD - n) % SEQ_MOD) as usize,
                };
                // A gap longer than a few frames is not lost audio so much as a
                // different stream (or a long fade): a few seconds at most of silence.
                let gap = gap.min(6);
                if gap > 0 {
                    out.lost = gap;
                    let n = self.codec.samples_per_frame() * self.per * gap;
                    out.pcm.extend(std::iter::repeat_n(0i16, n));
                }
                let bytes = bits.div_ceil(8);
                for f in unpack_frames(&data[1..], bits, bytes, self.per) {
                    out.pcm.extend(self.codec.decode(&f));
                }
                self.next = Some((seq + 1) % SEQ_MOD);
            }
            _ => {}
        }
        out
    }

    /// Seconds of speech in a modem frame (once one has been seen).
    pub fn frame_seconds(&self) -> f64 {
        self.per as f64 * self.codec.frame_seconds()
    }
}

/// Speech rate, Hz (what [`LiveDecoder`] gives and [`LiveEncoder`] takes).
pub const RATE: u32 = SPEECH_RATE;

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f64) -> Vec<i16> {
        let n = (seconds * 8000.0) as usize;
        let mut x = vec![0.0f64; n];
        let mut phase = 0.0;
        for (i, s) in x.iter_mut().enumerate() {
            phase += (120.0 + 20.0 * (i as f64 / 6000.0).sin()) / 8000.0;
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
    fn frames_are_numbered_filled_and_ended() {
        let mut enc = LiveEncoder::new(Codec::C450, 92).unwrap();
        assert!((enc.frame_seconds() - 1.6).abs() < 1e-9);
        // 10.5 s of speech in pieces: six whole modem frames so far.
        let speech = tone(10.5);
        let mut frames = Vec::new();
        for piece in speech.chunks(1234) {
            frames.extend(enc.push(piece));
        }
        assert_eq!(frames.len(), 6);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!((f.len(), f[0]), (92, i as u8));
        }
        // Letting go sends the rest, padded, then the end frame.
        let rest = enc.finish();
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0][0], 6);
        assert_eq!(rest[1][0], END);
        assert_eq!(enc.filler()[0], FILLER);
        assert_eq!(enc.filler().len(), 92);
    }

    #[test]
    fn the_speech_comes_back_in_order_with_gaps_for_lost_frames() {
        let mut enc = LiveEncoder::new(Codec::C450, 92).unwrap();
        let mut frames = enc.push(&tone(12.0));
        frames.extend(enc.finish());
        let mut dec = LiveDecoder::new(Codec::C450).unwrap();
        let (mut pcm, mut lost, mut ended) = (Vec::new(), 0, false);
        for (i, f) in frames.iter().enumerate() {
            // Frame 2 is lost, and a filler comes between 4 and 5.
            if i == 2 {
                continue;
            }
            if i == 5 {
                let p = dec.push(&enc.filler());
                assert_eq!(p, Played::default());
            }
            let p = dec.push(f);
            pcm.extend(p.pcm);
            lost += p.lost;
            ended |= p.ended;
        }
        assert!(ended && dec.ended());
        assert_eq!(lost, 1);
        // Eight speech frames sent (12 s is 7.5 of them, the last padded), one lost
        // and made up with silence: all eight frames' worth of speech in time.
        let per = (1.6 * 8000.0) as usize;
        assert_eq!(pcm.len(), 8 * per);
        assert!(
            pcm[2 * per..3 * per].iter().all(|&v| v == 0),
            "the gap is silent"
        );
        assert!(pcm[per..2 * per].iter().any(|&v| v != 0));
    }

    #[test]
    fn a_listener_who_joins_late_starts_where_it_is() {
        let mut enc = LiveEncoder::new(Codec::C450, 92).unwrap();
        let frames = enc.push(&tone(9.0));
        let mut dec = LiveDecoder::new(Codec::C450).unwrap();
        let p = dec.push(&frames[3]);
        assert_eq!(p.lost, 0, "frames before joining are not lost frames");
        assert_eq!(p.pcm.len(), (1.6 * 8000.0) as usize);
        let p = dec.push(&frames[4]);
        assert_eq!(p.lost, 0);
    }

    #[test]
    fn the_numbers_wrap_and_700c_fits_too() {
        let mut enc = LiveEncoder::new(Codec::C700C, 104).unwrap();
        // 29 codec frames of 700C at 40 ms: 1.16 s a modem frame.
        assert!((enc.frame_seconds() - 1.16).abs() < 1e-9);
        // (a little over 260 frames of speech, so rounding cannot drop one)
        let frames = enc.push(&tone(1.16 * 260.0 + 0.5));
        assert_eq!(frames.len(), 260);
        assert_eq!(frames[253][0], 253);
        assert_eq!(frames[254][0], 0, "wraps after 253");
        let mut dec = LiveDecoder::new(Codec::C700C).unwrap();
        let mut lost = 0;
        for f in &frames {
            lost += dec.push(f).lost;
        }
        assert_eq!(lost, 0, "a wrap is not a gap");
    }
}
