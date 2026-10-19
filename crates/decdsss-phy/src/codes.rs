// made by CasualArclamp
//! Spreading and scrambling codes: m-sequences, the degree-15 Gold family
//! that scrambles every chip, the CSK base sequences and the preamble codes.

use num_complex::Complex32;
use std::sync::OnceLock;

/// A primitive polynomial x^n + Σ x^j, as the degree and the exponents j
/// below n that are present (0 is the "+ 1"). The sequence a obeying
/// a[i+n] = XOR_j a[i+j] is then an m-sequence of period 2^n − 1.
#[derive(Clone, Copy, Debug)]
pub struct Poly {
    pub degree: u32,
    pub taps: &'static [u32],
}

/// x^5 + x^2 + 1.
pub const POLY5: Poly = Poly {
    degree: 5,
    taps: &[0, 2],
};
/// x^6 + x + 1.
pub const POLY6: Poly = Poly {
    degree: 6,
    taps: &[0, 1],
};
/// x^7 + x + 1.
pub const POLY7: Poly = Poly {
    degree: 7,
    taps: &[0, 1],
};
/// x^8 + x^4 + x^3 + x^2 + 1.
pub const POLY8: Poly = Poly {
    degree: 8,
    taps: &[0, 2, 3, 4],
};
/// x^9 + x^4 + 1.
pub const POLY9: Poly = Poly {
    degree: 9,
    taps: &[0, 4],
};
/// x^15 + x + 1: the scrambler's Gold family is built on it.
pub const POLY15: Poly = Poly {
    degree: 15,
    taps: &[0, 1],
};

/// One period of the m-sequence of `p` as bits (0/1), starting from the
/// state 0…01 (a[0] = 1, the rest 0).
pub fn m_sequence(p: Poly) -> Vec<u8> {
    let n = p.degree as usize;
    let len = (1usize << n) - 1;
    let mut a = vec![0u8; len + n];
    a[0] = 1;
    for i in 0..len {
        let mut b = 0;
        for &j in p.taps {
            b ^= a[i + j as usize];
        }
        a[i + n] = b;
    }
    a.truncate(len);
    a
}

/// Bits 0/1 to chips +1/−1.
pub fn bipolar(bits: &[u8]) -> Vec<f32> {
    bits.iter().map(|&b| 1.0 - 2.0 * f32::from(b)).collect()
}

/// Period of the Gold family (2^15 − 1 chips).
pub const GOLD_LEN: usize = (1 << 15) - 1;

/// Gold code 0: the call code every station shares for preambles' headers.
pub const CALL_CODE: u32 = 0;

/// The two m-sequences of the preferred pair: a from x^15 + x + 1 and b, its
/// decimation by 3. For odd n, decimating by 2^k + 1 with gcd(n, k) = 1 gives
/// a preferred pair, so the family's cross-correlations are three-valued:
/// −1 and −1 ± 2^((n+1)/2) (−1, −257, 255 for n = 15).
fn gold_pair() -> &'static (Vec<u8>, Vec<u8>) {
    // `OnceLock` computes the value the first time it is asked for and then
    // hands out the same reference: a lazily built, thread-safe global.
    static PAIR: OnceLock<(Vec<u8>, Vec<u8>)> = OnceLock::new();
    PAIR.get_or_init(|| {
        let a = m_sequence(POLY15);
        let b = (0..GOLD_LEN).map(|i| a[(3 * i) % GOLD_LEN]).collect();
        (a, b)
    })
}

/// Bit `i` of Gold code `code`: a[i] ⊕ b[i + code].
pub fn gold_bit(code: u32, i: usize) -> u8 {
    let (a, b) = gold_pair();
    a[i % GOLD_LEN] ^ b[(i + code as usize) % GOLD_LEN]
}

/// A station's scrambler: QPSK chips (±1 ± j)/√2 with I from Gold code
/// `code` and Q from the same code half a period later. It restarts at each
/// frame boundary; `chip(i)` is the i-th chip after that.
#[derive(Clone, Debug)]
pub struct Scrambler {
    pub code: u32,
}

impl Scrambler {
    pub fn new(code: u32) -> Scrambler {
        assert!((code as usize) < GOLD_LEN);
        Scrambler { code }
    }

    pub fn chip(&self, i: usize) -> Complex32 {
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let re = 1.0 - 2.0 * f32::from(gold_bit(self.code, i));
        let im = 1.0 - 2.0 * f32::from(gold_bit(self.code, i + GOLD_LEN / 2));
        Complex32::new(re * k, im * k)
    }

    /// `n` chips from `start`.
    pub fn chips(&self, start: usize, n: usize) -> Vec<Complex32> {
        (start..start + n).map(|i| self.chip(i)).collect()
    }
}

/// Chip `n` of Walsh–Hadamard code `m` (natural order): (−1)^popcount(m ∧ n).
/// The L codes of length L are exactly orthogonal; CSK sends code `m` to
/// carry the symbol value m, under the station's scrambler.
pub fn walsh_chip(m: usize, n: usize) -> f32 {
    if (m & n).count_ones().is_multiple_of(2) {
        1.0
    } else {
        -1.0
    }
}

/// The fast Walsh–Hadamard transform in place: afterwards x[m] is the
/// correlation of the input with Walsh code m. Length a power of two.
pub fn fwht(x: &mut [Complex32]) {
    let n = x.len();
    debug_assert!(n.is_power_of_two());
    let mut h = 1;
    while h < n {
        for i in (0..n).step_by(2 * h) {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
        }
        h *= 2;
    }
}

/// The preamble m-sequence for a profile's chip rate: 511 chips on 2k4 and
/// 3k (266 / 213 ms a period), 127 on 500 Hz (318 ms).
pub fn preamble_code(profile: decdsss_core::Profile) -> &'static [f32] {
    static LONG: OnceLock<Vec<f32>> = OnceLock::new();
    static SHORT: OnceLock<Vec<f32>> = OnceLock::new();
    match profile {
        decdsss_core::Profile::N500 => SHORT.get_or_init(|| bipolar(&m_sequence(POLY7))),
        _ => LONG.get_or_init(|| bipolar(&m_sequence(POLY9))),
    }
}

/// The alphabet callsigns are packed in: pad, A–Z, 0–9, '/'.
const CALL_ALPHABET: &[u8; 38] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/";
/// Callsigns are at most 9 characters: 38^9 < 2^48.
pub const CALL_MAX: usize = 9;
pub const CALL_BITS: u32 = 48;

/// A callsign normalised for packing and hashing: trimmed, upper case.
/// `None` if it is empty, too long or has a character outside the alphabet.
pub fn normalise_call(call: &str) -> Option<String> {
    let c = call.trim().to_ascii_uppercase();
    let ok = !c.is_empty()
        && c.len() <= CALL_MAX
        && c.bytes().all(|b| b != b' ' && CALL_ALPHABET.contains(&b));
    ok.then_some(c)
}

/// Packs a callsign into 48 bits (base 38, first character most significant).
pub fn pack_call(call: &str) -> Option<u64> {
    let c = normalise_call(call)?;
    let mut v = 0u64;
    for i in 0..CALL_MAX {
        let sym = c
            .as_bytes()
            .get(i)
            .map(|b| CALL_ALPHABET.iter().position(|a| a == b).unwrap())
            .unwrap_or(0);
        v = v * 38 + sym as u64;
    }
    Some(v)
}

pub fn unpack_call(mut v: u64) -> Option<String> {
    let mut chars = [b' '; CALL_MAX];
    for i in (0..CALL_MAX).rev() {
        chars[i] = CALL_ALPHABET[(v % 38) as usize];
        v /= 38;
    }
    if v != 0 {
        return None;
    }
    let s = String::from_utf8_lossy(&chars).trim_end().to_string();
    normalise_call(&s)
}

/// A station's own Gold code: FNV-1a (32-bit) of the normalised callsign,
/// mapped onto 1 ..= GOLD_LEN − 1 so it never lands on the call code.
pub fn own_code(call: &str) -> Option<u32> {
    let c = normalise_call(call)?;
    let mut h: u32 = 0x811c_9dc5;
    for b in c.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    Some(1 + h % (GOLD_LEN as u32 - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Primitive ⇔ the state first comes back after exactly 2^n − 1 steps,
    /// which shows as a two-valued periodic autocorrelation (N, −1).
    #[test]
    fn m_sequences_are_maximal() {
        for p in [POLY5, POLY6, POLY7, POLY8, POLY9, POLY15] {
            let s = m_sequence(p);
            let n = s.len();
            assert_eq!(n, (1 << p.degree) - 1);
            // Balance: one more 1 than 0.
            let ones = s.iter().filter(|&&b| b == 1).count();
            assert_eq!(ones, n.div_ceil(2), "degree {}", p.degree);
            if p.degree <= 9 {
                let c = bipolar(&s);
                for t in 1..n {
                    let r: f32 = (0..n).map(|i| c[i] * c[(i + t) % n]).sum();
                    assert_eq!(r, -1.0, "degree {} lag {t}", p.degree);
                }
            }
        }
    }

    #[test]
    fn gold_cross_correlation_is_three_valued() {
        // Two family members against each other over a stretch of lags.
        let x: Vec<f32> = (0..GOLD_LEN)
            .map(|i| 1.0 - 2.0 * f32::from(gold_bit(5, i)))
            .collect();
        let y: Vec<f32> = (0..GOLD_LEN)
            .map(|i| 1.0 - 2.0 * f32::from(gold_bit(1234, i)))
            .collect();
        for t in (0..GOLD_LEN).step_by(997) {
            let r: f32 = (0..GOLD_LEN).map(|i| x[i] * y[(i + t) % GOLD_LEN]).sum();
            assert!([-1.0, -257.0, 255.0].contains(&r), "lag {t}: {r}");
        }
    }

    #[test]
    fn fwht_correlates_with_every_walsh_code() {
        for l in [32usize, 64, 128, 256] {
            for m in [0, 1, l / 2 + 3, l - 1] {
                let mut x: Vec<Complex32> = (0..l)
                    .map(|n| Complex32::new(walsh_chip(m, n), 0.0))
                    .collect();
                fwht(&mut x);
                for (k, v) in x.iter().enumerate() {
                    let want = if k == m { l as f32 } else { 0.0 };
                    assert_eq!(v.re, want, "L={l} m={m} k={k}");
                }
            }
        }
    }

    #[test]
    fn callsigns_pack_and_hash() {
        for c in ["VK2ABC", "vk2abc/p", "W1AW", "VK2ABC/MM", "9A1A"] {
            let v = pack_call(c).unwrap();
            assert!(v < 1 << CALL_BITS);
            assert_eq!(unpack_call(v).unwrap(), normalise_call(c).unwrap());
        }
        assert_eq!(pack_call("VK2ABCDEFG"), None);
        assert_eq!(pack_call("VK2 ABC"), None);
        assert_eq!(own_code("vk2abc"), own_code("VK2ABC"));
        assert_ne!(own_code("VK2ABC"), Some(CALL_CODE));
    }
}
