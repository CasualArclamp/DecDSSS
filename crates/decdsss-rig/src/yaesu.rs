// made by CasualArclamp
//! Copied from voicemodem's `rig` crate (same author), with `set_line` added.
//!
//! Yaesu's CAT, as the FT-710 speaks it on its Enhanced COM port: two
//! letters, perhaps a value, and a semicolon, in ASCII. A command that sets
//! something is not answered; one that asks is answered with its own two
//! letters and the value; one the rig will not take is answered "?;".

use std::time::{Duration, Instant};

use crate::Cat;
use crate::link::{Line, Link};

/// What an FT-710 answers to "ID;".
pub const FT710_ID: &str = "0800";

/// How long the rig has to answer a question, and how often it is asked.
const ANSWER: Duration = Duration::from_millis(300);
const TRIES: usize = 2;

/// How long the rig is given to go to transmit or come back, asked this
/// often, since the command that keys it is not answered.
const SETTLE: Duration = Duration::from_millis(400);
const RECHECK: Duration = Duration::from_millis(20);

/// An FT-710, or any Yaesu that speaks the same.
pub struct Yaesu {
    link: Box<dyn Link>,
    buffer: Vec<u8>,
}

impl std::fmt::Debug for Yaesu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Yaesu").finish_non_exhaustive()
    }
}

impl Yaesu {
    pub fn new(link: Box<dyn Link>) -> Self {
        Self {
            link,
            buffer: Vec::new(),
        }
    }

    fn tell(&mut self, command: &str) -> Result<(), String> {
        self.link.send(command.as_bytes())
    }

    /// Ask with `question`, "ID;" say; the value of the answer that begins
    /// with its two letters. Anything else the rig says meanwhile -- a report
    /// of a turned dial, with its Auto Information on -- is passed over.
    fn ask(&mut self, question: &str) -> Result<String, String> {
        let letters = &question.as_bytes()[..2];
        for _ in 0..TRIES {
            self.link.discard();
            self.buffer.clear();
            self.link.send(question.as_bytes())?;
            let deadline = Instant::now() + ANSWER;
            while Instant::now() < deadline {
                self.link.receive(&mut self.buffer)?;
                while let Some(end) = self.buffer.iter().position(|&b| b == b';') {
                    let answer: Vec<u8> = self.buffer.drain(..=end).collect();
                    let answer = String::from_utf8_lossy(&answer);
                    let answer = answer.trim();
                    if answer == "?;" {
                        return Err(format!("would not take {question}"));
                    }
                    if answer.as_bytes().starts_with(letters) {
                        return Ok(answer[2..answer.len() - 1].to_string());
                    }
                }
            }
        }
        Err(format!("no answer to {question}"))
    }
}

impl Cat for Yaesu {
    fn identify(&mut self) -> Result<String, String> {
        let id = self.ask("ID;")?;
        Ok(if id == FT710_ID {
            format!("answered as ID {id}")
        } else {
            format!("answered as ID {id}, where an FT-710 answers {FT710_ID}")
        })
    }

    /// TX1 keys the rig by CAT and TX0 lets it go. Neither is answered, so
    /// the rig is asked with TX where it has got to: 0 receiving, 1 keyed by
    /// CAT, 2 keyed by its own PTT.
    fn key(&mut self, on: bool) -> Result<(), String> {
        self.tell(if on { "TX1;" } else { "TX0;" })?;
        let deadline = Instant::now() + SETTLE;
        loop {
            let state = self.ask("TX;")?;
            if (state != "0") == on {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("says TX{state} after TX{}", u8::from(on)));
            }
            std::thread::sleep(RECHECK);
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
    fn it_keys_and_lets_go_past_auto_information() {
        let rig = FakeRig::new(Model::Ft710).chatter(true);
        let mut yaesu = Yaesu::new(Box::new(rig.clone()));
        assert_eq!(yaesu.identify(), Ok("answered as ID 0800".into()));
        yaesu.key(true).unwrap();
        assert!(rig.transmitting());
        yaesu.key(false).unwrap();
        assert!(!rig.transmitting());
        assert_eq!(rig.heard(), "ID;TX1;TX;TX0;TX;");
    }

    #[test]
    fn a_rig_that_will_not_transmit_says_so() {
        let rig = FakeRig::new(Model::Ft710).refuse(true);
        let mut yaesu = Yaesu::new(Box::new(rig));
        assert_eq!(yaesu.key(true), Err("says TX0 after TX1".into()));
        let mut yaesu = Yaesu::new(Box::new(FakeRig::new(Model::Ft710).silent(true)));
        assert_eq!(yaesu.identify(), Err("no answer to ID;".into()));
    }

    #[test]
    fn another_yaesu_is_named_for_what_it_is() {
        let rig = FakeRig::new(Model::Ft710).id("0761");
        let mut yaesu = Yaesu::new(Box::new(rig));
        assert_eq!(
            yaesu.identify(),
            Ok("answered as ID 0761, where an FT-710 answers 0800".into())
        );
    }
}
