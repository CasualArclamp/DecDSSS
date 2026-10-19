// made by CasualArclamp
//! `roundtrip MODE IN.raw OUT.raw`: encode and decode raw 8 kHz 16-bit mono speech.
use decdsss_codec2::{Codec2, Mode};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mode = Mode::from_name(&a[1]).expect("mode: 450 or 700C ...");
    let raw = std::fs::read(&a[2]).expect("input");
    let x: Vec<i16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    let mut e = Codec2::new(mode).unwrap();
    let mut d = Codec2::new(mode).unwrap();
    let n = e.samples_per_frame();
    let mut out = Vec::new();
    for c in x.chunks_exact(n) {
        out.extend(d.decode(&e.encode(c)));
    }
    let bytes: Vec<u8> = out.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(&a[3], bytes).expect("output");
}
