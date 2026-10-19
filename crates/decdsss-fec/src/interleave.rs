// made by CasualArclamp
//! A golden-ratio stride interleaver. The coding-order index sent at transmit
//! position p is p·d mod N, with d the integer nearest N/φ that is coprime to
//! N. By the three-gap theorem that is the most even spreading any stride can
//! give: any run of B consecutive transmitted bits comes from B coding
//! positions about N/B apart, and from every codeword of the block in equal
//! share. A fade that wipes out a run of the signal is thereby spread thinly
//! over several codewords instead of killing one.

/// The permutation `perm`: transmit position p carries coding index perm[p].
#[derive(Clone, Debug)]
pub struct Interleaver {
    perm: Vec<usize>,
}

impl Interleaver {
    /// An interleaver over `n` bits.
    pub fn new(n: usize) -> Interleaver {
        assert!(n > 0);
        let phi = (1.0 + 5f64.sqrt()) / 2.0;
        let mut d = ((n as f64 / phi).round() as usize).max(1);
        while gcd(d, n) != 1 {
            d += 1;
        }
        Interleaver {
            perm: (0..n).map(|p| p * d % n).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.perm.len()
    }

    pub fn is_empty(&self) -> bool {
        self.perm.is_empty()
    }

    /// Transmit order from coding order. (`T` is any `Copy` type: bits, LLRs.)
    pub fn apply<T: Copy>(&self, x: &[T]) -> Vec<T> {
        assert_eq!(x.len(), self.perm.len());
        self.perm.iter().map(|&i| x[i]).collect()
    }

    /// Coding order from transmit order.
    pub fn invert<T: Copy + Default>(&self, y: &[T]) -> Vec<T> {
        assert_eq!(y.len(), self.perm.len());
        let mut out = vec![T::default(); y.len()];
        for (p, &i) in self.perm.iter().enumerate() {
            out[i] = y[p];
        }
        out
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_is_a_permutation() {
        for n in [1, 2, 36, 1152, 4608, 1000] {
            let il = Interleaver::new(n);
            let x: Vec<u32> = (0..n as u32).collect();
            let y = il.apply(&x);
            let mut sorted = y.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, x);
            assert_eq!(il.invert(&y), x);
        }
    }

    #[test]
    fn a_burst_is_spread_out_and_shared_between_codewords() {
        // Four codewords of 1152 bits. A fade over 400 consecutive
        // transmitted bits hits each codeword in about equal share, and
        // the hit bits are far apart within it.
        let n = 4608;
        let il = Interleaver::new(n);
        let idx: Vec<usize> = (0..n).collect();
        let sent = il.apply(&idx); // sent[p] = coding index at position p
        let mut per_cw = [0usize; 4];
        for &i in &sent[1000..1400] {
            per_cw[i / 1152] += 1;
        }
        for c in per_cw {
            assert!((85..=115).contains(&c), "{per_cw:?}");
        }
        let mut hit: Vec<usize> = sent[1000..1100].to_vec();
        hit.sort_unstable();
        let min_gap = hit.windows(2).map(|w| w[1] - w[0]).min().unwrap();
        assert!(min_gap >= 10, "min gap {min_gap}");
    }
}
