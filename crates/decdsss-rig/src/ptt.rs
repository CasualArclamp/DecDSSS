// made by CasualArclamp
//! Push to talk: the transmitter keyed by CAT or by a serial control line, on a
//! thread that owns the rig.
//!
//! The thread's shape (jobs in, the rig found first, and always let go on the
//! way out) is voicemodem's `Keyer`; what is new is that a caller can wait for
//! the answer, so audio is not started before the rig has keyed, and that the
//! key can be a control line as well as CAT.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::link::{Line, Link, Serial};
use crate::{Found, Model, Settings, find, open_link};

/// How long the port is tried when it will not open at first: a rig just let
/// go of by the program holds its port until its thread has finished, which may
/// be looking for the rig at every rate.
const OPENING: Duration = Duration::from_secs(4);
const RETRY: Duration = Duration::from_millis(100);

/// How often the rig is told again to let go, when the program is ending and
/// it did not answer the first time.
const LAST_TRIES: usize = 3;

/// How long a caller waits for the rig to key: the rig is found first (up to
/// every rate it offers, a few seconds) if that has not been done.
pub const KEY_WAIT: Duration = Duration::from_secs(8);
/// How long to wait for it to let go.
pub const UNKEY_WAIT: Duration = Duration::from_secs(2);

/// The rate a control-line port is opened at: it carries no data.
const LINES_BAUD: u32 = 9600;

/// What keys the transmitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// Nothing: the radio is on VOX, or keyed by hand.
    Vox,
    /// A command to the rig.
    Cat,
    /// The DTR line of a serial port, high to transmit.
    Dtr,
    /// The RTS line, high to transmit.
    Rts,
}

impl Method {
    pub const ALL: [Method; 4] = [Method::Vox, Method::Cat, Method::Dtr, Method::Rts];

    pub fn label(self) -> &'static str {
        match self {
            Method::Vox => "VOX (no keying)",
            Method::Cat => "CAT",
            Method::Dtr => "DTR",
            Method::Rts => "RTS",
        }
    }

    /// As the settings spell it.
    pub fn code(self) -> &'static str {
        match self {
            Method::Vox => "vox",
            Method::Cat => "cat",
            Method::Dtr => "dtr",
            Method::Rts => "rts",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|m| m.code().eq_ignore_ascii_case(text.trim()))
    }

    /// The serial line it keys on, if it is one.
    pub fn line(self) -> Option<Line> {
        match self {
            Method::Dtr => Some(Line::Dtr),
            Method::Rts => Some(Line::Rts),
            Method::Vox | Method::Cat => None,
        }
    }
}

/// Everything the CAT and PTT dialog sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// CAT control on: the rig is found on its port and asked what it is.
    pub cat: bool,
    pub rig: Settings,
    pub method: Method,
    /// The port a DTR or RTS key is on; empty for the CAT port.
    pub ptt_port: String,
    /// Hold DTR or RTS high on the CAT port, for an interface that takes its
    /// power from the line. Not the line that keys, on the port that keys.
    pub force_dtr: bool,
    pub force_rts: bool,
    /// Milliseconds between keying and the first sample, and between the last
    /// sample and letting go.
    pub lead_ms: u32,
    pub tail_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cat: false,
            rig: Settings::new(Model::Ft710, ""),
            method: Method::Vox,
            ptt_port: String::new(),
            force_dtr: false,
            force_rts: false,
            lead_ms: 100,
            tail_ms: 100,
        }
    }
}

/// Where a DTR or RTS key is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinesOn {
    /// On the CAT port, through the rig's own link.
    CatPort,
    /// On a port of its own.
    Port(String),
}

fn same_port(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// The longest lead or tail time accepted, milliseconds.
pub const MAX_DELAY_MS: u32 = 2000;

impl Config {
    /// The settings as `key=value` pairs, the keys the GUI's preferences file
    /// keeps them under (the CLI reads the same file, so both keep to one set).
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let flag = |b: bool| if b { "1" } else { "0" }.to_string();
        vec![
            ("rig_cat", flag(self.cat)),
            ("rig_model", self.rig.model.code().to_string()),
            ("rig_port", self.rig.port.clone()),
            ("rig_baud", self.rig.baud.to_string()),
            ("rig_civ", format!("{:02X}", self.rig.civ)),
            ("rig_ptt", self.method.code().to_string()),
            ("rig_ptt_port", self.ptt_port.clone()),
            ("rig_force_dtr", flag(self.force_dtr)),
            ("rig_force_rts", flag(self.force_rts)),
            ("rig_lead_ms", self.lead_ms.to_string()),
            ("rig_tail_ms", self.tail_ms.to_string()),
        ]
    }

    /// The settings from `get`, which looks a key up; what is missing or not
    /// understood is the default.
    pub fn from_lookup<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        let d = Config::default();
        let flag = |k: &str, default: bool| get(k).map_or(default, |v| v == "1");
        let number = |k: &str, default: u32| get(k).and_then(|v| v.parse().ok()).unwrap_or(default);
        let model = get("rig_model")
            .and_then(Model::parse)
            .unwrap_or(d.rig.model);
        let mut rig = Settings::new(model, get("rig_port").unwrap_or(""));
        rig.baud = number("rig_baud", rig.baud);
        if let Some(civ) = get("rig_civ").and_then(|v| u8::from_str_radix(v, 16).ok()) {
            rig.civ = civ;
        }
        Config {
            cat: flag("rig_cat", d.cat),
            rig,
            method: get("rig_ptt").and_then(Method::parse).unwrap_or(d.method),
            ptt_port: get("rig_ptt_port").unwrap_or("").to_string(),
            force_dtr: flag("rig_force_dtr", d.force_dtr),
            force_rts: flag("rig_force_rts", d.force_rts),
            lead_ms: number("rig_lead_ms", d.lead_ms).min(MAX_DELAY_MS),
            tail_ms: number("rig_tail_ms", d.tail_ms).min(MAX_DELAY_MS),
        }
    }

    /// Whether there is anything to connect to: the rig is to be found, or a
    /// line is to key.
    pub fn wants_link(&self) -> bool {
        self.cat || self.method.line().is_some()
    }

    /// Whether this keys the transmitter at all.
    pub fn keys(&self) -> bool {
        self.method != Method::Vox
    }

    fn lines_on(&self) -> Result<LinesOn, String> {
        let on_cat_port = self.cat && self.rig.model.serial();
        let own = self.ptt_port.trim();
        if !own.is_empty() && !(on_cat_port && same_port(own, &self.rig.port)) {
            Ok(LinesOn::Port(own.to_string()))
        } else if on_cat_port {
            Ok(LinesOn::CatPort)
        } else {
            Err("choose the serial port the PTT line is on".into())
        }
    }

    /// The lines held high on the CAT port: the ones asked for, where the CAT
    /// is on a serial port and the line is not the one keying through it.
    fn forced(&self) -> Vec<Line> {
        if !(self.cat && self.rig.model.serial()) {
            return Vec::new();
        }
        let keyed_here = self
            .method
            .line()
            .filter(|_| self.lines_on() == Ok(LinesOn::CatPort));
        [(Line::Dtr, self.force_dtr), (Line::Rts, self.force_rts)]
            .into_iter()
            .filter(|&(line, wanted)| wanted && Some(line) != keyed_here)
            .map(|(line, _)| line)
            .collect()
    }

    /// Whether this can work as set; if not, what to fix.
    pub fn check(&self) -> Result<(), String> {
        if self.cat && self.rig.port.trim().is_empty() {
            return Err(if self.rig.model.serial() {
                "choose the rig's serial device".into()
            } else {
                "give the rigctld's address, host:port".into()
            });
        }
        match self.method {
            Method::Vox => Ok(()),
            Method::Cat if !self.cat => Err("PTT by CAT needs CAT control switched on".into()),
            Method::Cat => Ok(()),
            Method::Dtr | Method::Rts => self.lines_on().map(|_| ()),
        }
    }

    /// One line on what this is set to, for a log or a tooltip.
    pub fn summary(&self) -> String {
        let rig = if self.cat {
            format!("{} on {}", self.rig.model.name(), self.rig.port)
        } else {
            "no CAT".into()
        };
        match self.method {
            Method::Vox => format!("{rig}, no PTT (VOX or by hand)"),
            Method::Cat => format!("{rig}, PTT by CAT"),
            Method::Dtr | Method::Rts => match self.lines_on() {
                Ok(LinesOn::Port(p)) => format!("{rig}, PTT on {} of {p}", self.method.label()),
                _ => format!("{rig}, PTT on {} of the CAT port", self.method.label()),
            },
        }
    }
}

/// The links a [`Ptt`] works on, opened: the CAT's, and a control-line port's
/// of its own if there is one.
#[derive(Default)]
pub struct Links {
    pub cat: Option<Box<dyn Link>>,
    pub lines: Option<Box<dyn Link>>,
}

/// Open a serial port, trying again for a few seconds if it will not open at
/// first (see [`OPENING`]).
fn serial_retrying<T>(open: impl Fn() -> Result<T, String>) -> Result<T, String> {
    let started = Instant::now();
    loop {
        match open() {
            Ok(link) => return Ok(link),
            Err(e) if started.elapsed() >= OPENING => return Err(e),
            Err(_) => std::thread::sleep(RETRY),
        }
    }
}

fn open_links(config: &Config) -> Result<Links, String> {
    let mut links = Links::default();
    if config.cat {
        links.cat = Some(if config.rig.model.serial() {
            serial_retrying(|| open_link(&config.rig))?
        } else {
            open_link(&config.rig)?
        });
    }
    if config.method.line().is_some()
        && let LinesOn::Port(name) = config.lines_on()?
    {
        links.lines = Some(serial_retrying(|| {
            Serial::open(&name, LINES_BAUD).map(|s| Box::new(s) as Box<dyn Link>)
        })?);
    }
    Ok(links)
}

/// What a [`Ptt`] is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The port is opening and the rig being found.
    Connecting,
    Ready,
    /// The rig was not found; asking again means a new [`Ptt`].
    Failed,
}

/// What a [`Ptt`] says of itself, for the display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    /// Where it was found and what the rig said, or why not.
    pub said: String,
    /// The transmitter is keyed, as far as the rig has said.
    pub keyed: bool,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    said: String,
    keyed: bool,
    /// The last job finished, and how it went.
    done: u64,
    result: Result<(), String>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, phase: Phase, said: String) {
        let mut state = self.lock();
        state.phase = phase;
        state.said = said;
        drop(state);
        self.changed.notify_all();
    }
}

struct Job {
    on: bool,
    seq: u64,
}

/// A transmitter key. The rig is found on a thread of its own, which then keys
/// it and lets it go as it is told; dropping the `Ptt` lets the rig go, since
/// the thread unkeys it before it ends.
pub struct Ptt {
    config: Config,
    shared: Arc<Shared>,
    jobs: Option<Sender<Job>>,
    /// The number of the last job sent; held for the length of a call, so one
    /// call's answer is never taken for another's.
    caller: Mutex<u64>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Ptt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ptt")
            .field("config", &self.config.summary())
            .finish_non_exhaustive()
    }
}

impl Ptt {
    /// Open `config`'s ports and find the rig, on a thread of its own: this
    /// returns at once, and [`Ptt::status`] says how it went.
    pub fn open(config: Config) -> Self {
        let for_thread = config.clone();
        Self::spawn(config, move || open_links(&for_thread))
    }

    /// The same on links already open: a stand-in rig, in tests.
    pub fn with_links(config: Config, links: Links) -> Self {
        Self::spawn(config, move || Ok(links))
    }

    fn spawn(
        config: Config,
        open: impl FnOnce() -> Result<Links, String> + Send + 'static,
    ) -> Self {
        let (jobs, inbox) = channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                phase: Phase::Connecting,
                said: format!("looking for the rig: {}", config.summary()),
                keyed: false,
                done: 0,
                result: Ok(()),
            }),
            changed: Condvar::new(),
        });
        let (for_thread, config_for_thread) = (Arc::clone(&shared), config.clone());
        let thread = std::thread::Builder::new()
            .name("rig".into())
            .spawn(move || run(&config_for_thread, open, &inbox, &for_thread))
            .expect("the rig thread starts");
        Self {
            config,
            shared,
            jobs: Some(jobs),
            caller: Mutex::new(0),
            thread: Some(thread),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn status(&self) -> Status {
        let state = self.shared.lock();
        Status {
            phase: state.phase,
            said: state.said.clone(),
            keyed: state.keyed,
        }
    }

    /// Key the transmitter, or let it go, and wait up to `wait` for the rig to
    /// say it has (for a rig that is still being found, that includes finding
    /// it). `Err` says why not.
    pub fn key(&self, on: bool, wait: Duration) -> Result<(), String> {
        let mut caller = self.caller.lock().unwrap_or_else(|e| e.into_inner());
        *caller += 1;
        let seq = *caller;
        if self.shared.lock().phase == Phase::Failed {
            return Err(self.shared.lock().said.clone());
        }
        let Some(jobs) = &self.jobs else {
            return Err("the rig has been let go".into());
        };
        if jobs.send(Job { on, seq }).is_err() {
            return Err(self.shared.lock().said.clone());
        }
        let deadline = Instant::now() + wait;
        let mut state = self.shared.lock();
        loop {
            if state.done >= seq {
                return state.result.clone();
            }
            if state.phase == Phase::Failed {
                return Err(state.said.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "the rig did not answer within {:.0} s",
                    wait.as_secs_f64()
                ));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Key the transmitter and give the rig the lead time before returning:
    /// audio may start. The [`Hold`] lets it go when dropped, which a
    /// transmission that ends any other way than its last sample (stopped,
    /// failed) does at once; [`Hold::finish`] waits the tail time first.
    pub fn hold(self: &Arc<Self>) -> Result<Hold, String> {
        if let Err(e) = self.key(true, KEY_WAIT) {
            // It may have keyed and the answer been lost.
            let _ = self.key(false, UNKEY_WAIT);
            return Err(e);
        }
        let hold = Hold {
            ptt: Arc::clone(self),
        };
        std::thread::sleep(Duration::from_millis(u64::from(self.config.lead_ms)));
        Ok(hold)
    }
}

impl Drop for Ptt {
    fn drop(&mut self) {
        // Closing the inbox ends the thread, which lets the rig go on its way.
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The transmitter is keyed for as long as this is held.
pub struct Hold {
    ptt: Arc<Ptt>,
}

impl Hold {
    /// The transmission has played out: wait the tail time, then let go.
    pub fn finish(self) {
        std::thread::sleep(Duration::from_millis(u64::from(self.ptt.config.tail_ms)));
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        for _ in 0..LAST_TRIES {
            if self.ptt.key(false, UNKEY_WAIT).is_ok() {
                break;
            }
        }
    }
}

/// What the thread works the rig with.
struct Rig {
    cat: Option<Box<dyn crate::Cat>>,
    /// A control-line port of its own, if the key is not on the CAT port.
    lines: Option<Box<dyn Link>>,
    method: Method,
}

impl Rig {
    fn key(&mut self, on: bool) -> Result<(), String> {
        match self.method {
            Method::Vox => Ok(()),
            Method::Cat => self
                .cat
                .as_mut()
                .ok_or("there is no CAT to key by")?
                .key(on),
            Method::Dtr | Method::Rts => {
                let line = self.method.line().expect("DTR and RTS are lines");
                match (&mut self.lines, &mut self.cat) {
                    (Some(port), _) => port.set_line(line, on),
                    (None, Some(cat)) => cat.set_line(line, on),
                    (None, None) => Err("there is no port for the PTT line".into()),
                }
            }
        }
    }
}

/// Find the rig and set the lines, as `config` says. What it found, in words.
fn connect(config: &Config, links: Links) -> Result<(Rig, String), String> {
    config.check()?;
    let mut said = Vec::new();
    let mut cat = None;
    if config.cat {
        let link = links.cat.ok_or("the CAT link did not open")?;
        let Found {
            cat: found,
            baud,
            answer,
        } = find(&config.rig, link)?;
        let rate = if baud > 0 {
            format!(" at {baud} baud")
        } else {
            String::new()
        };
        said.push(format!(
            "{} on {}{rate} {answer}",
            config.rig.model.name(),
            config.rig.port
        ));
        cat = Some(found);
    }
    let mut lines = None;
    if let Some(line) = config.method.line() {
        match config.lines_on()? {
            LinesOn::Port(name) => {
                lines = links.lines;
                said.push(format!("PTT on {} of {name}", line.name()));
            }
            LinesOn::CatPort => said.push(format!("PTT on {} of {}", line.name(), config.rig.port)),
        }
    } else if config.method == Method::Cat {
        said.push("PTT by CAT".into());
    }
    if let Some(cat) = &mut cat {
        for line in config.forced() {
            cat.set_line(line, true)
                .map_err(|e| format!("could not hold {} high: {e}", line.name()))?;
        }
    }
    Ok((
        Rig {
            cat,
            lines,
            method: config.method,
        },
        said.join("; "),
    ))
}

fn run(
    config: &Config,
    open: impl FnOnce() -> Result<Links, String>,
    inbox: &Receiver<Job>,
    shared: &Shared,
) {
    let (mut rig, said) = match open().and_then(|links| connect(config, links)) {
        Ok(found) => found,
        Err(e) => {
            shared.set(Phase::Failed, e);
            return;
        }
    };
    shared.set(Phase::Ready, said);
    // Whether the rig may be transmitting: keyed, or told to be and not
    // certainly let go since.
    let mut maybe_on = false;
    while let Ok(job) = inbox.recv() {
        maybe_on |= job.on;
        let result = rig.key(job.on);
        if result.is_ok() {
            maybe_on = job.on;
        }
        let mut state = shared.lock();
        state.keyed = maybe_on;
        state.done = job.seq;
        state.result = result.map_err(|e| {
            let what = if job.on {
                "would not key"
            } else {
                "would not let go, and may still be transmitting"
            };
            format!("the rig {what}: {e}")
        });
        drop(state);
        shared.changed.notify_all();
    }
    // Whoever keyed the rig has gone: it is never left transmitting.
    if maybe_on {
        for _ in 0..LAST_TRIES {
            if rig.key(false).is_ok() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeRig;

    fn with_cat(model: Model, method: Method) -> Config {
        Config {
            cat: true,
            rig: Settings::new(model, "COM5"),
            method,
            ..Config::default()
        }
    }

    fn links(rig: &FakeRig) -> Links {
        Links {
            cat: Some(Box::new(rig.clone())),
            lines: None,
        }
    }

    /// Wait for the thread to have found the rig, or failed to.
    fn found(ptt: &Ptt) -> Status {
        let started = Instant::now();
        while ptt.status().phase == Phase::Connecting && started.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        ptt.status()
    }

    #[test]
    fn it_keys_by_cat_and_says_where_the_rig_was_found() {
        for model in Model::ALL {
            let rig = FakeRig::new(model);
            let ptt = Ptt::with_links(with_cat(model, Method::Cat), links(&rig));
            let status = found(&ptt);
            assert_eq!(status.phase, Phase::Ready, "{status:?}");
            assert!(
                status
                    .said
                    .starts_with(&format!("{} on COM5", model.name())),
                "{}",
                status.said
            );
            assert!(status.said.ends_with("PTT by CAT"), "{}", status.said);
            ptt.key(true, KEY_WAIT).unwrap();
            assert!(rig.transmitting() && ptt.status().keyed, "{}", model.name());
            ptt.key(false, KEY_WAIT).unwrap();
            assert!(
                !rig.transmitting() && !ptt.status().keyed,
                "{}",
                model.name()
            );
        }
    }

    #[test]
    fn it_keys_on_a_line_of_the_cat_port() {
        for (method, line) in [(Method::Dtr, Line::Dtr), (Method::Rts, Line::Rts)] {
            let rig = FakeRig::new(Model::Ft710);
            let ptt = Ptt::with_links(with_cat(Model::Ft710, method), links(&rig));
            assert_eq!(found(&ptt).phase, Phase::Ready);
            ptt.key(true, KEY_WAIT).unwrap();
            ptt.key(false, KEY_WAIT).unwrap();
            assert_eq!(rig.lines(), [(line, true), (line, false)]);
            // The CAT itself is never asked to transmit.
            assert!(rig.keyings().is_empty());
        }
    }

    #[test]
    fn it_keys_on_a_port_of_its_own_with_no_cat_at_all() {
        let lines = FakeRig::new(Model::Ft710);
        let config = Config {
            method: Method::Rts,
            ptt_port: "COM8".into(),
            ..Config::default()
        };
        let ptt = Ptt::with_links(
            config,
            Links {
                cat: None,
                lines: Some(Box::new(lines.clone())),
            },
        );
        let status = found(&ptt);
        assert_eq!(
            (status.phase, status.said.as_str()),
            (Phase::Ready, "PTT on RTS of COM8")
        );
        ptt.key(true, KEY_WAIT).unwrap();
        assert_eq!(lines.lines(), [(Line::Rts, true)]);
        // Dropped while keyed: the line is let go on the way out.
        drop(ptt);
        assert_eq!(lines.lines(), [(Line::Rts, true), (Line::Rts, false)]);
    }

    #[test]
    fn a_line_port_that_is_the_cat_port_is_the_cat_port() {
        let config = Config {
            ptt_port: "com5".into(),
            ..with_cat(Model::Ft710, Method::Dtr)
        };
        assert_eq!(config.lines_on(), Ok(LinesOn::CatPort));
        let config = Config {
            ptt_port: "COM8".into(),
            ..config
        };
        assert_eq!(config.lines_on(), Ok(LinesOn::Port("COM8".into())));
    }

    #[test]
    fn forced_lines_go_high_when_the_rig_is_found_but_not_the_one_that_keys() {
        let rig = FakeRig::new(Model::Ft710);
        let config = Config {
            force_dtr: true,
            force_rts: true,
            ..with_cat(Model::Ft710, Method::Rts)
        };
        assert_eq!(config.forced(), [Line::Dtr]);
        let ptt = Ptt::with_links(config, links(&rig));
        assert_eq!(found(&ptt).phase, Phase::Ready);
        assert_eq!(rig.lines(), [(Line::Dtr, true)]);
        // Both are held with CAT keying, and neither over a rigctld.
        let config = Config {
            force_dtr: true,
            force_rts: true,
            ..with_cat(Model::Ft710, Method::Cat)
        };
        assert_eq!(config.forced(), [Line::Dtr, Line::Rts]);
        let config = Config {
            force_dtr: true,
            ..with_cat(Model::Rigctld, Method::Cat)
        };
        assert!(config.forced().is_empty());
    }

    #[test]
    fn a_hold_lets_go_when_dropped_and_finish_waits_the_tail() {
        let rig = FakeRig::new(Model::Ic7300);
        let config = Config {
            lead_ms: 10,
            tail_ms: 120,
            ..with_cat(Model::Ic7300, Method::Cat)
        };
        let ptt = Arc::new(Ptt::with_links(config, links(&rig)));
        let hold = ptt.hold().unwrap();
        assert!(rig.transmitting());
        let started = Instant::now();
        hold.finish();
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert!(!rig.transmitting());
        // Stopped: let go at once.
        let hold = ptt.hold().unwrap();
        let started = Instant::now();
        drop(hold);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(rig.keyings(), [true, false, true, false]);
    }

    #[test]
    fn a_keyed_rig_is_let_go_when_the_ptt_is_dropped() {
        let rig = FakeRig::new(Model::Ft710);
        let ptt = Ptt::with_links(with_cat(Model::Ft710, Method::Cat), links(&rig));
        ptt.key(true, KEY_WAIT).unwrap();
        drop(ptt);
        assert_eq!(rig.keyings(), [true, false]);
    }

    #[test]
    fn a_rig_that_will_not_key_says_so_and_is_not_left_on() {
        let rig = FakeRig::new(Model::Ic7300).refuse(true);
        let ptt = Arc::new(Ptt::with_links(
            with_cat(Model::Ic7300, Method::Cat),
            links(&rig),
        ));
        let e = ptt.hold().map(|_| ()).unwrap_err();
        assert_eq!(e, "the rig would not key: refused");
        assert!(!rig.transmitting());
    }

    #[test]
    fn a_rig_that_is_not_there_is_a_failure_that_says_why() {
        let rig = FakeRig::new(Model::Ft710).silent(true);
        let ptt = Ptt::with_links(with_cat(Model::Ft710, Method::Cat), links(&rig));
        let status = found(&ptt);
        assert_eq!(status.phase, Phase::Failed);
        assert!(status.said.contains("no FT-710 on COM5"), "{}", status.said);
        assert_eq!(ptt.key(true, KEY_WAIT), Err(status.said));
    }

    #[test]
    fn a_port_that_will_not_open_says_why() {
        let ptt = Ptt::spawn(with_cat(Model::Ft710, Method::Cat), || {
            Err("COM5: not there".into())
        });
        let status = found(&ptt);
        assert_eq!(
            (status.phase, status.said.as_str()),
            (Phase::Failed, "COM5: not there")
        );
    }

    #[test]
    fn a_config_that_cannot_work_says_what_to_fix() {
        let cat_off = Config {
            method: Method::Cat,
            ..Config::default()
        };
        assert_eq!(
            cat_off.check(),
            Err("PTT by CAT needs CAT control switched on".into())
        );
        let no_port = Config {
            cat: true,
            ..Config::default()
        };
        assert_eq!(
            no_port.check(),
            Err("choose the rig's serial device".into())
        );
        let no_address = Config {
            cat: true,
            rig: Settings::new(Model::Rigctld, ""),
            ..Config::default()
        };
        assert_eq!(
            no_address.check(),
            Err("give the rigctld's address, host:port".into())
        );
        // A line needs a port: its own, or a serial CAT port to share.
        let line = Config {
            method: Method::Dtr,
            ..Config::default()
        };
        assert_eq!(
            line.check(),
            Err("choose the serial port the PTT line is on".into())
        );
        let over_tcp = Config {
            method: Method::Dtr,
            ..with_cat(Model::Rigctld, Method::Dtr)
        };
        assert!(over_tcp.check().is_err());
        assert_eq!(Config::default().check(), Ok(()));
        assert!(!Config::default().wants_link());
    }
}
