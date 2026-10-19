// made by CasualArclamp
//! A complex FIR filter with real taps (the receiver's matched filter).

use crate::Iq;

pub struct Fir {
    taps: Vec<f32>,
    /// History written twice, so the newest `taps.len()` samples are always
    /// one contiguous slice: hist[pos .. pos + n].
    hist: Vec<Iq>,
    pos: usize,
}

impl Fir {
    pub fn new(taps: Vec<f32>) -> Fir {
        let n = taps.len();
        Fir {
            // Reversed, so the dot product runs oldest-to-newest on both.
            taps: taps.into_iter().rev().collect(),
            hist: vec![Iq::new(0.0, 0.0); 2 * n],
            pos: 0,
        }
    }

    /// Group delay of a symmetric filter, samples.
    pub fn delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    pub fn push(&mut self, x: Iq) -> Iq {
        let n = self.taps.len();
        self.hist[self.pos] = x;
        self.hist[self.pos + n] = x;
        self.pos = (self.pos + 1) % n;
        let w = &self.hist[self.pos..self.pos + n];
        let mut acc = Iq::new(0.0, 0.0);
        for (h, t) in w.iter().zip(&self.taps) {
            acc += h * t;
        }
        acc
    }

    pub fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        out.extend(x.iter().map(|&v| self.push(v)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impulse_response_is_the_taps() {
        let mut f = Fir::new(vec![1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        f.process(
            &[
                Iq::new(1.0, 0.0),
                Iq::new(0.0, 0.0),
                Iq::new(0.0, 0.0),
                Iq::new(0.0, 0.0),
            ],
            &mut out,
        );
        let re: Vec<f32> = out.iter().map(|z| z.re).collect();
        assert_eq!(re, vec![1.0, 2.0, 3.0, 0.0]);
    }
}
