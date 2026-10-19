// made by CasualArclamp
//! Prints every mode's frame size and rate.
use decdsss_codec2::{Codec2, Mode};

fn main() {
    for m in Mode::ALL {
        let c = Codec2::new(m).unwrap();
        println!(
            "{:>5}: {} samples = {:.0} ms, {} bits = {:.0} bit/s",
            m.name(),
            c.samples_per_frame(),
            c.frame_seconds() * 1000.0,
            c.bits_per_frame(),
            c.bits_per_frame() as f64 / c.frame_seconds()
        );
    }
}
