// made by CasualArclamp
//! Text: a CQ call or a few lines of chat as a payload of its own.
//!
//! A message rides the file machinery (the container, with no file name, and
//! erasure-coded frames), so it has the same protection and the same repeat passes,
//! and the receiver shows it as text instead of saving it. It is always sent at depth
//! 1: a message is a frame or two, and a deeper block would pad it out with empty
//! frames and take that many times the air time.

use crate::assemble::ReceivedFile;
use crate::container;
use crate::send::{Plan, SendSpec, plan};
use decdsss_phy::framing::PayloadType;

/// Longest message sent, bytes of UTF-8 (about 15 frames at the robust end).
pub const MAX_TEXT_BYTES: usize = 1000;

/// A CQ call from `call`: three calls, the callsign three times, and "please
/// answer".
pub fn cq(call: &str) -> String {
    format!("CQ CQ CQ de {call} {call} {call} pse K")
}

/// A message as it will be sent or shown: line ends as `\n`, control characters other
/// than newline and tab dropped, blank space trimmed from both ends, cut to
/// [`MAX_TEXT_BYTES`] at a character boundary.
pub fn tidy(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let kept: String = unified
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let mut out = kept.trim().to_string();
    if out.len() > MAX_TEXT_BYTES {
        let mut cut = MAX_TEXT_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// The transmission of a message: `spec` with the text type and depth 1.
pub fn plan_text(spec: &SendSpec, text: &str) -> Result<Plan, String> {
    let text = tidy(text);
    if text.is_empty() {
        return Err("nothing to send: type a message".into());
    }
    let spec = SendSpec {
        ptype: PayloadType::Text,
        depth: 1,
        ..spec.clone()
    };
    plan(&spec, &container::pack("", text.as_bytes()))
}

/// The message in a received file, if it is one.
pub fn read(f: &ReceivedFile) -> Option<String> {
    (f.ptype == PayloadType::Text).then(|| tidy(&String::from_utf8_lossy(&f.data)))
}

/// The message so far, from the front of its bytes that has arrived without a gap
/// ([`crate::assemble::Assembler::front`]). A frame can end inside a multi-byte
/// character, so the text stops at the last whole one; the rest shows when the next
/// frame brings it.
pub fn read_front(data: &[u8]) -> String {
    // `valid_up_to` is the length of the longest valid UTF-8 prefix.
    let whole = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&data[..e.valid_up_to()]).unwrap_or_default(),
    };
    tidy(whole)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_core::{Mode, Profile};

    fn spec() -> SendSpec {
        SendSpec {
            profile: Profile::W2400,
            mode: Mode::from_name("ds8-1/2").unwrap(),
            depth: 4,
            call: "VK2ABC".into(),
            session: 5,
            ptype: PayloadType::File,
            resync_s: 60.0,
            parity: 0.25,
            passes: 1,
        }
    }

    #[test]
    fn a_cq_names_the_station_three_times() {
        assert_eq!(cq("VK2ABC"), "CQ CQ CQ de VK2ABC VK2ABC VK2ABC pse K");
    }

    #[test]
    fn tidy_keeps_text_and_drops_the_rest() {
        assert_eq!(tidy("  hello\r\nworld\u{7}\ttab  "), "hello\nworld\ttab");
        let long = "é".repeat(MAX_TEXT_BYTES);
        let t = tidy(&long);
        assert!(t.len() <= MAX_TEXT_BYTES && t.len() >= MAX_TEXT_BYTES - 1);
        assert!(t.chars().all(|c| c == 'é'), "cut on a character boundary");
    }

    #[test]
    fn a_front_cut_inside_a_character_stops_before_it() {
        let bytes = "café".as_bytes(); // the é is two bytes
        assert_eq!(read_front(&bytes[..4]), "caf");
        assert_eq!(read_front(bytes), "café");
        assert_eq!(read_front(&[]), "");
    }

    #[test]
    fn a_message_reads_out_frame_by_frame_as_it_arrives() {
        use crate::assemble::Assembler;
        use decdsss_phy::rx::payload::BlockResult;

        let msg: String = (0..40).map(|i| format!("word{i} ")).collect();
        let msg = tidy(&msg);
        let p = plan_text(&spec(), &msg).unwrap();
        assert!(p.data_frames > 2, "long enough to take several frames");
        let header = &p.segments[0].header;
        let mut asm = Assembler::new();
        let mut shown = Vec::new();
        for f in p.segments.iter().flat_map(|s| s.frames.iter()) {
            let block = BlockResult {
                index: 0,
                first_frame: 0,
                frames: vec![Some(f.clone())],
                iterations: vec![],
                fingers: vec![],
                freq_hz: 0.0,
                snr_db: 0.0,
            };
            let done = asm.push(header, &block);
            if done.is_some() {
                break;
            }
            if let Some(front) = asm.front("VK2ABC", 5) {
                shown.push(read_front(&front.data));
            }
        }
        assert!(shown.len() >= 2, "{shown:?}");
        // Each look is the start of the message and none is shorter than the last.
        for s in &shown {
            assert!(msg.starts_with(s.as_str()), "{s:?}");
        }
        assert!(shown.windows(2).all(|w| w[0].len() <= w[1].len()));
        assert!(shown.last().unwrap().len() > shown[0].len());
        assert!(shown.last().unwrap().len() < msg.len());
    }

    #[test]
    fn a_message_is_sent_at_depth_one_as_text() {
        let p = plan_text(&spec(), "CQ CQ de VK2ABC K").unwrap();
        for s in &p.segments {
            assert_eq!(s.header.ptype, PayloadType::Text);
            assert_eq!(s.header.depth, 1);
        }
        // A short message is a single data frame, plus the parity a quarter asks for.
        assert_eq!(p.data_frames, 1);
        assert!(plan_text(&spec(), "   \n ").is_err());
    }
}
