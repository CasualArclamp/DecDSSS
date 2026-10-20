// made by CasualArclamp
//! Live loopback through VB-Audio Virtual Cable A (silent): a tone played on the cable's
//! input must come back from its output at the same frequency, at every sample rate and
//! with different rates on the two ends (so the time base is the caller's, not the card's).
//!
//! Needs the cable, so it is `#[ignore]`d:
//! `cargo test -p decdsss-audio --test mme_live -- --ignored --nocapture`

#![cfg(windows)]

use std::time::{Duration, Instant};

use decdsss_audio::{InputOptions, InputStream, OutputOptions, OutputStream};

const OUT: &str = "CABLE-A Input";
const IN: &str = "CABLE-A Output";
const TONE_HZ: f64 = 1000.0;

/// Goertzel power of `x` at `f` Hz (rate `fs`), as a fraction of the tone's total power.
fn tone_fraction(x: &[f32], fs: f64, f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / fs;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &s) in x.iter().enumerate() {
        re += f64::from(s) * (w * n as f64).cos();
        im += f64::from(s) * (w * n as f64).sin();
    }
    let n = x.len() as f64;
    let amp2 = 4.0 * (re * re + im * im) / (n * n); // amplitude² of a pure tone at f
    let power: f64 = x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / n;
    amp2 / 2.0 / power.max(1e-12)
}

/// Plays a 1 kHz tone at `out_rate` and records it at `in_rate`; returns the fraction of
/// the received power that sits at 1 kHz, and the measured amplitude.
fn loop_once(out_rate: u32, in_rate: u32) -> (f64, f32) {
    let mut input = InputStream::open(&InputOptions {
        device: Some(IN.into()),
        sample_rate: Some(in_rate),
        channels: Some(1),
        ..Default::default()
    })
    .expect("open input");
    let mut out = OutputStream::open(&OutputOptions {
        device: Some(OUT.into()),
        sample_rate: Some(out_rate),
        channels: Some(1),
        ..Default::default()
    })
    .expect("open output");
    assert_eq!(out.format().sample_rate, out_rate, "output kept its rate");
    assert_eq!(input.format().sample_rate, in_rate, "input kept its rate");
    let (och, ich) = (out.format().channels, input.format().channels);

    let secs = 1.5;
    let n = (f64::from(out_rate) * secs) as usize;
    let tone: Vec<f32> = (0..n)
        .flat_map(|i| {
            let s = (0.5
                * (2.0 * std::f64::consts::PI * TONE_HZ * i as f64 / f64::from(out_rate)).sin())
                as f32;
            std::iter::repeat_n(s, och)
        })
        .collect();
    let mut got = Vec::new();
    let start = Instant::now();
    let writer = std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            out.write_blocking(&tone, Duration::from_secs(10)).unwrap();
            out.drain(Duration::from_secs(5));
        });
        while !h.is_finished() && start.elapsed() < Duration::from_secs(12) {
            input.read_into(&mut got, 1 << 16);
            std::thread::sleep(Duration::from_millis(10));
        }
        h.join().is_ok()
    });
    assert!(writer, "writer thread");
    input.read_into(&mut got, usize::MAX);
    // Mono-ise, then take the loudest second-ish window (the cable idles around the tone).
    let mono: Vec<f32> = got.chunks_exact(ich).map(|f| f[0]).collect();
    let win = in_rate as usize / 2;
    assert!(
        mono.len() > 2 * win,
        "captured {} frames at {in_rate} Hz",
        mono.len()
    );
    let (best, _) = (0..mono.len() - win)
        .step_by(win / 4)
        .map(|s| (s, mono[s..s + win].iter().map(|x| x * x).sum::<f32>()))
        .fold((0, 0.0f32), |a, b| if b.1 > a.1 { b } else { a });
    let w = &mono[best..best + win];
    let amp = w.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    (tone_fraction(w, f64::from(in_rate), TONE_HZ), amp)
}

#[test]
#[ignore = "needs VB-Audio Virtual Cable A"]
fn tone_survives_every_rate() {
    let rates = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
    ];
    for r in rates {
        let (frac, amp) = loop_once(r, r);
        println!(
            "{r:>6} Hz -> {r:>6} Hz: {:.1} % of power at 1 kHz, peak {amp:.2}",
            frac * 100.0
        );
        assert!(
            frac > 0.95 && (0.4..0.6).contains(&amp),
            "{r} Hz: {frac} {amp}"
        );
    }
}

#[test]
#[ignore = "needs VB-Audio Virtual Cable A"]
fn tone_survives_mismatched_rates() {
    for (o, i) in [
        (48_000, 44_100),
        (44_100, 48_000),
        (44_100, 96_000),
        (96_000, 8000),
        (16_000, 48_000),
    ] {
        // 8 kHz cannot hold a 1 kHz tone's neighbours but 1 kHz is well inside it.
        let (frac, amp) = loop_once(o, i);
        println!(
            "{o:>6} Hz -> {i:>6} Hz: {:.1} % of power at 1 kHz, peak {amp:.2}",
            frac * 100.0
        );
        assert!(
            frac > 0.95 && (0.4..0.6).contains(&amp),
            "{o}->{i}: {frac} {amp}"
        );
    }
}
