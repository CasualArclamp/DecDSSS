// made by CasualArclamp
//! A built-in demonstration signal: two stations on the same 2.4 kHz channel
//! on different spreading codes, one DS-PSK sending a picture and one CSK
//! sending a text file, the second joining while the first is still sending, through moderate HF fading, a carrier
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

/// A synthetic landscape as a progressive JPEG of at most `budget` bytes.
fn picture(budget: usize) -> Vec<u8> {
    let (w, h) = (160u32, 120u32);
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let (u, v) = (x as f32, y as f32);
        let t = (v / h as f32).powf(1.5);
        let mix = |a: f32, b: f32| a + (b - a) * t;
        // Sky, with a sun and its glow.
        let d = ((u - 112.0).powi(2) + (v - 62.0).powi(2)).sqrt();
        let glow = (1.0 - d / 46.0).clamp(0.0, 1.0).powi(2);
        let mut c = [
            mix(20.0, 250.0) + 90.0 * glow,
            mix(40.0, 170.0) + 80.0 * glow,
            mix(110.0, 90.0) + 20.0 * glow,
        ];
        if d < 12.0 {
            c = [255.0, 244.0, 200.0];
        }
        // Two ridges of hills.
        let far = 70.0 + 14.0 * (u * 0.06).sin() + 8.0 * (u * 0.17 + 1.0).sin();
        let near = 92.0 + 10.0 * (u * 0.09 + 2.0).sin() + 5.0 * (u * 0.31).sin();
        if v > near {
            c = [30.0, 25.0, 45.0];
        } else if v > far {
            c = [78.0, 52.0, 96.0];
        }
        image::Rgb(c.map(|v| v.clamp(0.0, 255.0) as u8))
    });
    decdsss_image::encode_to_fit(&img, budget)
        .expect("demo picture")
        .bytes
}

/// One station's audio, as `lead` seconds of silence then its segments.
#[allow(clippy::too_many_arguments)]
fn station(
    call: &str,
    mode: &str,
    depth: usize,
    name: &str,
    data: &[u8],
    ptype: PayloadType,
    session: u8,
    lead: f64,
) -> Vec<f32> {
    let spec = SendSpec {
        profile: Profile::W2400,
        mode: Mode::from_name(mode).expect("demo mode"),
        depth,
        call: call.into(),
        session,
        ptype,
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
        "landscape.jpg",
        &picture(2100),
        PayloadType::Picture,
        11,
        1.0,
    );
    let b = station(
        "W1AW",
        "csk64-1/2",
        1,
        "demo-csk.txt",
        &text("CSK64", 520),
        PayloadType::File,
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
        what: "VK2ABC (DS8 ½, a picture) and W1AW (CSK64 ½, a file) on 2.4 kHz, moderate fading, +14 Hz, -15 ppm"
            .into(),
    }
}
