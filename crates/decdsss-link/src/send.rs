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
use decdsss_phy::tx::Segment;

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

/// Cuts `stream` (a container, or any bytes) into coded frames and the frames
/// into segments.
pub fn plan(spec: &SendSpec, stream: &[u8]) -> Result<Plan, String> {
    let layout = Layout::new(spec.profile, spec.mode, spec.depth);
    let fb = layout.frame_bytes();
    let shard_bytes = fb - FRAME_META;
    if stream.is_empty() {
        return Err("nothing to send".into());
    }
    let k = stream.len().div_ceil(shard_bytes);
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

    let block_frames = spec.depth;
    let blocks_total = frames.len().div_ceil(block_frames);
    let per_segment = ((spec.resync_s / layout.seconds()).floor() as usize).clamp(1, 4095);
    let mut segments = Vec::new();
    let mut block = 0;
    while block < blocks_total {
        let n = per_segment.min(blocks_total - block);
        let from = block * block_frames;
        let to = ((block + n) * block_frames).min(frames.len());
        segments.push(Segment {
            header: Header {
                profile: spec.profile,
                mode: spec.mode,
                depth: spec.depth,
                ptype: spec.ptype,
                call: spec.call.clone(),
                session: spec.session,
                frame0: from as u16,
                nblocks: n as u16,
                total_frames: frames.len() as u16,
            },
            frames: frames[from..to].to_vec(),
        });
        block += n;
    }
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
    fn refuses_empty_and_oversize() {
        assert!(plan(&spec(60.0, 0.25, 1), &[]).is_err());
        assert!(plan(&spec(60.0, 0.25, 1), &vec![0u8; 4_000_000]).is_err());
        assert!(plan(&spec(60.0, 3.0, 9), &vec![0u8; 100_000]).is_err());
    }
}
