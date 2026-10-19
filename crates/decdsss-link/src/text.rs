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
