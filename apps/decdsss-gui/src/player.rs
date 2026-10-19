// made by CasualArclamp
//! Plays live speech as it arrives. Chunks of 8 kHz speech are resampled to the
//! output device's rate and queued; the device waits for a second of speech
//! before it starts (the jitter buffer: speech comes in blocks of a second or
//! more, with the modem's own pace), and again after running dry, and plays
//! what is left when a transmission ends.

use decdsss_audio::{OutputOptions, OutputStream};
use decdsss_dsp::Iq;
use decdsss_dsp::resample::Resampler;
use std::collections::VecDeque;
use std::time::Duration;

/// Speech queued before playback starts, seconds.
const PREBUFFER_S: f64 = 1.0;

struct Open {
    stream: OutputStream,
    resampler: Resampler,
    rate: u32,
    channels: usize,
}

#[derive(Default)]
pub struct Player {
    device: Option<String>,
    open: Option<Open>,
    /// Audio at the device's rate not yet accepted by it.
    queue: VecDeque<f32>,
    /// The last chunk of a transmission was queued: play out what is there.
    draining: bool,
}

impl Player {
    /// Chooses the output; a changed device closes the stream.
    pub fn configure(&mut self, device: Option<String>) {
        if device != self.device {
            self.open = None;
            self.queue.clear();
        }
        self.device = device;
    }

    fn ensure_open(&mut self) -> Result<(), String> {
        if self.open.is_some() {
            return Ok(());
        }
        let stream = OutputStream::open(&OutputOptions {
            device: self.device.clone(),
            sample_rate: Some(48_000),
            channels: Some(1),
            buffer: Duration::from_secs(4),
            start_threshold: Duration::from_secs_f64(PREBUFFER_S),
        })
        .map_err(|e| format!("cannot open the speech output: {e}"))?;
        let f = stream.format();
        let rate = f.sample_rate;
        let resampler = Resampler::new(8000.0, f64::from(rate), 3500.0, 4000.0);
        self.open = Some(Open {
            stream,
            resampler,
            rate,
            channels: f.channels.max(1),
        });
        Ok(())
    }

    /// Queues speech (8 kHz). `last` says it is the end of the transmission.
    /// Returns an error message if the output cannot be opened.
    pub fn push(&mut self, pcm: &[i16], last: bool) -> Option<String> {
        if let Err(e) = self.ensure_open() {
            return Some(e);
        }
        let o = self.open.as_mut()?;
        if self.draining && !pcm.is_empty() {
            // A new transmission after one that ended: wait for a second again.
            self.draining = false;
            let frames = (PREBUFFER_S * f64::from(o.rate)) as usize;
            o.stream.set_start_threshold(frames);
        }
        let input: Vec<Iq> = pcm
            .iter()
            .map(|&v| Iq::new(f32::from(v) / 32768.0, 0.0))
            .collect();
        let mut out = Vec::new();
        o.resampler.process(&input, &mut out);
        for z in out {
            for _ in 0..o.channels {
                self.queue.push_back(z.re);
            }
        }
        if last {
            // What is queued plays out whatever its length.
            self.draining = true;
            o.stream.set_start_threshold(1);
        }
        self.tick();
        None
    }

    /// Hands the output what it has room for; call often.
    pub fn tick(&mut self) {
        let Some(o) = self.open.as_mut() else { return };
        if self.queue.is_empty() {
            return;
        }
        let free = o.stream.free_frames() * o.channels;
        let n = free.min(self.queue.len());
        if n == 0 {
            return;
        }
        let block: Vec<f32> = self.queue.drain(..n).collect();
        let _ = o.stream.write(&block);
    }
}
