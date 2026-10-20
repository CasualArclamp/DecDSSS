// made by CasualArclamp
// Copied from COFDMtv's `cofdmtv-io` (same author), itself from DecDRM's `decdrm-io`;
// the file readers and the resampler were left out, and the WAV helpers added.
//! Sound-card and WAV I/O for DecDSSS.
//!
//! | Need | Type |
//! |---|---|
//! | Enumerate sound cards | [`list_devices`], [`DeviceInfo`] |
//! | Capture from a sound card / virtual audio cable | [`InputStream`] |
//! | Play raw samples to a sound card | [`OutputStream`] |
//! | Read and write WAV files | [`wav`] |
//!
//! All sample data crossing this API is `f32`, interleaved, with integer full
//! scale mapped to `[-1.0, 1.0)`. Sound-card callbacks run on a high-priority
//! thread owned by the driver and never block: each connects to your code
//! through a lock-free single-producer / single-consumer ring buffer
//! ([`rtrb`]) and a few atomic counters (see [`stream`]).

#![warn(missing_docs)]

pub mod channels;
mod device;
mod error;
pub mod stream;
pub mod wav;

use std::fmt;
use std::time::Duration;

pub use device::{
    ConfigRange, DeviceInfo, HOST_ENV, available_hosts, default_device_name, host_name,
    list_devices, list_input_devices, list_output_devices,
};
pub use error::{Error, Result};
pub use stream::{
    InputOptions, InputStats, InputStream, OutputOptions, OutputState, OutputStats, OutputStream,
};

/// The rate streams ask a device for when the caller has no preference, Hz (what most
/// sound cards mix at).
pub const WORKING_RATE: u32 = 48_000;

/// Sample rate and channel count of an interleaved `f32` stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    /// Frames per second.
    pub sample_rate: u32,
    /// Samples per frame (1 = mono / real, 2 = stereo / I-Q, ...).
    pub channels: usize,
}

impl AudioFormat {
    /// Creates a format description.
    pub const fn new(sample_rate: u32, channels: usize) -> Self {
        AudioFormat {
            sample_rate,
            channels,
        }
    }

    /// Duration of `frames` frames at this sample rate.
    pub fn frames_to_duration(&self, frames: u64) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        let rate = u64::from(self.sample_rate);
        let secs = frames / rate;
        let rem = frames % rate;
        Duration::new(secs, ((rem * 1_000_000_000) / rate) as u32)
    }

    /// Number of whole frames in `duration` at this sample rate (rounded to nearest).
    pub fn duration_to_frames(&self, duration: Duration) -> u64 {
        (duration.as_secs_f64() * f64::from(self.sample_rate)).round() as u64
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz, {} ch", self.sample_rate, self.channels)
    }
}

/// Whether a sound-card device or stream captures (input) or plays (output).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Capture (microphone, line in, virtual cable output).
    Input,
    /// Playback (speakers, line out, virtual cable input).
    Output,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Input => "input",
            Direction::Output => "output",
        })
    }
}

/// Checks that an interleaved slice holds a whole number of frames.
pub(crate) fn check_whole_frames(samples: &[f32], channels: usize) -> Result<()> {
    if channels == 0 {
        return Err(Error::invalid("channel count must be > 0"));
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(Error::invalid(format!(
            "{} samples is not a whole number of {}-channel frames",
            samples.len(),
            channels
        )));
    }
    Ok(())
}

// Compile-time proof that the stateful types can move to another thread.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<InputStream>();
    assert_send::<OutputStream>();
    assert_send::<Error>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_durations() {
        let f = AudioFormat::new(48_000, 2);
        assert_eq!(f.frames_to_duration(48_000), Duration::from_secs(1));
        assert_eq!(f.frames_to_duration(24_000), Duration::from_millis(500));
        assert_eq!(f.duration_to_frames(Duration::from_millis(250)), 12_000);
        assert_eq!(
            AudioFormat::new(0, 1).frames_to_duration(10),
            Duration::ZERO
        );
        assert_eq!(f.to_string(), "48000 Hz, 2 ch");
    }

    #[test]
    fn whole_frames() {
        assert!(check_whole_frames(&[0.0; 4], 2).is_ok());
        assert!(check_whole_frames(&[0.0; 3], 2).is_err());
        assert!(check_whole_frames(&[0.0; 3], 0).is_err());
    }
}
