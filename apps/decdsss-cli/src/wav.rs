// made by CasualArclamp
//! WAV in and out for the command line.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// Writes mono 16-bit PCM. Scales down (and says so) if the audio would clip.
pub fn write(path: &Path, audio: &[f32], rate: u32) -> Result<f32> {
    let peak = audio.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let scale = if peak > 0.98 { 0.98 / peak } else { 1.0 };
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("cannot create {}", path.display()))?;
    for &v in audio {
        w.write_sample((v * scale * 32767.0).round() as i16)?;
    }
    w.finalize()?;
    Ok(peak * scale)
}

/// Reads a whole WAV as mono f32 (channels averaged) and its sample rate.
pub fn read(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut chunks = Vec::new();
    let rate = stream(path, 1 << 20, |c| chunks.extend_from_slice(c))?;
    Ok((chunks, rate))
}

/// Calls `f` with successive mono chunks of about `chunk` samples; returns
/// the sample rate.
pub fn stream(path: &Path, chunk: usize, mut f: impl FnMut(&[f32])) -> Result<u32> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let spec = r.spec();
    if spec.sample_rate < 8000 {
        bail!("sample rate {} Hz is too low", spec.sample_rate);
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
