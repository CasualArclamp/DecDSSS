// made by CasualArclamp
//! Voice in the GUI: the clip to send (a WAV, or a recording from a microphone)
//! and the card for speech that arrives.

use crate::engine::VoiceView;
use crate::scope::col;
use decdsss_audio::channels::downmix_to_mono;
use decdsss_audio::{InputOptions, InputStream, wav};
use decdsss_link::voice::speech_from_audio;
use eframe::egui::{self, Color32, RichText, Sense, Stroke, Ui, pos2, vec2};
use std::path::Path;
use std::time::{Duration, Instant};

/// Longest recording, seconds.
pub const MAX_CLIP_S: f64 = 60.0;

/// Speech to send: 8 kHz, mono.
pub struct Clip {
    pub name: String,
    pub pcm: Vec<i16>,
}

impl Clip {
    pub fn seconds(&self) -> f64 {
        self.pcm.len() as f64 / 8000.0
    }
}

/// A microphone being recorded.
struct Recorder {
    stream: InputStream,
    rate: u32,
    channels: usize,
    samples: Vec<f32>,
    /// Recent peak, 0–1, falling back.
    peak: f32,
    started: Instant,
}

impl Recorder {
    fn start(device: Option<String>) -> Result<Recorder, String> {
        let stream = InputStream::open(&InputOptions {
            device,
            channels: Some(1),
            ..InputOptions::default()
        })
        .map_err(|e| e.to_string())?;
        let f = stream.format();
        Ok(Recorder {
            rate: f.sample_rate,
            channels: f.channels.max(1),
            stream,
            samples: Vec::new(),
            peak: 0.0,
            started: Instant::now(),
        })
    }

    /// Takes what the sound card has; false once the device has stopped.
    fn poll(&mut self) -> bool {
        if self.stream.is_dead() {
            return false;
        }
        let frames = self.stream.available_frames();
        self.peak *= 0.9;
        if frames > 0 {
            let mono = downmix_to_mono(&self.stream.read(frames), self.channels);
            for &v in &mono {
                self.peak = self.peak.max(v.abs());
            }
            self.samples.extend(mono);
        }
        true
    }

    fn seconds(&self) -> f64 {
        self.samples.len() as f64 / f64::from(self.rate)
    }
}

/// 8 kHz speech as audio at `rate`, ready to play.
pub fn speech_to_audio(pcm: &[i16], rate: u32) -> Vec<f32> {
    let x: Vec<f32> = pcm.iter().map(|&v| f32::from(v) / 32768.0).collect();
    decdsss_dsp::resample::resample_real(&x, 8000.0, f64::from(rate), 3500.0, 4000.0)
}

/// What the voice controls ask the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Play {
    Nothing,
    /// Play the clip as it is.
    Clip,
    /// Play the clip as Codec2 will send it (compressed and expanded again).
    AsSent,
}

#[derive(Default)]
pub struct VoicePanel {
    pub clip: Option<Clip>,
    rec: Option<Recorder>,
    /// Input device for recording: 0 the default, n the nth listed.
    mic: usize,
    note: String,
}

impl VoicePanel {
    /// Loads a WAV as the clip.
    pub fn load(&mut self, path: &Path) {
        let loaded = wav::read(path)
            .map_err(|e| e.to_string())
            .and_then(|(audio, rate)| speech_from_audio(&audio, f64::from(rate)));
        match loaded {
            Ok(pcm) => {
                let name = path
                    .file_name()
                    .map_or("speech".to_string(), |n| n.to_string_lossy().into_owned());
                self.note.clear();
                self.clip = Some(Clip { name, pcm });
            }
            Err(e) => self.note = format!("{}: {e}", path.display()),
        }
    }

    /// Starts recording on the input whose name contains `hint` (the default
    /// microphone if none does), as if Record had been pressed.
    pub fn record_on(&mut self, devices_in: &[String], hint: &str) {
        self.mic = devices_in
            .iter()
            .position(|d| d.to_lowercase().contains(&hint.to_lowercase()))
            .map_or(0, |i| i + 1);
        self.start(devices_in);
    }

    /// Stops a recording, as if Stop had been pressed.
    pub fn finish(&mut self) {
        self.stop();
    }

    pub fn recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Keeps a recording going; call every frame.
    pub fn tick(&mut self, ctx: &egui::Context) {
        let Some(rec) = &mut self.rec else { return };
        let alive = rec.poll();
        if !alive || rec.seconds() >= MAX_CLIP_S {
            self.stop();
        } else {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
    }

    fn start(&mut self, devices_in: &[String]) {
        let device = (self.mic > 0)
            .then(|| devices_in.get(self.mic - 1).cloned())
            .flatten();
        match Recorder::start(device) {
            Ok(r) => {
                self.note.clear();
                self.rec = Some(r);
            }
            Err(e) => self.note = format!("cannot record: {e}"),
        }
    }

    fn stop(&mut self) {
        let Some(rec) = self.rec.take() else { return };
        match speech_from_audio(&rec.samples, f64::from(rec.rate)) {
            Ok(pcm) => {
                self.clip = Some(Clip {
                    name: "recording".into(),
                    pcm,
                });
                self.note.clear();
            }
            Err(e) => self.note = format!("the recording: {e}"),
        }
    }

    /// The controls: choose a WAV or record, and hear the clip. Returns what to play.
    pub fn ui(&mut self, ui: &mut Ui, devices_in: &[String]) -> Play {
        let mut play = Play::Nothing;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.recording(), egui::Button::new("Choose WAV…"))
                .clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("WAV audio", &["wav"])
                    .pick_file()
            {
                self.load(&p);
            }
            if self.recording() {
                if ui.button("■ Stop").clicked() {
                    self.stop();
                }
            } else if ui.button("● Record").clicked() {
                self.start(devices_in);
            }
            egui::ComboBox::from_id_salt("mic")
                .width(150.0)
                .selected_text(if self.mic == 0 {
                    "Default microphone".to_string()
                } else {
                    devices_in
                        .get(self.mic - 1)
                        .cloned()
                        .unwrap_or_else(|| "Default microphone".into())
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mic, 0, "Default microphone");
                    for (i, d) in devices_in.iter().enumerate() {
                        ui.selectable_value(&mut self.mic, i + 1, d);
                    }
                });
        });
        if let Some(rec) = &self.rec {
            ui.horizontal(|ui| {
                ui.colored_label(col::BAD, "●");
                ui.label(format!(
                    "recording {:.1} / {MAX_CLIP_S:.0} s",
                    rec.started.elapsed().as_secs_f64().min(rec.seconds() + 0.1)
                ));
                ui.add(
                    egui::ProgressBar::new(rec.peak.clamp(0.0, 1.0))
                        .desired_width(100.0)
                        .fill(if rec.peak > 0.95 { col::BAD } else { col::LOCK }),
                );
            });
        } else if let Some(c) = &self.clip {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{}: {:.1} s of speech", c.name, c.seconds()));
                if ui.button("▶ Play it").clicked() {
                    play = Play::Clip;
                }
                if ui
                    .button("▶ As Codec2 will send it")
                    .on_hover_text(
                        "The clip compressed and expanded again: what the other end will hear",
                    )
                    .clicked()
                {
                    play = Play::AsSent;
                }
            });
        }
        if !self.note.is_empty() {
            ui.colored_label(col::BAD, &self.note);
        }
        play
    }
}

/// A card for speech arriving or arrived. Returns true if Play was pressed.
pub fn voice_card(ui: &mut Ui, v: &VoiceView) -> bool {
    let mut play = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&v.call).strong().color(col::LOCK));
                ui.label(
                    RichText::new(format!("Codec2 {}  ·  session {}", v.codec, v.session)).weak(),
                );
            });
            let (rect, _) = ui.allocate_exact_size(vec2(300.0, 90.0), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 2.0, Color32::from_gray(20));
            // Frames that have not come, shaded.
            if v.total > 0 {
                let w = rect.width() / v.total as f32;
                for (i, have) in v.map.iter().enumerate() {
                    if !have {
                        let x0 = rect.left() + i as f32 * w;
                        p.rect_filled(
                            egui::Rect::from_min_max(
                                pos2(x0, rect.top()),
                                pos2(x0 + w, rect.bottom()),
                            ),
                            0.0,
                            Color32::from_rgba_unmultiplied(180, 60, 60, 60),
                        );
                    }
                }
            }
            let mid = rect.center().y;
            p.line_segment(
                [pos2(rect.left(), mid), pos2(rect.right(), mid)],
                Stroke::new(1.0, Color32::from_gray(60)),
            );
            let n = v.wave.len().max(1);
            for (i, (lo, hi)) in v.wave.iter().enumerate() {
                let x = rect.left() + (i as f32 + 0.5) / n as f32 * rect.width();
                let (y0, y1) = (
                    mid - hi * rect.height() * 0.5,
                    mid - lo * rect.height() * 0.5,
                );
                p.line_segment(
                    [pos2(x, y0), pos2(x, y1.max(y0 + 1.0))],
                    Stroke::new(1.0, col::TRACE),
                );
            }
            ui.horizontal(|ui| {
                if ui.button("▶ Play").clicked() {
                    play = true;
                }
                ui.label(
                    RichText::new(format!(
                        "{:.1} s  ·  {}/{} frames{}",
                        v.seconds,
                        v.have,
                        v.total,
                        if v.complete { "  ·  complete" } else { "" }
                    ))
                    .size(11.0),
                );
            });
        });
    });
    play
}
