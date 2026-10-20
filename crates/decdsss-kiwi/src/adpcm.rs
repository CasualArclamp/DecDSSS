// made by CasualArclamp
//! IMA ADPCM decoding, as a KiwiSDR compresses its audio when asked to (4 bits
//! a sample, two to a byte, the low nibble first; the state runs on from frame
//! to frame). The step and index tables are the IMA's (Interactive Multimedia
//! Association, *Recommended Practices for Enhancing Digital Audio
//! Compatibility in Multimedia Systems*, 1992), the same ones every IMA ADPCM
//! codec carries. DecDSSS asks the Kiwi not to compress; this is for a Kiwi that
//! does anyway.

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const INDEX: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// The decoder's state: the last sample and the step index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Adpcm {
    prev: i32,
    index: i32,
}

impl Adpcm {
    /// One 4-bit code to a sample.
    pub fn decode_nibble(&mut self, n: u8) -> i16 {
        let step = STEPS[self.index as usize];
        let mut diff = step >> 3;
        if n & 1 != 0 {
            diff += step >> 2;
        }
        if n & 2 != 0 {
            diff += step >> 1;
        }
        if n & 4 != 0 {
            diff += step;
        }
        if n & 8 != 0 {
            diff = -diff;
        }
        self.prev = (self.prev + diff).clamp(-32768, 32767);
        self.index = (self.index + INDEX[usize::from(n & 7)]).clamp(0, 88);
        self.prev as i16
    }

    /// A frame's bytes to samples, two a byte, the low nibble first.
    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<i16>) {
        for &b in bytes {
            out.push(self.decode_nibble(b & 0x0F));
            out.push(self.decode_nibble(b >> 4));
        }
    }

    /// The matching encoder step, for the tests: the code that best follows `x`.
    #[cfg(test)]
    fn encode_sample(&mut self, x: i16) -> u8 {
        let step = STEPS[self.index as usize];
        let mut d = i32::from(x) - self.prev;
        let mut n = 0u8;
        if d < 0 {
            n = 8;
            d = -d;
        }
        let mut s = step;
        for bit in [4u8, 2, 1] {
            if d >= s {
                n |= bit;
                d -= s;
            }
            s >>= 1;
        }
        // Decode it to keep the state as the decoder's.
        self.decode_nibble(n);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_comes_back_through_encode_and_decode() {
        let x: Vec<i16> = (0..4000)
            .map(|i| (8000.0 * (i as f64 * 0.05).sin()) as i16)
            .collect();
        let mut enc = Adpcm::default();
        let bytes: Vec<u8> = x
            .chunks(2)
            .map(|p| enc.encode_sample(p[0]) | (enc.encode_sample(p[1]) << 4))
            .collect();
        let mut dec = Adpcm::default();
        let mut y = Vec::new();
        dec.decode(&bytes, &mut y);
        assert_eq!(y.len(), x.len());
        // After the step size has grown to the signal (the first few dozen
        // samples), ADPCM follows a tone within a few per cent of its peak.
        let err = x[200..]
            .iter()
            .zip(&y[200..])
            .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
            .max()
            .unwrap();
        assert!(err < 400, "{err}");
    }
}
