// made by CasualArclamp
//! Hamlib's `rigctld`, over TCP: the way to any rig Hamlib knows without
//! linking Hamlib (C) into this program. Run `rigctld -m <model> -r COM7 -s
//! 4800` (`rigctl -l` lists the models) and point the rig's device at
//! `localhost:4532`.
//!
//! The protocol is lines of text. A command is a letter and its argument,
//! `T 1` to key and `T 0` to let go, `t` to ask; a command that sets is
//! answered `RPRT 0` when done and `RPRT -n` when not, and `t` is answered
//! with `0` (receiving) or `1` (transmitting).

use std::time::{Duration, Instant};

use crate::Cat;
use crate::link::{Line, Link};

/// How long rigctld has to answer, and how often a command is sent. It answers
/// after the rig has, which on a slow serial rate can be most of a second.
const ANSWER: Duration = Duration::from_millis(1500);
const TRIES: usize = 2;

/// A rig behind a rigctld.
pub struct Rigctld {
    link: Box<dyn Link>,
    buffer: Vec<u8>,
}

impl std::fmt::Debug for Rigctld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rigctld").finish_non_exhaustive()
    }
}

impl Rigctld {
    pub fn new(link: Box<dyn Link>) -> Self {
        Self {
            link,
            buffer: Vec::new(),
        }
    }

    /// Send `command` and take the line that answers it.
    fn ask(&mut self, command: &str) -> Result<String, String> {
        for _ in 0..TRIES {
            self.link.discard();
            self.buffer.clear();
            self.link.send(format!("{command}\n").as_bytes())?;
            let deadline = Instant::now() + ANSWER;
            while Instant::now() < deadline {
                self.link.receive(&mut self.buffer)?;
                if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.buffer.drain(..=end).collect();
                    return Ok(String::from_utf8_lossy(&line).trim().to_string());
                }
            }
        }
        Err(format!("no answer from rigctld to \"{command}\""))
    }
}

impl Cat for Rigctld {
    /// rigctld has no ID to give; reading the PTT shows that it is there and
    /// that it speaks the protocol. A rig that cannot be read back answers
    /// `RPRT -n`, which is still an answer.
    fn identify(&mut self) -> Result<String, String> {
        match self.ask("t")?.as_str() {
            "0" => Ok("answered, the rig is receiving".into()),
            "1" => Ok("answered, the rig is transmitting".into()),
            other if other.starts_with("RPRT") => Ok(format!("answered {other} to a PTT read")),
            other => Err(format!("answered \"{other}\", which is not rigctld")),
        }
    }

    fn key(&mut self, on: bool) -> Result<(), String> {
        match self.ask(&format!("T {}", u8::from(on)))?.as_str() {
            "RPRT 0" => Ok(()),
            other => Err(format!("rigctld answered \"{other}\"")),
        }
    }

    fn set_baud(&mut self, _baud: u32) -> Result<(), String> {
        // The serial rate is rigctld's own (`-s`), not ours.
        Ok(())
    }

    fn set_line(&mut self, _line: Line, _high: bool) -> Result<(), String> {
        Err("a rigctld connection has no serial lines of ours; give the DTR/RTS port its own device".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeRig, Model};

    #[test]
    fn it_keys_and_lets_go() {
        let rig = FakeRig::new(Model::Rigctld);
        let mut rigctld = Rigctld::new(Box::new(rig.clone()));
        assert_eq!(
            rigctld.identify(),
            Ok("answered, the rig is receiving".into())
        );
        rigctld.key(true).unwrap();
        assert!(rig.transmitting());
        assert_eq!(
            rigctld.identify(),
            Ok("answered, the rig is transmitting".into())
        );
        rigctld.key(false).unwrap();
        assert!(!rig.transmitting());
        assert_eq!(rig.heard(), "t\nT 1\nt\nT 0\n");
    }

    /// A rigctld stand-in on a real socket: keys on `T 1`, lets go on `T 0`.
    #[test]
    fn it_works_over_a_real_tcp_connection() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut heard = Vec::new();
            for line in BufReader::new(stream).lines() {
                let line = line.unwrap();
                let answer = match line.as_str() {
                    "t" => "0\n",
                    _ => "RPRT 0\n",
                };
                writer.write_all(answer.as_bytes()).unwrap();
                heard.push(line);
            }
            heard
        });
        let found = crate::connect(&crate::Settings::new(crate::Model::Rigctld, &address)).unwrap();
        let mut cat = found.cat;
        cat.key(true).unwrap();
        cat.key(false).unwrap();
        drop(cat);
        assert_eq!(server.join().unwrap(), ["t", "T 1", "T 0"]);
    }

    #[test]
    fn a_refusal_and_silence_are_errors() {
        let mut rigctld = Rigctld::new(Box::new(FakeRig::new(Model::Rigctld).refuse(true)));
        assert_eq!(
            rigctld.key(true),
            Err("rigctld answered \"RPRT -8\"".into())
        );
        let mut rigctld = Rigctld::new(Box::new(FakeRig::new(Model::Rigctld).silent(true)));
        assert_eq!(
            rigctld.identify(),
            Err("no answer from rigctld to \"t\"".into())
        );
    }
}
