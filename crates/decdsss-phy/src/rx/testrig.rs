// made by CasualArclamp
//! Shared helpers for the receiver's tests: synthesise a transmission through
//! the channel simulator and drive the front end, acquisition and trackers.

use super::acq::Acquirer;
use super::frontend::{Buf, Frontend};
use super::header::{HeaderOutcome, HeaderResult, demod_header};
use super::preamble::{PreambleLock, PreambleTracker, State};
use crate::framing::{Header, Layout, PayloadType};
use crate::tx::{Segment, chips_to_audio, segment_chips};
use decdsss_chan::Channel;
use decdsss_core::{Mode, Profile};

pub const FS: f64 = 48_000.0;
pub const LEVEL: f32 = 0.2;

pub fn header(profile: Profile, mode: &str, depth: usize, nblocks: u16) -> Header {
    Header {
        profile,
        mode: Mode::from_name(mode).unwrap(),
        depth,
        ptype: PayloadType::Test,
        call: "VK2ABC".into(),
        session: 7,
        frame0: 0,
        nblocks,
        total_frames: 0,
    }
}

/// A segment whose frames carry a counting pattern.
pub fn segment(profile: Profile, mode: &str, depth: usize, nblocks: u16) -> Segment {
    let header = header(profile, mode, depth, nblocks);
    let layout = Layout::new(profile, header.mode, depth);
    let frames = (0..usize::from(nblocks) * depth)
        .map(|i| {
            (0..layout.frame_bytes())
                .map(|b| (i * 31 + b * 7 + 3) as u8)
                .collect()
        })
        .collect();
    Segment { header, frames }
}

/// The segment through a channel: audio at 48 kHz with 1 s of silence ahead.
pub fn through(seg: &Segment, ch: &Channel) -> Vec<f32> {
    let chips = segment_chips(seg).unwrap();
    let audio = chips_to_audio(&chips, seg.header.profile, FS, LEVEL, 1.0, 1.0);
    ch.apply(&audio, FS)
}

pub fn channel(profile: Profile, snr_db: f64) -> Channel {
    Channel {
        snr_db: Some(snr_db),
        ..Channel::clean(f64::from(LEVEL), profile.bandwidth_hz())
    }
}

/// Pushes the audio through the front end, acquisition and preamble tracker
/// and demodulates the header. Returns what it found and the buffer for the
/// payload stage.
pub struct Run {
    pub fe: Frontend,
    pub buf: Buf,
    pub lock: PreambleLock,
    pub header: HeaderResult,
}

pub fn to_header(audio: &[f32], profile: Profile) -> Result<Run, String> {
    to_header_in(audio, profile, &crate::tx::HEADER_ALPHABETS)
}

/// [`to_header`], trying only the CSK alphabets in `alphabets`, in that order
/// (a measurement of one alphabet, including ones the receiver no longer tries).
pub fn to_header_in(audio: &[f32], profile: Profile, alphabets: &[usize]) -> Result<Run, String> {
    let mut fe = Frontend::new(profile, FS);
    let mut acq = Acquirer::new(profile, 100.0);
    let mut buf = Buf::default();
    let mut tracker: Option<PreambleTracker> = None;
    let mut lock: Option<PreambleLock> = None;
    let mut alphabet = 0;
    let rate = fe.out_rate();
    for chunk in audio.chunks(2400) {
        let mut bb = Vec::new();
        fe.push(chunk, &mut bb);
        buf.push(&bb);
        if lock.is_none() {
            if tracker.is_none()
                && let Some(d) = acq.run(&buf)
            {
                tracker = Some(PreambleTracker::new(profile, rate, &d));
            }
            if let Some(t) = tracker.as_mut() {
                match t.step(&buf) {
                    State::Locked(l) => lock = Some(l),
                    State::Failed(why) => return Err(format!("preamble: {why}")),
                    State::Working => {}
                }
            }
        }
        if let Some(l) = &lock {
            // The alphabets in turn, as the receiver tries them.
            while alphabet < alphabets.len() {
                match demod_header(&buf, l, rate, alphabets[alphabet]) {
                    HeaderOutcome::NeedMore(_) => break,
                    HeaderOutcome::Bad(_) => alphabet += 1,
                    HeaderOutcome::Decoded(h) => {
                        return Ok(Run {
                            fe,
                            buf,
                            lock: l.clone(),
                            header: *h,
                        });
                    }
                }
            }
            if alphabet == alphabets.len() {
                return Err("header: it did not decode in any alphabet".into());
            }
        }
    }
    Err("never got to the header".into())
}

/// A genie start for the payload demodulator: the true timing, clock, carrier
/// and paths from the channel's own parameters, so payload thresholds can be
/// measured without the preamble and header in the way.
pub fn genie(seg: &Segment, ch: &Channel, audio: &[f32]) -> (super::payload::PayloadRx, Buf) {
    use super::despread::Track;
    use super::header::{Finger, HeaderResult};
    let profile = seg.header.profile;
    let mut fe = Frontend::new(profile, FS);
    let rate = fe.out_rate();
    let mut samples = Vec::new();
    fe.push(audio, &mut samples);
    let mut buf = Buf::default();
    buf.push(&samples);
    let cr = f64::from(profile.chip_rate());
    let r = 1.0 + ch.clock_ppm * 1e-6;
    let lead = 1.0;
    // Position of the first payload chip.
    let first_chip = ((lead + 10.0 / cr) * rate + fe.delay()) / r;
    let lead = crate::tx::lead_chips_for(profile, seg.header.mode);
    let pos = first_chip + lead as f64 * super::frontend::SPS as f64 / r;
    let mut fingers = vec![Finger {
        lag: 0.0,
        power: 1.0,
    }];
    if let Some(w) = ch.fading {
        // The rake gets both paths where they are.
        fingers.push(Finger {
            lag: w.delay_s * cr,
            power: 1.0,
        });
    }
    let hr = HeaderResult {
        header: seg.header.clone(),
        track: Track {
            pos,
            step: super::frontend::SPS as f64 / r,
            freq: ch.offset_hz,
            phase: 0.0,
            rate,
        },
        fingers,
        iterations: 0,
        snr_db: 0.0,
    };
    (super::payload::PayloadRx::new(&hr, rate).unwrap(), buf)
}
