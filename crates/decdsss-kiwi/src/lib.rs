// made by CasualArclamp
//! A KiwiSDR as an audio source: its demodulated SSB audio taken straight off
//! the network, with no browser and no virtual cable in between.
//!
//! **Why.** Through a browser, the Kiwi's audio crosses two clocks: the Kiwi's
//! ADC and the listener's sound card (or VB-Cable). The web client bridges them
//! with a buffer that now and then runs dry or overflows, and each time the
//! audio after that point arrives some milliseconds early or late: a *slip*,
//! which a spread-spectrum receiver tracking a chip a few hundred microseconds
//! long sees as the signal jumping away. Taken off the socket, the samples
//! arrive exactly as the Kiwi made them, on the Kiwi's own clock, and a missing
//! frame (by its sequence number) is put back as silence of its own length, so
//! nothing after it moves.
//!
//! **The protocol** is the Kiwi's own `SND` stream over WebSocket, as the
//! KiwiSDR server (jks-prv/Beagle_SDR_GPS: `rx/rx_server.cpp`, `rx/rx_sound.cpp`,
//! `rx/rx_sound_cmd.cpp`, `rx/rx_cmd.cpp`) speaks it; written here from that
//! description. The connection identifies itself as a program (the
//! `/<timestamp>/SND` path, as the kiwirecorder tool uses) and by
//! `ident_user`, so the Kiwi's owner sees what it is; the owner's limits
//! (time, inactivity, how many programs may connect) apply as to anyone, and
//! a refusal comes back as an error.
//!
//! **Rust notes.** The stream runs on a thread of its own that owns the socket;
//! samples come back through a channel (`std::sync::mpsc`), the status through
//! a `Mutex`. Dropping the [`KiwiStream`] stops the thread and closes the
//! connection.

mod adpcm;
pub mod ws;

use adpcm::Adpcm;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use ws::{Message, Socket, WsError};

/// The port a KiwiSDR listens on unless its owner changed it.
pub const DEFAULT_PORT: u16 = 8073;
/// The passband asked for, Hz from the carrier: wide enough for every DecDSSS
/// band (300 to 3300 Hz, and 500 Hz slots up to 2000 Hz) with the 100 Hz the
/// receiver searches beyond each edge.
pub const LOW_CUT_HZ: i32 = 100;
pub const HIGH_CUT_HZ: i32 = 3600;
/// How often the Kiwi is told the connection is alive (it gives up after 60 s).
const KEEPALIVE: Duration = Duration::from_secs(2);
/// Missing frames put back as silence, at most (a longer run is a stream that
/// started again, not frames lost).
const MAX_GAP_FRAMES: u32 = 64;

#[derive(Debug, thiserror::Error)]
pub enum KiwiError {
    #[error("{0}")]
    Ws(#[from] WsError),
    #[error("{0}")]
    Address(String),
    /// The Kiwi refused or dropped the connection, in its words.
    #[error("the KiwiSDR says: {0}")]
    Refused(String),
    #[error("no audio from the KiwiSDR within {0:?}")]
    Timeout(Duration),
}

/// Where to listen.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// The dial (carrier) frequency, kHz, as a radio would show it.
    pub freq_khz: f64,
    /// Lower sideband (else upper).
    pub lsb: bool,
    /// Password, if the Kiwi needs one ("" for none).
    pub password: String,
    /// What the Kiwi's owner sees as the user's name.
    pub ident: String,
    /// The Kiwi's AGC (else a fixed gain of `man_gain_db`).
    pub agc: bool,
    pub man_gain_db: i32,
}

impl Config {
    /// A configuration for `address` (see [`parse_address`]) at `freq_khz` USB,
    /// with the Kiwi's AGC.
    pub fn new(address: &str, freq_khz: f64) -> Result<Config, KiwiError> {
        let (host, port) = parse_address(address)?;
        Ok(Config {
            host,
            port,
            freq_khz,
            lsb: false,
            password: String::new(),
            ident: "DecDSSS".into(),
            agc: true,
            man_gain_db: 50,
        })
    }
}

/// `host`, `host:port`, or a Kiwi's web address as a browser shows it
/// (`http://host:8073/?f=...`): the host and the port (8073 if none is given).
pub fn parse_address(s: &str) -> Result<(String, u16), KiwiError> {
    let s = s.trim();
    let s = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .or_else(|| s.strip_prefix("ws://"))
        .unwrap_or(s);
    let hostport = s.split(['/', '?', '#']).next().unwrap_or("");
    if hostport.is_empty() {
        return Err(KiwiError::Address("no KiwiSDR address given".into()));
    }
    // Rust note: `rsplit_once` splits at the last ':' only, so an address with
    // no port is left whole.
    match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => {
            let port = p
                .parse()
                .map_err(|_| KiwiError::Address(format!("{p:?} is not a port")))?;
            Ok((h.to_string(), port))
        }
        _ => Ok((hostport.to_string(), DEFAULT_PORT)),
    }
}

/// Percent-encoding for the values the Kiwi URL-decodes (password, name).
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The inverse, for the Kiwi's encoded messages.
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the stream knows about itself.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The Kiwi's true audio rate, Hz (its ADC clock over its decimation: about
    /// 11 999 for "12 kHz", 20 251 for "20.25 kHz").
    pub sample_rate: f64,
    /// The signal level in the passband, dBm, as the Kiwi's S-meter reads it.
    pub smeter_dbm: f32,
    /// Audio frames received, and samples of silence put in for frames that
    /// never came.
    pub frames: u64,
    pub gap_samples: u64,
    /// The Kiwi's ADC overloaded in a recent frame.
    pub overload: bool,
    /// Things the Kiwi said that the user should know (time limits).
    pub notes: Vec<String>,
    /// Why the stream stopped, once it has.
    pub error: Option<String>,
}

/// The protocol, apart from the socket's thread: what to send, and what the
/// Kiwi's messages mean.
struct Session {
    sock: Socket,
    cfg: Config,
    status: Status,
    freq_offset_khz: f64,
    adpcm: Adpcm,
    compressed: Option<bool>,
    last_seq: Option<u32>,
    last_frame_samples: usize,
    last_keepalive: Instant,
    set_up: bool,
}

impl Session {
    fn start(cfg: &Config, timeout: Duration) -> Result<Session, KiwiError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            & 0xFFFF_FFFF;
        let path = format!("/{ts}/SND");
        let read = Duration::from_millis(200);
        let sock = match Socket::connect(&cfg.host, cfg.port, &path, timeout, read) {
            Err(WsError::Redirect(to)) => {
                // Once, to where it points.
                let (h, p) = parse_address(&to)?;
                Socket::connect(&h, p, &path, timeout, read)?
            }
            r => r?,
        };
        let mut s = Session {
            sock,
            cfg: cfg.clone(),
            status: Status::default(),
            freq_offset_khz: 0.0,
            adpcm: Adpcm::default(),
            compressed: None,
            last_seq: None,
            last_frame_samples: 0,
            last_keepalive: Instant::now(),
            set_up: false,
        };
        let pw = if cfg.password.is_empty() {
            String::new()
        } else {
            url_encode(&cfg.password)
        };
        s.sock.send_text(&format!("SET auth t=kiwi p={pw}"))?;
        Ok(s)
    }

    fn tune_command(&self) -> String {
        let f = self.cfg.freq_khz - self.freq_offset_khz;
        let (mode, lo, hi) = if self.cfg.lsb {
            ("lsb", -HIGH_CUT_HZ, -LOW_CUT_HZ)
        } else {
            ("usb", LOW_CUT_HZ, HIGH_CUT_HZ)
        };
        format!("SET mod={mode} low_cut={lo} high_cut={hi} freq={f:.3}")
    }

    /// Everything audio waits for, once the Kiwi has said its rate.
    fn set_up(&mut self) -> Result<(), KiwiError> {
        let gain = if self.cfg.agc {
            "SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50".to_string()
        } else {
            format!(
                "SET agc=0 hang=0 thresh=-100 slope=6 decay=1000 manGain={}",
                self.cfg.man_gain_db
            )
        };
        let tune = self.tune_command();
        for c in [
            "SET squelch=0 max=0".to_string(),
            "SET genattn=0".into(),
            "SET gen=0 mix=-1".into(),
            format!("SET ident_user={}", url_encode(&self.cfg.ident)),
            tune,
            gain,
            // Uncompressed: ADPCM is lossy, and a weak signal under the noise
            // is just what it does worst.
            "SET compression=0".into(),
            "SET keepalive".into(),
        ] {
            self.sock.send_text(&c)?;
        }
        self.set_up = true;
        Ok(())
    }

    fn keepalive(&mut self) -> Result<(), KiwiError> {
        if self.last_keepalive.elapsed() >= KEEPALIVE {
            self.last_keepalive = Instant::now();
            self.sock.send_text("SET keepalive")?;
        }
        Ok(())
    }

    /// One `MSG`: its `name=value` pairs.
    fn on_msg(&mut self, text: &str) -> Result<(), KiwiError> {
        for tok in text.split_whitespace() {
            let (k, v) = tok.split_once('=').unwrap_or((tok, ""));
            match k {
                "audio_rate" => {
                    let n: u32 = v.parse().unwrap_or(12_000);
                    self.sock
                        .send_text(&format!("SET AR OK in={n} out=48000"))?;
                }
                "sample_rate" => {
                    self.status.sample_rate = v.parse().unwrap_or(0.0);
                    if !self.set_up && self.status.sample_rate > 0.0 {
                        self.set_up()?;
                    }
                }
                "freq_offset" => self.freq_offset_khz = v.parse().unwrap_or(0.0),
                "badp" => match v {
                    "0" => {}
                    "1" => return Err(KiwiError::Refused("wrong password, or try again".into())),
                    "5" => {
                        return Err(KiwiError::Refused(
                            "only one connection from your address".into(),
                        ));
                    }
                    "6" => {
                        return Err(KiwiError::Refused(
                            "busy updating its database, try later".into(),
                        ));
                    }
                    other => {
                        return Err(KiwiError::Refused(format!("not allowed in (badp={other})")));
                    }
                },
                "too_busy" => {
                    return Err(KiwiError::Refused(format!(
                        "all its channels are in use (or its limit for programs like this one, {v})"
                    )));
                }
                "redirect" => {
                    return Err(KiwiError::Refused(format!(
                        "busy; it suggests {}",
                        url_decode(v)
                    )));
                }
                "down" => {
                    return Err(KiwiError::Refused(
                        "down (being updated or backed up)".into(),
                    ));
                }
                "wb_only" => {
                    return Err(KiwiError::Refused("it runs wideband firmware only".into()));
                }
                "exclusive_use" => return Err(KiwiError::Refused("in exclusive use".into())),
                "password_timeout" => return Err(KiwiError::Refused("password timeout".into())),
                "kiwi_kick" => {
                    return Err(KiwiError::Refused(format!(
                        "disconnected: {}",
                        url_decode(v)
                    )));
                }
                "inactivity_timeout" => self.status.notes.push(format!(
                    "the Kiwi disconnects after {v} minutes without retuning"
                )),
                "ip_limit" => {
                    let d = url_decode(v);
                    let mins = d.split(',').next().unwrap_or("?").to_string();
                    self.status.notes.push(format!(
                        "the Kiwi allows {mins} minutes a day from your address"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// One `SND` frame to samples (with silence first for frames that went
    /// missing).
    fn on_snd(&mut self, f: &[u8], out: &mut Vec<f32>) -> Result<(), KiwiError> {
        if f.len() < 10 {
            return Ok(());
        }
        let flags = f[3];
        let seq = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
        let smeter = u16::from_be_bytes([f[8], f[9]]);
        self.status.smeter_dbm = 0.1 * f32::from(smeter) - 127.0;
        self.status.overload = flags & 0x02 != 0;
        if flags & 0x08 != 0 {
            return Err(KiwiError::Refused(
                "sent IQ audio, which DecDSSS did not ask for".into(),
            ));
        }
        let payload = &f[10..];
        let compressed = flags & 0x10 != 0;
        if self.compressed != Some(compressed) || flags & 0x20 != 0 {
            // The Kiwi resets its encoder at such a change; so does the browser.
            self.adpcm = Adpcm::default();
            self.compressed = Some(compressed);
        }
        let mut pcm: Vec<i16> = Vec::with_capacity(payload.len() * 2);
        if compressed {
            self.adpcm.decode(payload, &mut pcm);
        } else if flags & 0x80 != 0 {
            pcm.extend(
                payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
        } else {
            pcm.extend(
                payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_be_bytes(*b)),
            );
        }
        if let Some(last) = self.last_seq {
            let missing = seq.wrapping_sub(last).wrapping_sub(1);
            if missing > 0 && missing <= MAX_GAP_FRAMES {
                let n = missing as usize * self.last_frame_samples.max(pcm.len());
                out.extend(std::iter::repeat_n(0.0, n));
                self.status.gap_samples += n as u64;
            }
        }
        self.last_seq = Some(seq);
        self.last_frame_samples = pcm.len();
        self.status.frames += 1;
        out.extend(pcm.iter().map(|&v| f32::from(v) / 32768.0));
        Ok(())
    }

    /// Reads one message (or waits the read timeout) and acts on it; audio goes
    /// to `out`. False when the Kiwi has closed the connection.
    fn pump(&mut self, out: &mut Vec<f32>) -> Result<bool, KiwiError> {
        self.keepalive()?;
        let Some(m) = self.sock.read()? else {
            return Ok(true);
        };
        // A Kiwi sends everything, its text messages too, as binary frames.
        let bytes = match m {
            Message::Close(code) => {
                return Err(KiwiError::Refused(match code {
                    Some(c) => format!("closed the connection (code {c})"),
                    None => "closed the connection".into(),
                }));
            }
            Message::Text(b) | Message::Binary(b) => b,
        };
        if bytes.starts_with(b"MSG") {
            let text = String::from_utf8_lossy(&bytes[3..]).to_string();
            self.on_msg(&text)?;
        } else if bytes.starts_with(b"SND") {
            self.on_snd(&bytes, out)?;
        }
        Ok(true)
    }
}

/// A running KiwiSDR stream.
pub struct KiwiStream {
    samples: mpsc::Receiver<Vec<f32>>,
    commands: mpsc::Sender<Config>,
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    rate: f64,
    /// Audio that came while [`KiwiStream::open`] waited for the first frame.
    early: Vec<f32>,
}

impl KiwiStream {
    /// Connects, and waits (up to `timeout`) until the audio flows: so the
    /// sample rate is known, and a refusal (busy, wrong password) comes back
    /// here as the error.
    pub fn open(cfg: &Config, timeout: Duration) -> Result<KiwiStream, KiwiError> {
        let t0 = Instant::now();
        let mut session = Session::start(cfg, timeout)?;
        let mut early = Vec::new();
        while session.status.frames == 0 {
            if t0.elapsed() > timeout {
                session.sock.close();
                return Err(KiwiError::Timeout(timeout));
            }
            if let Err(e) = session.pump(&mut early) {
                session.sock.close();
                return Err(e);
            }
        }
        let rate = session.status.sample_rate;
        let status = Arc::new(Mutex::new(session.status.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, samples) = mpsc::channel();
        let (commands, cmd_rx) = mpsc::channel::<Config>();
        let thread = {
            let (status, stop) = (status.clone(), stop.clone());
            std::thread::Builder::new()
                .name("kiwisdr".into())
                .spawn(move || run(session, &tx, &cmd_rx, &status, &stop))
                .map_err(|e| KiwiError::Address(format!("cannot start the stream thread: {e}")))?
        };
        Ok(KiwiStream {
            samples,
            commands,
            status,
            stop,
            thread: Some(thread),
            rate,
            early,
        })
    }

    /// The Kiwi's true audio rate, Hz.
    pub fn sample_rate(&self) -> f64 {
        self.rate
    }

    /// The audio that has come since the last call (mono, ±1).
    pub fn read(&mut self) -> Vec<f32> {
        let mut out = std::mem::take(&mut self.early);
        // Rust note: `try_iter` yields what is in the channel without waiting.
        for v in self.samples.try_iter() {
            out.extend(v);
        }
        out
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// The stream has stopped (the Kiwi closed it, or the network went); the
    /// reason is in [`Status::error`].
    pub fn is_dead(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Retunes (frequency and sideband from `cfg`; the rest is kept).
    pub fn tune(&self, cfg: &Config) {
        let _ = self.commands.send(cfg.clone());
    }
}

impl Drop for KiwiStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The stream's thread: pumps the socket until told to stop or the Kiwi goes.
fn run(
    mut s: Session,
    tx: &mpsc::Sender<Vec<f32>>,
    commands: &mpsc::Receiver<Config>,
    status: &Mutex<Status>,
    stop: &AtomicBool,
) {
    let mut buf = Vec::new();
    let error = loop {
        if stop.load(Ordering::Relaxed) {
            break None;
        }
        if let Some(cfg) = commands.try_iter().last() {
            s.cfg.freq_khz = cfg.freq_khz;
            s.cfg.lsb = cfg.lsb;
            let c = s.tune_command();
            if let Err(e) = s.sock.send_text(&c) {
                break Some(e.to_string());
            }
        }
        match s.pump(&mut buf) {
            Ok(_) => {}
            Err(e) => break Some(e.to_string()),
        }
        if !buf.is_empty() && tx.send(std::mem::take(&mut buf)).is_err() {
            break None;
        }
        *status.lock().unwrap() = s.status.clone();
    };
    s.sock.close();
    let mut g = status.lock().unwrap();
    *g = s.status.clone();
    g.error = error.or_else(|| Some("stopped".into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_from_what_people_paste() {
        let p = |s| parse_address(s).unwrap();
        assert_eq!(p("kiwi.example.org"), ("kiwi.example.org".into(), 8073));
        assert_eq!(
            p("kiwi.example.org:8074"),
            ("kiwi.example.org".into(), 8074)
        );
        assert_eq!(
            p("http://kiwi.example.org:8073/?f=7074usb"),
            ("kiwi.example.org".into(), 8073)
        );
        assert_eq!(p("  192.168.1.20:8073/  "), ("192.168.1.20".into(), 8073));
        assert!(parse_address("").is_err());
        assert!(parse_address("host:port").is_err());
    }

    #[test]
    fn names_are_encoded_as_the_kiwi_decodes_them() {
        assert_eq!(url_encode("DecDSSS VK2ABC"), "DecDSSS%20VK2ABC");
        assert_eq!(url_decode("http%3A%2F%2Fother%3A8073"), "http://other:8073");
        assert_eq!(url_decode(&url_encode("a b/c%d")), "a b/c%d");
    }

    /// A stand-in KiwiSDR on localhost: answers the upgrade, checks the client
    /// logs in and sets up, sends its rate and three audio frames, the second
    /// of them a frame late (sequence 2 is never sent).
    #[test]
    fn a_stream_from_a_stand_in_kiwi_fills_a_lost_frame_with_silence() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                c.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            let head = String::from_utf8(head).unwrap();
            assert!(
                head.starts_with("GET /") && head.contains("/SND HTTP/1.1"),
                "{head}"
            );
            c.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .unwrap();
            // The client's text frames, unmasked, as they come.
            let mut pending = Vec::new();
            let mut next_text = |c: &mut std::net::TcpStream| -> String {
                loop {
                    if let Some((_, op, p, used)) = ws::parse_frame(&pending).unwrap() {
                        pending.drain(..used);
                        assert_eq!(op, 1);
                        return String::from_utf8(p).unwrap();
                    }
                    let mut buf = [0u8; 4096];
                    let n = c.read(&mut buf).unwrap();
                    pending.extend_from_slice(&buf[..n]);
                }
            };
            // Server frames: binary, unmasked.
            let send = |c: &mut std::net::TcpStream, p: &[u8]| {
                let mut f = vec![0x82u8];
                if p.len() < 126 {
                    f.push(p.len() as u8);
                } else {
                    f.push(126);
                    f.extend_from_slice(&(p.len() as u16).to_be_bytes());
                }
                f.extend_from_slice(p);
                c.write_all(&f).unwrap();
            };
            assert_eq!(next_text(&mut c), "SET auth t=kiwi p=");
            send(
                &mut c,
                b"MSG version_maj=1 version_min=711 freq_offset=0.000",
            );
            send(&mut c, b"MSG audio_init=0 audio_rate=12000");
            assert_eq!(next_text(&mut c), "SET AR OK in=12000 out=48000");
            send(&mut c, b"MSG sample_rate=11999.031");
            let mut setup = Vec::new();
            loop {
                let t = next_text(&mut c);
                let done = t == "SET keepalive";
                setup.push(t);
                if done {
                    break;
                }
            }
            assert!(
                setup.contains(&"SET mod=usb low_cut=100 high_cut=3600 freq=7074.000".to_string()),
                "{setup:?}"
            );
            assert!(setup.contains(&"SET compression=0".to_string()));
            assert!(setup.contains(&"SET ident_user=DecDSSS".to_string()));
            for seq in [1u32, 3, 4] {
                let mut f = b"SND".to_vec();
                f.push(0);
                f.extend_from_slice(&seq.to_le_bytes());
                f.extend_from_slice(&((-60.0f32 + 127.0) as u16 * 10).to_be_bytes());
                for i in 0..512i16 {
                    f.extend_from_slice(&(i * 16 + seq as i16).to_be_bytes());
                }
                send(&mut c, &f);
            }
            // Hold the connection until the client closes it.
            let _ = c.read(&mut [0u8; 64]);
        });
        let cfg = Config::new(&format!("127.0.0.1:{port}"), 7074.0).unwrap();
        let mut k = KiwiStream::open(&cfg, Duration::from_secs(5)).unwrap();
        assert!((k.sample_rate() - 11_999.031).abs() < 1e-6);
        let mut got = Vec::new();
        let t0 = Instant::now();
        while got.len() < 4 * 512 && t0.elapsed() < Duration::from_secs(5) {
            got.extend(k.read());
            std::thread::sleep(Duration::from_millis(10));
        }
        // Frame 1, a frame of silence for 2, then 3 and 4.
        assert_eq!(got.len(), 4 * 512);
        assert_eq!(got[1], (16.0 + 1.0) / 32768.0);
        assert!(got[512..1024].iter().all(|&v| v == 0.0));
        assert_eq!(got[1024 + 1], (16.0 + 3.0) / 32768.0);
        let st = k.status();
        assert_eq!((st.frames, st.gap_samples), (3, 512));
        assert!((st.smeter_dbm + 60.0).abs() < 0.2, "{}", st.smeter_dbm);
        drop(k);
        server.join().unwrap();
    }
}
