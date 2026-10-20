// made by CasualArclamp
//! Copied from voicemodem's `rig` crate (same author), with `set_line` added.
//!
//! Icom's CI-V, as the IC-7300 speaks it on its USB serial port.
//!
//! A frame is FE FE, the address it is for, the address it is from, a
//! command, perhaps a sub-command and data, and FD. The IC-7300 is at 94h
//! unless its menu says otherwise, and a computer is by custom E0h. The rig
//! answers a command that sets something with FB, done, or FA, refused; one
//! that asks, with the command and the answer.
//!
//! Two kinds of frame arrive that are not answers, and are passed over: the
//! computer's own, echoed back when the rig's CI-V USB Echo Back is on, and
//! the rig's reports to everyone (address 00) of a turned dial, when its CI-V
//! Transceive is on.

use std::time::{Duration, Instant};

use crate::link::{Line, Link};
use crate::{Cat, Tuning};

/// A computer's address, by custom.
pub const CONTROLLER: u8 = 0xE0;
/// The IC-7300's address, as it comes.
pub const IC7300: u8 = 0x94;

const PREAMBLE: u8 = 0xFE;
const END: u8 = 0xFD;
const OK: u8 = 0xFB;
const NG: u8 = 0xFA;

/// Read the transceiver's ID; switch the transmitter.
const READ_ID: [u8; 2] = [0x19, 0x00];
const PTT: [u8; 2] = [0x1C, 0x00];
/// Read the operating frequency, the operating mode, and whether DATA mode is
/// on (and which filter).
const READ_FREQ: [u8; 1] = [0x03];
const READ_MODE: [u8; 1] = [0x04];
const READ_DATA: [u8; 2] = [0x1A, 0x06];
/// Set the operating frequency (5 bytes of BCD) and the operating mode (a mode
/// and a filter). DATA mode is set with the same `1A 06` that reads it, with
/// 00 or 01 and a filter after it.
const SET_FREQ: u8 = 0x05;
const SET_MODE: u8 = 0x06;
/// The filter used when the rig does not say which is selected: FIL1.
const FILTER_1: u8 = 0x01;

/// How long the rig has to answer a command, and how often it is sent.
const ANSWER: Duration = Duration::from_millis(250);
const TRIES: usize = 2;

/// A frame from `from` to `to` carrying `body`: command, sub-command, data.
pub fn frame(to: u8, from: u8, body: &[u8]) -> Vec<u8> {
    let mut f = vec![PREAMBLE, PREAMBLE, to, from];
    f.extend_from_slice(body);
    f.push(END);
    f
}

/// A frame as it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub to: u8,
    pub from: u8,
    pub body: Vec<u8>,
}

/// Take the first whole frame off the front of `buffer`, with whatever came
/// before it. None, and the start of any frame left for more to arrive, if
/// there is no whole frame yet.
pub fn next_frame(buffer: &mut Vec<u8>) -> Option<Frame> {
    loop {
        let Some(start) = buffer.windows(2).position(|w| w == [PREAMBLE, PREAMBLE]) else {
            // Nothing starts a frame; a last FE may be the first of the next
            // preamble.
            let keep = usize::from(buffer.last() == Some(&PREAMBLE));
            buffer.drain(..buffer.len() - keep);
            return None;
        };
        buffer.drain(..start);
        let mut at = 2;
        while buffer.get(at) == Some(&PREAMBLE) {
            at += 1;
        }
        let end = at + buffer[at..].iter().position(|&b| b == END)?;
        if let Some(broken) = buffer[at..end].iter().position(|&b| b == PREAMBLE) {
            // A frame that broke off where another began: start again there.
            buffer.drain(..at + broken);
            continue;
        }
        let body = &buffer[at..end];
        let frame = (body.len() >= 2).then(|| Frame {
            to: body[0],
            from: body[1],
            body: body[2..].to_vec(),
        });
        buffer.drain(..=end);
        if frame.is_some() {
            return frame;
        }
    }
}

/// A frequency from CI-V's five bytes of BCD, the least significant pair
/// first: 14.097.000 Hz is `00 70 09 14 00`. None if a nibble is not a digit.
pub fn bcd_hz(bytes: &[u8]) -> Option<u64> {
    bytes.iter().rev().try_fold(0u64, |hz, &b| {
        let (tens, ones) = (b >> 4, b & 0x0F);
        (tens < 10 && ones < 10).then(|| hz * 100 + u64::from(tens * 10 + ones))
    })
}

/// A frequency as CI-V's five bytes of BCD (the stand-in rig answers with it).
pub fn hz_bcd(hz: u64) -> [u8; 5] {
    let mut bytes = [0; 5];
    let mut rest = hz;
    for b in &mut bytes {
        let pair = (rest % 100) as u8;
        *b = ((pair / 10) << 4) | (pair % 10);
        rest /= 100;
    }
    bytes
}

/// What an Icom's mode byte names. The IC-7300 has these eight (06, WFM, is
/// on its relatives).
pub fn mode_name(code: u8) -> String {
    match code {
        0x00 => "LSB".into(),
        0x01 => "USB".into(),
        0x02 => "AM".into(),
        0x03 => "CW".into(),
        0x04 => "RTTY".into(),
        0x05 => "FM".into(),
        0x06 => "WFM".into(),
        0x07 => "CW-R".into(),
        0x08 => "RTTY-R".into(),
        other => format!("mode {other:02X}"),
    }
}

/// The mode byte of the panel name `base` (no `-D`), the other way from
/// [`mode_name`]. None for a mode the IC-7300 has no byte for.
pub fn mode_code(base: &str) -> Option<u8> {
    Some(match base {
        "LSB" => 0x00,
        "USB" => 0x01,
        "AM" => 0x02,
        "CW" => 0x03,
        "RTTY" => 0x04,
        "FM" => 0x05,
        "CW-R" => 0x07,
        "RTTY-R" => 0x08,
        _ => return None,
    })
}

/// An IC-7300, or any Icom that speaks CI-V, at `rig`.
pub struct Icom {
    link: Box<dyn Link>,
    rig: u8,
    buffer: Vec<u8>,
}

impl std::fmt::Debug for Icom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Icom")
            .field("rig", &format_args!("{:02X}h", self.rig))
            .finish_non_exhaustive()
    }
}

impl Icom {
    pub fn new(link: Box<dyn Link>, rig: u8) -> Self {
        Self {
            link,
            rig,
            buffer: Vec::new(),
        }
    }

    /// Send `body` to the rig; the body of its answer, the first frame from it
    /// to the computer.
    fn ask(&mut self, body: &[u8]) -> Result<Vec<u8>, String> {
        let request = frame(self.rig, CONTROLLER, body);
        for _ in 0..TRIES {
            self.link.discard();
            self.buffer.clear();
            self.link.send(&request)?;
            let deadline = Instant::now() + ANSWER;
            while Instant::now() < deadline {
                self.link.receive(&mut self.buffer)?;
                while let Some(f) = next_frame(&mut self.buffer) {
                    if f.to == CONTROLLER && f.from == self.rig {
                        return Ok(f.body);
                    }
                }
            }
        }
        Err(format!("no answer at CI-V address {:02X}h", self.rig))
    }
}

impl Icom {
    /// Send a command that sets something: the rig answers done or refused.
    fn command(&mut self, body: &[u8]) -> Result<(), String> {
        match self.ask(body)?.as_slice() {
            [OK] => Ok(()),
            [NG] => Err("refused".into()),
            other => Err(format!("answered {other:02X?}")),
        }
    }
}

impl Cat for Icom {
    fn identify(&mut self) -> Result<String, String> {
        match self.ask(&READ_ID)?.as_slice() {
            [0x19, 0x00, id] => Ok(format!("answered as {id:02X}h")),
            [NG] => Err("refused to say what it is".into()),
            other => Err(format!("answered {other:02X?}, which is not an ID")),
        }
    }

    fn tuning(&mut self) -> Result<Tuning, String> {
        let hz = match self.ask(&READ_FREQ)?.as_slice() {
            [0x03, digits @ ..] => bcd_hz(digits)
                .ok_or_else(|| format!("gave a frequency that is not BCD: {digits:02X?}"))?,
            [NG] => return Err("refused to say its frequency".into()),
            other => return Err(format!("answered {other:02X?}, which is not a frequency")),
        };
        let mut mode = match self.ask(&READ_MODE)?.as_slice() {
            [0x04, code, ..] => mode_name(*code),
            [NG] => return Err("refused to say its mode".into()),
            other => return Err(format!("answered {other:02X?}, which is not a mode")),
        };
        // DATA mode is a setting of its own on top of the mode. A rig that will
        // not say (an older one) leaves the mode as it was, without the -D.
        if let Ok(body) = self.ask(&READ_DATA)
            && matches!(body.as_slice(), [0x1A, 0x06, 0x01, ..])
        {
            mode.push_str("-D");
        }
        Ok(Tuning { hz, mode })
    }

    fn set_frequency(&mut self, hz: u64) -> Result<(), String> {
        // Five bytes of BCD hold ten digits.
        if hz > 9_999_999_999 {
            return Err(format!("{hz} Hz is more than CI-V can say"));
        }
        let mut body = vec![SET_FREQ];
        body.extend(hz_bcd(hz));
        self.command(&body)
    }

    /// A mode is a mode byte and, on the IC-7300, DATA mode on top of it. Both
    /// carry a filter, which is left as the rig has it: asked first, and said
    /// back, since a command that sets must name one.
    fn set_mode(&mut self, mode: &str) -> Result<(), String> {
        let (base, data) = match mode.strip_suffix("-D") {
            Some(base) => (base, true),
            None => (mode, false),
        };
        let code = mode_code(base).ok_or_else(|| format!("the rig has no {mode} mode"))?;
        let filter = match self.ask(&READ_MODE)?.as_slice() {
            [0x04, _, filter, ..] => *filter,
            _ => FILTER_1,
        };
        self.command(&[SET_MODE, code, filter])?;
        // A rig with no DATA mode (an older one) cannot be asked to leave it.
        let data_filter = match self.ask(&READ_DATA) {
            Ok(body) => match body.as_slice() {
                [0x1A, 0x06, _, filter, ..] => *filter,
                _ => FILTER_1,
            },
            Err(_) if !data => return Ok(()),
            Err(e) => return Err(e),
        };
        match self.command(&[0x1A, 0x06, u8::from(data), data_filter]) {
            Err(e) if !data && e == "refused" => Ok(()),
            other => other,
        }
    }

    fn key(&mut self, on: bool) -> Result<(), String> {
        let mut body = PTT.to_vec();
        body.push(u8::from(on));
        self.command(&body)
    }

    fn set_baud(&mut self, baud: u32) -> Result<(), String> {
        self.link.set_baud(baud)
    }

    fn set_line(&mut self, line: Line, high: bool) -> Result<(), String> {
        self.link.set_line(line, high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeRig, Mode, Model};

    #[test]
    fn frames_are_as_the_manual_gives_them() {
        assert_eq!(
            frame(IC7300, CONTROLLER, &[0x1C, 0x00, 0x01]),
            [0xFE, 0xFE, 0x94, 0xE0, 0x1C, 0x00, 0x01, 0xFD]
        );
        assert_eq!(
            frame(IC7300, CONTROLLER, &READ_ID),
            [0xFE, 0xFE, 0x94, 0xE0, 0x19, 0x00, 0xFD]
        );
    }

    #[test]
    fn frames_come_out_of_whatever_surrounds_them() {
        let mut buffer = vec![0x00, 0x13, 0xFE, 0xFE, 0xFE, 0xE0, 0x94, 0xFB, 0xFD, 0xFE];
        assert_eq!(
            next_frame(&mut buffer),
            Some(Frame {
                to: 0xE0,
                from: 0x94,
                body: vec![0xFB]
            })
        );
        // The last FE is kept for the preamble it may begin.
        assert_eq!(buffer, [0xFE]);
        assert_eq!(next_frame(&mut buffer), None);
        // A frame in two pieces waits for its end.
        buffer.extend([0xFE, 0x00, 0x94, 0x00]);
        assert_eq!(next_frame(&mut buffer), None);
        buffer.extend([0x40, 0x07, 0x14, 0x00, 0xFD]);
        assert_eq!(
            next_frame(&mut buffer),
            Some(Frame {
                to: 0x00,
                from: 0x94,
                body: vec![0x00, 0x40, 0x07, 0x14, 0x00]
            })
        );
        // One that broke off is dropped for the one that began inside it.
        let mut buffer = vec![
            0xFE, 0xFE, 0xE0, 0x94, 0x1C, 0xFE, 0xFE, 0xE0, 0x94, 0xFA, 0xFD,
        ];
        assert_eq!(
            next_frame(&mut buffer),
            Some(Frame {
                to: 0xE0,
                from: 0x94,
                body: vec![0xFA]
            })
        );
    }

    #[test]
    fn it_keys_and_lets_go_past_echoes_and_reports() {
        for echo in [false, true] {
            let rig = FakeRig::new(Model::Ic7300).echo(echo).chatter(true);
            let mut icom = Icom::new(Box::new(rig.clone()), IC7300);
            assert_eq!(icom.identify(), Ok("answered as 94h".into()));
            icom.key(true).unwrap();
            assert!(rig.transmitting(), "echo {echo}");
            icom.key(false).unwrap();
            assert!(!rig.transmitting(), "echo {echo}");
        }
    }

    #[test]
    fn a_frequency_is_bcd_with_the_least_significant_pair_first() {
        assert_eq!(bcd_hz(&[0x00, 0x70, 0x09, 0x14, 0x00]), Some(14_097_000));
        assert_eq!(hz_bcd(14_097_000), [0x00, 0x70, 0x09, 0x14, 0x00]);
        for hz in [0, 1_840_000, 7_074_500, 145_500_000, 470_000_000] {
            assert_eq!(bcd_hz(&hz_bcd(hz)), Some(hz));
        }
        assert_eq!(bcd_hz(&[0x0A, 0x00, 0x00, 0x00, 0x00]), None);
    }

    #[test]
    fn it_reads_the_dial_and_the_mode_past_echoes_and_reports() {
        for echo in [false, true] {
            let rig = FakeRig::new(Model::Ic7300).echo(echo).chatter(true);
            let mut icom = Icom::new(Box::new(rig.clone()), IC7300);
            let said = |t: Tuning| (t.hz, t.mode);
            assert_eq!(
                icom.tuning().map(said),
                Ok((14_097_000, "USB-D".to_string()))
            );
            rig.tune(7_074_500, Mode::Lsb);
            assert_eq!(icom.tuning().map(said), Ok((7_074_500, "LSB".to_string())));
            rig.tune(145_500_000, Mode::Fm);
            assert_eq!(icom.tuning().map(said), Ok((145_500_000, "FM".to_string())));
            // Reading changes nothing and keys nothing.
            assert!(!rig.transmitting() && rig.keyings().is_empty());
        }
    }

    #[test]
    fn mode_bytes_are_the_panel_names_the_other_way() {
        for (code, name) in [(0x00, "LSB"), (0x01, "USB"), (0x05, "FM"), (0x07, "CW-R")] {
            assert_eq!(mode_code(name), Some(code));
            assert_eq!(mode_name(code), name);
        }
        assert_eq!(mode_code("AM-N"), None);
    }

    #[test]
    fn it_retunes_and_changes_mode_past_echoes_and_reports() {
        for echo in [false, true] {
            let rig = FakeRig::new(Model::Ic7300).echo(echo).chatter(true);
            let mut icom = Icom::new(Box::new(rig.clone()), IC7300);
            icom.set_frequency(7_074_000).unwrap();
            assert_eq!(rig.hz(), 7_074_000);
            for name in ["LSB", "LSB-D", "USB", "USB-D", "FM", "CW-R", "AM"] {
                icom.set_mode(name).unwrap();
                assert_eq!(rig.mode().name(), name, "echo {echo}");
                // What the rig says is what was asked.
                assert_eq!(icom.tuning().unwrap().mode, name);
            }
            // Retuning is not keying.
            assert!(!rig.transmitting() && rig.keyings().is_empty());
        }
    }

    #[test]
    fn a_mode_the_rig_has_not_got_is_not_sent() {
        let rig = FakeRig::new(Model::Ic7300);
        let mut icom = Icom::new(Box::new(rig.clone()), IC7300);
        assert_eq!(
            icom.set_mode("AM-N"),
            Err("the rig has no AM-N mode".into())
        );
        assert_eq!(rig.mode().name(), "USB-D");
        // An Icom with no DATA mode can still be put in a plain mode, not in a data one.
        let rig = FakeRig::new(Model::Ic7300).without_data_mode(true);
        let mut icom = Icom::new(Box::new(rig.clone()), IC7300);
        icom.set_mode("LSB").unwrap();
        assert_eq!(rig.mode().name(), "LSB");
        assert!(icom.set_mode("LSB-D").is_err());
        assert_eq!(
            icom.set_frequency(10_000_000_000),
            Err("10000000000 Hz is more than CI-V can say".into())
        );
    }

    #[test]
    fn a_rig_that_refuses_to_retune_says_so() {
        let rig = FakeRig::new(Model::Ic7300).refuse(true);
        let mut icom = Icom::new(Box::new(rig.clone()), IC7300);
        assert_eq!(icom.set_frequency(7_074_000), Err("refused".into()));
        assert_eq!(rig.hz(), 14_097_000);
    }

    #[test]
    fn a_rig_with_no_data_mode_to_read_is_still_read() {
        let rig = FakeRig::new(Model::Ic7300).without_data_mode(true);
        let mut icom = Icom::new(Box::new(rig), IC7300);
        assert_eq!(
            icom.tuning(),
            Ok(Tuning {
                hz: 14_097_000,
                mode: "USB".into()
            })
        );
    }

    #[test]
    fn a_rig_that_will_not_say_where_it_is_tuned_says_so() {
        let rig = FakeRig::new(Model::Ic7300).unreadable(true);
        let mut icom = Icom::new(Box::new(rig), IC7300);
        assert_eq!(icom.tuning(), Err("refused to say its frequency".into()));
        let mut icom = Icom::new(Box::new(FakeRig::new(Model::Ic7300)), 0x88);
        assert_eq!(icom.tuning(), Err("no answer at CI-V address 88h".into()));
    }

    #[test]
    fn a_refusal_and_silence_are_errors() {
        let rig = FakeRig::new(Model::Ic7300).refuse(true);
        let mut icom = Icom::new(Box::new(rig), IC7300);
        assert_eq!(icom.key(true), Err("refused".into()));
        // Another address answers nothing.
        let mut icom = Icom::new(Box::new(FakeRig::new(Model::Ic7300)), 0x88);
        assert_eq!(icom.key(true), Err("no answer at CI-V address 88h".into()));
    }
}
