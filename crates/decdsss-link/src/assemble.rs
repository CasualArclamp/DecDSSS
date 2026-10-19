// made by CasualArclamp
//! Receive-side assembly: frames from any segment, any pass, into files.

use crate::container;
use decdsss_phy::framing::Header;
use decdsss_phy::rx::payload::BlockResult;
use std::collections::BTreeMap;

/// A file that arrived whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedFile {
    pub call: String,
    pub session: u8,
    pub name: String,
    pub data: Vec<u8>,
}

/// How far a transmission has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub have: usize,
    /// 0 if the sender did not say.
    pub total: usize,
}

#[derive(Default)]
struct Transfer {
    total: usize,
    frames: BTreeMap<usize, Vec<u8>>,
    done: bool,
}

/// Gathers frames by (callsign, session).
#[derive(Default)]
pub struct Assembler {
    transfers: BTreeMap<(String, u8), Transfer>,
}

impl Assembler {
    pub fn new() -> Assembler {
        Assembler::default()
    }

    /// Takes a decoded block; returns the file if this block completed it.
    /// Frames already held are kept (a repeated pass cannot damage them: they
    /// carry a CRC-32).
    pub fn push(&mut self, header: &Header, block: &BlockResult) -> Option<ReceivedFile> {
        let t = self
            .transfers
            .entry((header.call.clone(), header.session))
            .or_default();
        let total = usize::from(header.total_frames);
        if total > 0 {
            t.total = total;
        }
        if t.done {
            return None;
        }
        for (i, f) in block.frames.iter().enumerate() {
            let idx = block.first_frame + i;
            if let Some(data) = f
                && (t.total == 0 || idx < t.total)
            {
                t.frames.entry(idx).or_insert_with(|| data.clone());
            }
        }
        let file = Self::try_finish(&header.call, header.session, t)?;
        t.done = true;
        Some(file)
    }

    fn try_finish(call: &str, session: u8, t: &Transfer) -> Option<ReceivedFile> {
        // The container says how long it is once its front is in.
        let first = t.frames.get(&0)?;
        let need = container::total_len(first)?;
        let fb = first.len();
        let frames_needed = need.div_ceil(fb);
        if t.total != 0 && frames_needed > t.total {
            return None;
        }
        let mut bytes = Vec::with_capacity(frames_needed * fb);
        for i in 0..frames_needed {
            bytes.extend_from_slice(t.frames.get(&i)?);
        }
        let (name, data) = container::unpack(&bytes)?;
        Some(ReceivedFile {
            call: call.to_string(),
            session,
            name,
            data,
        })
    }

    /// Progress of a transmission, if any frame of it has arrived.
    pub fn progress(&self, call: &str, session: u8) -> Option<Progress> {
        self.transfers
            .get(&(call.to_string(), session))
            .map(|t| Progress {
                have: t.frames.len(),
                total: t.total,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::{SendSpec, segments};
    use decdsss_core::{Mode, Profile};
    use decdsss_phy::framing::PayloadType;

    fn block(first: usize, frames: Vec<Option<Vec<u8>>>) -> BlockResult {
        BlockResult {
            index: 0,
            first_frame: first,
            frames,
            iterations: vec![],
            fingers: vec![],
            freq_hz: 0.0,
            snr_db: 0.0,
        }
    }

    #[test]
    fn a_file_arrives_across_two_passes_with_gaps() {
        let data: Vec<u8> = (0..2000u32).map(|i| (i * 13) as u8).collect();
        let stream = container::pack("a.bin", &data);
        let spec = SendSpec {
            profile: Profile::W2400,
            mode: Mode::from_name("ds8-1/2").unwrap(),
            depth: 1,
            call: "VK2ABC".into(),
            session: 9,
            ptype: PayloadType::File,
            resync_s: 20.0,
        };
        let segs = segments(&spec, &stream).unwrap();
        let all: Vec<Vec<u8>> = segs.iter().flat_map(|s| s.frames.clone()).collect();
        let header = &segs[0].header;
        let mut asm = Assembler::new();
        // Pass one loses every third frame.
        for (i, f) in all.iter().enumerate() {
            let got = (i % 3 != 1).then(|| f.clone());
            assert!(asm.push(header, &block(i, vec![got])).is_none());
        }
        let p = asm.progress("VK2ABC", 9).unwrap();
        assert!(p.have < p.total);
        // Pass two brings the rest.
        let mut done = None;
        for (i, f) in all.iter().enumerate().filter(|(i, _)| i % 3 == 1) {
            done = done.or(asm.push(header, &block(i, vec![Some(f.clone())])));
        }
        let file = done.expect("not complete");
        assert_eq!(
            (file.name.as_str(), file.data.as_slice()),
            ("a.bin", &data[..])
        );
        // And it is delivered only once.
        assert!(
            asm.push(header, &block(0, vec![Some(all[0].clone())]))
                .is_none()
        );
    }
}
