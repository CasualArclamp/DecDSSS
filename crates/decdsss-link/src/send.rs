// made by CasualArclamp
//! Sender planning: a byte stream into segments of frames.

use decdsss_core::{Mode, Profile};
use decdsss_phy::framing::{Header, Layout, PayloadType};
use decdsss_phy::tx::Segment;

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
}

/// Most frames one transmission may have (the header's 15-bit count).
pub const MAX_FRAMES: usize = (1 << 15) - 1;

/// Cuts `stream` (a container, or any bytes) into frames and the frames into
/// segments. The last frame is zero-padded.
pub fn segments(spec: &SendSpec, stream: &[u8]) -> Result<Vec<Segment>, String> {
    let layout = Layout::new(spec.profile, spec.mode, spec.depth);
    let fb = layout.frame_bytes();
    let frames: Vec<Vec<u8>> = stream
        .chunks(fb)
        .map(|c| {
            let mut f = c.to_vec();
            f.resize(fb, 0);
            f
        })
        .collect();
    if frames.is_empty() {
        return Err("nothing to send".into());
    }
    if frames.len() > MAX_FRAMES {
        return Err(format!(
            "{} frames is more than the {MAX_FRAMES} one transmission can number: use a faster mode or a smaller file",
            frames.len()
        ));
    }
    let block_frames = spec.depth;
    let blocks_total = frames.len().div_ceil(block_frames);
    let per_segment = ((spec.resync_s / layout.seconds()).floor() as usize).clamp(1, 4095);
    let mut out = Vec::new();
    let mut block = 0;
    while block < blocks_total {
        let n = per_segment.min(blocks_total - block);
        let from = block * block_frames;
        let to = ((block + n) * block_frames).min(frames.len());
        out.push(Segment {
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(resync_s: f64) -> SendSpec {
        SendSpec {
            profile: Profile::W2400,
            mode: Mode::from_name("ds8-1/2").unwrap(),
            depth: 2,
            call: "VK2ABC".into(),
            session: 3,
            ptype: PayloadType::File,
            resync_s,
        }
    }

    #[test]
    fn frames_cover_the_stream_and_segments_cover_the_frames() {
        let layout = Layout::new(Profile::W2400, spec(0.0).mode, 2);
        let stream: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let segs = segments(&spec(30.0), &stream).unwrap();
        assert!(segs.len() > 1);
        let mut next = 0u16;
        let mut bytes = Vec::new();
        for s in &segs {
            assert_eq!(s.header.frame0, next);
            assert_eq!(
                s.header.total_frames as usize,
                5000usize.div_ceil(layout.frame_bytes())
            );
            next += s.frames.len() as u16;
            for f in &s.frames {
                bytes.extend_from_slice(f);
            }
            let secs = f64::from(s.header.nblocks) * layout.seconds();
            assert!(secs <= 30.0 + layout.seconds(), "{secs}");
        }
        assert_eq!(&bytes[..5000], &stream[..]);
        assert!(bytes[5000..].iter().all(|&b| b == 0));
    }

    #[test]
    fn refuses_empty_and_oversize() {
        assert!(segments(&spec(60.0), &[]).is_err());
        assert!(segments(&spec(60.0), &vec![0u8; 4_000_000]).is_err());
    }
}
