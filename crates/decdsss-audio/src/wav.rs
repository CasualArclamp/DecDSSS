// made by CasualArclamp
//! WAV in and out for the command line and the GUI.

use crate::{Error, Result};
use std::path::Path;

/// Writes mono 16-bit PCM. Scales down if the audio would clip; returns the
/// peak level written.
pub fn write(path: &Path, audio: &[f32], rate: u32) -> Result<f32> {
    let peak = audio.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let scale = if peak > 0.98 { 0.98 / peak } else { 1.0 };
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &v in audio {
        w.write_sample((v * scale * 32767.0).round() as i16)?;
    }
    w.finalize()?;
    Ok(peak * scale)
}

/// The sample rate of a WAV file.
pub fn sample_rate(path: &Path) -> Result<u32> {
    Ok(hound::WavReader::open(path)?.spec().sample_rate)
}

/// Reads a whole WAV as mono f32 (channels averaged) and its sample rate.
pub fn read(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut all = Vec::new();
    let rate = stream(path, 1 << 20, |c| all.extend_from_slice(c))?;
    Ok((all, rate))
}

/// Calls `f` with successive mono chunks of about `chunk` samples; returns
/// the sample rate.
pub fn stream(path: &Path, chunk: usize, mut f: impl FnMut(&[f32])) -> Result<u32> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    if spec.sample_rate < 8000 {
        return Err(Error::InvalidArgument(format!(
            "sample rate {} Hz is too low",
            spec.sample_rate
        )));
    }
    let ch = usize::from(spec.channels.max(1));
    let mut buf: Vec<f32> = Vec::with_capacity(chunk);
    let mut frame = Vec::with_capacity(ch);
    let mut push = |v: f32, buf: &mut Vec<f32>, f: &mut dyn FnMut(&[f32])| {
        frame.push(v);
        if frame.len() == ch {
            buf.push(frame.iter().sum::<f32>() / ch as f32);
            frame.clear();
            if buf.len() >= chunk {
                f(buf);
                buf.clear();
            }
        }
    };
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, bits) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            for s in r.samples::<i32>() {
                push(s? as f32 * scale, &mut buf, &mut f);
            }
        }
        (hound::SampleFormat::Float, _) => {
            for s in r.samples::<f32>() {
                push(s?, &mut buf, &mut f);
            }
        }
    }
    if !buf.is_empty() {
        f(&buf);
    }
    Ok(spec.sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_clip_guard() {
        let dir = std::env::temp_dir().join("decdsss-audio-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        let x: Vec<f32> = (0..4800).map(|i| 0.4 * (i as f32 * 0.05).sin()).collect();
        write(&p, &x, 48_000).unwrap();
        let (y, rate) = read(&p).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(y.len(), x.len());
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-3));
        // Too loud: scaled down, not wrapped.
        let loud: Vec<f32> = x.iter().map(|v| v * 5.0).collect();
        let peak = write(&p, &loud, 48_000).unwrap();
        assert!(peak <= 0.981, "{peak}");
        let (z, _) = read(&p).unwrap();
        assert!(z.iter().all(|v| v.abs() < 0.99));
        std::fs::remove_dir_all(&dir).ok();
    }
}
