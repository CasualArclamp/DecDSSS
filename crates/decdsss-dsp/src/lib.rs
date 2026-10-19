// made by CasualArclamp
//! DSP building blocks: RRC pulses, the transmitter's shaper, resampling,
//! the matched filter, interpolation and FFT helpers.

pub mod decimate;
pub mod fir;
pub mod hilbert;
pub mod interp;
pub mod resample;
pub mod rng;
pub mod rrc;
pub mod shaper;

/// A complex baseband sample.
pub type Iq = num_complex::Complex32;

/// e^{jθ}.
pub fn cis(theta: f32) -> Iq {
    let (s, c) = theta.sin_cos();
    Iq::new(c, s)
}

/// 10·log10, with a floor so a zero reads as −300 dB rather than −inf.
pub fn db(x: f64) -> f64 {
    10.0 * x.max(1e-30).log10()
}
