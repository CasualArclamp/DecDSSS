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

use crate::Cat;
use crate::link::{Line, Link};

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

impl Cat for Icom {
    fn identify(&mut self) -> Result<String, String> {
        match self.ask(&READ_ID)?.as_slice() {
            [0x19, 0x00, id] => Ok(format!("answered as {id:02X}h")),
            [NG] => Err("refused to say what it is".into()),
            other => Err(format!("answered {other:02X?}, which is not an ID")),
        }
    }

    fn key(&mut self, on: bool) -> Result<(), String> {
        let mut body = PTT.to_vec();
        body.push(u8::from(on));
        match self.ask(&body)?.as_slice() {
            [OK] => Ok(()),
            [NG] => Err("refused".into()),
            other => Err(format!("answered {other:02X?}")),
        }
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
    use crate::testing::{FakeRig, Model};

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
    fn a_refusal_and_silence_are_errors() {
        let rig = FakeRig::new(Model::Ic7300).refuse(true);
        let mut icom = Icom::new(Box::new(rig), IC7300);
        assert_eq!(icom.key(true), Err("refused".into()));
        // Another address answers nothing.
        let mut icom = Icom::new(Box::new(FakeRig::new(Model::Ic7300)), 0x88);
        assert_eq!(icom.key(true), Err("no answer at CI-V address 88h".into()));
    }
}
