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
pub mod header;
pub mod payload;
pub mod preamble;
pub mod refine;
#[cfg(test)]
pub mod testrig;

use crate::framing::Header;
use acq::{Acquirer, Detection};
use decdsss_core::Profile;
use frontend::{Buf, Frontend, SPS};
use header::{HeaderOutcome, HeaderResult, HeaderView, HeaderWatch, demod_header};
use payload::{BlockResult, PayloadRx};
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
        let h = &self.rx.header;
        h.total_frames == 0
            && matches!(
                h.ptype,
                crate::framing::PayloadType::Voice | crate::framing::PayloadType::Voice450
            )
    }
}

pub struct Receiver {
    pub profile: Profile,
    fe: Frontend,
    buf: Buf,
    acq: Acquirer,
    attempts: Vec<Attempt>,
    channels: Vec<Channel>,
    next_channel: usize,
    rate: f64,
    /// Total input samples seen, for the caller's clock.
    pub samples_in: u64,
    pending: VecDeque<Event>,
}

impl Receiver {
    pub fn new(profile: Profile, sample_rate: f64) -> Receiver {
        let fe = Frontend::new(profile, sample_rate);
        let rate = fe.out_rate();
        let acq = Acquirer::new(profile, SEARCH_OFFSET_HZ);
        Receiver {
            profile,
            fe,
            buf: Buf::default(),
            attempts: Vec::new(),
            acq,
            channels: Vec::new(),
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
            if ch.rx.done() || (ch.is_live() && ch.lost_run >= LIVE_LOST_BLOCKS) {
                self.pending.push_back(Event::SegmentEnd { channel: ch.id });
                self.channels.remove(i);
            } else {
                i += 1;
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

/// A receiver per profile, fed the same audio.
pub struct MultiReceiver {
    rx: Vec<Receiver>,
}

impl MultiReceiver {
    pub fn new(profiles: &[Profile], sample_rate: f64) -> MultiReceiver {
        MultiReceiver {
            rx: profiles
                .iter()
                .map(|&p| Receiver::new(p, sample_rate))
                .collect(),
        }
    }

    pub fn snapshots(&self, search_cols: usize) -> Vec<Snapshot> {
        self.rx.iter().map(|r| r.snapshot(search_cols)).collect()
    }

    pub fn push(&mut self, audio: &[f32]) -> Vec<Tagged> {
        let mut out = Vec::new();
        for r in &mut self.rx {
            let profile = r.profile;
            out.extend(
                r.push(audio)
                    .into_iter()
                    .map(|event| Tagged { profile, event }),
            );
        }
        out
    }
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
