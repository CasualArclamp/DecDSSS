// made by CasualArclamp
//! The CAT and PTT dialog: which rig, on which port, and what keys the
//! transmitter (CAT, DTR, RTS or nothing). Laid out as WSJT-X's "CAT and PTT
//! Config", with Test PTT, OK, Cancel and Apply. The settings are remembered
//! in the preferences file.
//!
//! Differences from WSJT-X: there is no Hamlib (a C library) here; the IC-7300
//! and FT-710 are driven directly, and any other rig through Hamlib's `rigctld`
//! over TCP. DecDSSS keys the transmitter and reads the rig's frequency and
//! mode for the strip across the top of the window, but never sets them, so
//! there is no OmniRig, no PTT input and no left-channel VOX tone (the
//! Transmit panel's VOX box sends one).

use crate::prefs::Prefs;
use crate::radio::{Lamp, Radio};
use crate::scope::col;
use decdsss_rig::{Config, MAX_DELAY_MS, Method, Model, Port};
use eframe::egui::{self, Color32, RichText, Ui};

/// The CAT rates the radios' menus offer between them.
const RATES: [u32; 6] = [4800, 9600, 19_200, 38_400, 57_600, 115_200];

pub struct CatDialog {
    pub open: bool,
    /// The settings as being edited; [`Radio::config`] is what is in force.
    edit: Config,
    /// The serial ports there are, as last looked.
    ports: Vec<Port>,
    /// The CI-V address as typed, hexadecimal.
    civ: String,
}

/// The settings in `prefs`, or the defaults.
pub fn restore(p: &Prefs) -> Config {
    Config::from_lookup(|k| p.get(k))
}

pub fn remember(c: &Config, p: &mut Prefs) {
    for (k, v) in c.to_pairs() {
        p.set(k, v);
    }
}

impl CatDialog {
    pub fn new(config: Config) -> Self {
        let civ = format!("{:02X}", config.rig.civ);
        Self {
            open: false,
            edit: config,
            ports: Vec::new(),
            civ,
        }
    }

    /// Open the dialog on the settings in force.
    pub fn show_open(&mut self, radio: &Radio) {
        self.edit = radio.config();
        self.civ = format!("{:02X}", self.edit.rig.civ);
        self.ports = decdsss_rig::ports();
        self.open = true;
    }

    /// The device box: a text box to type in (COM7, or host:port) and a menu of
    /// the ports there are.
    fn device_box(
        ui: &mut Ui,
        id: &str,
        text: &mut String,
        ports: &[Port],
        hint: &str,
        enabled: bool,
    ) {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(text)
                        .desired_width(150.0)
                        .hint_text(hint),
                );
                egui::ComboBox::from_id_salt(id)
                    .selected_text("▾")
                    .width(24.0)
                    .show_ui(ui, |ui| {
                        if ports.is_empty() {
                            ui.label(RichText::new("no serial ports found").weak());
                        }
                        for p in ports {
                            let label = if p.description.is_empty() {
                                p.name.clone()
                            } else {
                                format!("{}  {}", p.name, p.description)
                            };
                            if ui
                                .selectable_label(text.as_str() == p.name, label)
                                .clicked()
                            {
                                *text = p.name.clone();
                            }
                        }
                    });
            });
        });
    }

    /// Draw the dialog if it is open. `persist`: write the settings to the
    /// preferences when they are applied (not in unattended runs).
    pub fn show(&mut self, ctx: &egui::Context, radio: &Radio, prefs: &mut Prefs, persist: bool) {
        if !self.open {
            return;
        }
        let mut open = true;
        let mut action = None;
        egui::Window::new("CAT and PTT Config")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| action = self.body(ui, radio));
        let status = radio.status();
        if status.testing {
            // The lamp and the status are live while the key is down.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut apply = |this: &mut Self| {
            this.edit.rig.civ =
                u8::from_str_radix(this.civ.trim(), 16).unwrap_or(this.edit.rig.civ);
            this.civ = format!("{:02X}", this.edit.rig.civ);
            radio.apply(this.edit.clone());
            if persist {
                remember(&this.edit, prefs);
                prefs.save();
            }
        };
        match action {
            Some(Action::Apply) => apply(self),
            Some(Action::Ok) => {
                apply(self);
                open = false;
            }
            Some(Action::Cancel) => open = false,
            Some(Action::Test) => {
                // Test what the dialog shows, not what was in force before.
                self.edit.rig.civ =
                    u8::from_str_radix(self.civ.trim(), 16).unwrap_or(self.edit.rig.civ);
                if self.edit != radio.config() {
                    apply(self);
                }
                radio.test_ptt(!status.testing);
            }
            None => {}
        }
        if !open {
            // Closing the dialog lets go of a Test PTT.
            if status.testing {
                radio.test_ptt(false);
            }
            self.open = false;
        }
    }

    fn body(&mut self, ui: &mut Ui, radio: &Radio) -> Option<Action> {
        let mut action = None;
        let cat = self.edit.cat;
        ui.label(RichText::new("Rig control").strong());
        ui.checkbox(&mut self.edit.cat, "Enable CAT control").on_hover_text(
            "Find the rig on its port, ask it what it is and show its frequency and mode across the top \
             of the window. DecDSSS only reads them and keys the transmitter: set the frequency and \
             the mode (USB, or data) on the radio.",
        );
        ui.add_enabled_ui(cat, |ui| {
            egui::Grid::new("cat_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Rig model");
                let before = self.edit.rig.model;
                egui::ComboBox::from_id_salt("rig_model").width(260.0).selected_text(before.label()).show_ui(ui, |ui| {
                    for m in Model::ALL {
                        ui.selectable_value(&mut self.edit.rig.model, m, m.label());
                    }
                });
                let model = self.edit.rig.model;
                if model != before {
                    // The new model's usual rate; a port and an address are not
                    // interchangeable, so the box is emptied between the two.
                    self.edit.rig.baud = model.default_baud();
                    if model.serial() != before.serial() {
                        self.edit.rig.port.clear();
                    }
                }
                ui.end_row();
                ui.label(if model.serial() { "Serial device" } else { "rigctld address" });
                Self::device_box(ui, "rig_ports", &mut self.edit.rig.port, &self.ports, model.device_hint(), true);
                ui.end_row();
                if model.serial() {
                    ui.label("Serial rate");
                    egui::ComboBox::from_id_salt("rig_baud")
                        .selected_text(self.edit.rig.baud.to_string())
                        .show_ui(ui, |ui| {
                            for r in RATES {
                                ui.selectable_value(&mut self.edit.rig.baud, r, r.to_string());
                            }
                        })
                        .response
                        .on_hover_text("Tried first; the rig's other rates are tried if it does not answer");
                    ui.end_row();
                }
                if model == Model::Ic7300 {
                    ui.label("CI-V address");
                    ui.add(egui::TextEdit::singleline(&mut self.civ).desired_width(40.0).char_limit(2))
                        .on_hover_text("Hexadecimal; 94 as the IC-7300 comes");
                    ui.end_row();
                }
            });
            if self.edit.rig.model == Model::Rigctld {
                ui.label(
                    RichText::new("Start Hamlib's rigctld first, e.g.  rigctld -m 1036 -r COM7 -s 4800  (rigctl -l lists the models).")
                        .weak()
                        .size(11.0),
                );
            }
        });
        ui.add_space(4.0);
        ui.separator();
        ui.label(RichText::new("Push to talk").strong());
        egui::Grid::new("ptt_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("PTT uses");
            egui::ComboBox::from_id_salt("ptt_method").selected_text(self.edit.method.label()).show_ui(ui, |ui| {
                for m in Method::ALL {
                    // CAT only works with CAT control on.
                    ui.add_enabled_ui(m != Method::Cat || cat, |ui| {
                        ui.selectable_value(&mut self.edit.method, m, m.label());
                    });
                }
            });
            ui.end_row();
            let line = self.edit.method.line().is_some();
            ui.label("PTT serial device");
            Self::device_box(ui, "ptt_ports", &mut self.edit.ptt_port, &self.ports, "empty: the CAT port", line);
            ui.end_row();
            ui.label("Hold high");
            ui.horizontal(|ui| {
                let serial_cat = cat && self.edit.rig.model.serial();
                ui.add_enabled_ui(serial_cat, |ui| {
                    ui.checkbox(&mut self.edit.force_rts, "Force RTS").on_hover_text(
                        "Keep RTS high on the CAT port, for an interface that takes its power from the line",
                    );
                    ui.checkbox(&mut self.edit.force_dtr, "Force DTR");
                });
            });
            ui.end_row();
            ui.label("Key delay");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.edit.lead_ms).range(0..=MAX_DELAY_MS).suffix(" ms"))
                    .on_hover_text("From keying to the first sample of audio");
                ui.label("before, ");
                ui.add(egui::DragValue::new(&mut self.edit.tail_ms).range(0..=MAX_DELAY_MS).suffix(" ms"))
                    .on_hover_text("From the last sample of audio to letting go");
                ui.label("after");
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        ui.separator();
        // What the rig said, or why not, and whether the key is down.
        let s = radio.status();
        ui.horizontal_wrapped(|ui| {
            let (colour, word): (Color32, &str) = match (s.keyed, &s.lamp) {
                (true, _) => (col::BAD, "TX"),
                (_, Lamp::Off) => (Color32::GRAY, "off"),
                (_, Lamp::Wait) => (col::MARK, "…"),
                (_, Lamp::Ok) => (col::LOCK, "ready"),
                (_, Lamp::Bad) => (col::BAD, "fault"),
            };
            ui.label(RichText::new(format!("● {word}")).color(colour).strong());
            ui.label(RichText::new(&s.text).weak());
        });
        if let Some(note) = &s.test_note {
            ui.label(RichText::new(note).color(col::MARK));
        }
        if let Err(e) = self.edit.check() {
            ui.label(RichText::new(e).color(col::MARK));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let test = if s.testing {
                egui::Button::new(RichText::new("Test PTT — release").strong()).fill(col::BAD)
            } else {
                egui::Button::new("Test PTT")
            };
            if ui
                .add(test)
                .on_hover_text("Key the transmitter, with no audio, until you press again (it lets go by itself after 15 s)")
                .clicked()
            {
                action = Some(Action::Test);
            }
            ui.add_space(24.0);
            if ui.button("OK").clicked() {
                action = Some(Action::Ok);
            }
            if ui.button("Cancel").clicked() {
                action = Some(Action::Cancel);
            }
            if ui.add_enabled(self.edit != radio.config(), egui::Button::new("Apply")).clicked() {
                action = Some(Action::Apply);
            }
        });
        action
    }
}

#[derive(Clone, Copy)]
enum Action {
    Test,
    Ok,
    Cancel,
    Apply,
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_rig::Settings;

    #[test]
    fn settings_survive_the_preferences_file() {
        let config = Config {
            cat: true,
            rig: Settings {
                model: Model::Ic7300,
                port: "COM9".into(),
                baud: 19_200,
                civ: 0xA4,
            },
            method: Method::Dtr,
            ptt_port: "COM11".into(),
            force_dtr: true,
            force_rts: false,
            lead_ms: 150,
            tail_ms: 40,
        };
        let mut p = Prefs::default();
        remember(&config, &mut p);
        assert_eq!(restore(&p), config);
        assert_eq!(restore(&Prefs::default()), Config::default());
    }
}
