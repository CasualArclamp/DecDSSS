// made by CasualArclamp
//! A built-in demonstration signal: two stations on the same 2.4 kHz channel
//! on different spreading codes, one DS-PSK and one CSK, the second joining
//! while the first is still sending, through moderate HF fading, a carrier
//! offset and a sound-card clock error. It is what `--demo` feeds the
//! receiver, so every panel has something to show without a radio.

use decdsss_chan::{Channel, Watterson};
use decdsss_core::{Mode, Profile};
use decdsss_link::{SendSpec, container, segments};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::tx::{chips_to_audio, segment_chips};

pub const RATE: u32 = 48_000;

pub struct Demo {
    pub audio: Vec<f32>,
    pub what: String,
}

fn text(tag: &str, bytes: usize) -> Vec<u8> {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < bytes {
        i += 1;
        s += &format!(
            "{tag} line {i}: spread spectrum under the noise, {:.1} dB down.\n",
            -3.0 - f64::from(i) * 0.7
        );
    }
    s.truncate(bytes);
    s.into_bytes()
}

/// One station's audio, as `lead` seconds of silence then its segments.
fn station(
    call: &str,
    mode: &str,
    depth: usize,
    name: &str,
    data: &[u8],
    session: u8,
    lead: f64,
) -> Vec<f32> {
    let spec = SendSpec {
        profile: Profile::W2400,
        mode: Mode::from_name(mode).expect("demo mode"),
        depth,
        call: call.into(),
        session,
        ptype: PayloadType::File,
        resync_s: 40.0,
        parity: 0.2,
        passes: 1,
    };
    let segs = segments(&spec, &container::pack(name, data)).expect("demo segments");
    let mut chips = Vec::new();
    for s in &segs {
        chips.extend(segment_chips(s).expect("demo header"));
    }
    chips_to_audio(&chips, Profile::W2400, f64::from(RATE), 0.2, lead, 2.0)
}

/// A linear-phase FIR band-pass (Hamming-windowed sinc, `taps` odd), applied
/// with the delay taken out.
fn bandpass(x: &[f32], lo_hz: f64, hi_hz: f64, taps: usize) -> Vec<f32> {
    let rate = f64::from(RATE);
    let half = (taps / 2) as i64;
    let h: Vec<f32> = (-half..=half)
        .map(|n| {
            let t = n as f64;
            let sinc = |fc: f64| {
                if n == 0 {
                    2.0 * fc / rate
                } else {
                    (std::f64::consts::TAU * fc * t / rate).sin() / (std::f64::consts::PI * t)
                }
            };
            let w = 0.54 + 0.46 * (std::f64::consts::PI * t / half as f64).cos();
            ((sinc(hi_hz) - sinc(lo_hz)) * w) as f32
        })
        .collect();
    let mut y = vec![0.0f32; x.len()];
    for (i, out) in y.iter_mut().enumerate() {
        let mut acc = 0.0f32;
        let lo = (i as i64 - half).max(0) as usize;
        let hi = ((i as i64 + half) as usize).min(x.len() - 1);
        for (j, &v) in x[lo..=hi].iter().enumerate() {
            // x[lo + j] meets h at offset (i - (lo + j)) + half.
            acc += v * h[(i as i64 - (lo + j) as i64 + half) as usize];
        }
        *out = acc;
    }
    y
}

pub fn make() -> Demo {
    let a = station(
        "VK2ABC",
        "ds8-1/2",
        2,
        "demo-ds.txt",
        &text("DS8", 1100),
        11,
        1.0,
    );
    let b = station(
        "W1AW",
        "csk64-1/2",
        1,
        "demo-csk.txt",
        &text("CSK64", 520),
        23,
        30.0,
    );
    let n = a.len().max(b.len());
    let mut mix = vec![0.0f32; n];
    for (i, v) in a.iter().enumerate() {
        mix[i] += v;
    }
    for (i, v) in b.iter().enumerate() {
        mix[i] += 0.8 * v;
    }
    let ch = Channel {
        snr_db: Some(2.0),
        offset_hz: 14.0,
        clock_ppm: -15.0,
        fading: Some(Watterson::MODERATE),
        seed: 77,
        ..Channel::clean(0.2, Profile::W2400.bandwidth_hz())
    };
    // A receiver's audio is band-limited (here 250–3400 Hz); the simulator's
    // noise is white across the whole 24 kHz, so filter it as a radio would.
    let audio = bandpass(&ch.apply(&mix, f64::from(RATE)), 250.0, 3400.0, 255);
    Demo {
        audio,
        what: "VK2ABC (DS8 ½) and W1AW (CSK64 ½) on 2.4 kHz, moderate fading, +14 Hz, -15 ppm"
            .into(),
    }
}
