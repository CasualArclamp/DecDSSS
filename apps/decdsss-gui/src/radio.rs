// made by CasualArclamp
//! The radio's keying as the window and the transmit threads share it: the CAT
//! and PTT settings in force, the [`Ptt`] they opened, and the Test PTT button.
//!
//! A transmission that goes to the radio calls [`Radio::hold`] before its first
//! sample and lets the [`Hold`] go after its last (or at once when stopped), so
//! the transmitter is keyed for exactly as long as there is audio, and never
//! left on when a thread ends any way at all. Audio played to the speakers
//! (the RF monitor, received speech) never comes here.

use decdsss_rig::{Config, Hold, Phase, Ptt, Tuning};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A Test PTT lets go by itself after this long, in case the window is left.
const TEST_MAX: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Inner {
    config: Config,
    /// The connection `config` opened, if it has anything to connect to.
    ptt: Option<Arc<Ptt>>,
    /// Why `config` cannot be used, if it cannot.
    problem: Option<String>,
    /// The Test PTT in progress: each press or release is a new generation, so
    /// a thread holding the key knows when it has been told to let go.
    test_gen: u64,
    testing: bool,
    test_note: Option<String>,
}

/// The shared handle; clones are the same radio.
#[derive(Clone, Default)]
pub struct Radio {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Radio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Radio")
            .field("config", &self.config().summary())
            .finish_non_exhaustive()
    }
}

/// How the radio's keying looks, for the bar and the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lamp {
    /// No keying set up: VOX, or the operator keys by hand.
    Off,
    /// Looking for the rig.
    Wait,
    Ok,
    Bad,
}

#[derive(Clone, Debug)]
pub struct RadioStatus {
    pub lamp: Lamp,
    /// What the rig said, or why not.
    pub text: String,
    /// The transmitter is keyed now.
    pub keyed: bool,
    /// A Test PTT is holding the key.
    pub testing: bool,
    /// What the last Test PTT came to, if it did not go as asked.
    pub test_note: Option<String>,
    /// Where the rig's dial is and its mode, as it last said (CAT only).
    pub tuning: Option<Tuning>,
    /// Why the rig is not saying, when it is found but cannot be read.
    pub tuning_problem: Option<String>,
}

impl Radio {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn config(&self) -> Config {
        self.lock().config.clone()
    }

    /// A radio keyed through a stand-in rig: `FakeRig` as the CAT of an
    /// FT-710, and PTT by CAT.
    #[cfg(test)]
    pub fn with_fake_rig(rig: &decdsss_rig::testing::FakeRig, lead_ms: u32, tail_ms: u32) -> Radio {
        let radio = Radio::default();
        radio.use_fake_rig(rig, lead_ms, tail_ms);
        radio
    }

    /// Put a stand-in rig in force in place of whatever is set: the program's
    /// `--fake-rig`, for trying the window and taking pictures of it with no
    /// radio. It is an FT-710 on a port that is not there, keyed by CAT, and
    /// keys nothing real.
    pub fn use_fake_rig(&self, rig: &decdsss_rig::testing::FakeRig, lead_ms: u32, tail_ms: u32) {
        use decdsss_rig::{Links, Model, Settings};
        let config = Config {
            cat: true,
            rig: Settings::new(Model::Ft710, "COM5"),
            method: decdsss_rig::Method::Cat,
            lead_ms,
            tail_ms,
            ..Config::default()
        };
        let mut g = self.lock();
        g.test_gen += 1;
        g.problem = None;
        g.ptt = Some(Arc::new(Ptt::with_links(
            config.clone(),
            Links {
                cat: Some(Box::new(rig.clone())),
                lines: None,
            },
        )));
        g.config = config;
    }

    /// Put `config` in force: the old connection is let go (off this thread,
    /// since a rig being found takes seconds to leave) and the new one opened.
    pub fn apply(&self, config: Config) {
        let mut g = self.lock();
        g.test_gen += 1;
        g.testing = false;
        g.test_note = None;
        let old = g.ptt.take();
        g.problem = None;
        if config.wants_link() {
            match config.check() {
                Ok(()) => g.ptt = Some(Arc::new(Ptt::open(config.clone()))),
                Err(e) => g.problem = Some(e),
            }
        }
        g.config = config;
        drop(g);
        if let Some(old) = old {
            std::thread::spawn(move || drop(old));
        }
    }

    /// The connection to key through, opened again if the rig was not there
    /// the last time (it may have been switched on since).
    fn ptt(g: &mut Inner) -> Result<Arc<Ptt>, String> {
        if let Some(e) = &g.problem {
            return Err(e.clone());
        }
        match &g.ptt {
            Some(p) if p.status().phase == Phase::Failed => {
                let fresh = Arc::new(Ptt::open(g.config.clone()));
                let old = g.ptt.replace(Arc::clone(&fresh));
                std::thread::spawn(move || drop(old));
                Ok(fresh)
            }
            Some(p) => Ok(Arc::clone(p)),
            None => Err("no way to key the radio is set up".into()),
        }
    }

    /// Key the transmitter for a transmission about to start, if the radio is
    /// set to be keyed; `Ok(None)` if it is not (VOX, or by hand). Returns when
    /// the rig has keyed and the lead time has passed: audio may start. An
    /// `Err` says why the transmission must not.
    pub fn hold(&self) -> Result<Option<Hold>, String> {
        let ptt = {
            let mut g = self.lock();
            if !g.config.keys() {
                return Ok(None);
            }
            Self::ptt(&mut g)?
        };
        ptt.hold().map(Some)
    }

    /// Test PTT: key the transmitter with no audio, until told to let go (or
    /// [`TEST_MAX`]). The result shows in [`Radio::status`].
    pub fn test_ptt(&self, on: bool) {
        let mut g = self.lock();
        g.test_gen += 1;
        let mine = g.test_gen;
        if !on {
            // The thread holding the key sees the generation move and lets go.
            g.testing = false;
            return;
        }
        g.test_note = None;
        let ptt = if g.config.keys() {
            Self::ptt(&mut g)
        } else {
            Err("PTT is set to VOX: there is nothing to key".into())
        };
        let ptt = match ptt {
            Ok(p) => p,
            Err(e) => {
                g.test_note = Some(e);
                return;
            }
        };
        g.testing = true;
        drop(g);
        let radio = self.clone();
        std::thread::spawn(move || {
            let outcome = ptt.hold().map(|hold| {
                let started = Instant::now();
                while radio.lock().test_gen == mine && started.elapsed() < TEST_MAX {
                    std::thread::sleep(Duration::from_millis(50));
                }
                // Dropping it lets go, at once.
                drop(hold);
                started.elapsed() >= TEST_MAX
            });
            let mut g = radio.lock();
            if g.test_gen == mine {
                g.testing = false;
                match outcome {
                    Ok(true) => {
                        g.test_note =
                            Some(format!("let go by itself after {} s", TEST_MAX.as_secs()))
                    }
                    Ok(false) => {}
                    Err(e) => g.test_note = Some(e),
                }
            }
        });
    }

    pub fn status(&self) -> RadioStatus {
        let g = self.lock();
        let (lamp, text, keyed, tuning, tuning_problem) = match (&g.problem, &g.ptt) {
            (Some(e), _) => (Lamp::Bad, e.clone(), false, None, None),
            (None, Some(p)) => {
                let s = p.status();
                let lamp = match s.phase {
                    Phase::Connecting => Lamp::Wait,
                    Phase::Ready => Lamp::Ok,
                    Phase::Failed => Lamp::Bad,
                };
                (lamp, s.said, s.keyed, s.tuning, s.tuning_problem)
            }
            (None, None) => (
                Lamp::Off,
                "no CAT, and the radio is not keyed by this program".into(),
                false,
                None,
                None,
            ),
        };
        RadioStatus {
            lamp,
            text,
            keyed,
            testing: g.testing,
            test_note: g.test_note.clone(),
            tuning,
            tuning_problem,
        }
    }

    /// The program is ending: let the transmitter go, and wait until it has
    /// been told.
    pub fn shut(&self) {
        let mut g = self.lock();
        g.test_gen += 1;
        let ptt = g.ptt.take();
        drop(g);
        if let Some(ptt) = ptt {
            if ptt.status().keyed {
                let _ = ptt.key(false, decdsss_rig::UNKEY_WAIT);
            }
            // Dropping the last handle ends the rig's thread; a transmission
            // still holding one lets go as its own thread ends.
            drop(ptt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_rig::Model;
    use decdsss_rig::testing::FakeRig;

    /// A radio keyed through a stand-in FT-710.
    fn radio_with(rig: &FakeRig) -> Radio {
        Radio::with_fake_rig(rig, 5, 5)
    }

    fn until(what: impl Fn() -> bool) {
        let started = Instant::now();
        while !what() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(what());
    }

    #[test]
    fn a_radio_on_vox_is_never_keyed() {
        let radio = Radio::default();
        assert!(matches!(radio.hold(), Ok(None)));
        radio.test_ptt(true);
        assert_eq!(
            radio.status().test_note.as_deref(),
            Some("PTT is set to VOX: there is nothing to key")
        );
        assert_eq!(radio.status().lamp, Lamp::Off);
    }

    #[test]
    fn a_transmission_keys_the_rig_for_as_long_as_it_holds() {
        let rig = FakeRig::new(Model::Ft710);
        let radio = radio_with(&rig);
        let hold = radio.hold().unwrap().expect("keyed");
        assert!(rig.transmitting() && radio.status().keyed);
        hold.finish();
        assert!(!rig.transmitting() && !radio.status().keyed);
    }

    #[test]
    fn test_ptt_keys_until_released() {
        let rig = FakeRig::new(Model::Ft710);
        let radio = radio_with(&rig);
        radio.test_ptt(true);
        until(|| rig.transmitting());
        assert!(radio.status().testing);
        radio.test_ptt(false);
        until(|| !rig.transmitting());
        until(|| !radio.status().testing);
        assert_eq!(rig.keyings(), [true, false]);
    }

    #[test]
    fn shutting_the_radio_lets_go_of_a_keyed_rig() {
        let rig = FakeRig::new(Model::Ft710);
        let radio = radio_with(&rig);
        radio.test_ptt(true);
        until(|| rig.transmitting());
        radio.shut();
        until(|| !rig.transmitting());
    }
}
