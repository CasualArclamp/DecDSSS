// made by CasualArclamp
//! The application window.

use crate::engine::{
    Cmd, Config, Engine, PictureView, Shared, Source, SpectrumView, Transfer, VoiceView,
};
use crate::prefs::Prefs;
use crate::scope::{self, HeatTexture, col};
use crate::tx::TxPanel;
use decdsss_audio::{list_input_devices, list_output_devices};
use decdsss_core::{Family, Profile};
use decdsss_phy::rx::{AttemptView, ChannelView, Snapshot};
use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, pos2, vec2,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Device,
    Wav,
    Demo,
}

/// What the window shows: the receiver, the transmitter, or both side by side
/// (the transmit controls on the left, as in FreeDV).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Rx,
    Tx,
    Both,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Rx => "rx",
            View::Tx => "tx",
            View::Both => "both",
        }
    }

    fn from_name(s: &str) -> Option<View> {
        match s {
            "rx" => Some(View::Rx),
            "tx" => Some(View::Tx),
            "both" => Some(View::Both),
            _ => None,
        }
    }
}

/// Unattended runs for documentation and smoke tests.
#[derive(Default, Clone)]
pub struct Automation {
    pub after: Option<f64>,
    pub screenshot: Option<PathBuf>,
    pub open_tx: bool,
    /// Open the transmit window with this file chosen.
    pub tx_file: Option<PathBuf>,
    /// Record speech in the transmit window for this many seconds from the
    /// input named `mic` (part of its name), starting at launch.
    pub record_for: Option<f64>,
    pub mic: Option<String>,
    /// Start on this view: rx, tx or both.
    pub view: Option<String>,
    /// Switch the RF monitor on at launch, on the output whose name contains this.
    pub monitor: Option<String>,
    /// Play the transmit window's transmission on this output one second in, and
    /// stop it this many seconds in (for checking the Stop button).
    pub tx_play: Option<String>,
    pub tx_stop: Option<f64>,
    /// Listen on the output whose name contains this, and (with `listen_test`) send
    /// a tone to it one second in, through the live speech player.
    pub listen: Option<String>,
    pub listen_test: bool,
    /// Hold the PTT this long from the input `mic` (half a second in), then let go:
    /// what was said is sent on the output named by `tx_play`.
    pub ptt_for: Option<f64>,
    /// Like `ptt_for`, but live: stream from the microphone for this long.
    pub live_for: Option<f64>,
    pub requested_at: Option<Instant>,
}

pub struct App {
    engine: Engine,
    source: SourceKind,
    wav: Option<PathBuf>,
    devices_in: Vec<String>,
    /// Output devices with the rate each runs at.
    devices_out: Vec<(String, u32)>,
    device_in: usize,
    details: bool,
    prefs: Prefs,
    profiles: [bool; 3],
    speed: f64,
    out_dir: PathBuf,
    log_open: bool,
    started: Instant,
    pub auto: Automation,
    waterfall: Option<crate::waterfall::Waterfall>,
    heat: [HeatTexture; 3],
    tx: TxPanel,
    view: View,
    /// The RF monitor: on or off, how loud, and on which output (0 the default).
    monitor_on: bool,
    monitor_volume: f32,
    monitor_device: usize,
    /// Why the monitor stopped, shown until it is switched on again.
    monitor_msg: String,
    /// Play a received speech transmission when it is whole.
    auto_play_rx: bool,
    /// The speech transmissions already played that way.
    played: std::collections::BTreeSet<(String, u8)>,
    /// Smoothed frames a second, for the top bar.
    fps: f32,
    /// Frames a second from the time between frames (updated in `logic`).
    measured_fps: f32,
    /// When the last frame began, for the 60 fps limit.
    last_frame: Instant,
    /// Frame counting for `DECDSSS_FPS`.
    frames: u32,
    fps_since: Instant,
    /// `--record-for` has started its recording.
    recording_started: bool,
    /// `--tx-play` has started its transmission.
    tx_play_started: bool,
    /// `--ptt-for` has let go.
    ptt_released: bool,
    /// `--listen-test` has sent its tone.
    listen_tested: bool,
    /// `--live-for` has keyed up, and let go.
    live_started: bool,
    live_stopped: bool,
    /// The last view of each station, kept after its segment ends so the card
    /// does not flicker away between segments: (profile, call, session).
    stations: std::collections::BTreeMap<(usize, String, u8), (ChannelView, Instant)>,
    /// The texture of each picture on show and the version it was made from.
    pictures: std::collections::BTreeMap<(String, u8), (u64, TextureHandle)>,
}

fn default_out_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join("Documents").join("DecDSSS"))
        .unwrap_or_else(|| PathBuf::from("received"))
}

impl App {
    pub fn new(
        auto: Automation,
        source: SourceKind,
        wav: Option<PathBuf>,
        speed: f64,
        out_dir: Option<PathBuf>,
        profiles: Option<[bool; 3]>,
        initial_device: Option<String>,
    ) -> App {
        let devices_in: Vec<String> = list_input_devices()
            .map(|v| v.into_iter().map(|d| d.name).collect())
            .unwrap_or_default();
        let devices_out: Vec<(String, u32)> = list_output_devices()
            .map(|v| {
                v.into_iter()
                    .map(|d| (d.name, d.default_format.map_or(48_000, |f| f.sample_rate)))
                    .collect()
            })
            .unwrap_or_default();
        let prefs = if auto.screenshot.is_some() {
            // Unattended runs neither take nor overwrite what the user last had.
            Prefs::default()
        } else {
            Prefs::load()
        };
        let mut tx = TxPanel::default();
        tx.restore(&prefs);
        let mut view = prefs
            .get("view")
            .and_then(View::from_name)
            .unwrap_or(View::Rx);
        if auto.open_tx
            || auto.tx_file.is_some()
            || auto.record_for.is_some()
            || auto.ptt_for.is_some()
            || auto.live_for.is_some()
        {
            view = View::Tx;
        }
        if let Some(v) = auto.view.as_deref().and_then(View::from_name) {
            view = v;
        }
        if let Some(p) = auto.tx_file.clone() {
            tx.preload(p);
        }
        let out_dir = out_dir.or_else(|| prefs.get("out_dir").map(PathBuf::from));
        let device_in = initial_device
            .as_deref()
            .or_else(|| prefs.get("device_in"))
            .and_then(|n| {
                devices_in
                    .iter()
                    .position(|d| d.to_lowercase().contains(&n.to_lowercase()))
            })
            .map_or(0, |i| i + 1);
        let monitor_volume = prefs
            .get("monitor_volume")
            .and_then(|v| v.parse().ok())
            .map_or(1.0, |v: f32| v.clamp(0.0, 4.0));
        let mut app = App {
            engine: Engine::spawn(),
            source,
            wav,
            devices_in,
            devices_out,
            device_in,
            details: false,
            prefs,
            profiles: profiles.unwrap_or([true, true, true]),
            speed,
            out_dir: out_dir.unwrap_or_else(default_out_dir),
            log_open: true,
            started: Instant::now(),
            auto,
            waterfall: None,
            heat: Default::default(),
            tx,
            view,
            monitor_on: false,
            monitor_volume,
            monitor_device: 0,
            monitor_msg: String::new(),
            auto_play_rx: true,
            played: Default::default(),
            fps: 0.0,
            measured_fps: 0.0,
            last_frame: Instant::now(),
            frames: 0,
            fps_since: Instant::now(),
            recording_started: false,
            tx_play_started: false,
            ptt_released: false,
            listen_tested: false,
            live_started: false,
            live_stopped: false,
            stations: Default::default(),
            pictures: Default::default(),
        };
        if let Some(hint) = app.auto.listen.clone() {
            app.monitor_device = app
                .devices_out
                .iter()
                .position(|d| d.0.to_lowercase().contains(&hint.to_lowercase()))
                .map_or(0, |i| i + 1);
            app.engine.send(Cmd::Listen {
                device: app.listen().0,
            });
        }
        if let Some(hint) = app.auto.monitor.clone() {
            app.monitor_on = true;
            app.monitor_device = app
                .devices_out
                .iter()
                .position(|d| d.0.to_lowercase().contains(&hint.to_lowercase()))
                .map_or(0, |i| i + 1);
            app.engine.send(Cmd::Monitor {
                on: true,
                device: (app.monitor_device > 0)
                    .then(|| app.devices_out[app.monitor_device - 1].0.clone()),
                volume: app.monitor_volume,
            });
        }
        app
    }

    pub fn start(&mut self) {
        let src = match self.source {
            SourceKind::Device => Source::Device(if self.device_in == 0 {
                None
            } else {
                self.devices_in.get(self.device_in - 1).cloned()
            }),
            SourceKind::Wav => match &self.wav {
                Some(p) => Source::Wav(p.clone()),
                None => return,
            },
            SourceKind::Demo => Source::Demo,
        };
        let profiles: Vec<Profile> = Profile::ALL
            .iter()
            .zip(self.profiles)
            .filter(|(_, on)| *on)
            .map(|(p, _)| *p)
            .collect();
        self.engine.send(Cmd::Start(Config {
            source: src,
            profiles,
            out_dir: self.out_dir.clone(),
            speed: self.speed,
            details: self.details,
        }));
        // Remember the choices.
        if self.auto.screenshot.is_none() {
            self.prefs
                .set("out_dir", self.out_dir.display().to_string());
            if let Some(n) = self.devices_in.get(self.device_in.wrapping_sub(1)) {
                self.prefs.set("device_in", n.clone());
            }
            self.tx.remember(&mut self.prefs);
            self.prefs.save();
        }
    }

    fn top_bar(&mut self, ui: &mut Ui, sh: &Shared) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("DecDSSS").strong().size(16.0));
            // Tabs: the receiver, the transmitter, or both on one page.
            let before = self.view;
            for (v, label) in [(View::Rx, "RX"), (View::Tx, "TX"), (View::Both, "RX + TX")] {
                ui.selectable_value(&mut self.view, v, label);
            }
            if self.view != before && self.auto.screenshot.is_none() {
                self.prefs.set("view", self.view.name());
                self.prefs.save();
            }
            ui.separator();
            ui.add_enabled_ui(!sh.running, |ui| {
                egui::ComboBox::from_id_salt("source")
                    .selected_text(match self.source {
                        SourceKind::Device => "Sound card",
                        SourceKind::Wav => "WAV file",
                        SourceKind::Demo => "Demo signal",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.source, SourceKind::Device, "Sound card");
                        ui.selectable_value(&mut self.source, SourceKind::Wav, "WAV file");
                        ui.selectable_value(&mut self.source, SourceKind::Demo, "Demo signal");
                    });
                match self.source {
                    SourceKind::Device => {
                        let name = if self.device_in == 0 {
                            "System default".to_string()
                        } else {
                            self.devices_in
                                .get(self.device_in - 1)
                                .cloned()
                                .unwrap_or_default()
                        };
                        egui::ComboBox::from_id_salt("device")
                            .width(220.0)
                            .selected_text(name)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.device_in, 0, "System default");
                                for (i, d) in self.devices_in.iter().enumerate() {
                                    ui.selectable_value(&mut self.device_in, i + 1, d);
                                }
                            });
                    }
                    SourceKind::Wav => {
                        if ui.button("Open WAV…").clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .add_filter("WAV audio", &["wav"])
                                .pick_file()
                        {
                            self.wav = Some(p);
                        }
                        if let Some(p) = &self.wav {
                            ui.label(
                                p.file_name()
                                    .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
                            );
                        }
                        ui.add(
                            egui::Slider::new(&mut self.speed, 1.0..=32.0)
                                .logarithmic(true)
                                .text("× speed"),
                        );
                    }
                    SourceKind::Demo => {
                        ui.add(
                            egui::Slider::new(&mut self.speed, 1.0..=32.0)
                                .logarithmic(true)
                                .text("× speed"),
                        );
                    }
                }
            });
            ui.separator();
            ui.add_enabled_ui(!sh.running, |ui| {
                for (i, p) in Profile::ALL.iter().enumerate() {
                    ui.checkbox(&mut self.profiles[i], p.label());
                }
            });
            ui.separator();
            if sh.running {
                if ui.button("■ Stop").clicked() {
                    self.engine.send(Cmd::Stop);
                }
            } else if ui.button("▶ Start").clicked() {
                self.start();
            }
            // Input level.
            let (rect, _) = ui.allocate_exact_size(vec2(90.0, 12.0), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
            let w = (sh.level.clamp(0.0, 1.0)).sqrt() * rect.width();
            p.rect_filled(
                egui::Rect::from_min_size(rect.min, vec2(w, rect.height())),
                2.0,
                if sh.level > 0.95 { col::BAD } else { col::LOCK },
            );
            ui.label(format!("{:.0} s", sh.seconds));
            // The measured frame rate of the window (the waterfall scrolls a row a frame).
            self.fps += 0.1 * (self.measured_fps - self.fps);
            ui.label(RichText::new(format!("{:.0} fps", self.fps)).weak());
            // Whatever tab is showing, a transmission in progress can be stopped.
            if (sh.tx.active || self.tx.live_active())
                && ui
                    .add(egui::Button::new(RichText::new("■ STOP").strong()).fill(col::BAD))
                    .on_hover_text(
                        "Stop the transmission (or the playback) now; if you key the radio by hand, release it too",
                    )
                    .clicked()
            {
                self.tx.abort_live();
                self.engine.send(Cmd::StopTransmit);
            }
            ui.separator();
            self.monitor_controls(ui, sh);
            ui.separator();
            ui.label(RichText::new(&sh.status).weak());
        });
    }

    /// The RF monitor: a button to hear what the receiver hears, its volume and
    /// the output it plays on (not the one that feeds the radio).
    fn monitor_controls(&mut self, ui: &mut Ui, sh: &Shared) {
        let before = (self.monitor_on, self.monitor_volume, self.monitor_device);
        ui.toggle_value(&mut self.monitor_on, "🔊 RF monitor")
            .on_hover_text(
                "Play the audio the receiver is hearing on an output device, to listen to the channel. \
                 Choose speakers or headphones, not the output that feeds the radio.",
            );
        if self.monitor_on {
            ui.add(
                egui::Slider::new(&mut self.monitor_volume, 0.0..=4.0)
                    .show_value(false)
                    .text("vol"),
            );
        }
        // The speakers or headphones: the monitor, received speech and the speech
        // previews all go here, never to the output that feeds the radio.
        let name = if self.monitor_device == 0 {
            "System default".to_string()
        } else {
            self.devices_out
                .get(self.monitor_device - 1)
                .map(|d| d.0.clone())
                .unwrap_or_default()
        };
        egui::ComboBox::from_id_salt("monitor_device")
            .width(150.0)
            .selected_text(format!("🔈 {name}"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.monitor_device, 0, "System default");
                for (i, d) in self.devices_out.iter().enumerate() {
                    ui.selectable_value(
                        &mut self.monitor_device,
                        i + 1,
                        format!("{} ({} Hz)", d.0, d.1),
                    );
                }
            })
            .response
            .on_hover_text(
                "Where you listen: the RF monitor, received speech and speech previews play \
                 here (speakers or headphones), never on the output that feeds the radio",
            );
        ui.checkbox(&mut self.auto_play_rx, "auto-play speech")
            .on_hover_text("Play a received speech transmission when it is whole");
        if let Some(e) = &sh.monitor_error {
            self.monitor_on = false;
            self.monitor_msg = e.clone();
        }
        if self.monitor_on && !before.0 {
            self.monitor_msg.clear();
        }
        if !self.monitor_msg.is_empty() {
            ui.colored_label(col::BAD, &self.monitor_msg);
        }
        if before != (self.monitor_on, self.monitor_volume, self.monitor_device) {
            self.engine.send(Cmd::Monitor {
                on: self.monitor_on,
                device: (self.monitor_device > 0)
                    .then(|| {
                        self.devices_out
                            .get(self.monitor_device - 1)
                            .map(|d| d.0.clone())
                    })
                    .flatten(),
                volume: self.monitor_volume,
            });
            self.engine.send(Cmd::Listen {
                device: self.listen().0,
            });
            if self.auto.screenshot.is_none() {
                self.prefs
                    .set("monitor_volume", format!("{:.2}", self.monitor_volume));
                self.prefs.save();
            }
        }
    }

    fn band_view(&mut self, ui: &mut Ui, sh: &Shared) {
        let Some(view) = &sh.spectrum else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Press Start. The demo signal needs no radio.").weak());
            });
            return;
        };
        let avail = ui.available_size();
        let psd_h = (avail.y * 0.30).clamp(110.0, 220.0);
        self.psd_panel(ui, view, vec2(avail.x, psd_h), &sh.snapshots);
        ui.add_space(4.0);
        let wf_h = (ui.available_height() - 6.0).max(80.0);
        self.waterfall_panel(ui, view, vec2(avail.x, wf_h));
    }

    fn psd_panel(
        &mut self,
        ui: &mut Ui,
        view: &SpectrumView,
        size: egui::Vec2,
        snaps: &[Snapshot],
    ) {
        let reading = format!("noise floor {:.0} dB", view.floor_db);
        let (painter, area) = scope::panel(ui, size, "Audio spectrum 0–4 kHz", &reading);
        let plot = egui::Rect::from_min_max(area.min + vec2(24.0, 0.0), area.max - vec2(0.0, 14.0));
        let top_hz = view.psd.len() as f64 * view.bin_hz;
        let x = |hz: f64| plot.left() + (hz / top_hz) as f32 * plot.width();
        let lo = view.floor_db - 8.0;
        let hi = (view.psd.iter().copied().fold(f32::MIN, f32::max) + 6.0).max(lo + 40.0);
        let y = |db: f32| plot.bottom() - ((db - lo) / (hi - lo)).clamp(0.0, 1.0) * plot.height();
        // The profile bands.
        for (p, c) in [
            (
                Profile::W3000,
                Color32::from_rgba_unmultiplied(86, 182, 236, 22),
            ),
            (
                Profile::W2400,
                Color32::from_rgba_unmultiplied(98, 200, 140, 26),
            ),
            (
                Profile::N500,
                Color32::from_rgba_unmultiplied(232, 176, 72, 40),
            ),
        ] {
            let (a, b) = p.edges_hz();
            painter.rect_filled(
                egui::Rect::from_min_max(pos2(x(a), plot.top()), pos2(x(b), plot.bottom())),
                0.0,
                c,
            );
            painter.text(
                pos2(
                    x(a) + 3.0,
                    plot.top()
                        + 2.0
                        + 11.0
                            * (2 - Profile::ALL.iter().position(|q| *q == p).unwrap_or(0)) as f32,
                ),
                Align2::LEFT_TOP,
                p.label(),
                FontId::proportional(9.5),
                col::REF,
            );
        }
        for hz in (0..=4000).step_by(500) {
            let xx = x(f64::from(hz));
            painter.line_segment(
                [pos2(xx, plot.top()), pos2(xx, plot.bottom())],
                (1.0, col::GRID),
            );
            painter.text(
                pos2(xx, plot.bottom() + 2.0),
                Align2::CENTER_TOP,
                hz.to_string(),
                FontId::proportional(9.5),
                col::AXIS,
            );
        }
        // Noise floor.
        let fy = y(view.floor_db);
        painter.line_segment(
            [pos2(plot.left(), fy), pos2(plot.right(), fy)],
            Stroke::new(1.0, col::MARK.gamma_multiply(0.6)),
        );
        let pts: Vec<_> = view
            .psd
            .iter()
            .enumerate()
            .map(|(i, &d)| pos2(x(i as f64 * view.bin_hz), y(d)))
            .collect();
        for w in pts.windows(2) {
            painter.line_segment([w[0], w[1]], Stroke::new(1.2, col::TRACE));
        }
        // Carriers the receivers have locked onto.
        let mut label_row = 0;
        for s in snaps {
            for c in &s.channels {
                let (a, b) = s.profile.edges_hz();
                let mid = 0.5 * (a + b) + c.carrier_hz;
                painter.line_segment(
                    [pos2(x(mid), plot.top()), pos2(x(mid), plot.bottom())],
                    Stroke::new(1.2, col::LOCK),
                );
                painter.text(
                    pos2(x(mid) + 3.0, plot.bottom() - 12.0 - 11.0 * label_row as f32),
                    Align2::LEFT_TOP,
                    &c.header.call,
                    FontId::proportional(10.0),
                    col::LOCK,
                );
                label_row += 1;
            }
        }
        painter.text(
            pos2(plot.left() - 4.0, fy),
            Align2::RIGHT_CENTER,
            "floor",
            FontId::proportional(8.5),
            col::MARK,
        );
    }

    fn waterfall_panel(&mut self, ui: &mut Ui, view: &SpectrumView, size: egui::Vec2) {
        let (painter, area) = scope::panel(ui, size, "Waterfall", "newest at the top");
        let plot = egui::Rect::from_min_max(area.min + vec2(24.0, 0.0), area.max);
        if let Some(wf) = &self.waterfall {
            wf.paint(&painter, plot);
        }
        let top_hz = view.psd.len() as f64 * view.bin_hz;
        for p in Profile::ALL {
            let (a, b) = p.edges_hz();
            for hz in [a, b] {
                let xx = plot.left() + (hz / top_hz) as f32 * plot.width();
                painter.line_segment(
                    [pos2(xx, plot.top()), pos2(xx, plot.bottom())],
                    Stroke::new(1.0, Color32::from_white_alpha(40)),
                );
            }
        }
    }

    /// Writes the spectrum rows the engine has made into the waterfall's ring.
    fn feed_waterfall(&mut self, ctx: &egui::Context, rows: &[(f32, Vec<f32>)]) {
        let Some((_, first)) = rows.first() else {
            return;
        };
        let width = first.len();
        if self.waterfall.as_ref().is_none_or(|w| w.width() != width) {
            self.waterfall = Some(crate::waterfall::Waterfall::new(ctx, width));
        }
        if let Some(w) = &mut self.waterfall {
            w.push(rows);
        }
    }

    fn profile_section(
        &mut self,
        ui: &mut Ui,
        snap: &Snapshot,
        index: usize,
        transfers: &[Transfer],
    ) {
        let n = snap.channels.len();
        let title = format!(
            "{}  ·  {}",
            snap.profile.label(),
            if n == 0 {
                "searching".to_string()
            } else {
                format!("{n} station{}", if n == 1 { "" } else { "s" })
            }
        );
        egui::CollapsingHeader::new(RichText::new(title).strong())
            .id_salt(("profile", index))
            .default_open(true)
            .show(ui, |ui| {
                let w = ui.available_width();
                if let Some(s) = &snap.search {
                    let ctx = ui.ctx().clone();
                    self.heat[index].show(ui, &ctx, s, 40.0, vec2(w, 118.0));
                }
                for a in &snap.attempts {
                    attempt_card(ui, a, w);
                }
                // Open channels, live; stations whose segment has just ended, dimmed.
                for c in &snap.channels {
                    self.stations.insert(
                        (index, c.header.call.clone(), c.header.session),
                        (c.clone(), Instant::now()),
                    );
                }
                let live: Vec<(String, u8)> = snap
                    .channels
                    .iter()
                    .map(|c| (c.header.call.clone(), c.header.session))
                    .collect();
                self.stations
                    .retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(150));
                let keep: Vec<(ChannelView, bool)> = self
                    .stations
                    .iter()
                    .filter(|((i, _, _), _)| *i == index)
                    .map(|((_, call, session), (v, _))| {
                        (v.clone(), live.contains(&(call.clone(), *session)))
                    })
                    .collect();
                for (c, is_live) in &keep {
                    let tr = transfers
                        .iter()
                        .find(|t| t.call == c.header.call && t.session == c.header.session);
                    channel_card(ui, c, tr, w, *is_live);
                }
            });
    }

    /// Pictures received or arriving, newest first, each as far as it has come.
    fn pictures_panel(
        &mut self,
        ui: &mut Ui,
        pictures: &[Arc<PictureView>],
        voices: &[Arc<VoiceView>],
    ) {
        self.pictures
            .retain(|k, _| pictures.iter().any(|p| (p.call.clone(), p.session) == *k));
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for p in pictures.iter().rev() {
                    self.picture_card(ui, p);
                }
                for v in voices.iter().rev() {
                    if crate::voice::voice_card(ui, v) {
                        self.play_speech(&v.speech);
                    }
                }
            });
        });
    }

    /// The output device for listening (the monitor's, speakers or headphones) and
    /// the rate it runs at; never the output that feeds the radio.
    fn listen(&self) -> (Option<String>, u32) {
        match self
            .monitor_device
            .checked_sub(1)
            .and_then(|i| self.devices_out.get(i))
        {
            Some((name, rate)) => (Some(name.clone()), *rate),
            None => (None, 48_000),
        }
    }

    /// Plays received speech on the listening output.
    fn play_speech(&mut self, speech: &[i16]) {
        let (device, rate) = self.listen();
        self.engine.send(Cmd::Transmit {
            audio: crate::voice::speech_to_audio(speech, rate),
            rate,
            device,
        });
    }

    fn picture_card(&mut self, ui: &mut Ui, p: &PictureView) {
        let key = (p.call.clone(), p.session);
        let tex = match self.pictures.get(&key) {
            Some((v, t)) if *v == p.version => t.clone(),
            _ => {
                let img = ColorImage::from_rgba_unmultiplied(
                    [p.width as usize, p.height as usize],
                    &p.rgba,
                );
                let t = ui.ctx().load_texture(
                    format!("picture-{}-{}", p.call, p.session),
                    img,
                    TextureOptions::LINEAR,
                );
                self.pictures.insert(key, (p.version, t.clone()));
                t
            }
        };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&p.call).strong().color(col::LOCK));
                    ui.label(RichText::new(&p.name).weak());
                });
                // Pictures are small: show them larger, in a box of a fixed height
                // so cards line up.
                let (bw, bh) = (300.0f32, 200.0f32);
                let scale = (bw / p.width as f32).min(bh / p.height as f32).min(8.0);
                let size = vec2(p.width as f32 * scale, p.height as f32 * scale);
                let (rect, _) = ui.allocate_exact_size(vec2(bw, bh), Sense::hover());
                ui.painter().rect_filled(rect, 2.0, Color32::from_gray(24));
                let image_rect = egui::Rect::from_center_size(rect.center(), size);
                ui.painter().image(
                    tex.id(),
                    image_rect,
                    egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                let frac = if p.total > 0 {
                    p.bytes as f32 / p.total as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(if p.complete { 1.0 } else { frac })
                        .desired_width(bw)
                        .text(if p.complete {
                            format!("complete  ·  {}×{}  ·  {} B", p.width, p.height, p.total)
                        } else {
                            format!(
                                "{} of {} B  ·  {} scan{}  ·  {}×{}",
                                p.bytes,
                                p.total,
                                p.scans,
                                if p.scans == 1 { "" } else { "s" },
                                p.width,
                                p.height
                            )
                        }),
                );
            });
        });
    }

    fn files_and_log(&mut self, ui: &mut Ui, sh: &Shared) {
        if !sh.files.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Received:").strong());
                for f in &sh.files {
                    if ui
                        .link(format!(
                            "{} ({} B, {})",
                            f.file.name,
                            f.file.data.len(),
                            f.file.call
                        ))
                        .on_hover_text(f.path.display().to_string())
                        .clicked()
                    {
                        let _ = open_path(&f.path);
                    }
                }
            });
        }
        for t in &sh.transfers {
            if !t.done && t.total > 0 {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} session {}: {}/{}",
                        t.call, t.session, t.have, t.total
                    ));
                    ui.add(
                        egui::ProgressBar::new(t.have as f32 / t.total as f32).desired_width(160.0),
                    );
                });
            }
        }
        ui.checkbox(
            &mut self.details,
            "Show the search in the log (applies at the next Start)",
        );
        egui::CollapsingHeader::new("Log")
            .default_open(self.log_open)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(130.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &sh.log {
                            ui.label(RichText::new(line).monospace().size(11.0));
                        }
                    });
            });
    }

    /// Screenshots and timed exits for unattended runs.
    fn automate(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if let Some(secs) = self.auto.record_for {
            let t = now.duration_since(self.started).as_secs_f64();
            if t >= 0.5 && !self.recording_started {
                self.recording_started = true;
                let hint = self.auto.mic.clone().unwrap_or_default();
                self.tx.auto_record(&self.devices_in, &hint);
            }
            if t >= secs {
                self.auto.record_for = None;
                self.tx.auto_stop();
            }
        }
        let t = now.duration_since(self.started).as_secs_f64();
        if self.auto.listen_test && t >= 1.0 && !self.listen_tested {
            self.listen_tested = true;
            // Three seconds of a 440 Hz tone, in chunks of a second.
            let tone: Vec<i16> = (0..24_000)
                .map(|i| {
                    (6000.0 * (std::f64::consts::TAU * 440.0 * f64::from(i) / 8000.0).sin()) as i16
                })
                .collect();
            for (k, c) in tone.chunks(8000).enumerate() {
                self.engine.send(Cmd::Speech {
                    pcm: c.to_vec(),
                    last: k == 2,
                });
            }
        }
        if let Some(hold) = self.auto.live_for {
            let (devices_out, devices_in) = (self.devices_out.clone(), self.devices_in.clone());
            if t >= 0.5 && !self.live_started {
                self.live_started = true;
                let mic = self.auto.mic.clone().unwrap_or_default();
                let out = self.auto.tx_play.clone().unwrap_or_default();
                self.tx
                    .auto_live_start(&devices_out, &devices_in, &mic, &out);
            }
            if t >= 0.5 + hold && !self.live_stopped {
                self.live_stopped = true;
                self.tx.auto_live_stop();
            }
        }
        if let Some(hold) = self.auto.ptt_for {
            if t >= 0.5 && !self.recording_started {
                self.recording_started = true;
                let hint = self.auto.mic.clone().unwrap_or_default();
                self.tx.auto_record(&self.devices_in, &hint);
            }
            if t >= 0.5 + hold && !self.ptt_released {
                self.ptt_released = true;
                let devices_out = self.devices_out.clone();
                let out_hint = self.auto.tx_play.clone().unwrap_or_default();
                self.tx.auto_release(&self.engine, &devices_out, &out_hint);
            }
        }
        if let Some(hint) = self.auto.tx_play.clone()
            && self.auto.ptt_for.is_none()
            && t >= 1.0
            && !self.tx_play_started
        {
            self.tx_play_started = true;
            let devices_out = self.devices_out.clone();
            self.tx.auto_play(&self.engine, &devices_out, &hint);
        }
        if self.auto.tx_stop.is_some_and(|s| t >= s) {
            self.auto.tx_stop = None;
            self.engine.send(Cmd::StopTransmit);
        }
        let Some(after) = self.auto.after else { return };
        if now.duration_since(self.started).as_secs_f64() < after {
            return;
        }
        if let Some(path) = self.auto.screenshot.clone() {
            let image = ctx.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = image {
                let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
                let (w, h) = (image.size[0] as u32, image.size[1] as u32);
                match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                    Ok(()) => eprintln!("screenshot saved to {}", path.display()),
                    Err(e) => eprintln!("cannot save screenshot {}: {e}", path.display()),
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            match self.auto.requested_at {
                None => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                        egui::UserData::default(),
                    ));
                    self.auto.requested_at = Some(now);
                }
                Some(t) if now.duration_since(t) > Duration::from_secs(3) => {
                    eprintln!("screenshot not delivered; quitting");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The right-hand panel: every station being received, with its instruments.
    fn stations_panel(&mut self, ui: &mut Ui, sh: &Shared) {
        egui::Panel::right("stations")
            .resizable(true)
            .default_size(560.0)
            .min_size(380.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if sh.snapshots.is_empty() {
                        ui.label(RichText::new("Nothing is being received.").weak());
                    }
                    let snaps = sh.snapshots.clone();
                    for (i, s) in snaps.iter().enumerate() {
                        let idx = Profile::ALL
                            .iter()
                            .position(|p| *p == s.profile)
                            .unwrap_or(i);
                        self.profile_section(ui, s, idx, &sh.transfers);
                    }
                });
            });
    }
}

fn open_path(p: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(p)
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(p)
            .spawn()
            .map(|_| ())
    }
}

fn attempt_card(ui: &mut Ui, a: &AttemptView, w: f32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(col::MARK));
        ui.label(format!(
            "candidate {:+.1} Hz: {}",
            a.freq_hz,
            if a.header_stage {
                "preamble ended, decoding the header"
            } else {
                "following the preamble"
            }
        ));
    });
    if !a.periods.is_empty() {
        scope::preamble_bars(ui, &a.periods, vec2(w, 54.0));
    }
    // The header is a stretch of its own, 2.9 to 8.5 s on 2k4: its symbols as
    // they come in, in the alphabet being tried (the shortest first, a longer one
    // if that does not decode), laid out like a CSK station's card.
    if let Some(h) = a.header.as_ref().filter(|_| a.header_stage) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let gap = ui.spacing().item_spacing.x;
            let side = ((w - 2.0 * gap - 24.0) / 3.0).clamp(90.0, 170.0);
            ui.horizontal(|ui| {
                scope::constellation(
                    ui,
                    "Symbol × previous",
                    &h.points,
                    &[(1.0, 0.0)],
                    side,
                    col::TRACE,
                );
                scope::bank(ui, &h.bank, side);
                scope::epl(ui, h.epl, side);
            });
            scope::header_symbols(
                ui,
                h,
                a.header_since_s,
                a.header_needs_s,
                vec2(w - 24.0, 72.0),
            );
            scope::rake(ui, &h.profile, &h.fingers, vec2(w - 24.0, 80.0));
        });
    }
}

fn channel_card(ui: &mut Ui, c: &ChannelView, tr: Option<&Transfer>, w: f32, is_live: bool) {
    let h = &c.header;
    let live = &c.live;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&h.call).strong().size(15.0).color(col::LOCK));
            ui.label(RichText::new(format!("{}  depth {}", h.mode, h.depth)).strong());
            ui.label(format!(
                "session {}  ·  block {}/{}  ·  symbol {}/{}  ·  carrier {:+.2} Hz",
                h.session,
                c.blocks_done + 1,
                h.nblocks,
                live.symbol,
                live.symbols_in_block,
                c.carrier_hz
            ));
            if !is_live {
                ui.label(RichText::new("between segments").italics());
            }
        });
        let gap = ui.spacing().item_spacing.x;
        let side = ((w - 2.0 * gap - 24.0) / 3.0).clamp(90.0, 170.0);
        ui.horizontal(|ui| match h.mode.family {
            Family::DsPsk => {
                scope::constellation(
                    ui,
                    "Despread symbols",
                    &live.symbols,
                    &scope::QPSK,
                    side,
                    col::TRACE,
                );
                scope::constellation(
                    ui,
                    "Symbol × previous",
                    &live.diffs,
                    &scope::QUADRANTS,
                    side,
                    col::LOCK,
                );
                scope::epl(ui, live.epl, side);
            }
            Family::Csk => {
                scope::bank(ui, &live.bank, side);
                scope::bank_bars(ui, live.bank.last(), side);
                scope::epl(ui, live.epl, side);
            }
        });
        ui.horizontal(|ui| {
            scope::rake(
                ui,
                &live.profile,
                &live.fingers,
                vec2(2.0 * side + gap, 96.0),
            );
            scope::chip_scope(ui, &live.raw_chips, &live.despread_chips, vec2(side, 96.0));
        });
        let snr = live.snr_db.last().copied().unwrap_or(f32::NAN);
        let f = live.freq_hz.last().copied().unwrap_or(f32::NAN);
        ui.horizontal(|ui| {
            let half = (w - gap - 24.0) / 2.0;
            let cap = decdsss_phy::rx::payload::LIVE_STRIP;
            scope::strip(
                ui,
                "Symbol SNR",
                &format!("{snr:.1} dB"),
                &live.snr_db,
                scope::Strip {
                    cap,
                    color: col::LOCK,
                    min_span: 6.0,
                },
                vec2(half, 56.0),
            );
            scope::strip(
                ui,
                "Carrier",
                &format!("{f:+.2} Hz"),
                &live.freq_hz,
                scope::Strip {
                    cap,
                    color: col::TRACE,
                    min_span: 0.5,
                },
                vec2(half, 56.0),
            );
        });
        scope::soft_bits(ui, &live.soft, vec2(w - 24.0, 44.0));
        if let Some(t) = tr {
            scope::frame_map(ui, &t.map, t.have, vec2(w - 24.0, 34.0));
        }
    });
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // At most 60 frames a second: a waterfall row is made 60 times a second, so
        // more frames only repeat rows (and a 144 or 165 Hz display would run the
        // whole window, scopes and all, that much harder).
        let frame = Duration::from_micros(16_667);
        let since = self.last_frame.elapsed();
        if since < frame {
            std::thread::sleep(frame - since);
        }
        // The frame rate as it is, from the time between frames.
        let dt = self.last_frame.elapsed().as_secs_f32().max(1e-3);
        self.measured_fps += 0.1 * (1.0 / dt - self.measured_fps);
        self.last_frame = Instant::now();
        self.tx.background(ctx);
        self.automate(ctx);
        if std::env::var_os("DECDSSS_FPS").is_some() {
            // Frames in the last second, on stderr, for checking the repaint rate.
            self.frames += 1;
            if self.fps_since.elapsed() >= Duration::from_secs(1) {
                eprintln!(
                    "{} frames in {:.2} s",
                    self.frames,
                    self.fps_since.elapsed().as_secs_f64()
                );
                self.frames = 0;
                self.fps_since = Instant::now();
            }
        }
        // 60 frames a second while receiving (the waterfall scrolls a row a frame),
        // a slow tick otherwise.
        let running = self.engine.shared.lock().unwrap().running;
        ctx.request_repaint_after(Duration::from_millis(if running { 16 } else { 100 }));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // The waterfall rows the engine has made since the last frame.
        let new_rows = std::mem::take(&mut self.engine.shared.lock().unwrap().new_rows);
        self.feed_waterfall(ui.ctx(), &new_rows);
        // Take what the engine published, then let go of the lock.
        let (
            running,
            status,
            seconds,
            level,
            snapshots,
            spectrum,
            log,
            files,
            transfers,
            pictures,
            voices,
            tx,
            monitor_error,
        ) = {
            let g = self.engine.shared.lock().unwrap();
            (
                g.running,
                g.status.clone(),
                g.seconds,
                g.level,
                g.snapshots.clone(),
                g.spectrum.clone(),
                g.log
                    .iter()
                    .rev()
                    .take(60)
                    .rev()
                    .cloned()
                    .collect::<std::collections::VecDeque<_>>(),
                g.files.clone(),
                g.transfers.clone(),
                g.pictures.clone(),
                g.voices.clone(),
                g.tx.clone(),
                g.monitor_error.clone(),
            )
        };
        let sh = Shared {
            running,
            status,
            seconds,
            level,
            snapshots,
            spectrum,
            log,
            files,
            transfers,
            pictures,
            voices,
            tx,
            monitor_error,
            ..Shared::default()
        };

        egui::Panel::top("bar").show(ui, |ui| self.top_bar(ui, &sh));
        let devices_out = self.devices_out.clone();
        let devices_in = self.devices_in.clone();
        let listen = self.listen();
        // Received speech plays when it is whole, once (not in unattended runs).
        if self.auto_play_rx && self.auto.screenshot.is_none() && !sh.tx.active {
            for v in sh.voices.iter().filter(|v| v.complete && !v.live) {
                if self.played.insert((v.call.clone(), v.session)) {
                    self.play_speech(&v.speech);
                }
            }
        }
        if self.view == View::Both {
            egui::Panel::left("transmit")
                .resizable(true)
                .default_size(400.0)
                .min_size(330.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(RichText::new("Transmit").strong().size(15.0));
                        self.tx
                            .show(ui, &self.engine, &sh.tx, &devices_out, &devices_in, &listen);
                    });
                });
        }
        if self.view != View::Tx {
            self.stations_panel(ui, &sh);
        }
        // The side panels are laid out first, so they run the full height under the
        // bar and the files, log and pictures sit in the column beside them rather
        // than cutting the instruments off.
        egui::Panel::bottom("log")
            .resizable(false)
            .show(ui, |ui| self.files_and_log(ui, &sh));
        if !sh.pictures.is_empty() || !sh.voices.is_empty() {
            egui::Panel::bottom("pictures")
                .resizable(false)
                .show(ui, |ui| self.pictures_panel(ui, &sh.pictures, &sh.voices));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                if self.view == View::Tx {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Transmit").strong().size(15.0));
                        self.tx
                            .show(ui, &self.engine, &sh.tx, &devices_out, &devices_in, &listen);
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Received files go to").weak());
                        ui.label(
                            RichText::new(self.out_dir.display().to_string())
                                .weak()
                                .monospace(),
                        );
                    });
                    self.band_view(ui, &sh);
                }
            });
    }
}
