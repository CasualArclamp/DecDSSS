// made by CasualArclamp
//! Keying a transmitter: through a rig's CAT port (the Icom IC-7300 by CI-V,
//! the Yaesu FT-710 by Yaesu's ASCII commands, any rig Hamlib knows through its
//! `rigctld`), or by a serial port's DTR or RTS line.
//!
//! Only the transmitter's key. DecDSSS does not tune the rig: the operator
//! sets the frequency and the mode (USB, data) on the radio, as with a sound
//! card interface. The CAT, line and model code started as voicemodem's `rig`
//! crate (same author); the Hamlib `rigctld` model, the DTR/RTS lines and
//! [`Ptt`] are new here.
//!
//! [`Ptt`] owns the rig on a thread of its own, so a port that is slow to
//! answer, or never does, holds nothing else up; it keys and lets go as it is
//! told, and whenever it is dropped it lets the rig go.

mod civ;
mod link;
mod ptt;
mod rigctl;
pub mod testing;
mod yaesu;

pub use link::{Line, Link, Port, RIGCTLD_PORT, Serial, Tcp, ports, with_default_port};
pub use ptt::{Config, Hold, KEY_WAIT, Links, MAX_DELAY_MS, Method, Phase, Ptt, Status, UNKEY_WAIT};

/// A rig this crate can key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Model {
    Ic7300,
    Ft710,
    /// Any rig, through Hamlib's `rigctld` on this or another machine.
    Rigctld,
}

impl Model {
    pub const ALL: [Model; 3] = [Model::Ic7300, Model::Ft710, Model::Rigctld];

    pub fn name(self) -> &'static str {
        match self {
            Model::Ic7300 => "IC-7300",
            Model::Ft710 => "FT-710",
            Model::Rigctld => "rigctld",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Model::Ic7300 => "Icom IC-7300",
            Model::Ft710 => "Yaesu FT-710",
            Model::Rigctld => "Hamlib rigctld (any rig, over TCP)",
        }
    }

    /// As the settings and the command line spell it.
    pub fn code(self) -> &'static str {
        match self {
            Model::Ic7300 => "ic7300",
            Model::Ft710 => "ft710",
            Model::Rigctld => "rigctld",
        }
    }

    /// `text` as a rig: "ic7300", "IC-7300", "Icom IC-7300", "7300", the same
    /// for the FT-710, and "rigctld", "hamlib".
    pub fn parse(text: &str) -> Option<Self> {
        let squashed: String = text
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        match squashed
            .trim_start_matches("icom")
            .trim_start_matches("yaesu")
        {
            "ic7300" | "7300" => Some(Model::Ic7300),
            "ft710" | "710" => Some(Model::Ft710),
            "rigctld" | "hamlib" | "net" => Some(Model::Rigctld),
            _ => None,
        }
    }

    /// Whether the rig is on a serial port (and has a CAT rate), not behind a
    /// rigctld's `host:port`.
    pub fn serial(self) -> bool {
        self != Model::Rigctld
    }

    /// The CAT rate tried first when none is set: for the IC-7300, its CI-V
    /// USB Baud Rate as it comes.
    pub fn default_baud(self) -> u32 {
        self.bauds().first().copied().unwrap_or(0)
    }

    /// The CAT rates the rig's menu offers, most likely first: the ones tried
    /// when the rate set gets no answer. A rigctld has none of ours.
    pub fn bauds(self) -> &'static [u32] {
        match self {
            Model::Ic7300 => &[115_200, 19_200, 9_600, 57_600, 38_400, 4_800],
            Model::Ft710 => &[38_400, 4_800, 9_600, 19_200, 115_200],
            Model::Rigctld => &[],
        }
    }

    /// What goes in the device box: a serial port, or an address.
    pub fn device_hint(self) -> &'static str {
        match self {
            Model::Rigctld => "host:port, e.g. localhost:4532",
            Model::Ft710 => "its COM port (Standard or Enhanced)",
            Model::Ic7300 => "the CP210x COM port",
        }
    }
}

/// Where a rig is, and how to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub model: Model,
    /// Its serial port, COM5 say, or for a rigctld its address.
    pub port: String,
    /// The CAT rate tried first (a serial rig only).
    pub baud: u32,
    /// The IC-7300's CI-V address, 94h as it comes. The others have none.
    pub civ: u8,
}

impl Settings {
    pub fn new(model: Model, port: &str) -> Self {
        Self {
            model,
            port: port.to_string(),
            baud: model.default_baud(),
            civ: civ::IC7300,
        }
    }
}

/// The IC-7300's CI-V address as it comes.
pub const IC7300_ADDRESS: u8 = civ::IC7300;

/// A rig's CAT, on a link.
pub trait Cat: Send {
    /// Ask the rig what it is; what it said.
    fn identify(&mut self) -> Result<String, String>;

    /// Key the transmitter, or let it go, and see that the rig has.
    fn key(&mut self, on: bool) -> Result<(), String>;

    fn set_baud(&mut self, baud: u32) -> Result<(), String>;

    /// Raise or lower a control line of the port the CAT is on.
    fn set_line(&mut self, line: Line, high: bool) -> Result<(), String>;
}

/// `settings`'s rig's CAT, on `link`.
pub fn cat(settings: &Settings, link: Box<dyn Link>) -> Box<dyn Cat> {
    match settings.model {
        Model::Ic7300 => Box::new(civ::Icom::new(link, settings.civ)),
        Model::Ft710 => Box::new(yaesu::Yaesu::new(link)),
        Model::Rigctld => Box::new(rigctl::Rigctld::new(link)),
    }
}

/// A rig found on its link: its CAT, the rate it answered at (0 for a
/// rigctld), and what it said.
pub struct Found {
    pub cat: Box<dyn Cat>,
    pub baud: u32,
    pub answer: String,
}

impl std::fmt::Debug for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Found")
            .field("baud", &self.baud)
            .field("answer", &self.answer)
            .finish_non_exhaustive()
    }
}

/// Find `settings`'s rig on `link`: at the rate set, or failing that at each
/// other rate the rig offers, asking it only what it is.
pub fn find(settings: &Settings, link: Box<dyn Link>) -> Result<Found, String> {
    let mut cat = cat(settings, link);
    let mut rates = vec![settings.baud];
    rates.extend(
        settings
            .model
            .bauds()
            .iter()
            .copied()
            .filter(|&b| b != settings.baud),
    );
    if !settings.model.serial() {
        // A rigctld has one answer to give, whatever rate is set.
        rates.truncate(1);
    }
    let mut why = String::new();
    for baud in rates {
        cat.set_baud(baud)?;
        match cat.identify() {
            Ok(answer) => return Ok(Found { cat, baud, answer }),
            Err(e) => why = e,
        }
    }
    let hint = match settings.model {
        Model::Ic7300 => "its CI-V address is in MENU » SET » Connectors » CI-V",
        Model::Ft710 => "it has two COM ports (Standard and Enhanced); check its CAT rate setting",
        Model::Rigctld => "is rigctld running, with the rig's model and port?",
    };
    if settings.model.serial() {
        Err(format!(
            "no {} on {} at any rate it offers ({why}); {hint}",
            settings.model.name(),
            settings.port
        ))
    } else {
        Err(format!("no rig behind {} ({why}); {hint}", settings.port))
    }
}

/// Open the link `settings` names: a serial port, or a TCP connection.
pub fn open_link(settings: &Settings) -> Result<Box<dyn Link>, String> {
    Ok(if settings.model.serial() {
        Box::new(Serial::open(&settings.port, settings.baud)?)
    } else {
        Box::new(Tcp::open(&settings.port)?)
    })
}

/// Open `settings`'s port and find the rig on it.
pub fn connect(settings: &Settings) -> Result<Found, String> {
    find(settings, open_link(settings)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeRig;

    #[test]
    fn rigs_are_known_by_any_of_their_names() {
        for (text, model) in [
            ("ic7300", Model::Ic7300),
            ("IC-7300", Model::Ic7300),
            ("Icom IC-7300", Model::Ic7300),
            ("7300", Model::Ic7300),
            ("ft710", Model::Ft710),
            ("FT-710", Model::Ft710),
            ("Yaesu FT-710", Model::Ft710),
            ("Hamlib", Model::Rigctld),
        ] {
            assert_eq!(Model::parse(text), Some(model), "{text}");
        }
        assert_eq!(Model::parse("ic705"), None);
        for m in Model::ALL {
            assert_eq!(Model::parse(m.code()), Some(m));
        }
        for m in [Model::Ic7300, Model::Ft710] {
            assert_eq!(Model::parse(m.label()), Some(m));
        }
    }

    #[test]
    fn a_rig_is_found_at_whatever_rate_its_menu_is_set_to() {
        for (model, at) in [(Model::Ic7300, 19_200), (Model::Ft710, 4_800)] {
            let rig = FakeRig::new(model).answers_at(at);
            let found = find(&Settings::new(model, "COM5"), Box::new(rig)).unwrap();
            assert_eq!(found.baud, at, "{}", model.name());
        }
    }

    #[test]
    fn a_rigctld_is_asked_once_not_at_every_rate() {
        let rig = FakeRig::new(Model::Rigctld);
        let found = find(
            &Settings::new(Model::Rigctld, "localhost"),
            Box::new(rig.clone()),
        )
        .unwrap();
        assert_eq!(found.baud, 0);
        assert_eq!(rig.heard(), "t\n");
        let silent = FakeRig::new(Model::Rigctld).silent(true);
        let e = find(
            &Settings::new(Model::Rigctld, "localhost"),
            Box::new(silent),
        )
        .unwrap_err();
        assert!(e.starts_with("no rig behind localhost"), "{e}");
    }
}
