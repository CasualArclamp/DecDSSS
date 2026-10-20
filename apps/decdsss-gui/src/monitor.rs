// made by CasualArclamp
//! The RF monitor: the audio coming from the radio (or the file or demo being
//! played in), sent on to an output device so you can hear the channel: the
//! hiss, the spread signal under it, an interferer. It is the receiver's own
//! input, tapped before anything is decoded, and it never touches the signal.

use decdsss_audio::{OutputOptions, OutputStream};
use std::time::Duration;

/// How much audio may wait in the output before new audio is dropped, so the
/// monitor does not fall behind the receiver.
const MAX_BACKLOG: Duration = Duration::from_millis(400);

/// A fixed 25 % cut (x0.75, about -2.5 dB) under the volume slider: the monitor
/// was too loud at the old slider positions.
const TRIM: f32 = 0.75;

struct Open {
    stream: OutputStream,
    rate: u32,
    channels: usize,
}

#[derive(Default)]
pub struct Monitor {
    on: bool,
    device: Option<String>,
    /// Gain, 0 …; the audio is clipped to ±1 after it.
    volume: f32,
    open: Option<Open>,
}

impl Monitor {
    /// Sets what the monitor does; a changed device closes the stream so the next
    /// chunk opens the new one.
    pub fn configure(&mut self, on: bool, device: Option<String>, volume: f32) {
        if !on || device != self.device {
            self.open = None;
        }
        self.on = on;
        self.device = device;
        self.volume = volume;
    }

    /// Plays `chunk` (mono, at `rate`). Returns an error message if the output
    /// could not be opened or has died (the monitor then switches itself off).
    pub fn feed(&mut self, chunk: &[f32], rate: f64) -> Option<String> {
        if !self.on || chunk.is_empty() {
            return None;
        }
        let rate = rate.round() as u32;
        if self.open.as_ref().is_some_and(|o| o.rate != rate) {
            self.open = None;
        }
        if self.open.is_none() {
            match OutputStream::open(&OutputOptions {
                device: self.device.clone(),
                sample_rate: Some(rate),
                channels: Some(1),
                ..OutputOptions::default()
            }) {
                Ok(stream) => {
                    let f = stream.format();
                    if f.sample_rate != rate {
                        self.on = false;
                        return Some(format!(
                            "the monitor output runs at {} Hz but the input at {rate} Hz: choose an output at {rate} Hz",
                            f.sample_rate
                        ));
                    }
                    self.open = Some(Open {
                        stream,
                        rate,
                        channels: f.channels.max(1),
                    });
                }
                Err(e) => {
                    self.on = false;
                    return Some(format!("cannot open the monitor output: {e}"));
                }
            }
        }
        let o = self.open.as_mut()?;
        if o.stream.is_dead() {
            self.open = None;
            self.on = false;
            return Some("the monitor output stopped".into());
        }
        if o.stream.buffered() > MAX_BACKLOG {
            return None;
        }
        let mut block = Vec::with_capacity(chunk.len() * o.channels);
        for &v in chunk {
            let v = (v * self.volume * TRIM).clamp(-1.0, 1.0);
            block.extend(std::iter::repeat_n(v, o.channels));
        }
        let _ = o.stream.write(&block);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_does_nothing_and_a_missing_device_switches_it_off() {
        let mut m = Monitor::default();
        m.configure(false, None, 1.0);
        assert_eq!(m.feed(&[0.1; 100], 48_000.0), None);
        assert!(m.open.is_none());
        // A device that does not exist: an error, once, and off.
        m.configure(true, Some("no such device, really".into()), 1.0);
        let e = m.feed(&[0.1; 100], 48_000.0);
        assert!(e.is_some(), "{e:?}");
        assert_eq!(m.feed(&[0.1; 100], 48_000.0), None);
    }
}
