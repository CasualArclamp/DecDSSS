// made by CasualArclamp
//! The application window.

use crate::cat::{self, CatDialog};
use crate::engine::{
    Cmd, Config, Engine, PictureView, Shared, Source, SpectrumView, Transfer, VoiceView,
};
use crate::prefs::Prefs;
use crate::radio::{Lamp, Radio};
use crate::scope::{self, HeatTexture, col};
use crate::tx::TxPanel;
use decdsss_audio::{list_input_devices, list_output_devices};
use decdsss_core::{Band, Family, Profile, Slot};
use decdsss_phy::rx::{AttemptView, ChannelView, Snapshot};
use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, pos2, vec2,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The version of this build, from the workspace's `Cargo.toml` (the same number the
/// executables' file names carry).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    /// Open the CAT and PTT dialog at launch.
    pub open_cat: bool,
    /// Open the transmit window with this file chosen.
    pub tx_file: Option<PathBuf>,
    /// Open the transmit window set to send this text (a message; `cq` for a CQ call).
    pub tx_text: Option<String>,
    /// With `tx_text` or `tx_file`: the profile, and for 500 Hz a slot, as `500@+500`.
    pub tx_band: Option<String>,
    /// Open the transmit window with the waterfall ID on.
    pub tx_wfid: bool,
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
    /// Which bands to listen on, in `Band::ALL` order.
    bands: [bool; 5],
    speed: f64,
    out_dir: PathBuf,
    log_open: bool,
    started: Instant,
    pub auto: Automation,
    waterfall: Option<crate::waterfall::Waterfall>,
    heat: [HeatTexture; 5],
    tx: TxPanel,
    /// The radio's keying (CAT, DTR, RTS) and the dialog that sets it.
    radio: Radio,
    cat: CatDialog,
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
    /// The last view of each station, kept for [`STATION_LINGER`] after its
    /// segment ends so the card does not flicker away between close segments:
    /// (profile, call, session).
    stations: std::collections::BTreeMap<(usize, String, u8), (ChannelView, Instant)>,
    /// Channels still open when *Clear decoders* was pressed, as (profile, channel
    /// id): their cards stay hidden until the channel closes. Ids are not reused,
    /// so the station's next segment shows again.
    hidden: std::collections::HashSet<(usize, usize)>,
    /// What the two plots of every station card show (left, right).
    card_plots: [Plot; 2],
    /// The texture of each picture on show and the version it was made from.
    pictures: std::collections::BTreeMap<(String, u8), (u64, TextureHandle)>,
}

/// How long a station's card stays after its channel closes with the transfer
/// still incomplete. A finished transfer (every frame in, or the postamble heard)
/// goes at once.
const STATION_LINGER: Duration = Duration::from_secs(5);

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
        bands: Option<[bool; 5]>,
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
        // The rig is looked for at once, so it is found by the first transmission.
        let radio = tx.radio.clone();
        let cat_config = cat::restore(&prefs);
        radio.apply(cat_config.clone());
        let mut view = prefs
            .get("view")
            .and_then(View::from_name)
            .unwrap_or(View::Rx);
        if auto.open_tx
            || auto.tx_file.is_some()
            || auto.tx_text.is_some()
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
        if let Some(b) = auto.tx_band.as_deref() {
            tx.preset_band(b);
        }
        if auto.tx_wfid {
            tx.preset_wfid();
        }
        if let Some(m) = auto.tx_text.clone() {
            tx.preload_text(m);
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
        let card_plots = [
            prefs
                .get("card_left")
                .and_then(Plot::from_key)
                .unwrap_or(Plot::Symbols),
            prefs
                .get("card_right")
                .and_then(Plot::from_key)
                .unwrap_or(Plot::Delay),
        ];
        let mut app = App {
            engine: Engine::spawn(radio.clone()),
            source,
            wav,
            devices_in,
            devices_out,
            device_in,
            details: false,
            prefs,
            bands: bands.unwrap_or([true; 5]),
            speed,
            out_dir: out_dir.unwrap_or_else(default_out_dir),
            log_open: true,
            started: Instant::now(),
            auto,
            waterfall: None,
            heat: Default::default(),
            tx,
            radio,
            cat: CatDialog::new(cat_config),
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
            hidden: Default::default(),
            card_plots,
            pictures: Default::default(),
        };
        if app.auto.open_cat {
            let radio = app.radio.clone();
            app.cat.show_open(&radio);
        }
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
        let bands: Vec<Band> = Band::ALL
            .iter()
            .zip(self.bands)
            .filter(|(_, on)| *on)
            .map(|(b, _)| *b)
            .collect();
        self.engine.send(Cmd::Start(Config {
            source: src,
            bands,
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

    /// The top bar: a run row (view, Start / Stop), the status line, then
    /// labelled cards for the source, bands, level, radio and listening.
    /// "DecDSSS v0.0.1".
    fn brand(&self, ui: &mut Ui) {
        ui.label(RichText::new("DecDSSS").strong().size(16.0));
        ui.label(RichText::new(format!("v{VERSION}")).weak().size(12.0))
            .on_hover_text(format!("decdsss-gui {VERSION}"));
    }

    /// RX / TX / RX + TX. `big` makes them tall, flat buttons.
    fn view_tabs(&mut self, ui: &mut Ui, big: bool) {
        let before = self.view;
        for (v, label) in [(View::Rx, "RX"), (View::Tx, "TX"), (View::Both, "RX + TX")] {
            if big {
                let text = RichText::new(label).strong().size(14.0);
                let b = egui::Button::selectable(self.view == v, text).min_size(vec2(56.0, 28.0));
                if ui.add(b).clicked() {
                    self.view = v;
                }
            } else {
                ui.selectable_value(&mut self.view, v, label);
            }
        }
        if self.view != before && self.auto.screenshot.is_none() {
            self.prefs.set("view", self.view.name());
            self.prefs.save();
        }
    }

    /// Where the audio comes from: sound card, WAV file or the demo signal.
    fn source_controls(&mut self, ui: &mut Ui, sh: &Shared) {
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
    }

    /// Which bands to listen on. `chips` draws them as toggle buttons.
    fn band_checks(&mut self, ui: &mut Ui, sh: &Shared, chips: bool) {
        ui.add_enabled_ui(!sh.running, |ui| {
            for (i, b) in Band::ALL.iter().enumerate() {
                if chips {
                    ui.toggle_value(&mut self.bands[i], b.label());
                } else {
                    ui.checkbox(&mut self.bands[i], b.label());
                }
            }
        });
    }

    /// Start (green) or Stop (red). `big` makes it a wide, tall button.
    fn run_button(&mut self, ui: &mut Ui, sh: &Shared, big: bool) {
        let size = if big { vec2(110.0, 32.0) } else { vec2(0.0, 0.0) };
        let (label, fill) = if sh.running {
            ("■  Stop", col::BAD)
        } else {
            ("▶  Start", col::LOCK)
        };
        let text = RichText::new(label)
            .strong()
            .size(if big { 15.0 } else { 13.0 })
            .color(if sh.running { Color32::WHITE } else { Color32::BLACK });
        if ui
            .add(egui::Button::new(text).fill(fill).min_size(size))
            .clicked()
        {
            if sh.running {
                self.engine.send(Cmd::Stop);
            } else {
                self.start();
            }
        }
    }

    /// The input level: a plain bar, or `segmented` LED blocks (green, amber, red).
    fn level_meter(&self, ui: &mut Ui, sh: &Shared, width: f32, height: f32, segmented: bool) {
        let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        let lvl = sh.level.clamp(0.0, 1.0).sqrt();
        if segmented {
            const N: usize = 24;
            let gap = 2.0;
            let w = (rect.width() - gap * (N as f32 - 1.0)) / N as f32;
            for i in 0..N {
                let f = i as f32 / N as f32;
                let on = lvl > f;
                let c = if f > 0.85 {
                    col::BAD
                } else if f > 0.65 {
                    col::MARK
                } else {
                    col::LOCK
                };
                let c = if on { c } else { c.gamma_multiply(0.18) };
                let x = rect.left() + i as f32 * (w + gap);
                let r = egui::Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
                p.rect_filled(r, 1.0, c);
            }
        } else {
            let w = lvl * rect.width();
            p.rect_filled(
                egui::Rect::from_min_size(rect.min, vec2(w, rect.height())),
                2.0,
                if sh.level > 0.95 { col::BAD } else { col::LOCK },
            );
        }
    }

    /// Whatever tab is showing, a transmission in progress can be stopped.
    fn tx_stop(&mut self, ui: &mut Ui, sh: &Shared, big: bool) {
        if !(sh.tx.active || self.tx.live_active()) {
            return;
        }
        let size = if big { vec2(100.0, 32.0) } else { vec2(0.0, 0.0) };
        if ui
            .add(
                egui::Button::new(RichText::new("■ STOP TX").strong().color(Color32::WHITE))
                    .fill(col::BAD)
                    .min_size(size),
            )
            .on_hover_text(
                "Stop the transmission (or the playback) now; if you key the radio by hand, release it too",
            )
            .clicked()
        {
            self.tx.abort_live();
            self.engine.send(Cmd::StopTransmit);
        }
    }

    /// Seconds run, and the measured frame rate of the window (the waterfall
    /// scrolls a row a frame).
    fn clock_and_fps(&mut self, ui: &mut Ui, sh: &Shared) {
        ui.label(format!("{:.0} s", sh.seconds));
        if sh.dropped_share > 0.0 {
            let pct = sh.dropped_share * 100.0;
            let pct = if pct < 1.0 {
                "<1".to_string()
            } else {
                format!("{pct:.0}")
            };
            ui.label(
                RichText::new(format!("falling behind: {pct}% audio dropped"))
                    .strong()
                    .color(col::BAD),
            )
            .on_hover_text(format!(
                "The receiver is not keeping up with the sound card, so audio is thrown                  away and nothing will decode. Untick bands you do not need, close other                  heavy programs, or set the input to a lower sample rate.
                 Share over the last {} s; {:.1} s of audio dropped since Start{}.",
                crate::engine::DROP_WINDOW.as_secs(),
                sh.dropped_frames as f64 / sh.sample_rate.max(1.0),
                if sh.xruns > 0 {
                    format!(", and {} losses reported by the driver", sh.xruns)
                } else {
                    String::new()
                }
            ));
        }
        self.fps += 0.1 * (self.measured_fps - self.fps);
        ui.label(RichText::new(format!("{:.0} fps", self.fps)).weak());
    }

    /// A small caps caption over a group of controls.
    fn caption(ui: &mut Ui, text: &str) {
        ui.label(RichText::new(text).weak().size(10.0).strong());
    }

    fn top_bar(&mut self, ui: &mut Ui, sh: &Shared) {
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            self.brand(ui);
            ui.separator();
            self.view_tabs(ui, true);
            ui.separator();
            self.run_button(ui, sh, true);
            self.tx_stop(ui, sh, true);
        });
        ui.label(RichText::new(&sh.status).weak().size(11.0));
        ui.horizontal_wrapped(|ui| {
            let frame = |ui: &Ui| egui::Frame::group(ui.style()).inner_margin(6.0);
            frame(ui).show(ui, |ui| {
                ui.vertical(|ui| {
                    Self::caption(ui, "SOURCE");
                    ui.horizontal(|ui| self.source_controls(ui, sh));
                });
            });
            frame(ui).show(ui, |ui| {
                ui.vertical(|ui| {
                    Self::caption(ui, "BANDS");
                    ui.horizontal(|ui| self.band_checks(ui, sh, true));
                });
            });
            frame(ui).show(ui, |ui| {
                ui.vertical(|ui| {
                    Self::caption(ui, "LEVEL");
                    ui.horizontal(|ui| {
                        self.level_meter(ui, sh, 110.0, 14.0, false);
                        self.clock_and_fps(ui, sh);
                    });
                });
            });
            frame(ui).show(ui, |ui| {
                ui.vertical(|ui| {
                    Self::caption(ui, "RADIO");
                    ui.horizontal(|ui| self.cat_button(ui));
                });
            });
            // The listen card is the widest: it gets a line of its own.
            ui.end_row();
            frame(ui).show(ui, |ui| {
                ui.vertical(|ui| {
                    Self::caption(ui, "LISTEN");
                    ui.horizontal_wrapped(|ui| self.monitor_controls(ui, sh));
                });
            });
        });
        ui.add_space(2.0);
    }

    /// The CAT and PTT button, with a lamp: grey when the program keys nothing,
    /// amber while the rig is being found, green when it is ready, red when the
    /// transmitter is keyed or the rig is not working.
    fn cat_button(&mut self, ui: &mut Ui) {
        let s = self.radio.status();
        let colour = match (s.keyed, &s.lamp) {
            (true, _) | (_, Lamp::Bad) => col::BAD,
            (_, Lamp::Wait) => col::MARK,
            (_, Lamp::Ok) => col::LOCK,
            (_, Lamp::Off) => Color32::GRAY,
        };
        ui.label(RichText::new("●").color(colour));
        let label = if s.keyed {
            "CAT / PTT — TX"
        } else {
            "CAT / PTT…"
        };
        if ui
            .button(label)
            .on_hover_text(format!(
                "{}
{}",
                self.radio.config().summary(),
                s.text
            ))
            .clicked()
        {
            let radio = self.radio.clone();
            self.cat.show_open(&radio);
        }
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
        // The bands: the two wide profiles, and the three 500 Hz slots side by side
        // (their labels in one row, the wide ones above).
        for (band, row, c) in [
            (
                Band::mid(Profile::W3000),
                0usize,
                Color32::from_rgba_unmultiplied(86, 182, 236, 22),
            ),
            (
                Band::mid(Profile::W2400),
                1,
                Color32::from_rgba_unmultiplied(98, 200, 140, 26),
            ),
            (
                Band::new(Profile::N500, Slot::Low),
                2,
                Color32::from_rgba_unmultiplied(232, 176, 72, 40),
            ),
            (
                Band::new(Profile::N500, Slot::Mid),
                2,
                Color32::from_rgba_unmultiplied(232, 176, 72, 56),
            ),
            (
                Band::new(Profile::N500, Slot::High),
                2,
                Color32::from_rgba_unmultiplied(232, 176, 72, 40),
            ),
        ] {
            let (a, b) = band.edges_hz();
            painter.rect_filled(
                egui::Rect::from_min_max(pos2(x(a), plot.top()), pos2(x(b), plot.bottom())),
                0.0,
                c,
            );
            painter.text(
                pos2(x(a) + 3.0, plot.top() + 2.0 + 11.0 * row as f32),
                Align2::LEFT_TOP,
                if band.profile == Profile::N500 && band.slot != Slot::Mid {
                    // Three of them side by side: no room for the long label.
                    format!("500 {}", band.slot.name())
                } else {
                    band.label()
                },
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
        for band in Band::ALL {
            let (a, b) = band.edges_hz();
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
            snap.band.label(),
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
                // A channel cleared while open stays hidden until it closes.
                self.hidden
                    .retain(|&(i, id)| i != index || snap.channels.iter().any(|c| c.id == id));
                let shown: Vec<&ChannelView> = snap
                    .channels
                    .iter()
                    .filter(|c| !self.hidden.contains(&(index, c.id)))
                    .collect();
                for c in &shown {
                    self.stations.insert(
                        (index, c.header.call.clone(), c.header.session),
                        ((*c).clone(), Instant::now()),
                    );
                }
                let live: Vec<(String, u8)> = shown
                    .iter()
                    .map(|c| (c.header.call.clone(), c.header.session))
                    .collect();
                // A closed channel's card goes once its transfer is finished, or
                // when nothing more has come for `STATION_LINGER`.
                self.stations.retain(|(i, call, session), (_, seen)| {
                    if *i != index || live.contains(&(call.clone(), *session)) {
                        return true;
                    }
                    let finished = transfers
                        .iter()
                        .any(|t| t.call == *call && t.session == *session && (t.done || t.ended));
                    !finished && seen.elapsed() < STATION_LINGER
                });
                let keep: Vec<(ChannelView, bool)> = self
                    .stations
                    .iter()
                    .filter(|((i, _, _), _)| *i == index)
                    .map(|((_, call, session), (v, _))| {
                        (v.clone(), live.contains(&(call.clone(), *session)))
                    })
                    .collect();
                // Every card shares one choice of plots: a tab clicked on any
                // card changes them all, and is remembered.
                let mut plots = self.card_plots;
                for (c, is_live) in &keep {
                    let tr = transfers
                        .iter()
                        .find(|t| t.call == c.header.call && t.session == c.header.session);
                    channel_card(ui, c, tr, w, *is_live, &mut plots);
                }
                if plots != self.card_plots {
                    self.card_plots = plots;
                    self.prefs.set("card_left", plots[0].key());
                    self.prefs.set("card_right", plots[1].key());
                    self.prefs.save();
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
            key: false,
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

    /// Received text, newest at the bottom, each line the time, who sent it, in which
    /// band, and the message.
    fn messages_panel(
        &mut self,
        ui: &mut Ui,
        messages: &[crate::engine::Message],
        max_height: f32,
    ) {
        ui.label(RichText::new("Messages").strong());
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for m in messages {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{:02}:{:02}",
                                m.at_s as u64 / 60,
                                m.at_s as u64 % 60
                            ))
                            .weak()
                            .monospace(),
                        );
                        ui.label(RichText::new(&m.call).strong().color(col::LOCK))
                            .on_hover_text(format!("session {}", m.session));
                        ui.label(RichText::new(format!("({})", m.band)).weak().size(11.0));
                        ui.label(&m.text);
                    });
                }
            });
    }

    /// The received files as links that open them, in a row that wraps.
    fn file_links(ui: &mut Ui, files: &[crate::engine::FileEntry]) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Files:").strong());
            for f in files {
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

    /// What has been received (messages, files, pictures, speech), stacked in a
    /// column: the place for it beside the transmit controls, where the window has
    /// the room.
    fn received_column(&mut self, ui: &mut Ui, sh: &Shared) {
        ui.label(RichText::new("Received").strong().size(15.0));
        if sh.messages.is_empty()
            && sh.files.is_empty()
            && sh.pictures.is_empty()
            && sh.voices.is_empty()
        {
            ui.label(
                RichText::new("Messages, files, pictures and speech that arrive show here.")
                    .weak()
                    .size(11.0),
            );
            return;
        }
        if !sh.messages.is_empty() {
            self.messages_panel(ui, &sh.messages, 220.0);
        }
        if !sh.files.is_empty() {
            Self::file_links(ui, &sh.files);
        }
        self.pictures.retain(|k, _| {
            sh.pictures
                .iter()
                .any(|p| (p.call.clone(), p.session) == *k)
        });
        for p in sh.pictures.iter().rev() {
            self.picture_card(ui, p);
        }
        for v in sh.voices.iter().rev() {
            if crate::voice::voice_card(ui, v) {
                self.play_speech(&v.speech);
            }
        }
    }

    /// The transfers under way, the search switch and the log; and the received
    /// files unless they are shown elsewhere (`files_here`).
    fn files_and_log(&mut self, ui: &mut Ui, sh: &Shared, files_here: bool) {
        if files_here && !sh.files.is_empty() {
            Self::file_links(ui, &sh.files);
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
                    // Empties the panel: closed stations go, and open channels are
                    // hidden until they close (the receiver goes on decoding them).
                    if ui
                        .add_enabled(
                            !self.stations.is_empty(),
                            egui::Button::new("Clear decoders"),
                        )
                        .on_hover_text("Remove every station card; a station shows again when its next segment starts")
                        .clicked()
                    {
                        for ((i, _, _), (v, _)) in &self.stations {
                            self.hidden.insert((*i, v.id));
                        }
                        self.stations.clear();
                    }
                    let snaps = sh.snapshots.clone();
                    for (i, s) in snaps.iter().enumerate() {
                        let idx = Band::ALL.iter().position(|b| *b == s.band).unwrap_or(i);
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
    // if that does not decode), plots side by side as on a station card.
    if let Some(h) = a.header.as_ref().filter(|_| a.header_stage) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let inner = w - 24.0;
            let gap = ui.spacing().item_spacing.x;
            let half = ((inner - gap) / 2.0).floor();
            ui.horizontal_top(|ui| {
                let size = vec2(half, half.min(CARD_PLOT_H));
                scope::constellation(
                    ui,
                    "Symbol × previous",
                    &h.points,
                    &[(1.0, 0.0)],
                    size,
                    col::TRACE,
                );
                scope::bank(ui, &h.bank, size);
            });
            scope::header_symbols(ui, h, a.header_since_s, a.header_needs_s, vec2(inner, 72.0));
            ui.horizontal_top(|ui| {
                scope::rake(ui, &h.profile, &h.fingers, vec2(half, 96.0));
                scope::epl(ui, h.epl, vec2(half, 96.0));
            });
        });
    }
}

/// What one of a station card's two plots shows. `Symbols` and `Detail`
/// depend on the family: the Walsh bank and the last symbol's bars for CSK,
/// the despread constellation and symbol × previous for DS-PSK.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plot {
    Symbols,
    Detail,
    Timing,
    Delay,
    Chips,
    Soft,
    Snr,
    Carrier,
}

impl Plot {
    const ALL: [Plot; 8] = [
        Plot::Symbols,
        Plot::Detail,
        Plot::Timing,
        Plot::Delay,
        Plot::Chips,
        Plot::Soft,
        Plot::Snr,
        Plot::Carrier,
    ];

    /// The tab's name.
    fn label(self, family: Family) -> &'static str {
        match (self, family) {
            (Plot::Symbols, Family::Csk) => "Walsh",
            (Plot::Symbols, Family::DsPsk) => "Const",
            (Plot::Detail, Family::Csk) => "Bars",
            (Plot::Detail, Family::DsPsk) => "Sym×prev",
            (Plot::Timing, _) => "Timing",
            (Plot::Delay, _) => "Delay",
            (Plot::Chips, _) => "Chips",
            (Plot::Soft, _) => "LLR",
            (Plot::Snr, _) => "SNR",
            (Plot::Carrier, _) => "Carrier",
        }
    }

    /// The name it is saved under.
    fn key(self) -> &'static str {
        match self {
            Plot::Symbols => "symbols",
            Plot::Detail => "detail",
            Plot::Timing => "timing",
            Plot::Delay => "delay",
            Plot::Chips => "chips",
            Plot::Soft => "soft",
            Plot::Snr => "snr",
            Plot::Carrier => "carrier",
        }
    }

    fn from_key(s: &str) -> Option<Plot> {
        // Rust note: `into_iter().find` walks the array by value and stops at
        // the first match.
        Plot::ALL.into_iter().find(|p| p.key() == s)
    }
}

/// Height of a station card's two plots, px.
const CARD_PLOT_H: f32 = 190.0;

/// The symbol SNR over the last three strip points (about 1.5 s), averaged as
/// powers; `None` with no points yet.
fn recent_snr_db(v: &[f32]) -> Option<f32> {
    let tail = &v[v.len().saturating_sub(3)..];
    if tail.is_empty() {
        return None;
    }
    let p = tail.iter().map(|d| 10f32.powf(d / 10.0)).sum::<f32>() / tail.len() as f32;
    Some(10.0 * p.log10())
}

/// One station: who and where, the mode and its link margin, the numbers that
/// matter, two plots chosen by tabs (`plots`, shared by every card), and the
/// frame map.
fn channel_card(
    ui: &mut Ui,
    c: &ChannelView,
    tr: Option<&Transfer>,
    w: f32,
    is_live: bool,
    plots: &mut [Plot; 2],
) {
    let h = &c.header;
    let live = &c.live;
    let family = h.mode.family;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let inner = w - 24.0;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&h.call).strong().size(15.0).color(col::LOCK));
            ui.label(RichText::new(format!("depth {}", h.depth)).strong());
            ui.label(format!(
                "session {}  ·  block {}/{}  ·  symbol {}/{}",
                h.session,
                c.blocks_done + 1,
                h.nblocks,
                live.symbol,
                live.symbols_in_block,
            ));
            if !is_live {
                if tr.is_some_and(|t| t.ended) {
                    ui.label(RichText::new("■ end of transmission").strong());
                } else {
                    ui.label(RichText::new("between segments").italics());
                }
            }
        });

        // The mode and its margin. Between segments there is no reading.
        let es = if is_live {
            recent_snr_db(&live.snr_db)
        } else {
            None
        };
        let ebn0 = es.map(|e| h.mode.ebn0_from_es_db(f64::from(e)));
        let (need, measured) = h.mode.ebn0_needed_db(h.profile);
        scope::margin_bar(
            ui,
            &scope::Margin {
                mode: h.mode.to_string(),
                ebn0_db: ebn0.map(|e| e as f32),
                snr_db: ebn0.map(|e| (e + h.mode.snr_offset_db(h.profile)) as f32),
                need_db: need as f32,
                measured,
                between: !is_live,
                ended: tr.is_some_and(|t| t.ended),
            },
            inner,
        );

        // The numbers, latest values.
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            // Each name and value in a group of its own, so a narrow card
            // wraps between quantities, never inside one.
            let kpi = |ui: &mut Ui, name: &str, value: String| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).weak());
                    ui.label(RichText::new(value).monospace().strong());
                });
                ui.add_space(8.0);
            };
            let e = live.epl;
            let disc = (e[0] - e[2]) / (e[0] + e[2]).max(1e-6);
            match live.snr_db.last() {
                Some(s) => kpi(ui, "Symbol SNR", format!("{s:.1} dB")),
                None => kpi(ui, "Symbol SNR", "–".into()),
            }
            kpi(ui, "Carrier", format!("{:+.2} Hz", c.carrier_hz));
            kpi(ui, "E−L", format!("{disc:+.2}"));
        });

        // The two plots, each under its own row of tabs.
        let gap = ui.spacing().item_spacing.x;
        let half = ((inner - gap) / 2.0).floor();
        ui.horizontal_top(|ui| {
            // Rust note: `iter_mut` hands out `&mut Plot` for each slot, so a
            // click writes straight into the caller's array.
            for slot in plots.iter_mut() {
                ui.vertical(|ui| {
                    ui.set_width(half);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for p in Plot::ALL {
                            let text = RichText::new(p.label(family)).size(11.0);
                            if ui.selectable_label(*slot == p, text).clicked() {
                                *slot = p;
                            }
                        }
                    });
                    plot(ui, *slot, c, vec2(half, CARD_PLOT_H));
                });
            }
        });
        if let Some(t) = tr {
            scope::frame_map(ui, &t.map, t.have, vec2(inner, 34.0));
        }
    });
}

/// Draws one of a station card's plots `size` big.
fn plot(ui: &mut Ui, p: Plot, c: &ChannelView, size: egui::Vec2) {
    let live = &c.live;
    let ds = c.header.mode.family == Family::DsPsk;
    let cap = decdsss_phy::rx::payload::LIVE_STRIP;
    // Rust note: `Plot::Symbols if ds` is a match guard: the arm is taken only
    // when the pattern matches and the condition holds, else the next arm is
    // tried.
    match p {
        Plot::Symbols if ds => scope::constellation(
            ui,
            "Despread symbols",
            &live.symbols,
            &scope::QPSK,
            size,
            col::TRACE,
        ),
        Plot::Symbols => scope::bank(ui, &live.bank, size),
        Plot::Detail if ds => scope::constellation(
            ui,
            "Symbol × previous",
            &live.diffs,
            &scope::QUADRANTS,
            size,
            col::LOCK,
        ),
        Plot::Detail => scope::bank_bars(ui, live.bank.last(), size),
        Plot::Timing => scope::epl(ui, live.epl, size),
        Plot::Delay => scope::rake(ui, &live.profile, &live.fingers, size),
        Plot::Chips => scope::chip_scope(ui, &live.raw_chips, &live.despread_chips, size),
        Plot::Soft => scope::soft_bits(ui, &live.soft, size),
        Plot::Snr => {
            let s = live.snr_db.last().copied().unwrap_or(f32::NAN);
            scope::strip(
                ui,
                "Symbol SNR",
                &format!("{s:.1} dB"),
                &live.snr_db,
                scope::Strip {
                    cap,
                    color: col::LOCK,
                    min_span: 6.0,
                },
                size,
            );
        }
        Plot::Carrier => {
            let f = live.freq_hz.last().copied().unwrap_or(f32::NAN);
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
                size,
            );
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Never leave the transmitter keyed behind a window that is gone.
        self.radio.shut();
    }

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
            messages,
            transfers,
            pictures,
            voices,
            tx,
            monitor_error,
            (dropped_share, dropped_frames, xruns, sample_rate),
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
                g.messages.clone(),
                g.transfers.clone(),
                g.pictures.clone(),
                g.voices.clone(),
                g.tx.clone(),
                g.monitor_error.clone(),
                (g.dropped_share, g.dropped_frames, g.xruns, g.sample_rate),
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
            messages,
            transfers,
            pictures,
            voices,
            tx,
            monitor_error,
            dropped_share,
            dropped_frames,
            xruns,
            sample_rate,
            ..Shared::default()
        };

        egui::Panel::top("bar").show(ui, |ui| self.top_bar(ui, &sh));
        self.cat.show(
            ui.ctx(),
            &self.radio,
            &mut self.prefs,
            self.auto.screenshot.is_none(),
        );
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
                        ui.separator();
                        self.received_column(ui, &sh);
                    });
                });
        }
        if self.view != View::Tx {
            self.stations_panel(ui, &sh);
        }
        // The side panels are laid out first, so they run the full height under the
        // bar and the files, log and pictures sit in the column beside them rather
        // than cutting the instruments off.
        // With the transmit controls beside the instruments (RX+TX) what was received
        // goes in the left column under them; otherwise it sits along the bottom.
        let received_below = self.view != View::Both;
        egui::Panel::bottom("log")
            .resizable(false)
            .show(ui, |ui| self.files_and_log(ui, &sh, received_below));
        if received_below && !sh.messages.is_empty() {
            egui::Panel::bottom("messages")
                .resizable(false)
                .show(ui, |ui| self.messages_panel(ui, &sh.messages, 110.0));
        }
        if received_below && (!sh.pictures.is_empty() || !sh.voices.is_empty()) {
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
