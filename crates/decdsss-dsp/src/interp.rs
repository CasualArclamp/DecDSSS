// made by CasualArclamp
//! Fractional-delay interpolation of an oversampled signal.

use crate::Iq;

/// The signal at fractional index `pos` by Catmull–Rom cubic interpolation
/// through samples ⌊pos⌋ − 1 … ⌊pos⌋ + 2. Positions outside the slice read
/// as zero.
pub fn cubic(x: &[Iq], pos: f64) -> Iq {
    let i = pos.floor() as i64;
    let t = (pos - i as f64) as f32;
    let at = |k: i64| {
        if k < 0 || k >= x.len() as i64 {
            Iq::new(0.0, 0.0)
        } else {
            x[k as usize]
        }
    };
    let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
    let t2 = t * t;
    let t3 = t2 * t;
    p1 * (1.0 - 2.5 * t2 + 1.5 * t3)
        + p0 * (-0.5 * t + t2 - 0.5 * t3)
        + p2 * (0.5 * t + 2.0 * t2 - 1.5 * t3)
        + p3 * (-0.5 * t2 + 0.5 * t3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_samples_and_follows_a_slow_sine() {
        let x: Vec<Iq> = (0..64)
            .map(|n| Iq::new((n as f32 * 0.2).sin(), (n as f32 * 0.2).cos()))
            .collect();
        assert_eq!(cubic(&x, 10.0), x[10]);
        let want = (10.5f32 * 0.2).sin();
        assert!((cubic(&x, 10.5).re - want).abs() < 1e-3);
    }
}
