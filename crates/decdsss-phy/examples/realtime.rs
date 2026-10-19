// made by CasualArclamp
//! How fast the whole receiver runs against real time, at several sound-card
//! rates: every band listening (as the GUI does by default), noise throughout and
//! one 2.4 kHz transmission in the middle so the header and payload stages run too.
//!
//!     cargo run --release -p decdsss-phy --example realtime [RATE ...]
//!
//! Prints the real-time factor (seconds of audio per second of CPU) for the
//! whole receiver and for the five front ends alone. Below 1× the GUI falls
//! behind its sound card and has to drop audio.

use decdsss_chan::Channel;
use decdsss_core::{Band, Mode, Profile};
use decdsss_phy::framing::{Header, Layout, PayloadType};
use decdsss_phy::rx::MultiReceiver;
use decdsss_phy::rx::frontend::Frontend;
use decdsss_phy::sim::frames;
use decdsss_phy::tx::{Segment, chips_to_audio, segment_chips};
use std::time::Instant;

fn signal(rate: f64) -> Vec<f32> {
    let profile = Profile::W2400;
    let mode = Mode::from_name("ds8-1/2").unwrap();
    let layout = Layout::new(profile, mode, 1);
    let nblocks = 6;
    let seg = Segment {
        header: Header {
            profile,
            mode,
            depth: 1,
            ptype: PayloadType::Test,
            call: "VK2ABC".into(),
            session: 1,
            frame0: 0,
            nblocks,
            total_frames: nblocks,
        },
        frames: frames(&layout, usize::from(nblocks)),
    };
    let chips = segment_chips(&seg).unwrap();
    // Ten seconds of noise either side, as a receiver mostly hears.
    let audio = chips_to_audio(&chips, profile, rate, 0.2, 10.0, 10.0);
    let ch = Channel {
        snr_db: Some(6.0),
        offset_hz: 3.0,
        seed: 7,
        ..Channel::clean(0.2, profile.bandwidth_hz())
    };
    ch.apply(&audio, rate)
}

/// A recording as raw little-endian `f32` samples (`ffmpeg … -f f32le`).
fn raw(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    // `as_chunks::<4>()` views the bytes as `[u8; 4]` arrays (and the odd bytes
    // left over, ignored here): exactly what `from_le_bytes` takes.
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect()
}

fn main() {
    // Each argument is a rate (the synthetic signal at that rate) or
    // FILE.f32:RATE (a recording).
    let mut runs: Vec<(String, f64)> = std::env::args()
        .skip(1)
        .filter_map(|a| match a.rsplit_once(':') {
            Some((path, rate)) if path.ends_with(".f32") => {
                Some((path.to_string(), rate.parse().ok()?))
            }
            _ => Some((String::new(), a.parse().ok()?)),
        })
        .collect();
    if runs.is_empty() {
        runs = [8_000.0, 12_000.0, 44_100.0, 48_000.0]
            .map(|r| (String::new(), r))
            .to_vec();
    }
    println!("  rate      audio   receiver (5 bands)   front ends alone   headers");
    for (path, rate) in runs {
        let audio = if path.is_empty() {
            signal(rate)
        } else {
            raw(&path)
        };
        let secs = audio.len() as f64 / rate;
        // 20 ms a push, about what the GUI reads from the sound card each pass.
        let chunk = (rate / 50.0) as usize;

        let mut mr = MultiReceiver::with_bands(&Band::ALL, rate);
        let t0 = Instant::now();
        let mut headers = 0;
        for c in audio.chunks(chunk) {
            headers += mr
                .push(c)
                .iter()
                .filter(|t| matches!(t.event, decdsss_phy::rx::Event::Header { .. }))
                .count();
        }
        let whole = t0.elapsed().as_secs_f64();

        let mut fes: Vec<Frontend> = Band::ALL.iter().map(|&b| Frontend::at(b, rate)).collect();
        let t0 = Instant::now();
        let mut out = Vec::new();
        for c in audio.chunks(chunk) {
            for fe in &mut fes {
                out.clear();
                fe.push(c, &mut out);
            }
        }
        let front = t0.elapsed().as_secs_f64();

        println!(
            "  {:>6.0} Hz {secs:5.1} s   {:6.2} s = {:5.1}×      {:6.2} s = {:5.1}×    {headers}",
            rate,
            whole,
            secs / whole,
            front,
            secs / front,
        );
        // Each band alone (shared decimator included), to see which one sets the pace
        // now that they run side by side.
        let per: Vec<String> = Band::ALL
            .iter()
            .map(|&b| {
                let mut one = MultiReceiver::with_bands(&[b], rate);
                let t0 = Instant::now();
                for c in audio.chunks(chunk) {
                    one.push(c);
                }
                format!("{} {:.0}×", b.label(), secs / t0.elapsed().as_secs_f64())
            })
            .collect();
        println!("             bands alone: {}", per.join(", "));
    }
}
