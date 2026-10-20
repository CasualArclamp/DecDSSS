// made by CasualArclamp
//! Copied from voicemodem's `rig` crate (same author), with `set_line` added.
//!
//! Yaesu's CAT, as the FT-710 speaks it on its Enhanced COM port: two
//! letters, perhaps a value, and a semicolon, in ASCII. A command that sets
//! something is not answered; one that asks is answered with its own two
//! letters and the value; one the rig will not take is answered "?;".

use std::time::{Duration, Instant};

use crate::link::{Line, Link};
use crate::{Cat, Tuning};

/// What an FT-710 answers to "ID;".
pub const FT710_ID: &str = "0800";

/// How long the rig has to answer a question, and how often it is asked.
const ANSWER: Duration = Duration::from_millis(300);
const TRIES: usize = 2;

/// How long the rig is given to go to transmit or come back, asked this
/// often, since the command that keys it is not answered.
const SETTLE: Duration = Duration::from_millis(400);
const RECHECK: Duration = Duration::from_millis(20);

/// What the mode digit of an "MD0;" answer names (the FT-710's CAT manual lists
/// more, which are shown by their digit).
pub fn mode_name(code: char) -> String {
    match code {
        '1' => "LSB".into(),
        '2' => "USB".into(),
        '3' => "CW".into(),
        '4' => "FM".into(),
        '5' => "AM".into(),
        '6' => "RTTY".into(),
        '7' => "CW-R".into(),
        '8' => "LSB-D".into(),
        '9' => "RTTY-R".into(),
        'A' => "FM-D".into(),
        'B' => "FM-N".into(),
        'C' => "USB-D".into(),
        'D' => "AM-N".into(),
        other => format!("mode {other}"),
    }
}

/// The mode digit of the panel name `name`, the other way from [`mode_name`].
pub fn mode_char(name: &str) -> Option<char> {
    Some(match name {
        "LSB" => '1',
        "USB" => '2',
        "CW" => '3',
        "FM" => '4',
        "AM" => '5',
        "RTTY" => '6',
        "CW-R" => '7',
        "LSB-D" => '8',
        "RTTY-R" => '9',
        "FM-D" => 'A',
        "FM-N" => 'B',
        "USB-D" => 'C',
        "AM-N" => 'D',
        _ => return None,
    })
}

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

    /// "FA;" is the main VFO's frequency, in hertz, as digits; "MD0;" is the
    /// main receiver's mode: a 0 for the receiver and a digit for the mode.
    fn tuning(&mut self) -> Result<Tuning, String> {
        let digits = self.ask("FA;")?;
        let hz = digits
            .parse::<u64>()
            .map_err(|_| format!("answered FA{digits}, which is not a frequency"))?;
        let mode = self.ask("MD0;")?;
        let mode = mode
            .chars()
            .nth(1)
            .map(mode_name)
            .ok_or_else(|| format!("answered MD{mode}, which is not a mode"))?;
        Ok(Tuning { hz, mode })
    }

    /// "FA" followed by nine digits sets the main VFO. A command that sets is
    /// not answered, so the rig is asked where it has got to.
    fn set_frequency(&mut self, hz: u64) -> Result<(), String> {
        if hz > 999_999_999 {
            return Err(format!("{hz} Hz is more than an FT-710 can be set to"));
        }
        self.tell(&format!("FA{hz:09};"))?;
        let now = self.ask("FA;")?;
        if now.parse() == Ok(hz) {
            Ok(())
        } else {
            Err(format!("says FA{now} after FA{hz:09}"))
        }
    }

    /// "MD0" and the mode digit sets the main receiver's mode; asked after,
    /// as the frequency is.
    fn set_mode(&mut self, mode: &str) -> Result<(), String> {
        let code = mode_char(mode).ok_or_else(|| format!("the rig has no {mode} mode"))?;
        self.tell(&format!("MD0{code};"))?;
        let now = self.ask("MD0;")?;
        if now.chars().nth(1) == Some(code) {
            Ok(())
        } else {
            Err(format!("says MD{now} after MD0{code}"))
        }
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
    use crate::testing::{FakeRig, Mode, Model};

    #[test]
    fn it_reads_the_dial_and_the_mode() {
        let rig = FakeRig::new(Model::Ft710);
        let mut yaesu = Yaesu::new(Box::new(rig.clone()));
        assert_eq!(
            yaesu.tuning(),
            Ok(Tuning {
                hz: 14_097_000,
                mode: "USB-D".into()
            })
        );
        rig.tune(3_573_000, Mode::Lsb);
        let t = yaesu.tuning().unwrap();
        assert_eq!((t.hz, t.mode.as_str()), (3_573_000, "LSB"));
        // Only questions: nothing set, nothing keyed.
        assert_eq!(rig.heard(), "FA;MD0;FA;MD0;");
        assert!(rig.keyings().is_empty());
        assert_eq!(mode_name('E'), "mode E");
    }

    #[test]
    fn mode_digits_are_the_panel_names_the_other_way() {
        for code in "123456789ABCD".chars() {
            assert_eq!(mode_char(&mode_name(code)), Some(code));
        }
        // From the FT-710 CAT manual's MD list.
        assert_eq!(mode_char("USB-D"), Some('C'));
        assert_eq!(mode_char("CW-R"), Some('7'));
        assert_eq!(mode_char("C4FM"), None);
    }

    #[test]
    fn it_retunes_and_changes_mode_and_checks_it_took() {
        let rig = FakeRig::new(Model::Ft710);
        let mut yaesu = Yaesu::new(Box::new(rig.clone()));
        yaesu.set_frequency(3_573_000).unwrap();
        assert_eq!(rig.hz(), 3_573_000);
        assert!(rig.heard().contains("FA003573000;"));
        for name in ["LSB", "LSB-D", "USB", "USB-D", "FM-N", "CW-R", "AM-N"] {
            yaesu.set_mode(name).unwrap();
            assert_eq!(rig.mode().name(), name);
        }
        assert!(rig.keyings().is_empty());
        assert_eq!(
            yaesu.set_mode("AM-D"),
            Err("the rig has no AM-D mode".into())
        );
        assert_eq!(
            yaesu.set_frequency(1_000_000_000),
            Err("1000000000 Hz is more than an FT-710 can be set to".into())
        );
    }

    #[test]
    fn a_rig_that_does_not_take_a_retune_says_so() {
        let rig = FakeRig::new(Model::Ft710).refuse(true);
        let mut yaesu = Yaesu::new(Box::new(rig));
        assert_eq!(
            yaesu.set_frequency(7_074_000),
            Err("says FA014097000 after FA007074000".into())
        );
        assert_eq!(yaesu.set_mode("LSB"), Err("says MD0C after MD01".into()));
    }

    #[test]
    fn a_rig_that_will_not_say_where_it_is_tuned_says_so() {
        let rig = FakeRig::new(Model::Ft710).unreadable(true);
        let mut yaesu = Yaesu::new(Box::new(rig));
        assert_eq!(yaesu.tuning(), Err("would not take FA;".into()));
    }

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
