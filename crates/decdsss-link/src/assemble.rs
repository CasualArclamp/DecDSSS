// made by CasualArclamp
//! Receive-side assembly: erasure-coded frames from any segment and any pass,
//! into files.

use crate::container;
use crate::erasure::Decoder;
use crate::send::FRAME_META;
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Distinct frames in (data or coded: any `need` of them suffice).
    pub have: usize,
    /// Frames needed to rebuild the file.
    pub need: usize,
    /// Which data frames are held as themselves.
    pub data_held: Vec<bool>,
}

struct Transfer {
    decoder: Decoder,
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
    pub fn push(&mut self, header: &Header, block: &BlockResult) -> Option<ReceivedFile> {
        let key = (header.call.clone(), header.session);
        let mut finished = None;
        for data in block.frames.iter().flatten() {
            if data.len() < FRAME_META + 2 {
                continue;
            }
            let ident = u16::from_le_bytes([data[0], data[1]]);
            let count = usize::from(u16::from_le_bytes([data[2], data[3]]));
            let shard = &data[FRAME_META..];
            if count == 0 || !shard.len().is_multiple_of(2) {
                continue;
            }
            // A different message under the same call and session starts over.
            let fresh = match self.transfers.get(&key) {
                Some(t) => t.decoder.count() != count || t.decoder.block_bytes() != shard.len(),
                None => true,
            };
            if fresh {
                self.transfers.insert(
                    key.clone(),
                    Transfer {
                        decoder: Decoder::new(count, shard.len()),
                        done: false,
                    },
                );
            }
            let t = self.transfers.get_mut(&key)?;
            if t.done || !t.decoder.push(ident, shard) {
                continue;
            }
            if t.decoder.have() < t.decoder.count() {
                continue;
            }
            let Some(bytes) = t.decoder.solve() else {
                continue;
            };
            match container::unpack(&bytes) {
                Some((name, file)) => {
                    t.done = true;
                    finished = Some(ReceivedFile {
                        call: header.call.clone(),
                        session: header.session,
                        name,
                        data: file,
                    });
                }
                None => {
                    // Frames of two different files mixed (a session number reused
                    // while the last file was incomplete): forget what is held and
                    // start again. The new file then needs another pass.
                    self.transfers.remove(&key);
                }
            }
        }
        finished
    }

    /// Progress of a transmission, if any frame of it has arrived.
    pub fn progress(&self, call: &str, session: u8) -> Option<Progress> {
        self.transfers
            .get(&(call.to_string(), session))
            .map(|t| Progress {
                have: t.decoder.have().min(t.decoder.count()),
                need: t.decoder.count(),
                data_held: t.decoder.data_held(),
            })
    }

    /// True once the file of a transmission has been delivered.
    pub fn is_done(&self, call: &str, session: u8) -> bool {
        self.transfers
            .get(&(call.to_string(), session))
            .is_some_and(|t| t.done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::{SendSpec, plan};
    use decdsss_core::{Mode, Profile};
    use decdsss_phy::framing::PayloadType;

    fn block(frames: Vec<Option<Vec<u8>>>) -> BlockResult {
        BlockResult {
            index: 0,
            first_frame: 0,
            frames,
            iterations: vec![],
            fingers: vec![],
            freq_hz: 0.0,
            snr_db: 0.0,
        }
    }

    fn spec(parity: f64, passes: usize) -> SendSpec {
        SendSpec {
            profile: Profile::W2400,
            mode: Mode::from_name("ds8-1/2").unwrap(),
            depth: 1,
            call: "VK2ABC".into(),
            session: 9,
            ptype: PayloadType::File,
            resync_s: 20.0,
            parity,
            passes,
        }
    }

    fn frames_of(p: &crate::send::Plan) -> Vec<Vec<u8>> {
        p.segments.iter().flat_map(|s| s.frames.clone()).collect()
    }

    #[test]
    fn a_file_arrives_with_a_third_of_the_frames_lost() {
        let data: Vec<u8> = (0..2000u32).map(|i| (i * 13) as u8).collect();
        let stream = container::pack("a.bin", &data);
        let p = plan(&spec(0.5, 1), &stream).unwrap();
        let all = frames_of(&p);
        let header = &p.segments[0].header;
        let mut asm = Assembler::new();
        let mut done = None;
        // Every third frame is lost, data and coded alike.
        for (i, f) in all.iter().enumerate() {
            let got = (i % 3 != 1).then(|| f.clone());
            done = done.or(asm.push(header, &block(vec![got])));
        }
        let file = done.expect("a half-again of parity covers a third lost");
        assert_eq!(
            (file.name.as_str(), file.data.as_slice()),
            ("a.bin", &data[..])
        );
        assert!(asm.is_done("VK2ABC", 9));
        // And it is delivered only once.
        assert!(
            asm.push(header, &block(vec![Some(all[0].clone())]))
                .is_none()
        );
    }

    #[test]
    fn two_passes_fill_what_one_could_not() {
        let data: Vec<u8> = (0..1500u32).map(|i| (i * 7 + 1) as u8).collect();
        let stream = container::pack("b.bin", &data);
        let p = plan(&spec(0.1, 2), &stream).unwrap();
        let all = frames_of(&p);
        let header = &p.segments[0].header;
        // Lose the first 30 % of the sequence, every frame.
        let lost = all.len() * 3 / 10;
        let mut asm = Assembler::new();
        let mut done = None;
        for (i, f) in all.iter().enumerate() {
            if i < lost {
                continue;
            }
            done = done.or(asm.push(header, &block(vec![Some(f.clone())])));
        }
        assert!(done.is_some(), "{:?}", asm.progress("VK2ABC", 9));
    }

    #[test]
    fn progress_counts_distinct_frames_and_a_reused_session_starts_over() {
        let a = container::pack("a", &vec![1u8; 600]);
        let b = container::pack("b", &vec![2u8; 600]);
        let pa = plan(&spec(0.3, 1), &a).unwrap();
        let pb = plan(&spec(0.3, 1), &b).unwrap();
        let (fa, fb) = (frames_of(&pa), frames_of(&pb));
        let header = &pa.segments[0].header;
        let mut asm = Assembler::new();
        asm.push(
            header,
            &block(vec![
                Some(fa[0].clone()),
                Some(fa[0].clone()),
                Some(fa[2].clone()),
            ]),
        );
        let pr = asm.progress("VK2ABC", 9).unwrap();
        assert_eq!((pr.have, pr.need), (2, pa.data_frames));
        assert_eq!(pr.data_held.iter().filter(|b| **b).count(), 2);
        // The same call and session with a different file of the same size: the
        // frames of the two mix, the container check fails and what was held is
        // dropped, so b needs a second pass.
        let mut done = None;
        for f in fb.iter().chain(fb.iter()) {
            done = done.or(asm.push(header, &block(vec![Some(f.clone())])));
        }
        let file = done.expect("b arrives");
        assert_eq!(file.name, "b");
        assert!(file.data.iter().all(|&x| x == 2));
    }
}
