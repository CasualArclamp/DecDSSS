// made by CasualArclamp
//! The transmit window: a file in, audio out (to a sound card or a WAV).
//! DecDSSS never keys a transmitter; it plays audio and you do the rest.

use crate::engine::{Cmd, Engine, TxState};
use crate::prefs::Prefs;
use decdsss_audio::wav;
use decdsss_core::{Mode, Profile};
use decdsss_link::{SendSpec, container, segments};
use decdsss_phy::codes::normalise_call;
use decdsss_phy::framing::{DEPTHS, Layout, PayloadType};
use decdsss_phy::tx::{chips_to_audio, segment_chips};
use eframe::egui::{self, Context, RichText};
use std::path::{Path, PathBuf};

pub struct TxPanel {
    pub open: bool,
    file: Option<PathBuf>,
    call: String,
    profile: Profile,
    mode: String,
    depth: usize,
    resync: f64,
    repeat: usize,
    level: f32,
    rate: u32,
    device: usize,
    note: String,
}

impl Default for TxPanel {
    fn default() -> Self {
        TxPanel {
            open: false,
            file: None,
            call: String::new(),
            profile: Profile::W2400,
            mode: "ds8-1/2".into(),
            depth: 4,
            resync: 60.0,
            repeat: 1,
            level: 0.2,
            rate: 48_000,
            device: 0,
            note: String::new(),
        }
    }
}

struct Plan {
    audio: Vec<f32>,
    frames: usize,
    segments: usize,
    seconds: f64,
}

impl TxPanel {
    pub fn restore(&mut self, p: &Prefs) {
        if let Some(c) = p.get("call") {
            self.call = c.to_string();
        }
        if let Some(p) = p.get("tx_profile").and_then(Profile::from_name) {
            self.profile = p;
        }
        if let Some(m) = p.get("tx_mode")
            && Mode::from_name(m).is_some()
        {
            self.mode = m.to_string();
        }
        if let Some(d) = p.get("tx_depth").and_then(|d| d.parse().ok())
            && DEPTHS.contains(&d)
        {
            self.depth = d;
        }
    }

    pub fn remember(&self, p: &mut Prefs) {
        p.set("call", self.call.clone());
        p.set("tx_profile", self.profile.name());
        p.set("tx_mode", self.mode.clone());
        p.set("tx_depth", self.depth.to_string());
    }

    /// The rate the selected output device runs at.
    fn device_rate(&self, devices_out: &[(String, u32)]) -> u32 {
        if self.device == 0 {
            self.rate
        } else {
            devices_out.get(self.device - 1).map_or(self.rate, |d| d.1)
        }
    }

    fn build(&self, rate: u32) -> Result<Plan, String> {
        let path = self.file.as_ref().ok_or("choose a file")?;
        let call = normalise_call(&self.call)
            .ok_or("enter your callsign (letters, digits and /, up to 9)")?;
        let mode = Mode::from_name(&self.mode).ok_or("choose a mode")?;
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path
            .file_name()
            .map_or("file".to_string(), |n| n.to_string_lossy().into_owned());
        let spec = SendSpec {
            profile: self.profile,
            mode,
            depth: self.depth,
            call,
            session: (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                % 64) as u8,
            ptype: PayloadType::File,
            resync_s: self.resync,
        };
        let segs = segments(&spec, &container::pack(&name, &data))?;
        let mut chips = Vec::new();
        for _ in 0..self.repeat.max(1) {
            for s in &segs {
                chips.extend(segment_chips(s).ok_or("cannot build the header")?);
            }
        }
        let audio = chips_to_audio(&chips, self.profile, f64::from(rate), self.level, 1.0, 1.0);
        Ok(Plan {
            frames: segs.iter().map(|s| s.frames.len()).sum(),
            segments: segs.len(),
            seconds: audio.len() as f64 / f64::from(rate),
            audio,
        })
    }

    pub fn show(
        &mut self,
        ctx: &Context,
        engine: &Engine,
        state: &TxState,
        devices_out: &[(String, u32)],
        _out_dir: &Path,
    ) {
        let mut open = self.open;
        egui::Window::new("Transmit")
            .open(&mut open)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Plays audio to a sound card (or saves a WAV). You key the radio. \
                         Spread spectrum is restricted on HF in some countries: check your licence.",
                    )
                    .weak()
                    .size(11.0),
                );
                egui::Grid::new("txgrid").num_columns(2).show(ui, |ui| {
                    ui.label("File");
                    ui.horizontal(|ui| {
                        if ui.button("Choose…").clicked()
                            && let Some(p) = rfd::FileDialog::new().pick_file()
                        {
                            self.file = Some(p);
                        }
                        ui.label(
                            self.file
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map_or("none".into(), |n| n.to_string_lossy().into_owned()),
                        );
                    });
                    ui.end_row();
                    ui.label("Callsign");
                    ui.text_edit_singleline(&mut self.call);
                    ui.end_row();
                    ui.label("Bandwidth");
                    ui.horizontal(|ui| {
                        for p in Profile::ALL {
                            ui.selectable_value(&mut self.profile, p, p.label());
                        }
                    });
                    ui.end_row();
                    ui.label("Mode");
                    egui::ComboBox::from_id_salt("txmode")
                        .selected_text(&self.mode)
                        .show_ui(ui, |ui| {
                            for m in Mode::all() {
                                ui.selectable_value(
                                    &mut self.mode,
                                    m.name(),
                                    format!("{}   {:.0} bit/s, needs ≈ {:.0} dB SNR", m.name(), m.info_bit_rate(self.profile), m.snr_offset_db(self.profile) + 4.0),
                                );
                            }
                        });
                    ui.end_row();
                    ui.label("Depth");
                    ui.horizontal(|ui| {
                        for d in DEPTHS {
                            ui.selectable_value(&mut self.depth, d, d.to_string());
                        }
                    });
                    ui.end_row();
                    ui.label("Preamble every");
                    ui.add(egui::Slider::new(&mut self.resync, 15.0..=300.0).suffix(" s").logarithmic(true));
                    ui.end_row();
                    ui.label("Repeat");
                    ui.add(egui::Slider::new(&mut self.repeat, 1..=5));
                    ui.end_row();
                    ui.label("Level");
                    ui.add(egui::Slider::new(&mut self.level, 0.02..=0.3));
                    ui.end_row();
                    ui.label("Output");
                    egui::ComboBox::from_id_salt("txdev")
                        .selected_text(if self.device == 0 {
                            "System default".to_string()
                        } else {
                            devices_out
                                .get(self.device - 1)
                                .map(|d| d.0.clone())
                                .unwrap_or_default()
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.device, 0, "System default");
                            for (i, d) in devices_out.iter().enumerate() {
                                ui.selectable_value(&mut self.device, i + 1, format!("{} ({} Hz)", d.0, d.1));
                            }
                        });
                    ui.end_row();
                });
                if let Some(layout) = Mode::from_name(&self.mode).map(|m| Layout::new(self.profile, m, self.depth)) {
                    ui.label(
                        RichText::new(format!(
                            "{} data bytes a frame, {:.1} s a block",
                            layout.frame_bytes(),
                            layout.seconds()
                        ))
                        .weak()
                        .size(11.0),
                    );
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!state.active, |ui| {
                        if ui.button("▶ Play to the sound card").clicked() {
                            let rate = self.device_rate(devices_out);
                            match self.build(rate) {
                                Ok(plan) => {
                                    self.note = format!(
                                        "{} frames in {} segment(s), {:.0} s of audio",
                                        plan.frames, plan.segments, plan.seconds
                                    );
                                    engine.send(Cmd::Transmit {
                                        audio: plan.audio,
                                        rate,
                                        device: if self.device == 0 {
                                            None
                                        } else {
                                            devices_out.get(self.device - 1).map(|d| d.0.clone())
                                        },
                                    });
                                }
                                Err(e) => self.note = e,
                            }
                        }
                        if ui.button("Save WAV…").clicked() {
                            match self.build(self.rate) {
                                Ok(plan) => {
                                    if let Some(p) = rfd::FileDialog::new()
                                        .add_filter("WAV audio", &["wav"])
                                        .set_file_name("transmission.wav")
                                        .save_file()
                                    {
                                        self.note = match wav::write(&p, &plan.audio, self.rate) {
                                            Ok(_) => format!(
                                                "saved {} ({} frames, {:.0} s)",
                                                p.display(),
                                                plan.frames,
                                                plan.seconds
                                            ),
                                            Err(e) => e.to_string(),
                                        };
                                    }
                                }
                                Err(e) => self.note = e,
                            }
                        }
                    });
                });
                if state.active {
                    ui.add(
                        egui::ProgressBar::new((state.played / state.seconds.max(1.0)) as f32)
                            .text(format!("sending {:.0} / {:.0} s", state.played, state.seconds)),
                    );
                }
                if let Some(e) = &state.error {
                    ui.colored_label(crate::scope::col::BAD, e);
                }
                if !self.note.is_empty() {
                    ui.label(RichText::new(&self.note).size(11.0));
                }
            });
        self.open = open;
    }
}
