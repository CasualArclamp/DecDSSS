// made by CasualArclamp
//! Copied from voicemodem's `rig` crate (same author), with a rigctld and the
//! control lines added.
//!
//! A stand-in rig on a link, answering as an IC-7300 or an FT-710 would, for
//! the tests here and in the program that keys through this crate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

pub use crate::Model;
use crate::civ::{CONTROLLER, frame, hz_bcd, next_frame};
use crate::link::{Line, Link};

/// A mode a stand-in rig can be in; each model answers with its own code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lsb,
    Usb,
    /// Upper sideband with DATA mode on.
    UsbData,
    Fm,
}

/// A rig that answers what it is sent. Clones share one rig, so a test can
/// keep one to look at while a keyer owns another.
#[derive(Debug, Clone)]
pub struct FakeRig {
    state: Arc<Mutex<State>>,
}

#[derive(Debug)]
struct State {
    model: Model,
    /// The CI-V address it answers at, and the ID a Yaesu gives.
    address: u8,
    id: String,
    /// The rate the link is set to, and the only one it answers at, if any.
    baud: u32,
    answers_at: Option<u32>,
    /// CI-V USB Echo Back; reports nobody asked for, before every answer;
    /// refusing to transmit; answering nothing at all.
    echo: bool,
    chatter: bool,
    refuse: bool,
    silent: bool,
    transmitting: bool,
    /// Where the dial is, hertz, and the mode.
    hz: u64,
    mode: Mode,
    /// Will not say where the dial is or what mode it is in; an Icom that has
    /// no DATA mode to ask about.
    unreadable: bool,
    no_data_mode: bool,
    /// Every change of a control line, in order.
    lines: Vec<(Line, bool)>,
    /// Every change of transmitting, in order, and every byte it was sent.
    keyings: Vec<bool>,
    heard: Vec<u8>,
    /// What it has said and nobody has read.
    said: VecDeque<u8>,
}

impl FakeRig {
    pub fn new(model: Model) -> Self {
        let state = State {
            model,
            address: crate::civ::IC7300,
            id: crate::yaesu::FT710_ID.to_string(),
            baud: model.default_baud(),
            answers_at: None,
            echo: false,
            chatter: false,
            refuse: false,
            silent: false,
            transmitting: false,
            hz: 14_097_000,
            mode: Mode::UsbData,
            unreadable: false,
            no_data_mode: false,
            lines: Vec::new(),
            keyings: Vec::new(),
            heard: Vec::new(),
            said: VecDeque::new(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn echo(self, on: bool) -> Self {
        self.state().echo = on;
        self
    }

    pub fn chatter(self, on: bool) -> Self {
        self.state().chatter = on;
        self
    }

    pub fn refuse(self, on: bool) -> Self {
        self.state().refuse = on;
        self
    }

    pub fn silent(self, on: bool) -> Self {
        self.state().silent = on;
        self
    }

    /// Refuse to say where the dial is or what mode it is in.
    pub fn unreadable(self, on: bool) -> Self {
        self.state().unreadable = on;
        self
    }

    /// An Icom that has no DATA mode to read (an older one): the mode is all
    /// it says.
    pub fn without_data_mode(self, on: bool) -> Self {
        self.state().no_data_mode = on;
        self
    }

    pub fn id(self, id: &str) -> Self {
        self.state().id = id.to_string();
        self
    }

    pub fn answers_at(self, baud: u32) -> Self {
        self.state().answers_at = Some(baud);
        self
    }

    pub fn transmitting(&self) -> bool {
        self.state().transmitting
    }

    /// Turn the dial: the rig's frequency and mode are these from now on. (It
    /// starts at 14.097.000 in USB with DATA mode on.)
    pub fn tune(&self, hz: u64, mode: Mode) {
        let mut s = self.state();
        s.hz = hz;
        s.mode = mode;
    }

    /// Every change of transmitting, in order.
    pub fn keyings(&self) -> Vec<bool> {
        self.state().keyings.clone()
    }

    /// Every change of a control line, in order.
    pub fn lines(&self) -> Vec<(Line, bool)> {
        self.state().lines.clone()
    }

    /// Everything it was sent, as text.
    pub fn heard(&self) -> String {
        String::from_utf8_lossy(&self.state().heard).into_owned()
    }
}

impl State {
    fn transmit(&mut self, on: bool) {
        if self.transmitting != on {
            self.transmitting = on;
            self.keyings.push(on);
        }
    }

    fn icom(&mut self, bytes: &[u8]) {
        if self.echo {
            self.said.extend(bytes);
        }
        let mut buffer = bytes.to_vec();
        while let Some(f) = next_frame(&mut buffer) {
            if f.to != self.address {
                continue;
            }
            if self.chatter {
                // The dial turned: 14.074 MHz, to everyone.
                self.said.extend(frame(
                    0x00,
                    self.address,
                    &[0x00, 0x00, 0x40, 0x07, 0x14, 0x00],
                ));
            }
            let answer: Vec<u8> = match f.body.as_slice() {
                [0x19, 0x00] => vec![0x19, 0x00, self.address],
                [0x1C, 0x00, on @ (0 | 1)] if !self.refuse => {
                    self.transmit(*on == 1);
                    vec![0xFB]
                }
                [0x1C, 0x00] => vec![0x1C, 0x00, u8::from(self.transmitting)],
                [0x03 | 0x04] if self.unreadable => vec![0xFA],
                [0x1A, 0x06] if self.unreadable || self.no_data_mode => vec![0xFA],
                [0x03] => [&[0x03][..], &hz_bcd(self.hz)].concat(),
                [0x04] => vec![
                    0x04,
                    match self.mode {
                        Mode::Lsb => 0x00,
                        Mode::Usb | Mode::UsbData => 0x01,
                        Mode::Fm => 0x05,
                    },
                    0x01,
                ],
                [0x1A, 0x06] => vec![0x1A, 0x06, u8::from(self.mode == Mode::UsbData), 0x01],
                _ => vec![0xFA],
            };
            self.said.extend(frame(CONTROLLER, self.address, &answer));
        }
    }

    /// A rigctld: `t` asks the PTT, `T 1` and `T 0` set it.
    fn rigctld(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        for command in text.lines() {
            let answer = match command {
                "t" => format!(
                    "{}
",
                    u8::from(self.transmitting)
                ),
                "f" | "m" if self.unreadable => "RPRT -11\n".to_string(),
                "f" => format!("{}\n", self.hz),
                "m" => format!(
                    "{}\n2400\n",
                    match self.mode {
                        Mode::Lsb => "LSB",
                        Mode::Usb => "USB",
                        Mode::UsbData => "PKTUSB",
                        Mode::Fm => "FM",
                    }
                ),
                "T 1" | "T 0" if self.refuse => "RPRT -8
"
                .to_string(),
                "T 1" | "T 0" => {
                    self.transmit(command == "T 1");
                    "RPRT 0
"
                    .to_string()
                }
                _ => "RPRT -1
"
                .to_string(),
            };
            self.said.extend(answer.as_bytes());
        }
    }

    fn yaesu(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        for command in text.split_inclusive(';') {
            if self.chatter {
                self.said.extend(b"FA014074000;");
            }
            let answer = match command {
                "ID;" => format!("ID{};", self.id),
                "TX1;" | "TX0;" => {
                    if !self.refuse {
                        self.transmit(command == "TX1;");
                    }
                    continue;
                }
                "TX;" => format!("TX{};", u8::from(self.transmitting)),
                "FA;" | "MD0;" if self.unreadable => "?;".into(),
                "FA;" => format!("FA{:09};", self.hz),
                "MD0;" => format!(
                    "MD0{};",
                    match self.mode {
                        Mode::Lsb => '1',
                        Mode::Usb => '2',
                        Mode::UsbData => 'C',
                        Mode::Fm => '4',
                    }
                ),
                _ => "?;".into(),
            };
            self.said.extend(answer.as_bytes());
        }
    }
}

impl Link for FakeRig {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut s = self.state();
        s.heard.extend_from_slice(bytes);
        if s.silent || s.answers_at.is_some_and(|b| b != s.baud) {
            return Ok(());
        }
        match s.model {
            Model::Ic7300 => s.icom(bytes),
            Model::Ft710 => s.yaesu(bytes),
            Model::Rigctld => s.rigctld(bytes),
        }
        Ok(())
    }

    fn receive(&mut self, into: &mut Vec<u8>) -> Result<(), String> {
        let said: Vec<u8> = self.state().said.drain(..).collect();
        if said.is_empty() {
            // As a port would, wait a moment for something.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        into.extend(said);
        Ok(())
    }

    fn discard(&mut self) {
        self.state().said.clear();
    }

    fn set_baud(&mut self, baud: u32) -> Result<(), String> {
        self.state().baud = baud;
        Ok(())
    }

    fn set_line(&mut self, line: Line, high: bool) -> Result<(), String> {
        self.state().lines.push((line, high));
        Ok(())
    }
}
