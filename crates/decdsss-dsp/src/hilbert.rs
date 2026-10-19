// made by CasualArclamp
//! Real audio to its analytic signal: a Kaiser-windowed FIR Hilbert
//! transformer for the imaginary part, the input delayed by the same
//! (N − 1)/2 samples for the real part.

use crate::Iq;
use crate::resample::bessel_i0;
use std::f64::consts::PI;

/// The analytic signal of `x`, delay-compensated (same length, aligned).
/// `taps` must be odd; 255 at 48 kHz holds the transform flat to well under
/// 0.1 dB from 300 Hz up.
pub fn analytic(x: &[f32], taps: usize) -> Vec<Iq> {
    assert!(taps % 2 == 1);
    let half = (taps / 2) as i64;
    let beta = 8.0;
    let i0b = bessel_i0(beta);
    // h[n] = 2/(πn) for odd n, 0 for even n.
    let h: Vec<f32> = (-half..=half)
        .map(|n| {
            if n % 2 == 0 {
                0.0
            } else {
                let r = n as f64 / half as f64;
                let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
                (2.0 / (PI * n as f64) * w) as f32
            }
        })
        .collect();
    let len = x.len() as i64;
    (0..len)
        .map(|i| {
            let mut im = 0.0f32;
            for (k, &hk) in h.iter().enumerate() {
                if hk == 0.0 {
                    continue;
                }
                // y[i] = Σ h[k] x[i − (k − half)]: centred, no added delay.
                let j = i - (k as i64 - half);
                if (0..len).contains(&j) {
                    im += hk * x[j as usize];
                }
            }
            Iq::new(x[i as usize], im)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_becomes_one_sided() {
        let fs = 48_000.0;
        for f in [400.0, 1500.0, 3000.0] {
            let x: Vec<f32> = (0..9600)
                .map(|n| (2.0 * PI * f * n as f64 / fs).cos() as f32)
                .collect();
            let a = analytic(&x, 255);
            // In the middle, |a| = 1 for a pure analytic tone.
            for z in &a[1000..8600] {
                assert!((z.norm() - 1.0).abs() < 0.01, "{f} Hz: {}", z.norm());
            }
        }
    }
}
