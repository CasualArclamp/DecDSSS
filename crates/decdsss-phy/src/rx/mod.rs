// made by CasualArclamp
//! The receiver: audio in, events out.
//!
//! One [`Receiver`] listens on one bandwidth profile. A search stage
//! (acquisition, preamble tracking, header decoding) runs all the time on the
//! call code; each header it decodes opens a payload channel on that
//! station's own code, and channels run side by side, so several stations
//! sharing the frequency are all decoded. [`MultiReceiver`] runs one
//! receiver per profile on the same audio.

pub mod acq;
pub mod despread;
pub mod frontend;
pub mod grake;
pub mod header;
pub mod payload;
pub mod preamble;
pub mod refine;
#[cfg(test)]
pub mod testrig;

use crate::framing::Header;
use acq::{Acquirer, Detection};
use decdsss_core::{Band, Profile};
use decdsss_dsp::decimate::Decimator;
use frontend::{Buf, Frontend, MAX_OFFSET_HZ, SPS};
use header::{HeaderOutcome, HeaderResult, HeaderView, HeaderWatch, demod_header};
use payload::{BlockResult, PayloadRx};
use rayon::prelude::*;
use preamble::{PreambleLock, PreambleTracker, State};
use std::collections::VecDeque;

/// Largest carrier offset the receiver searches, Hz.
pub const SEARCH_OFFSET_HZ: f64 = 100.0;

/// Something the receiver tells its user.
#[derive(Clone, Debug)]
pub enum Event {
    /// A preamble was detected.
    Acquired(Box<Detection>),
    /// The preamble ended where it should.
    PreambleLocked(Box<PreambleLock>),
    /// The search gave up on a detection.
    Lost(String),
    /// A header decoded; a payload channel is open.
    Header {
        channel: usize,
        header: Box<Header>,
        snr_db: f64,
        fingers: usize,
    },
    /// A payload block was decoded (frames inside may be missing).
    Block {
        channel: usize,
        header: Box<Header>,
        result: Box<BlockResult>,
    },
    /// The segment's last block is done; the channel is closed.
    SegmentEnd { channel: usize },
    /// The end-of-transmission postamble followed: the station has finished, so a
    /// live stream is over and no more segments are coming. `ratio` is the peak
    /// of its delay profile over the median ([`crate::eot`]). A transmission
    /// whose postamble is missed or too weak never gets this event.
    EndOfTransmission {
        channel: usize,
        header: Box<Header>,
        ratio: f32,
    },
}

/// What one open payload channel shows.
#[derive(Clone, Debug)]
pub struct ChannelView {
    pub id: usize,
    pub header: Header,
    pub blocks_done: usize,
    pub live: payload::Live,
    pub carrier_hz: f64,
}

/// What a candidate preamble shows.
#[derive(Clone, Debug)]
pub struct AttemptView {
    pub freq_hz: f64,
    /// True once the preamble has ended and the header is being decoded.
    pub header_stage: bool,
    /// While the header is being decoded: the CSK alphabet being tried, the
    /// seconds since the preamble ended, and the seconds from then that this
    /// alphabet's header needs.
    pub header_alphabet: usize,
    pub header_since_s: f64,
    pub header_needs_s: f64,
    /// Per-period correlation power of A and of B (once the preamble ended).
    pub periods: Vec<(f32, f32)>,
    /// While the header is being decoded: its symbols so far, in the alphabet
    /// being tried.
    pub header: Option<HeaderView>,
}

/// Everything a display needs from one receiver.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub profile: Profile,
    /// Where in the passband it listens (the slot, for the 500 Hz profile).
    pub band: Band,
    pub samples_in: u64,
    /// The acquisition search surface, if the search has seen a window.
    pub search: Option<acq::Surface>,
    pub attempts: Vec<AttemptView>,
    pub channels: Vec<ChannelView>,
}

/// Events with the profile they came from.
#[derive(Clone, Debug)]
pub struct Tagged {
    pub profile: Profile,
    /// The band that heard it: with `profile`, which 500 Hz slot.
    pub band: Band,
    pub event: Event,
}

/// One candidate preamble on its way to a header. Several can be in flight:
/// a false alarm dies on its own without blocking a real one, and stations
/// that start together are followed together.
enum Step {
    Preamble(Box<PreambleTracker>),
    /// The preamble has ended: the header is tried in each alphabet in turn
    /// (the index into `tx::HEADER_ALPHABETS`), the shortest first, as soon as
    /// its chips are in; the watch shows the symbols of the one being tried.
    Header(Box<PreambleLock>, usize, Box<HeaderWatch>),
}

struct Attempt {
    /// Where the search found it (a period boundary) and its carrier, to
    /// recognise the same preamble found twice.
    boundary: f64,
    freq_hz: f64,
    period: f64,
    step: Step,
}

/// Candidates followed at once.
const MAX_ATTEMPTS: usize = 4;

/// A live voice transmission has no end announced in its header; it is over when
/// this many blocks in a row decode nothing (the talker has let go, or the signal
/// is gone).
const LIVE_LOST_BLOCKS: usize = 6;

struct Channel {
    id: usize,
    rx: PayloadRx,
    /// Blocks in a row with no frame decoded.
    lost_run: usize,
}

impl Channel {
    /// A live voice stream: speech with no frame total.
    fn is_live(&self) -> bool {
        self.rx.is_live()
    }
}

/// A station whose segment has just ended, still listened to for the second in
/// which its postamble would come. Whatever the answer, it is dropped then.
struct Tail {
    id: usize,
    rx: PayloadRx,
}

/// Tails kept at most (a stream of short segments, none of them the last).
const MAX_TAILS: usize = 8;

pub struct Receiver {
    pub profile: Profile,
    pub band: Band,
    fe: Frontend,
    buf: Buf,
    acq: Acquirer,
    attempts: Vec<Attempt>,
    channels: Vec<Channel>,
    tails: Vec<Tail>,
    next_channel: usize,
    rate: f64,
    /// Total input samples seen, for the caller's clock.
    pub samples_in: u64,
    pending: VecDeque<Event>,
}

impl Receiver {
    pub fn new(profile: Profile, sample_rate: f64) -> Receiver {
        Receiver::at(Band::mid(profile), sample_rate)
    }

    /// A receiver for one band: the profile's place in the passband, or a slot of
    /// the 500 Hz profile.
    pub fn at(band: Band, sample_rate: f64) -> Receiver {
        let profile = band.profile;
        let fe = Frontend::at(band, sample_rate);
        let rate = fe.out_rate();
        let acq = Acquirer::new(profile, SEARCH_OFFSET_HZ);
        Receiver {
            profile,
            band,
            fe,
            buf: Buf::default(),
            attempts: Vec::new(),
            acq,
            channels: Vec::new(),
            tails: Vec::new(),
            next_channel: 0,
            rate,
            samples_in: 0,
            pending: VecDeque::new(),
        }
    }

    /// Open payload channels.
    pub fn channels(&self) -> usize {
        self.channels.len()
    }

    /// The state for a display; cheap enough to take a few times a second.
    pub fn snapshot(&self, search_cols: usize) -> Snapshot {
        Snapshot {
            profile: self.profile,
            band: self.band,
            samples_in: self.samples_in,
            search: self.acq.preview(search_cols),
            attempts: self
                .attempts
                .iter()
                .map(|a| AttemptView {
                    freq_hz: a.freq_hz,
                    header_stage: matches!(a.step, Step::Header(..)),
                    header_alphabet: match &a.step {
                        Step::Header(_, i, _) => crate::tx::HEADER_ALPHABETS[*i],
                        Step::Preamble(_) => 0,
                    },
                    header_since_s: match &a.step {
                        Step::Header(l, _, _) => {
                            ((self.buf.end() as f64 - l.header_start) / self.rate).max(0.0)
                        }
                        Step::Preamble(_) => 0.0,
                    },
                    header_needs_s: match &a.step {
                        Step::Header(l, i, _) => {
                            (header::header_need(l, crate::tx::HEADER_ALPHABETS[*i])
                                - l.header_start)
                                / self.rate
                        }
                        Step::Preamble(_) => 0.0,
                    },
                    periods: match &a.step {
                        Step::Header(l, _, _) => l.periods.clone(),
                        Step::Preamble(_) => Vec::new(),
                    },
                    header: match &a.step {
                        Step::Header(_, _, w) => Some(w.view().clone()),
                        Step::Preamble(_) => None,
                    },
                })
                .collect(),
            channels: self
                .channels
                .iter()
                .map(|c| ChannelView {
                    id: c.id,
                    header: c.rx.header.clone(),
                    blocks_done: c.rx.blocks_done(),
                    live: c.rx.live().clone(),
                    carrier_hz: c.rx.track.freq,
                })
                .collect(),
        }
    }

    /// Feeds audio; returns the events it caused.
    pub fn push(&mut self, audio: &[f32]) -> Vec<Event> {
        self.samples_in += audio.len() as u64;
        let mut bb = Vec::with_capacity(audio.len() / 6 + 16);
        self.fe.push(audio, &mut bb);
        self.buf.push(&bb);
        self.work();
        self.pending.drain(..).collect()
    }

    fn work(&mut self) {
        // Payload channels first: they hold the most data.
        let mut i = 0;
        while i < self.channels.len() {
            let ch = &mut self.channels[i];
            while let Some(result) = ch.rx.step(&self.buf) {
                ch.lost_run = if result.frames.iter().any(Option::is_some) {
                    0
                } else {
                    ch.lost_run + 1
                };
                self.pending.push_back(Event::Block {
                    channel: ch.id,
                    header: Box::new(ch.rx.header.clone()),
                    result: Box::new(result),
                });
            }
            let heard_eot = ch.rx.eot();
            if ch.rx.done()
                || heard_eot.is_some()
                || (ch.is_live() && ch.lost_run >= LIVE_LOST_BLOCKS)
            {
                self.pending.push_back(Event::SegmentEnd { channel: ch.id });
                let ch = self.channels.remove(i);
                match heard_eot {
                    // A live stream saw it between blocks.
                    Some(ratio) => self.pending.push_back(Event::EndOfTransmission {
                        channel: ch.id,
                        header: Box::new(ch.rx.header.clone()),
                        ratio,
                    }),
                    // A finished segment: the postamble, if any, is the next second.
                    None if ch.rx.done() => {
                        if self.tails.len() >= MAX_TAILS {
                            self.tails.remove(0);
                        }
                        self.tails.push(Tail {
                            id: ch.id,
                            rx: ch.rx,
                        });
                    }
                    None => {}
                }
            } else {
                i += 1;
            }
        }

        // Stations that have just finished a segment: is the postamble next?
        let mut t = 0;
        while t < self.tails.len() {
            let Some(ratio) = self.tails[t].rx.probe_eot(&self.buf) else {
                t += 1;
                continue;
            };
            let tail = self.tails.remove(t);
            if ratio >= crate::eot::EOT_PEAK_OVER_MEDIAN {
                self.pending.push_back(Event::EndOfTransmission {
                    channel: tail.id,
                    header: Box::new(tail.rx.header.clone()),
                    ratio,
                });
            }
        }

        // The search runs all the time, whatever else is in flight.
        while let Some(d) = self.acq.run(&self.buf) {
            self.acq.restart();
            let n = (SPS * crate::codes::preamble_len(self.profile)) as f64;
            let same = self.attempts.iter().any(|a| {
                let dt = (d.boundary - a.boundary).rem_euclid(a.period);
                dt.min(a.period - dt) < 3.0 && (d.freq_hz - a.freq_hz).abs() < 3.0
            });
            if same || self.attempts.len() >= MAX_ATTEMPTS {
                continue;
            }
            self.pending.push_back(Event::Acquired(Box::new(d.clone())));
            self.attempts.push(Attempt {
                boundary: d.boundary,
                freq_hz: d.freq_hz,
                period: n,
                step: Step::Preamble(Box::new(PreambleTracker::new(self.profile, self.rate, &d))),
            });
        }

        // Each candidate advances as far as the data allows.
        enum Next {
            Stay,
            Drop,
            Lock(Box<PreambleLock>),
            Open(Box<HeaderResult>),
        }
        let mut k = 0;
        while k < self.attempts.len() {
            let next = match &mut self.attempts[k].step {
                Step::Preamble(t) => match t.step(&self.buf) {
                    State::Working => Next::Stay,
                    State::Failed(why) => {
                        self.pending.push_back(Event::Lost(why.into()));
                        Next::Drop
                    }
                    State::Locked(lock) => Next::Lock(Box::new(lock)),
                },
                Step::Header(lock, alpha, watch) => {
                    // The shortest alphabet first; one that does not decode gives way
                    // to the next, and only the last one's failure ends the attempt.
                    let alphabets = crate::tx::HEADER_ALPHABETS;
                    let mut out = Next::Stay;
                    while *alpha < alphabets.len() {
                        match demod_header(&self.buf, lock, self.rate, alphabets[*alpha]) {
                            HeaderOutcome::NeedMore(_) => break,
                            HeaderOutcome::Bad(why) => {
                                *alpha += 1;
                                if *alpha == alphabets.len() {
                                    self.pending.push_back(Event::Lost(why.into()));
                                    out = Next::Drop;
                                } else {
                                    // Rust note: `**watch` is the value inside the
                                    // box; assigning it drops the old watch in place.
                                    **watch = HeaderWatch::new(
                                        lock,
                                        self.rate,
                                        alphabets[*alpha],
                                        alphabets[..*alpha].to_vec(),
                                    );
                                }
                            }
                            HeaderOutcome::Decoded(h) => {
                                out = Next::Open(h);
                                break;
                            }
                        }
                    }
                    if matches!(out, Next::Stay) {
                        watch.advance(&self.buf);
                    }
                    out
                }
            };
            match next {
                Next::Stay => k += 1,
                Next::Drop => {
                    self.attempts.remove(k);
                }
                Next::Lock(lock) => {
                    self.pending
                        .push_back(Event::PreambleLocked(Box::new((*lock).clone())));
                    let alphabet = crate::tx::HEADER_ALPHABETS[0];
                    let watch = HeaderWatch::new(&lock, self.rate, alphabet, Vec::new());
                    self.attempts[k].step = Step::Header(lock, 0, Box::new(watch));
                    // Look at it again straight away: the header may be in.
                }
                Next::Open(h) => {
                    self.attempts.remove(k);
                    self.open_channel(*h);
                }
            }
        }

        // Release what nobody needs any more.
        let mut keep = self.acq.oldest_needed();
        for a in &self.attempts {
            keep = keep.min(match &a.step {
                Step::Preamble(t) => t.oldest_needed(),
                Step::Header(l, _, _) => (l.header_start - 12.0 * l.step) as i64,
            });
        }
        for ch in &self.channels {
            keep = keep.min(ch.rx.oldest_needed());
        }
        for tail in &self.tails {
            keep = keep.min(tail.rx.oldest_needed());
        }
        self.buf.trim(keep);
    }

    fn open_channel(&mut self, h: HeaderResult) {
        // The same preamble can be found twice; one channel is enough.
        let dup = self.channels.iter().any(|c| {
            c.rx.header == h.header && (c.rx.track.pos - h.track.pos).abs() < 40.0 * h.track.step
        });
        if dup {
            return;
        }
        let Some(rx) = PayloadRx::new(&h, self.rate) else {
            return;
        };
        let id = self.next_channel;
        self.next_channel += 1;
        self.pending.push_back(Event::Header {
            channel: id,
            header: Box::new(h.header.clone()),
            snr_db: h.snr_db,
            fingers: h.fingers.len(),
        });
        // A header announcing no blocks is complete already.
        if rx.done() {
            self.pending.push_back(Event::SegmentEnd { channel: id });
        } else {
            self.channels.push(Channel {
                id,
                rx,
                lost_run: 0,
            });
        }
    }
}

/// A transmission some band has heard, to recognise the same one in another band.
struct Heard {
    key: (Profile, String, u8, u16),
    band: Band,
    channel: usize,
    snr_db: f64,
    /// Samples fed when it was heard.
    at: u64,
}

/// How long a header is remembered against a copy in another slot, seconds.
const HEARD_S: f64 = 120.0;

/// A header must be this much stronger (dB of symbol SNR) in one slot than in another
/// for the weaker to be taken for the other's leakage.
const LEAK_MARGIN_DB: f64 = 6.0;

/// A receiver per band, fed the same audio.
///
/// A strong station in one 500 Hz slot is also found, faintly, by the receivers of
/// the other slots: the preamble search correlates at one sample a chip, so a signal
/// 500 Hz away folds onto 100 Hz (500 mod 400, the chip rate) after the matched
/// filter has taken some 40 dB off it, and a station 50 dB or more over the noise
/// is still above the detection threshold there. The same header decoded in two slots
/// is the one transmission, in the slot where it is stronger: the other copy's events
/// are dropped.
pub struct MultiReceiver {
    rx: Vec<Receiver>,
    /// Brings a fast sound card down to about three times the highest band edge
    /// for all the bands at once; `None` when the card is already slow enough.
    decimator: Option<Decimator>,
    /// The sound card's rate (the receivers run at `rate / factor`).
    rate: f64,
    samples: u64,
    heard: Vec<Heard>,
    /// (band, channel) whose events are dropped: a weaker copy of a transmission.
    muted: Vec<(Band, usize)>,
}

impl MultiReceiver {
    /// One receiver per profile, each at its own place in the passband.
    pub fn new(profiles: &[Profile], sample_rate: f64) -> MultiReceiver {
        let bands: Vec<Band> = profiles.iter().map(|&p| Band::mid(p)).collect();
        MultiReceiver::with_bands(&bands, sample_rate)
    }

    /// One receiver per band: for the 500 Hz profile, several slots side by side.
    pub fn with_bands(bands: &[Band], sample_rate: f64) -> MultiReceiver {
        let decimator = shared_decimator(bands, sample_rate);
        let band_rate = sample_rate / decimator.as_ref().map_or(1, |d| d.factor()) as f64;
        MultiReceiver {
            rx: bands.iter().map(|&b| Receiver::at(b, band_rate)).collect(),
            decimator,
            rate: sample_rate,
            samples: 0,
            heard: Vec::new(),
            muted: Vec::new(),
        }
    }

    pub fn snapshots(&self, search_cols: usize) -> Vec<Snapshot> {
        self.rx
            .iter()
            .map(|r| {
                let mut s = r.snapshot(search_cols);
                let band = s.band;
                s.channels.retain(|c| !self.muted.contains(&(band, c.id)));
                s
            })
            .collect()
    }

    pub fn push(&mut self, audio: &[f32]) -> Vec<Tagged> {
        self.samples += audio.len() as u64;
        // `decimated` lives out here so the slice borrowed from it below stays
        // valid for the rest of the function; without a decimator `audio` is
        // passed through untouched.
        let mut decimated = Vec::new();
        let audio = match &mut self.decimator {
            Some(d) => {
                decimated.reserve(audio.len() / d.factor() + 1);
                d.process(audio, &mut decimated);
                &decimated[..]
            }
            None => audio,
        };
        // The bands are independent, so they run side by side on rayon's thread
        // pool (one worker a core, started once; a fresh OS thread per band per
        // push cost more than it saved). `par_iter_mut` hands each worker its own
        // `&mut Receiver`, which the borrow checker allows because no two get the
        // same one; `collect` keeps the bands' order.
        let batches: Vec<Vec<Event>> = self.rx.par_iter_mut().map(|r| r.push(audio)).collect();
        let mut out = Vec::new();
        for (i, events) in batches.into_iter().enumerate() {
            let (profile, band) = (self.rx[i].profile, self.rx[i].band);
            for event in events {
                // The channel an event belongs to, and whether it opens one.
                let (channel, opens) = match &event {
                    Event::Header { channel, .. } => (Some(*channel), true),
                    Event::Block { channel, .. }
                    | Event::SegmentEnd { channel }
                    | Event::EndOfTransmission { channel, .. } => (Some(*channel), false),
                    _ => (None, false),
                };
                if let Some(c) = channel {
                    if opens && self.is_leakage(profile, band, c, &event) {
                        continue;
                    }
                    if self.muted.contains(&(band, c)) {
                        continue;
                    }
                }
                out.push(Tagged {
                    profile,
                    band,
                    event,
                });
            }
        }
        out
    }

    /// Whether this header is the weaker copy, in another slot, of a transmission
    /// already heard (and if it is the stronger one, mutes the copy heard before).
    fn is_leakage(&mut self, profile: Profile, band: Band, channel: usize, event: &Event) -> bool {
        let Event::Header { header, snr_db, .. } = event else {
            return false;
        };
        let key = (profile, header.call.clone(), header.session, header.frame0);
        let horizon = (HEARD_S * self.rate) as u64;
        let now = self.samples;
        self.heard.retain(|h| now.saturating_sub(h.at) < horizon);
        let snr = *snr_db;
        if let Some(h) = self
            .heard
            .iter_mut()
            .find(|h| h.key == key && h.band != band)
        {
            if snr > h.snr_db + LEAK_MARGIN_DB {
                // This one is the real slot: the copy heard first is the leak.
                self.muted.push((h.band, h.channel));
                (h.band, h.channel, h.snr_db, h.at) = (band, channel, snr, now);
                return false;
            }
            // Weaker or about equal: the copy.
            self.muted.push((band, channel));
            return true;
        }
        self.heard.push(Heard {
            key,
            band,
            channel,
            snr_db: snr,
            at: now,
        });
        false
    }
}

/// The decimator in front of all the bands: a whole factor that leaves at least
/// three times the highest band edge (carrier offset included), so the low-pass
/// can pass that edge and stop at the new Nyquist frequency with room between.
/// At 44.1 or 48 kHz with every band on that is ÷4, to 11 025 or 12 000 Hz; at
/// 8, 12 or 16 kHz there is nothing to gain and no decimator.
fn shared_decimator(bands: &[Band], rate: f64) -> Option<Decimator> {
    let top = bands
        .iter()
        .map(|b| b.edges_hz().1 + MAX_OFFSET_HZ)
        .fold(0.0, f64::max);
    if top <= 0.0 {
        return None;
    }
    let factor = (rate / (3.0 * top)).floor() as usize;
    // `bool::then` builds the `Some` only when the condition holds.
    (factor >= 2).then(|| Decimator::new(rate, factor, top, rate / factor as f64 / 2.0))
}

/// Samples a chip after the front end, re-exported for callers that talk
/// in baseband sample counts.
pub const SAMPLES_PER_CHIP: usize = SPS;

#[cfg(test)]
mod tests {
    use super::testrig::*;
    use super::*;
    use crate::tx::{Segment, chips_to_audio, segment_chips};
    use decdsss_chan::Channel;
    use decdsss_core::Slot;

    fn run(rx: &mut Receiver, audio: &[f32]) -> Vec<Event> {
        let mut events = Vec::new();
        for chunk in audio.chunks(2048) {
            events.extend(rx.push(chunk));
        }
        events
    }

    fn frames_of(events: &[Event]) -> Vec<(usize, Option<Vec<u8>>)> {
        let mut v = Vec::new();
        for e in events {
            if let Event::Block { result, .. } = e {
                for (i, f) in result.frames.iter().enumerate() {
                    v.push((result.first_frame + i, f.clone()));
                }
            }
        }
        v
    }

    #[test]
    fn receives_a_whole_segment_from_a_stream() {
        for (profile, mode) in [
            (Profile::W2400, "ds8-1/2"),
            (Profile::W3000, "csk64-2/3"),
            (Profile::N500, "ds4-1/2"),
        ] {
            let seg = segment(profile, mode, 2, 3);
            let audio = through(&seg, &channel(profile, 0.0));
            let mut rx = Receiver::new(profile, FS);
            let events = run(&mut rx, &audio);
            let headers = events
                .iter()
                .filter(|e| matches!(e, Event::Header { .. }))
                .count();
            assert_eq!(headers, 1, "{profile}: headers");
            assert!(
                events.iter().any(|e| matches!(e, Event::SegmentEnd { .. })),
                "{profile}: no segment end"
            );
            let got = frames_of(&events);
            assert_eq!(got.len(), seg.frames.len(), "{profile}");
            for (i, f) in got {
                assert_eq!(f.as_ref(), Some(&seg.frames[i]), "{profile} frame {i}");
            }
            assert_eq!(rx.channels(), 0);
        }
    }

    #[test]
    fn receives_back_to_back_segments_and_joins_late() {
        // Two segments in one stream; a receiver that starts listening after
        // the first has begun still gets the second.
        let profile = Profile::W2400;
        let mut s1 = segment(profile, "ds8-1/2", 1, 2);
        let mut s2 = segment(profile, "ds8-1/2", 1, 2);
        s2.header.frame0 = 2;
        s2.header.total_frames = 4;
        s1.header.total_frames = 4;
        // Distinct frame data in the second segment.
        for (i, f) in s2.frames.iter_mut().enumerate() {
            for b in f.iter_mut() {
                *b = b.wrapping_add(i as u8 + 17);
            }
        }
        let chips = |s: &Segment| segment_chips(s).unwrap();
        let mut all = chips(&s1);
        all.extend(chips(&s2));
        let audio = chips_to_audio(&all, profile, FS, LEVEL, 1.0, 1.0);
        let audio = channel(profile, -2.0).apply(&audio, FS);

        let mut rx = Receiver::new(profile, FS);
        let events = run(&mut rx, &audio);
        let got = frames_of(&events);
        assert_eq!(got.len(), 4);
        for (i, f) in &got {
            let want = if *i < 2 {
                &s1.frames[*i]
            } else {
                &s2.frames[*i - 2]
            };
            assert_eq!(f.as_ref(), Some(want), "frame {i}");
        }

        // Late join: drop the first 70 % of the first segment's audio.
        let cut = audio.len() / 5;
        let mut late = Receiver::new(profile, FS);
        let events = run(&mut late, &audio[cut..]);
        let got = frames_of(&events);
        assert!(
            got.iter().any(|(i, f)| *i >= 2 && f.is_some()),
            "late joiner got nothing: {got:?}"
        );
    }

    /// Segments back to back: each preamble starts right after the previous
    /// payload ends. The search must start the preamble tracker where the
    /// signal began, not at the oldest search window (which then holds payload,
    /// where the A..A B B pattern test can match by chance and lock too early).
    #[test]
    fn five_back_to_back_segments_all_arrive() {
        let profile = Profile::W2400;
        let segs: Vec<Segment> = (0..5usize)
            .map(|i| {
                let mut s = segment(profile, "csk32-1/2", 1, 2);
                s.header.frame0 = (i * 2) as u16;
                s.header.total_frames = 10;
                for (j, f) in s.frames.iter_mut().enumerate() {
                    for b in f.iter_mut() {
                        *b = b.wrapping_add((i * 2 + j) as u8 * 11);
                    }
                }
                s
            })
            .collect();
        let mut chips = Vec::new();
        for s in &segs {
            chips.extend(segment_chips(s).unwrap());
        }
        let audio = chips_to_audio(&chips, profile, FS, LEVEL, 1.0, 1.0);
        let mut ch = channel(profile, -3.0);
        ch.offset_hz = 9.0;
        ch.clock_ppm = -25.0;
        let audio = ch.apply(&audio, FS);
        let mut rx = Receiver::new(profile, FS);
        let events = run(&mut rx, &audio);
        let got = frames_of(&events);
        assert_eq!(got.len(), 10, "frames: {}", got.len());
        for (i, f) in got {
            assert_eq!(f.as_ref(), Some(&segs[i / 2].frames[i % 2]), "frame {i}");
        }
    }

    /// The display must follow the signal through a block, not wait for the
    /// pre-pass's first seconds and then catch up: fed in tenths of a second, the
    /// symbol counter of the channel never stands still for long while the payload
    /// is arriving (it stood still for about four seconds in every block when a
    /// block waited for its pre-pass; the longest wait it now allows is the
    /// start-up of the first block).
    #[test]
    fn the_channel_keeps_up_with_the_signal_between_blocks() {
        let profile = Profile::W2400;
        let seg = segment(profile, "ds4-3/4", 2, 4);
        let audio = through_all(std::slice::from_ref(&seg), &channel(profile, 12.0), true);
        let mut rx = Receiver::new(profile, FS);
        let chunk = (FS / 10.0) as usize;
        let (mut last_p, mut last_change, mut worst, mut seen) = (0usize, 0.0f64, 0.0f64, false);
        for (i, c) in audio.chunks(chunk).enumerate() {
            rx.push(c);
            let t = i as f64 / 10.0;
            let Some(ch) = rx.snapshot(0).channels.first().cloned() else {
                if seen {
                    break;
                }
                continue;
            };
            let p = ch.blocks_done * ch.live.symbols_in_block.max(1) + ch.live.symbol;
            if !seen {
                seen = true;
                last_p = p;
                last_change = t;
            }
            if p != last_p {
                last_p = p;
                last_change = t;
            }
            worst = worst.max(t - last_change);
        }
        assert!(seen, "the channel opened");
        assert!(worst < 1.0, "the display stood still for {worst:.1} s");
    }

    /// One station's audio at a band: its segment and postamble, shifted by its
    /// own carrier offset (a clean channel, no noise), scaled by `gain`.
    fn station(
        call: &str,
        seed: u8,
        band: Band,
        mode: &str,
        offset_hz: f64,
        gain: f32,
    ) -> (Segment, Vec<f32>) {
        let mut seg = segment(band.profile, mode, 1, 2);
        seg.header.call = call.into();
        for (i, f) in seg.frames.iter_mut().enumerate() {
            for b in f.iter_mut() {
                *b = b.wrapping_mul(seed).wrapping_add(i as u8);
            }
        }
        let chips = crate::tx::transmission_chips(std::slice::from_ref(&seg)).unwrap();
        let audio = crate::tx::chips_to_audio_at(&chips, band, FS, LEVEL * gain, 1.0, 1.0);
        let mut shift = Channel::clean(f64::from(LEVEL), band.profile.bandwidth_hz());
        shift.offset_hz = offset_hz;
        (seg, shift.apply(&audio, FS))
    }

    /// Sum of stations' audio plus noise at `snr_db` in the 500 Hz bandwidth,
    /// relative to a station at the usual level.
    fn on_one_frequency(stations: &[&Vec<f32>], snr_db: f64) -> Vec<f32> {
        let n = stations.iter().map(|a| a.len()).max().unwrap();
        let mut mix = vec![0.0f32; n];
        for a in stations {
            for (m, v) in mix.iter_mut().zip(a.iter()) {
                *m += v;
            }
        }
        channel(Profile::N500, snr_db).apply(&mix, FS)
    }

    /// Frames each (call, band) delivered.
    fn delivered(events: &[Tagged]) -> std::collections::BTreeMap<(String, Band), usize> {
        let mut got = std::collections::BTreeMap::new();
        for t in events {
            if let Event::Block { header, result, .. } = &t.event {
                *got.entry((header.call.clone(), t.band)).or_insert(0) +=
                    result.frames.iter().flatten().count();
            }
        }
        got
    }

    /// The 500 Hz profile has three slots side by side, 500 Hz apart: three stations
    /// send at once, one in each, and all three are received, each by the receiver
    /// at its own slot and only that one.
    #[test]
    fn three_500_hz_stations_share_the_passband() {
        let bands = [
            Band::new(Profile::N500, Slot::Low),
            Band::new(Profile::N500, Slot::Mid),
            Band::new(Profile::N500, Slot::High),
        ];
        let sts: Vec<(Segment, Vec<f32>)> = [("VK2ABC", 3u8), ("W1AW", 5), ("JA1XYZ", 7)]
            .iter()
            .zip(bands)
            .map(|(&(call, seed), band)| station(call, seed, band, "ds4-1/2", 0.0, 1.0))
            .collect();
        let mix = on_one_frequency(&[&sts[0].1, &sts[1].1, &sts[2].1], 12.0);
        let mut mr = MultiReceiver::with_bands(&bands, FS);
        let mut events = Vec::new();
        for c in mix.chunks(4800) {
            events.extend(mr.push(c));
        }
        let got = delivered(&events);
        for (i, (seg, _)) in sts.iter().enumerate() {
            assert_eq!(
                got.get(&(seg.header.call.clone(), bands[i])),
                Some(&seg.frames.len()),
                "{} at {}: {got:?}",
                seg.header.call,
                bands[i].label()
            );
        }
        assert_eq!(
            got.len(),
            3,
            "no station heard in a slot that is not its own: {got:?}"
        );
    }

    /// What a strong neighbour 500 Hz away does to a weak station: the weak one in
    /// the middle slot at 8 dB in-band SNR, a neighbour in the slot above (and again
    /// below) louder by up to 70 dB, its carrier off by 0 and ±25 Hz (toward the weak
    /// one and away). Run with
    /// `cargo test --release -p decdsss-phy fdma_neighbour -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    fn fdma_neighbour_curve() {
        let trials = 8u64;
        let weak_band = Band::new(Profile::N500, Slot::Mid);
        for mode in ["ds4-1/2"] {
            println!("weak station {mode}, 8 dB in-band SNR");
            for (side, slot) in [("above", Slot::High), ("below", Slot::Low)] {
                for off in [0.0, 25.0, -25.0] {
                    let mut row = String::new();
                    for louder_db in [-100.0f64, 40.0, 50.0, 60.0, 70.0] {
                        let (mut ok, mut all) = (0usize, 0usize);
                        for seed in 0..trials {
                            let (w, wa) = station("VK2ABC", 3, weak_band, mode, 0.0, 1.0);
                            let gain = 10f32.powf(louder_db as f32 / 20.0);
                            let (_, na) = station(
                                "W1AW",
                                5,
                                Band::new(Profile::N500, slot),
                                "ds4-1/2",
                                off,
                                gain,
                            );
                            let mut ch = channel(Profile::N500, 8.0);
                            ch.seed = 7 + seed;
                            let n = wa.len().max(na.len());
                            let mut mix = vec![0.0f32; n];
                            for (m, v) in mix.iter_mut().zip(&wa) {
                                *m += v;
                            }
                            for (m, v) in mix.iter_mut().zip(&na) {
                                *m += v;
                            }
                            let mix = ch.apply(&mix, FS);
                            let mut mr = MultiReceiver::with_bands(&[weak_band], FS);
                            let mut events = Vec::new();
                            for c in mix.chunks(4800) {
                                events.extend(mr.push(c));
                            }
                            ok += delivered(&events)
                                .get(&("VK2ABC".to_string(), weak_band))
                                .copied()
                                .unwrap_or(0);
                            all += w.frames.len();
                        }
                        row += &format!("  +{louder_db:>4.0} dB: {ok:>2}/{all}");
                    }
                    println!("  neighbour {side}, {off:+.0} Hz:{row}");
                }
            }
        }
    }

    /// A very strong station (no noise at all, as over a cable) in the upper slot is
    /// found a little by the other slots' preamble searches, folded by the chip rate;
    /// it must be reported once, by the slot it is in, with no copy from another and
    /// no channel left open in another's snapshot.
    #[test]
    fn a_strong_station_is_reported_by_its_own_slot_only() {
        let high = Band::new(Profile::N500, Slot::High);
        let (seg, audio) = station("VK2ABC", 3, high, "ds4-1/2", 0.0, 1.0);
        let mut mix = audio;
        mix.extend(vec![0.0; (4.0 * FS) as usize]);
        let mut mr = MultiReceiver::with_bands(&Band::ALL[..3], FS);
        let (mut events, mut snapshots_with_channels) = (Vec::new(), 0usize);
        for c in mix.chunks(4800) {
            events.extend(mr.push(c));
            for s in mr.snapshots(0) {
                if s.band != high {
                    snapshots_with_channels += s.channels.len();
                }
            }
        }
        let headers: Vec<Band> = events
            .iter()
            .filter(|t| matches!(t.event, Event::Header { .. }))
            .map(|t| t.band)
            .collect();
        assert_eq!(headers, vec![high], "one header, from the slot it is in");
        let got = delivered(&events);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            got.get(&("VK2ABC".to_string(), high)),
            Some(&seg.frames.len())
        );
        assert_eq!(
            snapshots_with_channels, 0,
            "no ghost channel in another slot"
        );
    }

    fn eots(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, Event::EndOfTransmission { .. }))
            .count()
    }

    /// The postamble after the last segment is heard once, after that segment
    /// ended; a transmission without one never reports an end; a postamble after
    /// each of three back-to-back segments does not appear (only the last carries
    /// one) and does not disturb the segments after it.
    #[test]
    fn the_postamble_ends_the_transmission_once() {
        for (profile, mode) in [
            (Profile::W2400, "ds8-1/2"),
            (Profile::W3000, "csk64-2/3"),
            (Profile::N500, "ds4-1/2"),
        ] {
            let seg = segment(profile, mode, 1, 2);
            let with = through_all(std::slice::from_ref(&seg), &channel(profile, 6.0), true);
            let events = run(&mut Receiver::new(profile, FS), &with);
            let end = events
                .iter()
                .position(|e| matches!(e, Event::SegmentEnd { .. }))
                .expect("segment end");
            let eot = events
                .iter()
                .position(|e| matches!(e, Event::EndOfTransmission { .. }))
                .unwrap_or_else(|| panic!("{profile} {mode}: no end of transmission"));
            assert!(
                eot > end,
                "{profile}: the postamble comes after the segment"
            );
            assert_eq!(eots(&events), 1, "{profile}");
            assert_eq!(frames_of(&events).len(), seg.frames.len(), "{profile}");

            let without = through_all(std::slice::from_ref(&seg), &channel(profile, 6.0), false);
            let events = run(&mut Receiver::new(profile, FS), &without);
            assert_eq!(eots(&events), 0, "{profile}: none was sent");
        }

        let profile = Profile::W2400;
        let segs: Vec<Segment> = (0..3usize)
            .map(|i| {
                let mut s = segment(profile, "csk32-1/2", 1, 2);
                s.header.frame0 = (i * 2) as u16;
                s.header.total_frames = 6;
                s
            })
            .collect();
        let audio = through_all(&segs, &channel(profile, 3.0), true);
        let events = run(&mut Receiver::new(profile, FS), &audio);
        assert_eq!(frames_of(&events).len(), 6);
        assert_eq!(eots(&events), 1);
        let last_block = events
            .iter()
            .rposition(|e| matches!(e, Event::Block { .. }))
            .unwrap();
        let eot = events
            .iter()
            .position(|e| matches!(e, Event::EndOfTransmission { .. }))
            .unwrap();
        assert!(eot > last_block);
    }

    /// How often the postamble is heard against signal-to-noise ratio, and how
    /// often something that is not one is taken for it. Run with
    /// `cargo test --release -p decdsss-phy eot_curve -- --ignored --nocapture`.
    /// "data" is the place of the next block in the middle of a stream (what a live
    /// receiver tests after every block), "quiet" the noise after a transmission
    /// that sent no postamble.
    #[test]
    #[ignore = "measurement"]
    fn eot_curve() {
        use decdsss_chan::Watterson;
        let profile = Profile::W2400;
        let trials = 24;
        // The postamble does not depend on the mode; the modes only differ in what
        // the "data" case sees. Short blocks keep the signals short.
        for (mode, fading) in [
            ("ds4-3/4", None),
            ("ds4-3/4", Some(Watterson::MODERATE)),
            ("ds8-1/2", None),
        ] {
            println!("{mode} fading {}", fading.is_some());
            for snr in [-27.0, -24.0, -21.0, -18.0, -15.0, -12.0, -6.0, 6.0] {
                let (mut hit, mut data_false, mut quiet_false) = (0, 0, 0);
                let (mut worst_data, mut worst_quiet, mut worst_hit) = (0.0f32, 0.0f32, f32::MAX);
                for seed in 0..trials {
                    let seg = segment(profile, mode, 1, 2);
                    let mut ch = channel(profile, snr);
                    ch.fading = fading;
                    ch.seed = 100 + seed;
                    ch.offset_hz = 4.0;
                    let audio = through_all(std::slice::from_ref(&seg), &ch, true);
                    let (mut rx, buf) = genie(&seg, &ch, &audio);
                    // The data case: after block 0, with block 1 coming.
                    rx.skip_blocks(1);
                    if let Some(r) = rx.probe_eot(&buf) {
                        worst_data = worst_data.max(r);
                        data_false += usize::from(r >= crate::eot::EOT_PEAK_OVER_MEDIAN);
                    }
                    rx.skip_blocks(1);
                    if let Some(r) = rx.probe_eot(&buf) {
                        worst_hit = worst_hit.min(r);
                        hit += usize::from(r >= crate::eot::EOT_PEAK_OVER_MEDIAN);
                    }
                    let quiet = through_all(std::slice::from_ref(&seg), &ch, false);
                    let (mut rx, buf) = genie(&seg, &ch, &quiet);
                    rx.skip_blocks(2);
                    if let Some(r) = rx.probe_eot(&buf) {
                        worst_quiet = worst_quiet.max(r);
                        quiet_false += usize::from(r >= crate::eot::EOT_PEAK_OVER_MEDIAN);
                    }
                }
                println!(
                    "  {snr:5.1} dB: heard {hit}/{trials} (weakest ratio {worst_hit:.1}); false on data {data_false} (highest {worst_data:.1}), on quiet {quiet_false} (highest {worst_quiet:.1})"
                );
            }
        }
    }

    #[test]
    fn two_stations_at_once_on_different_codes() {
        let profile = Profile::W2400;
        let mk = |call: &str, frame_seed: u8, delay_s: f64| {
            let mut seg = segment(profile, "csk64-1/2", 1, 2);
            seg.header.call = call.into();
            for (i, f) in seg.frames.iter_mut().enumerate() {
                for b in f.iter_mut() {
                    *b = b.wrapping_mul(frame_seed).wrapping_add(i as u8);
                }
            }
            let chips = segment_chips(&seg).unwrap();
            let audio = chips_to_audio(&chips, profile, FS, LEVEL, delay_s, 1.0);
            (seg, audio)
        };
        // The second starts 12 s later, while the first is still sending.
        let (a, au) = mk("VK2ABC", 3, 1.0);
        let (b, bu) = mk("W1AW", 5, 13.0);
        let n = au.len().max(bu.len());
        let mut mix = vec![0.0f32; n];
        for (i, v) in au.iter().enumerate() {
            mix[i] += v;
        }
        for (i, v) in bu.iter().enumerate() {
            mix[i] += v * 0.7;
        }
        let mixed = channel(profile, 3.0).apply(&mix, FS);
        let mut rx = Receiver::new(profile, FS);
        let events = run(&mut rx, &mixed);
        let mut got = std::collections::BTreeMap::new();
        for e in &events {
            if let Event::Block { header, result, .. } = e {
                for (i, f) in result.frames.iter().enumerate() {
                    if f.is_some() {
                        got.insert(
                            (header.call.clone(), result.first_frame + i),
                            f.clone().unwrap(),
                        );
                    }
                }
            }
        }
        for (seg, call) in [(&a, "VK2ABC"), (&b, "W1AW")] {
            for (i, want) in seg.frames.iter().enumerate() {
                assert_eq!(
                    got.get(&(call.to_string(), i)),
                    Some(want),
                    "{call} frame {i}; got {:?}",
                    got.keys().collect::<Vec<_>>()
                );
            }
        }
        let _ = Channel::clean(0.0, 1.0);
    }
}
