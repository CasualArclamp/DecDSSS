// made by CasualArclamp
//! Sender planning: a byte stream into erasure-coded frames, and the frames
//! into segments.
//!
//! Each frame's data is `[ident: u16 LE][count: u16 LE][shard]`. The stream is
//! cut into `count` data shards (the last zero-padded); a frame carries shard
//! number `ident` if that is below `count`, otherwise a coded shard (a Cauchy
//! combination of all the data shards, see [`crate::erasure`]). Any `count`
//! distinct frames rebuild the stream, so a frame lost to a fade costs one
//! more frame, not a retransmission of that one.
//!
//! A pass sends the data shards then some coded ones; each further pass sends
//! *new* coded frames, so a listener who missed any part of the first pass is
//! helped by every frame of the next, not only by the ones it lost.

use crate::erasure::{self, MAX_BLOCKS};
use decdsss_core::{Mode, Profile};
use decdsss_phy::framing::{Header, Layout, PayloadType};
use decdsss_phy::tx::{Segment, lead_chips_for};

/// Bytes of frame data ahead of the shard: the ident and the shard count.
pub const FRAME_META: usize = 4;

/// What to send and how.
#[derive(Clone, Debug)]
pub struct SendSpec {
    pub profile: Profile,
    pub mode: Mode,
    /// Codewords per block (1, 2, 4 or 8): deeper rides out longer fades,
    /// shallower starts and ends sooner.
    pub depth: usize,
    pub call: String,
    pub session: u8,
    pub ptype: PayloadType,
    /// Seconds of payload between preambles. Each segment repeats the preamble
    /// and header so a receiver can join late.
    pub resync_s: f64,
    /// Coded frames sent with the data frames, as a fraction of their number
    /// (0.25: a quarter again). Later passes send as many frames as the first.
    pub parity: f64,
    /// How many passes to send (at least one).
    pub passes: usize,
}

/// Most frames one transmission may have (the header's 15-bit count).
pub const MAX_FRAMES: usize = (1 << 15) - 1;

/// What a plan came to.
#[derive(Clone, Debug)]
pub struct Plan {
    pub segments: Vec<Segment>,
    /// Data frames: any this many distinct frames rebuild the stream.
    pub data_frames: usize,
    /// Coded frames in the first pass.
    pub coded_first_pass: usize,
    pub passes: usize,
    /// Bytes of stream a frame carries.
    pub shard_bytes: usize,
}

impl Plan {
    pub fn frames(&self) -> usize {
        self.segments.iter().map(|s| s.frames.len()).sum()
    }
}

fn frame(ident: u16, count: usize, shard: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(FRAME_META + shard.len());
    f.extend_from_slice(&ident.to_le_bytes());
    f.extend_from_slice(&(count as u16).to_le_bytes());
    f.extend_from_slice(shard);
    f
}

/// The arithmetic of a plan, without the frames.
struct Shape {
    shard_bytes: usize,
    /// Data frames.
    k: usize,
    /// Coded frames in the first pass.
    p: usize,
    passes: usize,
    per_pass: usize,
    total: usize,
}

fn shape(spec: &SendSpec, stream_len: usize) -> Result<Shape, String> {
    let layout = Layout::new(spec.profile, spec.mode, spec.depth);
    let shard_bytes = layout.frame_bytes() - FRAME_META;
    if stream_len == 0 {
        return Err("nothing to send".into());
    }
    let k = stream_len.div_ceil(shard_bytes);
    if k > MAX_BLOCKS {
        return Err(format!(
            "{k} frames is more than a transmission can number: use a faster mode or a smaller file"
        ));
    }
    let passes = spec.passes.max(1);
    let p = (spec.parity.max(0.0) * k as f64).ceil() as usize;
    let per_pass = k + p;
    let total = per_pass * passes;
    if total > MAX_FRAMES || k + total > usize::from(u16::MAX) {
        return Err(format!(
            "{total} frames is more than the {MAX_FRAMES} one transmission can number: fewer passes, less parity, a faster mode or a smaller file"
        ));
    }
    Ok(Shape {
        shard_bytes,
        k,
        p,
        passes,
        per_pass,
        total,
    })
}

/// Seconds of signal `total_frames` frames take: every block, the preamble and
/// header of every segment, and the postamble after the last.
pub(crate) fn air_time(p: &Frames, total_frames: usize) -> f64 {
    let layout = Layout::new(p.profile, p.mode, p.depth);
    let blocks = total_frames.div_ceil(p.depth);
    let per_segment = ((p.resync_s / layout.seconds()).floor() as usize).clamp(1, 4095);
    let segments = blocks.div_ceil(per_segment);
    let lead = lead_chips_for(p.profile, p.mode) as f64 / f64::from(p.profile.chip_rate());
    blocks as f64 * layout.seconds() + segments as f64 * lead + decdsss_phy::eot::seconds(p.profile)
}

/// Seconds of signal a `stream_len`-byte stream takes to send under `spec`.
pub fn air_seconds(spec: &SendSpec, stream_len: usize) -> Result<f64, String> {
    let sh = shape(spec, stream_len)?;
    Ok(air_time(
        &Frames {
            profile: spec.profile,
            mode: spec.mode,
            depth: spec.depth,
            ptype: spec.ptype,
            call: &spec.call,
            session: spec.session,
            resync_s: spec.resync_s,
        },
        sh.total,
    ))
}

/// How a transmission's time is spent, for drawing it to scale: the parts
/// [`air_seconds`] adds up. Worked out from the arithmetic alone, so no frame is
/// encoded.
#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    /// The preamble and header that open every segment, seconds.
    pub lead_s: f64,
    /// One block (`depth` frames), seconds.
    pub block_s: f64,
    /// Frames a block.
    pub depth: usize,
    /// Blocks in each segment, in order.
    pub segments: Vec<usize>,
    /// Data frames: the first this many; the rest are coded.
    pub data_frames: usize,
    /// Frames in all.
    pub frames: usize,
    /// Frames a pass: the first is the data frames and their coded ones, each
    /// later pass as many new coded frames.
    pub per_pass: usize,
    /// The postamble after the last segment, seconds.
    pub eot_s: f64,
}

impl Timeline {
    /// Seconds of signal in all.
    pub fn seconds(&self) -> f64 {
        let blocks: usize = self.segments.iter().sum();
        self.segments.len() as f64 * self.lead_s + blocks as f64 * self.block_s + self.eot_s
    }

    /// The timeline of segments already cut, every frame of them data (speech).
    pub fn of_segments(segments: &[Segment]) -> Option<Timeline> {
        let h = &segments.first()?.header;
        let frames: usize = segments.iter().map(|s| s.frames.len()).sum();
        Some(Timeline {
            lead_s: lead_chips_for(h.profile, h.mode) as f64 / f64::from(h.profile.chip_rate()),
            block_s: Layout::new(h.profile, h.mode, h.depth).seconds(),
            depth: h.depth,
            segments: segments
                .iter()
                .map(|s| usize::from(s.header.nblocks))
                .collect(),
            data_frames: frames,
            frames,
            per_pass: frames,
            eot_s: decdsss_phy::eot::seconds(h.profile),
        })
    }
}

/// The [`Timeline`] of sending a `stream_len`-byte stream under `spec`: the
/// segments [`cut_segments`] makes, without making them.
pub fn timeline(spec: &SendSpec, stream_len: usize) -> Result<Timeline, String> {
    let sh = shape(spec, stream_len)?;
    let layout = Layout::new(spec.profile, spec.mode, spec.depth);
    let blocks = sh.total.div_ceil(spec.depth);
    let per_segment = ((spec.resync_s / layout.seconds()).floor() as usize).clamp(1, 4095);
    // Whole segments of `per_segment` blocks, and what is left over in the last.
    let segments = (0..blocks.div_ceil(per_segment))
        .map(|i| per_segment.min(blocks - i * per_segment))
        .collect();
    Ok(Timeline {
        lead_s: lead_chips_for(spec.profile, spec.mode) as f64
            / f64::from(spec.profile.chip_rate()),
        block_s: layout.seconds(),
        depth: spec.depth,
        segments,
        data_frames: sh.k,
        frames: sh.total,
        per_pass: sh.per_pass,
        eot_s: decdsss_phy::eot::seconds(spec.profile),
    })
}

/// The longest stream (container included) that goes in `seconds` of signal
/// under `spec`; 0 if not even one frame does.
pub fn max_stream_bytes(spec: &SendSpec, seconds: f64) -> usize {
    let fits = |n: usize| air_seconds(spec, n).is_ok_and(|t| t <= seconds);
    if !fits(1) {
        return 0;
    }
    let (mut lo, mut hi) = (1usize, 8usize << 20);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// What goes in the header of every segment of a transmission.
pub(crate) struct Frames<'a> {
    pub profile: Profile,
    pub mode: Mode,
    pub depth: usize,
    pub ptype: PayloadType,
    pub call: &'a str,
    pub session: u8,
    pub resync_s: f64,
}

/// Cuts frames into segments of whole blocks, a segment lasting about
/// `resync_s` seconds.
pub(crate) fn cut_segments(p: &Frames, frames: &[Vec<u8>]) -> Vec<Segment> {
    let layout = Layout::new(p.profile, p.mode, p.depth);
    let block_frames = p.depth;
    let blocks_total = frames.len().div_ceil(block_frames);
    let per_segment = ((p.resync_s / layout.seconds()).floor() as usize).clamp(1, 4095);
    let mut segments = Vec::new();
    let mut block = 0;
    while block < blocks_total {
        let n = per_segment.min(blocks_total - block);
        let from = block * block_frames;
        let to = ((block + n) * block_frames).min(frames.len());
        segments.push(Segment {
            header: Header {
                profile: p.profile,
                mode: p.mode,
                depth: p.depth,
                ptype: p.ptype,
                call: p.call.to_string(),
                // Cut to what this mode and type's header holds: 6 bits as it comes,
                // 5 for a wide payload type, 4 for bi-orthogonal CSK.
                session: p.ptype.fit_session(p.mode.family, p.session),
                frame0: from as u16,
                nblocks: n as u16,
                total_frames: frames.len() as u16,
            },
            frames: frames[from..to].to_vec(),
        });
        block += n;
    }
    segments
}

/// Cuts `stream` (a container, or any bytes) into coded frames and the frames
/// into segments.
pub fn plan(spec: &SendSpec, stream: &[u8]) -> Result<Plan, String> {
    let Shape {
        shard_bytes,
        k,
        p,
        passes,
        per_pass,
        total,
    } = shape(spec, stream.len())?;
    let mut data = stream.to_vec();
    data.resize(k * shard_bytes, 0);
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(total);
    for (i, shard) in data.chunks(shard_bytes).enumerate() {
        frames.push(frame(i as u16, k, shard));
    }
    let mut next = k;
    for pass in 0..passes {
        let coded = if pass == 0 { p } else { per_pass };
        for _ in 0..coded {
            let shard = erasure::encode(&data, next as u16, shard_bytes);
            frames.push(frame(next as u16, k, &shard));
            next += 1;
        }
    }
    debug_assert_eq!(frames.len(), total);

    let segments = cut_segments(
        &Frames {
            profile: spec.profile,
            mode: spec.mode,
            depth: spec.depth,
            ptype: spec.ptype,
            call: &spec.call,
            session: spec.session,
            resync_s: spec.resync_s,
        },
        &frames,
    );
    Ok(Plan {
        segments,
        data_frames: k,
        coded_first_pass: p,
        passes,
        shard_bytes,
    })
}

/// The segments of [`plan`].
pub fn segments(spec: &SendSpec, stream: &[u8]) -> Result<Vec<Segment>, String> {
    plan(spec, stream).map(|p| p.segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(resync_s: f64, parity: f64, passes: usize) -> SendSpec {
        SendSpec {
            profile: Profile::W2400,
            mode: Mode::from_name("ds8-1/2").unwrap(),
            depth: 2,
            call: "VK2ABC".into(),
            session: 3,
            ptype: PayloadType::File,
            resync_s,
            parity,
            passes,
        }
    }

    #[test]
    fn frames_are_systematic_then_coded_and_segments_cover_them() {
        let layout = Layout::new(Profile::W2400, spec(0.0, 0.0, 1).mode, 2);
        let stream: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let p = plan(&spec(30.0, 0.25, 1), &stream).unwrap();
        assert_eq!(p.shard_bytes, layout.frame_bytes() - FRAME_META);
        assert_eq!(p.data_frames, 5000usize.div_ceil(p.shard_bytes));
        assert_eq!(
            p.coded_first_pass,
            (0.25 * p.data_frames as f64).ceil() as usize
        );
        assert_eq!(p.frames(), p.data_frames + p.coded_first_pass);
        assert!(p.segments.len() > 1);
        let mut next = 0u16;
        let mut all = Vec::new();
        for s in &p.segments {
            assert_eq!(s.header.frame0, next);
            assert_eq!(s.header.total_frames as usize, p.frames());
            next += s.frames.len() as u16;
            let secs = f64::from(s.header.nblocks) * layout.seconds();
            assert!(secs <= 30.0 + layout.seconds(), "{secs}");
            all.extend(s.frames.iter().cloned());
        }
        for (i, f) in all.iter().enumerate() {
            assert_eq!(
                u16::from_le_bytes([f[0], f[1]]) as usize,
                i,
                "idents count up"
            );
            assert_eq!(u16::from_le_bytes([f[2], f[3]]) as usize, p.data_frames);
            assert_eq!(f.len(), layout.frame_bytes());
        }
        // The systematic frames hold the stream.
        let bytes: Vec<u8> = all[..p.data_frames]
            .iter()
            .flat_map(|f| f[FRAME_META..].iter().copied())
            .collect();
        assert_eq!(&bytes[..5000], &stream[..]);
    }

    #[test]
    fn later_passes_send_new_coded_frames() {
        let stream = vec![7u8; 1000];
        let p = plan(&spec(60.0, 0.5, 3), &stream).unwrap();
        let mut idents: Vec<usize> = p
            .segments
            .iter()
            .flat_map(|s| s.frames.iter())
            .map(|f| u16::from_le_bytes([f[0], f[1]]) as usize)
            .collect();
        let n = idents.len();
        assert_eq!(n, 3 * (p.data_frames + p.coded_first_pass));
        idents.sort_unstable();
        idents.dedup();
        assert_eq!(idents.len(), n, "every frame of every pass is different");
    }

    #[test]
    fn air_time_matches_the_signal_and_the_budget_inverts_it() {
        let sp = spec(30.0, 0.25, 1);
        let stream = vec![5u8; 4000];
        let p = plan(&sp, &stream).unwrap();
        // The chips of the real segments: the same seconds.
        let chips = decdsss_phy::tx::transmission_chips(&p.segments)
            .unwrap()
            .len();
        let secs = chips as f64 / f64::from(sp.profile.chip_rate());
        let est = air_seconds(&sp, stream.len()).unwrap();
        assert!((secs - est).abs() < 1e-6, "{secs} vs {est}");
        // The largest stream that fits is within one frame of the budget.
        let n = max_stream_bytes(&sp, 100.0);
        assert!(air_seconds(&sp, n).unwrap() <= 100.0);
        assert!(air_seconds(&sp, n + p.shard_bytes).unwrap() > 100.0);
        assert_eq!(max_stream_bytes(&sp, 0.1), 0);
    }

    #[test]
    fn the_timeline_is_the_plan_without_the_frames() {
        for (resync, parity, passes, depth, len) in [
            (30.0, 0.25, 1, 2, 4000),
            (15.0, 0.5, 3, 4, 9000),
            (300.0, 0.0, 1, 1, 50),
            (0.0, 1.0, 2, 8, 2000),
        ] {
            let sp = SendSpec {
                depth,
                ..spec(resync, parity, passes)
            };
            let stream = vec![3u8; len];
            let p = plan(&sp, &stream).unwrap();
            let t = timeline(&sp, len).unwrap();
            let blocks: Vec<usize> = p
                .segments
                .iter()
                .map(|s| usize::from(s.header.nblocks))
                .collect();
            assert_eq!(t.segments, blocks, "{resync} {parity} {passes} {depth}");
            assert_eq!(t.data_frames, p.data_frames);
            assert_eq!(t.frames, p.frames());
            assert_eq!(t.per_pass, p.data_frames + p.coded_first_pass);
            let est = air_seconds(&sp, len).unwrap();
            assert!((t.seconds() - est).abs() < 1e-9, "{} vs {est}", t.seconds());
            let cut = Timeline::of_segments(&p.segments).unwrap();
            assert_eq!(cut.segments, t.segments);
            assert!((cut.seconds() - est).abs() < 1e-9);
        }
        assert!(timeline(&spec(60.0, 0.25, 1), 0).is_err());
    }

    #[test]
    fn refuses_empty_and_oversize() {
        assert!(plan(&spec(60.0, 0.25, 1), &[]).is_err());
        assert!(plan(&spec(60.0, 0.25, 1), &vec![0u8; 4_000_000]).is_err());
        assert!(plan(&spec(60.0, 3.0, 9), &vec![0u8; 100_000]).is_err());
    }
}
