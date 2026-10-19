// made by CasualArclamp
//! A small deterministic random source (PCG32) with Gaussian samples, so
//! simulations and tests are reproducible from a seed without a dependency.

pub struct Rng {
    state: u64,
    inc: u64,
    spare: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut r = Rng {
            state: 0,
            inc: (seed << 1) | 1,
            spare: None,
        };
        r.next_u32();
        r.state = r.state.wrapping_add(seed ^ 0x853c_49e6_748f_ea9b);
        r.next_u32();
        r
    }

    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        f64::from(self.next_u32()) / 4_294_967_296.0
    }

    pub fn bit(&mut self) -> u8 {
        (self.next_u32() >> 31) as u8
    }

    /// Standard normal (Box–Muller, both values used).
    pub fn gauss(&mut self) -> f64 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let u1 = (1.0 - self.uniform()).max(1e-300);
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let (s, c) = (std::f64::consts::TAU * u2).sin_cos();
        self.spare = Some(r * s);
        r * c
    }

    /// Circular complex Gaussian with E|z|² = 1.
    pub fn cgauss(&mut self) -> crate::Iq {
        let k = std::f64::consts::FRAC_1_SQRT_2;
        crate::Iq::new((self.gauss() * k) as f32, (self.gauss() * k) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_moments() {
        let mut r = Rng::new(42);
        let n = 200_000;
        let v: Vec<f64> = (0..n).map(|_| r.gauss()).collect();
        let mean = v.iter().sum::<f64>() / n as f64;
        let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        assert!(
            mean.abs() < 0.01 && (var - 1.0).abs() < 0.01,
            "{mean} {var}"
        );
    }

    #[test]
    fn seeds_differ_and_repeat() {
        let a: Vec<u32> = (0..4).map(|_| Rng::new(1).next_u32()).collect();
        assert!(a.windows(2).all(|w| w[0] == w[1]));
        assert_ne!(Rng::new(1).next_u32(), Rng::new(2).next_u32());
    }
}
