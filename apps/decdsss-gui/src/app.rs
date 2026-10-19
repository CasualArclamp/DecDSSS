// made by CasualArclamp
//! The application window.

use crate::engine::{Cmd, Config, Engine, Shared, Source, SpectrumView, Transfer};
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
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Device,
    Wav,
    Demo,
}

/// Unattended runs for documentation and smoke tests.
#[derive(Default, Clone)]
pub struct Automation {
    pub after: Option<f64>,
    pub screenshot: Option<PathBuf>,
    pub open_tx: bool,
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
    waterfall: Option<TextureHandle>,
    waterfall_version: u64,
    heat: [HeatTexture; 3],
    tx: TxPanel,
    /// The last view of each station, kept after its segment ends so the card
    /// does not flicker away between segments: (profile, call, session).
    stations: std::collections::BTreeMap<(usize, String, u8), (ChannelView, Instant)>,
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
        tx.open = auto.open_tx;
        tx.restore(&prefs);
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
        App {
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
            waterfall_version: u64::MAX,
            heat: Default::default(),
            tx,
            stations: Default::default(),
        }
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
            ui.separator();
            ui.label(RichText::new(&sh.status).weak());
        });
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
        if view.rows.is_empty() {
            return;
        }
        if self.waterfall_version != view.row_version || self.waterfall.is_none() {
            let w = view.psd.len();
            let h = view.rows.len();
            // From just under the noise floor to 26 dB over it, or to the recent
            // peaks where those are higher (a clean cable has no noise to speak of).
            let mut recent: Vec<f32> = view
                .rows
                .iter()
                .rev()
                .take(40)
                .flat_map(|r| r.iter().copied())
                .collect();
            recent.sort_by(f32::total_cmp);
            let p99 = recent
                .get(recent.len() * 995 / 1000)
                .copied()
                .unwrap_or(0.0);
            let lo = view.floor_db - 3.0;
            let hi = (view.floor_db + 26.0).max(p99 + 3.0);
            let mut pixels = Vec::with_capacity(w * h);
            for row in view.rows.iter().rev() {
                for &d in row.iter().take(w) {
                    pixels.push(scope::palette((d - lo) / (hi - lo)));
                }
            }
            let img = ColorImage::new([w, h], pixels);
            let opts = TextureOptions::LINEAR;
            match &mut self.waterfall {
                Some(t) => t.set(img, opts),
                None => self.waterfall = Some(ui.ctx().load_texture("waterfall", img, opts)),
            }
            self.waterfall_version = view.row_version;
        }
        if let Some(t) = &self.waterfall {
            painter.image(
                t.id(),
                plot,
                egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
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
        let Some(after) = self.auto.after else { return };
        let now = Instant::now();
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
        self.automate(ctx);
        ctx.request_repaint_after(Duration::from_millis(40));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // Take what the engine published, then let go of the lock.
        let (running, status, seconds, level, snapshots, spectrum, log, files, transfers, tx) = {
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
                g.tx.clone(),
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
            tx,
            ..Shared::default()
        };

        egui::Panel::top("bar").show(ui, |ui| self.top_bar(ui, &sh));
        egui::Panel::bottom("log")
            .resizable(false)
            .show(ui, |ui| self.files_and_log(ui, &sh));
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
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Transmit…").clicked() {
                        self.tx.open = !self.tx.open;
                    }
                    ui.label(RichText::new("Received files go to").weak());
                    ui.label(
                        RichText::new(self.out_dir.display().to_string())
                            .weak()
                            .monospace(),
                    );
                });
                self.band_view(ui, &sh);
            });
        let devices_out = self.devices_out.clone();
        self.tx
            .show(ui.ctx(), &self.engine, &sh.tx, &devices_out, &self.out_dir);
    }
}
