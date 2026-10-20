// made by CasualArclamp
//! The transmit window: a file in, audio out (to a sound card or a WAV).
//! DecDSSS never keys a transmitter; it plays audio and you do the rest.

use crate::app::matching_device;
use crate::engine::{Cmd, Engine, TxState};
use crate::live::{LiveConfig, LiveTxSession, Phase};
use crate::prefs::Prefs;
use crate::radio::Radio;
use crate::scope::col;
use crate::voice::{Play, VoicePanel};
use decdsss_audio::wav;
use decdsss_codec2::Mode as Codec;
use decdsss_core::profile::ROLLOFF;
use decdsss_core::{Band, Family, Mode, Profile, Slot};
use decdsss_image::Source;
use decdsss_link::voice::{CODECS, default_codec, render_speech, voice_mode};
use decdsss_link::{
    SendSpec, Timeline, VoiceSpec, air_seconds, container, max_stream_bytes, plan_voice, segments,
    timeline,
};
use decdsss_phy::banner;
use decdsss_phy::codes::normalise_call;
use decdsss_phy::framing::{DEPTHS, Layout, PayloadType};
use decdsss_phy::tx::{chips_to_audio_at, transmission_chips, vox_lead};
use eframe::egui::{self, Color32, ColorImage, Context, RichText, TextureHandle, TextureOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// File extensions the picture mode is switched on for.
const PICTURE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff"];

/// Panel width (points) from which the preview goes beside the controls rather
/// than under them: the transmit-only view has it, the side panel does not.
const TWO_COLUMNS: f32 = 640.0;

/// The VOX lead: seconds of 1 kHz tone, then of silence, ahead of the signal.
const VOX_TONE_S: f64 = 0.4;
const VOX_GAP_S: f64 = 0.3;

/// The families the Spreading buttons offer, and what each is.
const FAMILIES: [(Family, &str, &str); 3] = [
    (
        Family::DsPsk,
        "DS-PSK",
        "Direct sequence: a QPSK symbol held for SF chips, coherent, pilot-aided",
    ),
    (
        Family::Csk,
        "CSK",
        "Code-shift keying: one of L Walsh codes of L chips, log2(L) bits a symbol, \
         noncoherent",
    ),
    (
        Family::BiCsk,
        "BCSK",
        "Bi-orthogonal CSK: CSK plus the code's sign as one more bit a symbol, read against the symbol before; 12 to 20 % more rate for the same signal",
    ),
];

/// What a prepared picture depends on.
#[derive(Clone, PartialEq)]
struct PicKey {
    path: PathBuf,
    /// Bytes the picture may take (what the air time carries).
    budget: usize,
    min_quality: u8,
}

/// A picture scaled and compressed for sending.
struct Prepared {
    name: String,
    jpeg: Vec<u8>,
    width: u32,
    height: u32,
    quality: u8,
    /// Size of the picture it came from.
    source: (u32, u32),
    /// The JPEG decoded, RGBA, as the receiver will show it.
    preview: Vec<u8>,
}

type PicResult = Result<Prepared, String>;

/// Speech as planned for sending: a line saying what it comes to, and its timeline.
type VoicePlanned = Result<(String, Option<Timeline>), String>;

/// Compressing a picture takes a moment, so it runs off the UI thread.
struct PicJob {
    key: PicKey,
    rx: mpsc::Receiver<(Option<Arc<Source>>, PicResult)>,
}

/// What the message box shows for a CQ packet (the packet itself is only the callsign).
fn cq_label(call: &str) -> String {
    format!("CQ de {call}")
}

fn is_picture(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| PICTURE_EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

/// The `family` mode nearest `from` in information rate, for switching family
/// without jumping along the ladder; the family's fastest mode without a `from`.
/// `snr_offset_db` is 10·log10(Rb / B) with B fixed by the profile, so the nearest
/// offset is the nearest rate on a log scale.
fn nearest_mode(family: Family, from: Option<Mode>, profile: Profile) -> Option<Mode> {
    let mut modes = Mode::all().into_iter().filter(|m| m.family == family);
    let Some(from) = from else {
        return modes.next();
    };
    let target = from.snr_offset_db(profile);
    // `total_cmp` orders f64s (NaN included), which `min_by` needs: plain f64
    // is only `PartialOrd`.
    modes.min_by(|a, b| {
        (a.snr_offset_db(profile) - target)
            .abs()
            .total_cmp(&(b.snr_offset_db(profile) - target).abs())
    })
}

fn picture_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map_or_else(|| "picture".into(), |s| s.to_string_lossy().into_owned());
    format!("{stem}.jpg")
}

fn prepare(key: &PicKey, cached: Option<Arc<Source>>) -> (Option<Arc<Source>>, PicResult) {
    let src = match cached {
        Some(s) => s,
        None => match Source::open(&key.path) {
            Ok(s) => Arc::new(s),
            Err(e) => return (None, Err(e)),
        },
    };
    let result = decdsss_image::encode_auto(&src.image, key.budget, 1 << 20, key.min_quality)
        .and_then(|enc| {
            let preview = decdsss_image::decode(&enc.bytes).ok_or("the JPEG does not decode")?;
            Ok(Prepared {
                name: picture_name(&key.path),
                width: enc.width,
                height: enc.height,
                quality: enc.quality,
                source: src.image.dimensions(),
                preview: preview.into_raw(),
                jpeg: enc.bytes,
            })
        });
    (Some(src), result)
}

pub struct TxPanel {
    file: Option<PathBuf>,
    call: String,
    profile: Profile,
    /// Where a 500 Hz signal sits (the other profiles have no slots).
    slot: Slot,
    mode: String,
    depth: usize,
    resync: f64,
    parity: f64,
    repeat: usize,
    level: f32,
    rate: u32,
    device: usize,
    note: String,
    /// Send the file as a picture: scaled and compressed to the air time.
    picture: bool,
    /// Seconds of signal one pass of a picture may take.
    air: f64,
    min_quality: u8,
    pic_source: Option<(PathBuf, Arc<Source>)>,
    pic_job: Option<PicJob>,
    pic_result: Option<(PicKey, PicResult)>,
    pic_tex: Option<(PicKey, TextureHandle)>,
    /// Send speech: a recording or a WAV, as Codec2.
    voice: bool,
    /// Send a text message (a CQ call, chat) instead of a file.
    text: bool,
    message: String,
    /// The message is a CQ packet (the callsign in a header-only transmission), not
    /// text: set by the CQ button, cleared as soon as the message is edited.
    cq_packet: bool,
    vp: VoicePanel,
    /// The Codec2 mode; `None` is the profile's own (450 on 2.4 kHz, 700C on 3 kHz).
    codec: Option<Codec>,
    /// A 1 kHz tone ahead of the signal, to key a radio on VOX.
    vox: bool,
    /// The callsign and mode written on the waterfall ahead of the signal
    /// (`decdsss_phy::banner`).
    wfid: bool,
    /// What the speech summary was worked out for, the summary, and the
    /// transmission's timeline for the preview.
    voice_summary: Option<(String, VoicePlanned)>,
    /// Live voice (streamed while the PTT is held) rather than overs.
    live_mode: bool,
    /// The live transmission in progress.
    live: Option<LiveTxSession>,
    /// The radio's keying, shared with the window and the transmit threads.
    pub radio: Radio,
}

impl Default for TxPanel {
    fn default() -> Self {
        TxPanel {
            file: None,
            call: String::new(),
            profile: Profile::W2400,
            slot: Slot::Mid,
            mode: "ds8-1/2".into(),
            depth: 4,
            resync: 60.0,
            parity: 25.0,
            repeat: 1,
            level: 0.2,
            rate: 48_000,
            device: 0,
            note: String::new(),
            picture: false,
            air: 120.0,
            min_quality: 30,
            pic_source: None,
            pic_job: None,
            pic_result: None,
            pic_tex: None,
            voice: false,
            text: false,
            message: String::new(),
            cq_packet: false,
            vp: VoicePanel::default(),
            codec: None,
            vox: false,
            wfid: false,
            voice_summary: None,
            live_mode: false,
            live: None,
            radio: Radio::default(),
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
        if let Some(s) = p.get("tx_slot").and_then(Slot::from_name) {
            self.slot = s;
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
        if let Some(a) = p.get("tx_air").and_then(|a| a.parse::<f64>().ok()) {
            self.air = a.clamp(20.0, 900.0);
        }
        if let Some(w) = p.get("tx_wfid") {
            self.wfid = w == "1";
        }
    }

    /// Switches to voice and starts recording (for `--record-for`).
    pub fn auto_record(&mut self, devices_in: &[String], hint: &str) {
        self.voice = true;
        if let Some(vm) = self.suggested_voice_mode() {
            self.mode = vm.name();
        }
        self.vp.record_on(devices_in, hint);
    }

    /// Plays what the window is set up to send on the output whose name contains
    /// `hint`, as if Play had been pressed (for `--tx-play`).
    pub fn auto_play(&mut self, engine: &Engine, devices_out: &[(String, u32)], hint: &str) {
        if self.call.is_empty() {
            self.call = "VK2ABC".into();
        }
        self.device = devices_out
            .iter()
            .position(|d| d.0.to_lowercase().contains(&hint.to_lowercase()))
            .map_or(0, |i| i + 1);
        let (device, rate) = self.output(devices_out);
        match self.build(rate) {
            Ok(plan) => engine.send(Cmd::Transmit {
                audio: plan.audio,
                rate,
                device,
                key: true,
            }),
            Err(e) => self.note = e,
        }
    }

    /// Lets go of the PTT after [`TxPanel::auto_record`] and sends what was said on
    /// the output whose name contains `out_hint` (for `--ptt-for`).
    pub fn auto_release(&mut self, engine: &Engine, devices_out: &[(String, u32)], out_hint: &str) {
        if self.call.is_empty() {
            self.call = "VK2ABC".into();
        }
        self.device = devices_out
            .iter()
            .position(|d| d.0.to_lowercase().contains(&out_hint.to_lowercase()))
            .map_or(0, |i| i + 1);
        if self.vp.release() == Play::Send {
            self.send_transmission(engine, devices_out);
        }
    }

    /// Starts a live transmission as if the PTT had gone down: the microphone is the
    /// input whose name contains `mic_hint`, the output the one with `out_hint` (for
    /// `--live-for`).
    pub fn auto_live_start(
        &mut self,
        devices_out: &[(String, u32)],
        devices_in: &[String],
        mic_hint: &str,
        out_hint: &str,
    ) {
        if self.call.is_empty() {
            self.call = "VK2ABC".into();
        }
        self.voice = true;
        self.live_mode = true;
        if let Some(vm) = self.suggested_voice_mode() {
            self.mode = vm.name();
        }
        self.depth = 2;
        self.vp.select_mic(devices_in, mic_hint);
        self.device = devices_out
            .iter()
            .position(|d| d.0.to_lowercase().contains(&out_hint.to_lowercase()))
            .map_or(0, |i| i + 1);
        self.start_live(devices_out, devices_in);
    }

    /// Lets go of the PTT of [`TxPanel::auto_live_start`].
    pub fn auto_live_stop(&mut self) {
        if let Some(s) = &self.live {
            s.stop();
        }
    }

    /// Stops a recording started by [`TxPanel::auto_record`].
    pub fn auto_stop(&mut self) {
        self.vp.finish();
    }

    /// Sets the bandwidth, and for 500 Hz the slot, from `500@+500` (for `--tx-band`).
    pub fn preset_band(&mut self, spec: &str) {
        let (prof, slot) = match spec.split_once('@') {
            Some((p, s)) => (p, Slot::from_name(s)),
            None => (spec, None),
        };
        if let Some(p) = Profile::from_name(prof) {
            self.profile = p;
        }
        if let Some(s) = slot {
            self.slot = s;
        }
    }

    /// Turns the waterfall ID on (for `--tx-wfid`).
    pub fn preset_wfid(&mut self) {
        self.wfid = true;
    }

    /// Switches to a text message (for `--tx-text`); `cq` writes a CQ call.
    pub fn preload_text(&mut self, msg: String) {
        if self.call.is_empty() {
            self.call = "VK2ABC".into();
        }
        self.text = true;
        self.picture = false;
        self.voice = false;
        self.cq_packet = msg.eq_ignore_ascii_case("cq");
        self.message = if self.cq_packet {
            cq_label(&normalise_call(&self.call).unwrap_or_default())
        } else {
            msg
        };
    }

    /// Chooses a file, as if from the dialog.
    pub fn preload(&mut self, path: PathBuf) {
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
        {
            // A WAV is speech to send.
            self.voice = true;
            if let Some(vm) = self.suggested_voice_mode() {
                self.mode = vm.name();
            }
            self.vp.load(&path);
        } else {
            self.picture = is_picture(&path);
            self.file = Some(path);
        }
    }

    /// The output device chosen and the rate it runs at.
    pub fn output(&self, devices_out: &[(String, u32)]) -> (Option<String>, u32) {
        let device = if self.device == 0 {
            None
        } else {
            devices_out.get(self.device - 1).map(|d| d.0.clone())
        };
        (device, self.device_rate(devices_out))
    }

    /// The mode that suits the codec in use on this profile: DS4 for the profile's own
    /// codec, DS2 or DS1 for 1200 and 1600 ([`voice_mode`]); the profile's voice mode where
    /// the codec has no mode of its own; none on 500 Hz.
    fn suggested_voice_mode(&self) -> Option<Mode> {
        self.codec
            .or_else(|| default_codec(self.profile))
            .and_then(|c| voice_mode(self.profile, c))
            .or_else(|| Mode::voice(self.profile))
    }

    /// The transmission as speech will be sent.
    fn voice_spec(&self) -> Option<VoiceSpec> {
        Some(VoiceSpec {
            profile: self.profile,
            mode: Mode::from_name(&self.mode)?,
            depth: self.depth,
            call: "AA0AA".into(),
            session: 0,
            codec: self.codec.or_else(|| default_codec(self.profile))?,
            resync_s: self.resync,
        })
    }

    /// Plays the clip, as it is or as Codec2 will send it.
    fn play_voice(&mut self, which: Play, engine: &Engine, listen: &(Option<String>, u32)) {
        let Some(clip) = &self.vp.clip else { return };
        let pcm: Vec<i16> = match which {
            Play::Nothing | Play::Send | Play::LiveStart | Play::LiveStop => return,
            Play::Clip => clip.pcm.clone(),
            Play::AsSent => {
                let Some(spec) = self.voice_spec() else {
                    self.note = "this bandwidth is too narrow for voice".into();
                    return;
                };
                match plan_voice(&spec, &clip.pcm) {
                    Ok(p) => {
                        let frames: Vec<Option<&[u8]>> = p
                            .segments
                            .iter()
                            .flat_map(|s| s.frames.iter().map(|f| Some(f.as_slice())))
                            .collect();
                        render_speech(spec.codec, &frames).0
                    }
                    Err(e) => {
                        self.note = e;
                        return;
                    }
                }
            }
        };
        // To the speakers, never the output that feeds the radio.
        let (device, rate) = listen.clone();
        engine.send(Cmd::Transmit {
            audio: crate::voice::speech_to_audio(&pcm, rate),
            rate,
            device,
            key: false,
        });
    }

    /// Builds the transmission as set up and plays it on the output device.
    fn send_transmission(&mut self, engine: &Engine, devices_out: &[(String, u32)]) {
        let rate = self.device_rate(devices_out);
        match self.build(rate) {
            Ok(plan) => {
                self.note = if plan.frames == 0 {
                    // A CQ packet is headers only.
                    format!(
                        "CQ packet, {} cop{}, {:.0} s of audio",
                        plan.segments,
                        if plan.segments == 1 { "y" } else { "ies" },
                        plan.seconds
                    )
                } else {
                    format!(
                        "{} frames in {} segment(s), {:.0} s of audio",
                        plan.frames, plan.segments, plan.seconds
                    )
                };
                engine.send(Cmd::Transmit {
                    audio: plan.audio,
                    rate,
                    device: if self.device == 0 {
                        None
                    } else {
                        devices_out.get(self.device - 1).map(|d| d.0.clone())
                    },
                    key: true,
                });
            }
            Err(e) => self.note = e,
        }
    }

    /// The message box with a CQ button that sets up a CQ packet from the callsign.
    fn message_editor(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            let edit = ui.add(
                egui::TextEdit::multiline(&mut self.message)
                    .desired_rows(3)
                    // As wide as the grid's column leaves, up to 300.
                    .desired_width(ui.available_width().clamp(160.0, 300.0))
                    .hint_text("type a message, or press CQ"),
            );
            if edit.changed() {
                // Typing over a CQ makes it an ordinary message again.
                self.cq_packet = false;
            }
            ui.horizontal(|ui| {
                if ui
                    .button("CQ")
                    .on_hover_text(
                        "A CQ packet: your callsign alone in the most robust header the modem \
                         has, sent in two copies. It decodes far below the noise where a text \
                         CQ would not.",
                    )
                    .clicked()
                {
                    match normalise_call(&self.call) {
                        Some(call) => {
                            self.message = cq_label(&call);
                            self.cq_packet = true;
                        }
                        None => {
                            self.note =
                                "enter your callsign (letters, digits and /, up to 9)".into();
                        }
                    }
                }
                if ui.button("Clear").clicked() {
                    self.message.clear();
                    self.cq_packet = false;
                }
                if self.cq_packet {
                    ui.label(
                        RichText::new("CQ packet: only the callsign is sent")
                            .weak()
                            .size(11.0),
                    );
                } else {
                    let bytes = decdsss_link::text::tidy(&self.message).len();
                    ui.label(
                        RichText::new(format!(
                            "{bytes} / {} bytes",
                            decdsss_link::text::MAX_TEXT_BYTES
                        ))
                        .weak()
                        .size(11.0),
                    );
                }
            });
        });
    }

    /// Keys up and starts streaming the microphone.
    fn start_live(&mut self, devices_out: &[(String, u32)], devices_in: &[String]) {
        if self.live.is_some() {
            return;
        }
        let Some(call) = normalise_call(&self.call) else {
            self.note = "enter your callsign (letters, digits and /, up to 9)".into();
            return;
        };
        let Some(spec) = self.voice_spec() else {
            self.note = "this bandwidth is too narrow for voice".into();
            return;
        };
        let (out_device, out_rate) = self.output(devices_out);
        let mic_device = self.vp.mic_name(devices_in);
        let session = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            % 64) as u8;
        let cfg = LiveConfig {
            profile: spec.profile,
            mode: spec.mode,
            depth: spec.depth,
            call,
            session,
            codec: spec.codec,
            level: self.level,
            vox: self.vox,
            wfid: self.wfid,
            out_device,
            out_rate,
            mic_device,
            radio: self.radio.clone(),
        };
        match LiveTxSession::start(&cfg) {
            Ok(s) => {
                self.note.clear();
                self.live = Some(s);
            }
            Err(e) => self.note = e,
        }
    }

    /// Whether a live transmission is going out.
    pub fn live_active(&self) -> bool {
        self.live.is_some()
    }

    /// Cuts a live transmission at once.
    pub fn abort_live(&mut self) {
        if let Some(s) = self.live.take() {
            s.abort();
        }
    }

    /// The speech controls and what sending the clip comes to.
    fn voice_section(
        &mut self,
        ui: &mut egui::Ui,
        devices_in: &[String],
        engine: &Engine,
        devices_out: &[(String, u32)],
        listen: &(Option<String>, u32),
    ) {
        ui.horizontal(|ui| {
            ui.label("Voice");
            ui.selectable_value(&mut self.live_mode, false, "overs")
                .on_hover_text("Hold to talk, let go and it is sent as one over");
            ui.selectable_value(&mut self.live_mode, true, "live")
                .on_hover_text(
                    "Streamed while you hold the PTT, and played as it arrives at the other end",
                );
        });
        let on_air = self.live.is_some();
        let play = self.vp.ui(ui, devices_in, self.live_mode, on_air);
        match play {
            Play::Send => self.send_transmission(engine, devices_out),
            Play::LiveStart => self.start_live(devices_out, devices_in),
            Play::LiveStop => {
                if let Some(s) = &self.live {
                    s.stop();
                }
            }
            _ => self.play_voice(play, engine, listen),
        }
        if let Some(s) = &self.live {
            let st = s.status();
            ui.horizontal(|ui| {
                ui.colored_label(
                    crate::scope::col::BAD,
                    match st.phase {
                        Phase::Starting => format!(
                            "● ON AIR: keying up, speech goes out in {:.0} s",
                            (st.lead_s - st.elapsed).max(0.0)
                        ),
                        Phase::OnAir => format!("● ON AIR  {:.0} s", st.elapsed - st.lead_s),
                        Phase::Ending => "sending the end".into(),
                        Phase::Done => String::new(),
                    },
                );
                ui.add(
                    egui::ProgressBar::new(st.mic_peak.clamp(0.0, 1.0))
                        .desired_width(90.0)
                        .fill(crate::scope::col::LOCK),
                );
            });
            ui.label(
                RichText::new(format!(
                    "{:.1} s of speech waiting to go out  ·  {} blocks sent, {} fillers",
                    st.backlog_s, st.blocks, st.fillers
                ))
                .size(11.0),
            );
        }
        ui.horizontal(|ui| {
            ui.label("Codec2");
            let auto =
                default_codec(self.profile).map_or("none".to_string(), |c| c.name().to_string());
            let before = self.codec;
            ui.selectable_value(&mut self.codec, None, format!("automatic ({auto})"));
            for c in CODECS {
                ui.selectable_value(&mut self.codec, Some(c), c.name());
            }
            // A new codec brings the mode that carries it along (1200 and 1600 need
            // DS2 or DS1, 450 does not); the button below still says what is suggested.
            if self.codec != before
                && let Some(vm) = self.suggested_voice_mode()
            {
                self.mode = vm.name();
            }
        });
        if let Some(vm) = self.suggested_voice_mode()
            && vm.name() != self.mode
        {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} is the voice mode for {}",
                    vm.name(),
                    self.codec
                        .or_else(|| default_codec(self.profile))
                        .map_or_else(
                            || self.profile.to_string(),
                            |c| { format!("Codec2 {c} on {}", self.profile) }
                        )
                ));
                if ui.button("Use it").clicked() {
                    self.mode = vm.name();
                }
            });
        }
        // Compressing a minute of speech takes a few milliseconds: do it when
        // the clip, the mode or the codec has changed, not every frame.
        let Some(clip) = &self.vp.clip else { return };
        let Some(spec) = self.voice_spec() else {
            ui.colored_label(crate::scope::col::BAD, "500 Hz is too narrow for voice");
            return;
        };
        let key = format!(
            "{} {} {} {} {} {} {}",
            clip.pcm.len(),
            clip.pcm.first().copied().unwrap_or(0),
            spec.profile,
            spec.mode,
            spec.depth,
            spec.codec,
            spec.resync_s
        );
        if self.voice_summary.as_ref().is_none_or(|(k, _)| *k != key) {
            let result = plan_voice(&spec, &clip.pcm).map(|p| {
                let s = format!(
                    "{:.1} s of speech → {} frames, {:.0} s of signal; the payload runs at {:.2}× the codec's rate{}",
                    p.speech_seconds,
                    p.frames,
                    p.air_seconds,
                    p.throughput_ratio,
                    if p.throughput_ratio < 1.0 {
                        " (slower than the speech: the other end waits for it all)"
                    } else {
                        ""
                    }
                );
                (s, Timeline::of_segments(&p.segments))
            });
            self.voice_summary = Some((key, result));
        }
        match self.voice_summary.as_ref().map(|(_, r)| r) {
            Some(Ok((s, _))) => {
                ui.label(RichText::new(s).size(11.0));
            }
            Some(Err(e)) => {
                ui.colored_label(crate::scope::col::BAD, e);
            }
            None => {}
        }
    }

    /// After the audio API changed and the device lists with it: keeps the transmit output
    /// and the microphone on the same cards (the lists are indexed, so the numbers moved).
    pub fn remap_devices(
        &mut self,
        old_out: &[(String, u32)],
        new_out: &[(String, u32)],
        old_in: &[String],
        new_in: &[String],
    ) {
        self.device = self
            .device
            .checked_sub(1)
            .and_then(|i| old_out.get(i))
            .map_or(0, |d| {
                matching_device(new_out.iter().map(|d| d.0.as_str()), &d.0)
            });
        self.vp.remap_mic(old_in, new_in);
    }

    pub fn remember(&self, p: &mut Prefs) {
        p.set("call", self.call.clone());
        p.set("tx_profile", self.profile.name());
        p.set("tx_slot", self.slot.name());
        p.set("tx_mode", self.mode.clone());
        p.set("tx_depth", self.depth.to_string());
        p.set("tx_air", format!("{:.0}", self.air));
        p.set("tx_wfid", if self.wfid { "1" } else { "0" });
    }

    /// Where in the passband the signal goes: the profile's place, or for 500 Hz
    /// the slot chosen.
    fn band(&self) -> decdsss_core::Band {
        match self.profile {
            Profile::N500 => decdsss_core::Band::new(Profile::N500, self.slot),
            p => decdsss_core::Band::mid(p),
        }
    }

    /// The transmission as the picture will be sent, `passes` of it.
    fn picture_spec(&self, passes: usize) -> Option<SendSpec> {
        Some(SendSpec {
            profile: self.profile,
            mode: Mode::from_name(&self.mode)?,
            depth: self.depth,
            call: "AA0AA".into(),
            session: 0,
            ptype: PayloadType::Picture,
            resync_s: self.resync,
            parity: self.parity / 100.0,
            passes,
        })
    }

    /// What the picture should be prepared for now, if it is to be sent.
    fn wanted_key(&self) -> Option<PicKey> {
        if !self.picture {
            return None;
        }
        let path = self.file.clone()?;
        let spec = self.picture_spec(1)?;
        let overhead = container::pack(&picture_name(&path), &[]).len();
        Some(PicKey {
            budget: max_stream_bytes(&spec, self.air).saturating_sub(overhead),
            min_quality: self.min_quality,
            path,
        })
    }

    /// Collects a finished picture job and starts the next if what is wanted
    /// has changed.
    fn drive_picture(&mut self, ctx: &Context) {
        if let Some(job) = self.pic_job.take() {
            match job.rx.try_recv() {
                Ok((src, res)) => {
                    if let Some(s) = src {
                        self.pic_source = Some((job.key.path.clone(), s));
                    }
                    self.pic_result = Some((job.key, res));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.pic_job = Some(job);
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pic_result = Some((job.key, Err("the picture worker stopped".into())));
                }
            }
        }
        if self.pic_job.is_some() {
            return;
        }
        let Some(key) = self.wanted_key() else { return };
        if self.pic_result.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        if key.budget < 200 {
            let e = format!(
                "{:.0} s of {} {} carries only {} bytes: raise the air time or choose a faster mode",
                self.air, self.profile, self.mode, key.budget
            );
            self.pic_result = Some((key, Err(e)));
            return;
        }
        let cached = self
            .pic_source
            .as_ref()
            .filter(|(p, _)| *p == key.path)
            .map(|(_, s)| s.clone());
        let (tx, rx) = mpsc::channel();
        let job_key = key.clone();
        std::thread::spawn(move || {
            let _ = tx.send(prepare(&job_key, cached));
        });
        self.pic_job = Some(PicJob { key, rx });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// The prepared picture and what sending it costs.
    fn picture_status(&mut self, ui: &mut egui::Ui) {
        if self.pic_job.is_some() {
            ui.label(RichText::new("preparing the picture…").weak());
        }
        let passes = self.repeat.max(1);
        let spec = self.picture_spec(passes);
        let Some((key, res)) = &self.pic_result else {
            return;
        };
        match res {
            Err(e) => {
                ui.colored_label(crate::scope::col::BAD, e);
            }
            Ok(p) => {
                let tex = match &self.pic_tex {
                    Some((k, t)) if k == key => t.clone(),
                    _ => {
                        let img = ColorImage::from_rgba_unmultiplied(
                            [p.width as usize, p.height as usize],
                            &p.preview,
                        );
                        let t = ui
                            .ctx()
                            .load_texture("tx-picture", img, TextureOptions::LINEAR);
                        self.pic_tex = Some((key.clone(), t.clone()));
                        t
                    }
                };
                let scale = (280.0 / p.width as f32)
                    .min(200.0 / p.height as f32)
                    .min(8.0);
                ui.add(egui::Image::new((
                    tex.id(),
                    egui::vec2(p.width as f32 * scale, p.height as f32 * scale),
                )));
                let stream = container::pack(&p.name, &p.jpeg).len();
                let air = spec
                    .and_then(|s| air_seconds(&s, stream).ok())
                    .map_or(String::new(), |s| format!(", {s:.0} s of signal"));
                ui.label(
                    RichText::new(format!(
                        "{}×{} → {}×{} progressive JPEG, quality {}, {} bytes{air}",
                        p.source.0,
                        p.source.1,
                        p.width,
                        p.height,
                        p.quality,
                        p.jpeg.len()
                    ))
                    .size(11.0),
                );
            }
        }
    }

    /// The rate the selected output device runs at.
    fn device_rate(&self, devices_out: &[(String, u32)]) -> u32 {
        if self.device == 0 {
            self.rate
        } else {
            devices_out.get(self.device - 1).map_or(self.rate, |d| d.1)
        }
    }

    /// What the waterfall ID writes: `call` and the mode (CQ for a CQ packet, which
    /// goes out in its own mode whatever is chosen).
    fn wfid_words(&self, call: &str) -> [String; 2] {
        let said = if self.text && self.cq_packet {
            "CQ".to_string()
        } else {
            Mode::from_name(&self.mode).map_or_else(|| self.mode.clone(), |m| m.to_string())
        };
        [call.to_string(), said]
    }

    /// Seconds the waterfall ID takes if it is on (the gap after it included).
    fn wfid_seconds(&self) -> f64 {
        if !self.wfid {
            return 0.0;
        }
        let call = normalise_call(&self.call).unwrap_or_else(|| "CALL".into());
        let [call, said] = self.wfid_words(&call);
        banner::seconds(&[&call, &said], self.band())
    }

    /// The leads asked for, ahead of the audio: the VOX tone and pause, then the
    /// waterfall ID of `call` (the callsign and mode written on the spectrum).
    fn with_lead(&self, rate: u32, call: &str, audio: Vec<f32>) -> Vec<f32> {
        let mut v = Vec::new();
        if self.vox {
            v = vox_lead(f64::from(rate), self.level, VOX_TONE_S, VOX_GAP_S);
        }
        if self.wfid {
            let [call, said] = self.wfid_words(call);
            v.extend(banner::banner(
                &[&call, &said],
                self.band(),
                f64::from(rate),
                self.level,
            ));
        }
        v.extend(audio);
        v
    }

    fn build(&self, rate: u32) -> Result<Plan, String> {
        let call = normalise_call(&self.call)
            .ok_or("enter your callsign (letters, digits and /, up to 9)")?;
        let session = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            % 64) as u8;
        if self.text && self.cq_packet {
            // Header only: the mode, depth and parity do not apply (`link::cq`).
            let segs = decdsss_link::cq::plan_cq(
                self.profile,
                &call,
                session,
                self.repeat.max(decdsss_link::cq::DEFAULT_COPIES),
            )?;
            let chips = transmission_chips(&segs).ok_or("cannot build the header")?;
            let audio =
                chips_to_audio_at(&chips, self.band(), f64::from(rate), self.level, 1.0, 1.0);
            let audio = self.with_lead(rate, &call, audio);
            return Ok(Plan {
                frames: 0,
                segments: segs.len(),
                seconds: audio.len() as f64 / f64::from(rate),
                audio,
            });
        }
        if self.text {
            let mode = Mode::from_name(&self.mode).ok_or("choose a mode")?;
            let spec = SendSpec {
                profile: self.profile,
                mode,
                depth: 1,
                call: call.clone(),
                session,
                ptype: PayloadType::Text,
                resync_s: self.resync,
                parity: self.parity / 100.0,
                passes: self.repeat.max(1),
            };
            let plan = decdsss_link::plan_text(&spec, &self.message)?;
            let chips = transmission_chips(&plan.segments).ok_or("cannot build the header")?;
            let audio =
                chips_to_audio_at(&chips, self.band(), f64::from(rate), self.level, 1.0, 1.0);
            let audio = self.with_lead(rate, &call, audio);
            return Ok(Plan {
                frames: plan.frames(),
                segments: plan.segments.len(),
                seconds: audio.len() as f64 / f64::from(rate),
                audio,
            });
        }
        if self.voice {
            let clip = self
                .vp
                .clip
                .as_ref()
                .ok_or("record some speech or choose a WAV")?;
            let mut spec = self
                .voice_spec()
                .ok_or("this bandwidth is too narrow for voice")?;
            spec.call = call.clone();
            spec.session = session;
            let plan = plan_voice(&spec, &clip.pcm)?;
            let chips = transmission_chips(&plan.segments).ok_or("cannot build the header")?;
            let audio =
                chips_to_audio_at(&chips, self.band(), f64::from(rate), self.level, 1.0, 1.0);
            let audio = self.with_lead(rate, &call, audio);
            return Ok(Plan {
                frames: plan.frames,
                segments: plan.segments.len(),
                seconds: audio.len() as f64 / f64::from(rate),
                audio,
            });
        }
        let path = self.file.as_ref().ok_or("choose a file")?;
        let mode = Mode::from_name(&self.mode).ok_or("choose a mode")?;
        let (name, data, ptype) = if self.picture {
            let key = self.wanted_key().ok_or("choose a picture")?;
            match &self.pic_result {
                Some((k, Ok(p))) if *k == key => {
                    (p.name.clone(), p.jpeg.clone(), PayloadType::Picture)
                }
                Some((_, Err(e))) => return Err(e.clone()),
                _ => return Err("the picture is still being prepared: try again".into()),
            }
        } else {
            let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path
                .file_name()
                .map_or("file".to_string(), |n| n.to_string_lossy().into_owned());
            (name, data, PayloadType::File)
        };
        let spec = SendSpec {
            profile: self.profile,
            mode,
            depth: self.depth,
            call: call.clone(),
            session,
            ptype,
            resync_s: self.resync,
            parity: self.parity / 100.0,
            passes: self.repeat.max(1),
        };
        let segs = segments(&spec, &container::pack(&name, &data))?;
        let chips = transmission_chips(&segs).ok_or("cannot build the header")?;
        let audio = chips_to_audio_at(&chips, self.band(), f64::from(rate), self.level, 1.0, 1.0);
        let audio = self.with_lead(rate, &call, audio);
        Ok(Plan {
            frames: segs.iter().map(|s| s.frames.len()).sum(),
            segments: segs.len(),
            seconds: audio.len() as f64 / f64::from(rate),
            audio,
        })
    }

    /// What runs whether or not the transmit controls are on show: the picture
    /// being prepared and a recording being taken. Call every frame.
    pub fn background(&mut self, ctx: &Context) {
        self.drive_picture(ctx);
        self.vp.tick(ctx);
        if let Some(s) = &self.live {
            let st = s.status();
            if st.phase == Phase::Done {
                if let Some(e) = st.error {
                    self.note = e;
                }
                self.live = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
    }

    /// The transmit controls, and beside them (under them in a narrow panel) what
    /// goes on air, then Play and Save.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        engine: &Engine,
        state: &TxState,
        devices_out: &[(String, u32)],
        devices_in: &[String],
        listen: &(Option<String>, u32),
    ) {
        ui.label(
            RichText::new(
                "Plays audio to a sound card (or saves a WAV). You key the radio. \
                 Spread spectrum is restricted on HF in some countries: check your licence.",
            )
            .weak()
            .size(11.0),
        );
        let avail = ui.available_width();
        if avail >= TWO_COLUMNS {
            let gap = 2.0 * ui.spacing().item_spacing.x;
            let left = (avail * 0.5).min(460.0);
            let right = (avail - left - gap).min(560.0);
            let top_down = egui::Layout::top_down(egui::Align::Min);
            // `horizontal_top` lines the two columns up at their tops; each is a
            // top-down `Ui` of its own, as wide as asked and as tall as it needs.
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(egui::vec2(left, 0.0), top_down, |ui| {
                    ui.set_width(left);
                    self.controls(ui, engine, devices_out, devices_in, listen);
                });
                ui.add_space(gap);
                ui.allocate_ui_with_layout(egui::vec2(right, 0.0), top_down, |ui| {
                    ui.set_width(right);
                    self.preview(ui);
                });
            });
        } else {
            self.controls(ui, engine, devices_out, devices_in, listen);
            ui.add_space(6.0);
            self.preview(ui);
        }
        ui.separator();
        self.actions(ui, engine, state, devices_out);
    }

    /// What to send and how: the grid of settings, and the speech controls when
    /// sending voice.
    fn controls(
        &mut self,
        ui: &mut egui::Ui,
        engine: &Engine,
        devices_out: &[(String, u32)],
        devices_in: &[String],
        listen: &(Option<String>, u32),
    ) {
        egui::Grid::new("txgrid").num_columns(2).show(ui, |ui| {
            ui.label(if self.voice {
                "Speech"
            } else if self.text {
                "Message"
            } else {
                "File"
            });
            // The message box is taller than a line, so it is not inside a horizontal
            // layout (which would size the row to one line and let it spill over the next).
            if self.text {
                self.message_editor(ui);
            } else {
                ui.horizontal(|ui| {
                if self.voice {
                    ui.label(RichText::new("record, or choose a WAV, below").weak());
                } else if ui.button("Choose…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("Pictures", &PICTURE_EXTENSIONS)
                        .add_filter("All files", &["*"])
                        .pick_file()
                {
                    // A picture is sent as one unless the box is cleared.
                    self.picture = is_picture(&p);
                    self.file = Some(p);
                }
                if !self.voice {
                    ui.label(
                        self.file
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map_or("none".into(), |n| n.to_string_lossy().into_owned()),
                    );
                }
                });
            }
            ui.end_row();
            ui.label("Send as");
            ui.horizontal(|ui| {
                let mut kind = if self.voice {
                    2
                } else if self.text {
                    3
                } else {
                    usize::from(self.picture)
                };
                ui.radio_value(&mut kind, 0, "file");
                ui.radio_value(&mut kind, 1, "picture")
                    .on_hover_text("Scaled and compressed to the air time");
                ui.radio_value(&mut kind, 2, "voice")
                    .on_hover_text("Speech, as Codec2");
                ui.radio_value(&mut kind, 3, "text")
                    .on_hover_text("A CQ call or a few lines of chat, shown as text on the receiver");
                let was_voice = self.voice;
                self.picture = kind == 1;
                self.voice = kind == 2;
                self.text = kind == 3;
                if self.voice
                    && !was_voice
                    && let Some(vm) = self.suggested_voice_mode()
                {
                    self.mode = vm.name();
                }
            });
            ui.end_row();
            if self.picture {
                ui.label("Air time");
                ui.add(
                    egui::Slider::new(&mut self.air, 20.0..=900.0)
                        .suffix(" s a pass")
                        .logarithmic(true),
                );
                ui.end_row();
                ui.label("Quality at least");
                ui.add(egui::Slider::new(&mut self.min_quality, 5..=90));
                ui.end_row();
            }
            ui.label("Callsign");
            ui.text_edit_singleline(&mut self.call);
            ui.end_row();
            ui.label("VOX");
            ui.checkbox(
                &mut self.vox,
                "a tone ahead of the signal, to key the radio",
            )
            .on_hover_text(format!(
                "{VOX_TONE_S} s of 1 kHz and a {VOX_GAP_S} s pause, so the radio is \
                 transmitting before the preamble"
            ));
            ui.end_row();
            ui.label("Waterfall ID");
            let call = normalise_call(&self.call).unwrap_or_else(|| "CALL".into());
            let [call, said] = self.wfid_words(&call);
            let parts = [call.as_str(), said.as_str()];
            ui.checkbox(
                &mut self.wfid,
                "write the callsign and mode on the waterfall first",
            )
            .on_hover_text(format!(
                "{:.1} s of tones whose spectrogram spells {:?} across the band the \
                 signal will fill, then a pause, before the preamble: anyone watching \
                 the band sees who is calling and in what mode. Nothing decodes it.",
                banner::seconds(&parts, self.band()),
                banner::lines(&parts, self.band()).join(" / ")
            ));
            ui.end_row();
            ui.label("Bandwidth");
            ui.horizontal(|ui| {
                for p in Profile::ALL {
                    ui.selectable_value(&mut self.profile, p, p.label());
                }
            });
            ui.end_row();
            if self.profile == Profile::N500 {
                ui.label("Slot");
                ui.horizontal(|ui| {
                    for s in Slot::ALL {
                        let hz = Band::new(Profile::N500, s).centre_hz();
                        ui.selectable_value(
                            &mut self.slot,
                            s,
                            match s {
                                Slot::Low => "−500 Hz",
                                Slot::Mid => "middle",
                                Slot::High => "+500 Hz",
                            },
                        )
                        .on_hover_text(format!(
                            "Audio centre {hz:.0} Hz. Up to three 500 Hz stations fit side by side in a 2.4 kHz filter; the receiver listens on all three."
                        ));
                    }
                });
                ui.end_row();
            }
            // The family is read from the chosen mode rather than kept beside it,
            // so the two cannot disagree (a voice mode or a saved setting moves both).
            let current = Mode::from_name(&self.mode);
            let family = current.map_or(Family::DsPsk, |m| m.family);
            ui.label("Spreading");
            ui.horizontal(|ui| {
                for (f, label, tip) in FAMILIES {
                    if ui.selectable_label(family == f, label).on_hover_text(tip).clicked()
                        && family != f
                        && let Some(m) = nearest_mode(f, current, self.profile)
                    {
                        self.mode = m.name();
                    }
                }
            });
            ui.end_row();
            ui.label("Mode");
            egui::ComboBox::from_id_salt("txmode")
                .selected_text(&self.mode)
                .show_ui(ui, |ui| {
                    for m in Mode::all().into_iter().filter(|m| m.family == family) {
                        ui.selectable_value(
                            &mut self.mode,
                            m.name(),
                            format!(
                                "{}   {:.0} bit/s, needs ≈ {:.0} dB SNR",
                                m.name(),
                                m.info_bit_rate(self.profile),
                                m.snr_offset_db(self.profile) + m.ebn0_needed_db(self.profile).0
                            ),
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
            ui.add(
                egui::Slider::new(&mut self.resync, 15.0..=300.0)
                    .suffix(" s")
                    .logarithmic(true),
            );
            ui.end_row();
            // Speech has no coded frames or passes: it goes out once, as it is.
            if !self.voice {
                ui.label("Coded frames");
                ui.add(egui::Slider::new(&mut self.parity, 0.0..=100.0).suffix(" % extra"));
                ui.end_row();
                ui.label("Passes");
                ui.add(egui::Slider::new(&mut self.repeat, 1..=5));
                ui.end_row();
            }
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
                        ui.selectable_value(
                            &mut self.device,
                            i + 1,
                            format!("{} ({} Hz)", d.0, d.1),
                        );
                    }
                });
            ui.end_row();
        });
        if self.voice {
            self.voice_section(ui, devices_in, engine, devices_out, listen);
        }
    }

    /// What goes on air: where the signal sits in the audio passband, the
    /// transmission to scale, the mode's rates and, for a picture, the picture.
    fn preview(&mut self, ui: &mut egui::Ui) {
        let band = self.band();
        let (lo, hi) = band.edges_hz();
        caption(ui, "Audio band", format!("{lo:.0}–{hi:.0} Hz"));
        band_strip(ui, band).on_hover_text(format!(
            "{} chip/s × (1 + {ROLLOFF} roll-off) = {:.0} Hz occupied, {lo:.0}–{hi:.0} Hz{}",
            band.profile.chip_rate(),
            band.profile.bandwidth_hz(),
            if band.profile == Profile::N500 {
                "; the outlines are the other two 500 Hz slots"
            } else {
                ""
            }
        ));
        ui.add_space(8.0);
        self.transmission(ui);
        // A CQ packet is headers only, in its own mode: the mode's rates do not apply.
        if !(self.text && self.cq_packet) {
            ui.add_space(8.0);
            self.rate_tiles(ui);
        }
        if self.picture {
            ui.add_space(8.0);
            self.picture_status(ui);
        }
    }

    /// The transmission as set up, worked out without building it.
    fn timeline(&self) -> Result<Timeline, NoTimeline> {
        use NoTimeline::{Problem, Waiting};
        if self.text && self.cq_packet {
            let call = normalise_call(&self.call).ok_or(Waiting("enter your callsign"))?;
            let copies = self.repeat.max(decdsss_link::cq::DEFAULT_COPIES);
            let segs =
                decdsss_link::cq::plan_cq(self.profile, &call, 0, copies).map_err(Problem)?;
            return Timeline::of_segments(&segs).ok_or(Waiting("no copies to send"));
        }
        if self.voice {
            if self.vp.clip.is_none() {
                return Err(Waiting("record some speech or choose a WAV"));
            }
            // `voice_section` plans the speech when the clip or the settings change.
            return match self.voice_summary.as_ref().map(|(_, r)| r) {
                Some(Ok((_, Some(t)))) => Ok(t.clone()),
                Some(Err(e)) => Err(Problem(e.clone())),
                _ => Err(Waiting("planning the speech…")),
            };
        }
        let mode = Mode::from_name(&self.mode).ok_or(Waiting("choose a mode"))?;
        let spec = |ptype, depth| SendSpec {
            profile: self.profile,
            mode,
            depth,
            call: String::new(),
            session: 0,
            ptype,
            resync_s: self.resync,
            parity: self.parity / 100.0,
            passes: self.repeat.max(1),
        };
        if self.text {
            let text = decdsss_link::text::tidy(&self.message);
            if text.is_empty() {
                return Err(Waiting("type a message, or press CQ"));
            }
            // As `plan_text` sends it: at depth 1, in a container with no name.
            let stream = container::pack("", text.as_bytes()).len();
            return timeline(&spec(PayloadType::Text, 1), stream).map_err(Problem);
        }
        let path = self.file.as_ref().ok_or(Waiting("choose a file"))?;
        let (stream, ptype) = if self.picture {
            match (&self.pic_result, self.wanted_key()) {
                (Some((k, Ok(p))), Some(want)) if *k == want => (
                    container::pack(&p.name, &[]).len() + p.jpeg.len(),
                    PayloadType::Picture,
                ),
                // `picture_status` says what is wrong with it.
                (Some((k, Err(_))), Some(want)) if *k == want => {
                    return Err(Waiting("no picture to send"));
                }
                _ => return Err(Waiting("preparing the picture…")),
            }
        } else {
            let name = path
                .file_name()
                .map_or("file".to_string(), |n| n.to_string_lossy().into_owned());
            let len = std::fs::metadata(path)
                .map_err(|e| Problem(format!("{}: {e}", path.display())))?
                .len();
            // The container adds a fixed head and tail to the file (`container::pack`).
            (
                container::pack(&name, &[]).len() + len as usize,
                PayloadType::File,
            )
        };
        timeline(&spec(ptype, self.depth), stream).map_err(Problem)
    }

    /// The transmission to scale and what it adds up to.
    fn transmission(&self, ui: &mut egui::Ui) {
        let t = self.timeline();
        let vox_s = if self.vox {
            VOX_TONE_S + VOX_GAP_S
        } else {
            0.0
        };
        let wfid_s = self.wfid_seconds();
        let total = t
            .as_ref()
            .map_or(String::new(), |t| air_time(vox_s + wfid_s + t.seconds()));
        caption(ui, "Transmission", total);
        let t = match t {
            Ok(t) => t,
            Err(NoTimeline::Waiting(why)) => {
                ui.label(RichText::new(why).weak().size(11.0));
                return;
            }
            Err(NoTimeline::Problem(e)) => {
                ui.colored_label(col::BAD, RichText::new(e).size(11.0));
                return;
            }
        };
        let blocks: usize = t.segments.iter().sum();
        let mut tip = format!(
            "To scale. Preamble and header {:.2} s, × {}\n{blocks} blocks of {:.2} s\nPostamble {:.2} s",
            t.lead_s,
            t.segments.len(),
            t.block_s,
            t.eot_s
        );
        if vox_s > 0.0 {
            tip = format!("{tip}\nVOX tone and pause {vox_s:.1} s");
        }
        if wfid_s > 0.0 {
            tip = format!("{tip}\nWaterfall ID and pause {wfid_s:.1} s");
        }
        timeline_bar(ui, &t, &[(vox_s, col::REF), (wfid_s, col::LOCK)]).on_hover_text(tip);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if vox_s > 0.0 {
                swatch(ui, col::REF, "VOX");
            }
            if wfid_s > 0.0 {
                swatch(ui, col::LOCK, "waterfall ID");
            }
            swatch(ui, col::MARK, "preamble + header");
            if t.frames > 0 {
                swatch(ui, col::TRACE, "data");
            }
            if t.frames > t.data_frames {
                swatch(ui, CODED, "coded");
            }
        });
        let segments = match t.segments.len() {
            1 => "1 segment".to_string(),
            n => format!("{n} segments"),
        };
        let what = if t.frames == 0 {
            // A CQ packet: each segment is one copy of the header.
            match t.segments.len() {
                1 => "one header, nothing after it".to_string(),
                n => format!("{n} copies of the header, nothing after them"),
            }
        } else if t.frames == t.data_frames {
            format!("{} frames in {segments}", t.frames)
        } else {
            let passes = t.frames / t.per_pass.max(1);
            format!(
                "{} data + {} coded frames in {segments}{}",
                t.data_frames,
                t.frames - t.data_frames,
                if passes > 1 {
                    format!(", {passes} passes")
                } else {
                    String::new()
                }
            )
        };
        ui.label(RichText::new(what).size(11.0));
    }

    /// The mode's numbers: its information rate, the SNR it needs, a frame's data
    /// and a block's length.
    fn rate_tiles(&self, ui: &mut egui::Ui) {
        let Some(m) = Mode::from_name(&self.mode) else {
            return;
        };
        // A message goes out at depth 1: a deeper block would pad it with empty frames.
        let depth = if self.text { 1 } else { self.depth };
        let layout = Layout::new(self.profile, m, depth);
        let rb = m.info_bit_rate(self.profile);
        let (ebn0, measured) = m.ebn0_needed_db(self.profile);
        let offset = m.snr_offset_db(self.profile);
        // `columns` splits the width into equal parts, one `Ui` each.
        ui.columns(4, |c| {
            tile(
                &mut c[0],
                "info rate",
                if rb >= 100.0 {
                    format!("{rb:.0}")
                } else {
                    format!("{rb:.1}")
                },
                "bit/s",
            )
            .on_hover_text(format!(
                "{}: the coded rate × the code rate, pilots taken out; preambles, headers \
                 and CRCs come off this",
                m.name()
            ));
            tile(&mut c[1], "needs SNR", format!("≈ {:.0}", ebn0 + offset), "dB").on_hover_text(
                format!(
                    "In the {} occupied width: Eb/N0 {ebn0:.1} dB ({}) + 10·log10(Rb / B) {offset:.1} dB",
                    self.profile,
                    if measured {
                        "measured"
                    } else {
                        "not measured: 4 dB assumed"
                    }
                ),
            );
            tile(&mut c[2], "a frame", layout.frame_bytes().to_string(), "bytes")
                .on_hover_text("Data bytes a frame carries");
            tile(&mut c[3], "a block", format!("{:.1}", layout.seconds()), "s")
                .on_hover_text(format!("{depth} frame(s) interleaved"));
        });
    }

    /// Play and Save, the transmission under way, and what came of the last one.
    fn actions(
        &mut self,
        ui: &mut egui::Ui,
        engine: &Engine,
        state: &TxState,
        devices_out: &[(String, u32)],
    ) {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!state.active, |ui| {
                if ui.button("▶ Play to the sound card").clicked() {
                    self.send_transmission(engine, devices_out);
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
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new((state.played / state.seconds.max(1.0)) as f32)
                        .desired_width(200.0)
                        .text(format!(
                            "sending {:.0} / {:.0} s",
                            state.played, state.seconds
                        )),
                );
                if ui
                    .add(
                        egui::Button::new(RichText::new("■ Stop transmission").strong())
                            .fill(col::BAD),
                    )
                    .clicked()
                {
                    engine.send(Cmd::StopTransmit);
                }
            });
        } else if state.stopped {
            ui.label(RichText::new("stopped").weak());
        }
        if let Some(e) = &state.error {
            ui.colored_label(col::BAD, e);
        }
        if !self.note.is_empty() {
            ui.label(RichText::new(&self.note).size(11.0));
        }
    }
}

/// Why there is no transmission to draw.
enum NoTimeline {
    /// Nothing to send yet: what to do about it.
    Waiting(&'static str),
    /// What is set up cannot be sent.
    Problem(String),
}

/// Coded frames: the data colour, paler (a dimmer one vanished into the panel).
const CODED: Color32 = Color32::from_rgb(170, 216, 242);

/// A small heading with a figure on the right of the same line.
fn caption(ui: &mut egui::Ui, title: &str, figure: String) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).weak().size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(figure).size(11.0));
        });
    });
}

/// Seconds as `43.2 s`, `2 min 23 s` or `1 h 38 min`.
fn air_time(s: f64) -> String {
    if s < 60.0 {
        return format!("{s:.1} s");
    }
    let s = s.round() as u64;
    if s < 3600 {
        format!("{} min {:02} s", s / 60, s % 60)
    } else {
        let m = (s + 30) / 60;
        format!("{} h {:02} min", m / 60, m % 60)
    }
}

/// The audio passband from 0 to 3.5 kHz with `band` filled in, and for 500 Hz
/// the other two slots outlined (where two more stations can go).
fn band_strip(ui: &mut egui::Ui, band: Band) -> egui::Response {
    const TOP_HZ: f64 = 3500.0;
    let w = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(w, 40.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let x = |hz: f64| rect.left() + (hz / TOP_HZ) as f32 * rect.width();
    let base = rect.top() + 24.0;
    let span = |(a, b): (f64, f64), top: f32| {
        egui::Rect::from_min_max(egui::pos2(x(a), top), egui::pos2(x(b), base))
    };
    if band.profile == Profile::N500 {
        for s in Slot::ALL {
            p.rect_stroke(
                span(Band::new(Profile::N500, s).edges_hz(), rect.top() + 6.0),
                2.0,
                egui::Stroke::new(1.0, col::REF),
                egui::StrokeKind::Inside,
            );
        }
    }
    let r = span(band.edges_hz(), rect.top() + 2.0);
    p.rect_filled(r, 2.0, col::TRACE.gamma_multiply(0.35));
    p.rect_stroke(
        r,
        2.0,
        egui::Stroke::new(1.0, col::TRACE),
        egui::StrokeKind::Inside,
    );
    let axis = egui::Stroke::new(1.0, col::AXIS);
    p.line_segment(
        [
            egui::pos2(rect.left(), base),
            egui::pos2(rect.right(), base),
        ],
        axis,
    );
    let weak = ui.visuals().weak_text_color();
    for (hz, label, anchor) in [
        (0.0, "0", egui::Align2::LEFT_TOP),
        (1000.0, "1 kHz", egui::Align2::CENTER_TOP),
        (2000.0, "2 kHz", egui::Align2::CENTER_TOP),
        (3000.0, "3 kHz", egui::Align2::CENTER_TOP),
    ] {
        let at = x(hz);
        p.line_segment([egui::pos2(at, base), egui::pos2(at, base + 3.0)], axis);
        p.text(
            egui::pos2(at, base + 4.0),
            anchor,
            label,
            egui::FontId::proportional(11.0),
            weak,
        );
    }
    response
}

/// The transmission drawn to scale: what goes ahead of the signal (`ahead`: seconds
/// and colour of the VOX tone, the waterfall ID), then each segment's preamble
/// and header and its blocks (data frames first, then coded ones), then the
/// postamble. The preambles and the postamble are drawn at least a pixel wide,
/// so they show on a long transmission.
fn timeline_bar(ui: &mut egui::Ui, t: &Timeline, ahead: &[(f64, Color32)]) -> egui::Response {
    let w = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(w, 18.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let lead_s: f64 = ahead.iter().map(|a| a.0).sum();
    let px = f64::from(rect.width()) / (lead_s + t.seconds());
    let piece = |from: f64, secs: f64, colour: Color32, min_px: f32| {
        let x0 = rect.left() + (from * px) as f32;
        let x1 = (rect.left() + ((from + secs) * px) as f32).max(x0 + min_px);
        p.rect_filled(
            egui::Rect::from_x_y_ranges(x0..=x1, rect.y_range()),
            0.0,
            colour,
        );
    };
    let mut from = 0.0;
    for &(secs, colour) in ahead {
        if secs > 0.0 {
            piece(from, secs, colour, 1.0);
        }
        from += secs;
    }
    // The bodies first, then the leads over them, so a lead widened to a pixel
    // is not painted over by the block after it.
    let mut leads = Vec::with_capacity(t.segments.len() + 1);
    let (mut at, mut frame0) = (lead_s, 0usize);
    for &n in &t.segments {
        leads.push(at);
        at += t.lead_s;
        let body = n as f64 * t.block_s;
        // Frames go `depth` to a block, the data ones first.
        let data =
            (t.data_frames.saturating_sub(frame0) as f64 / t.depth as f64 * t.block_s).min(body);
        if data > 0.0 {
            piece(at, data, col::TRACE, 0.0);
        }
        if body > data {
            piece(at + data, body - data, CODED, 0.0);
        }
        at += body;
        frame0 += n * t.depth;
    }
    for &from in &leads {
        piece(from, t.lead_s, col::MARK, 1.0);
    }
    piece(at, t.eot_s, col::MARK, 1.0);
    // Headers back to back (a CQ's copies) would run together: a hairline where
    // one segment with no blocks ends and the next begins.
    let line = egui::Stroke::new(1.0, ui.visuals().panel_fill);
    for (i, &from) in leads.iter().enumerate().skip(1) {
        if t.segments[i - 1] == 0 {
            let x = rect.left() + (from * px) as f32;
            p.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                line,
            );
        }
    }
    response
}

/// A legend entry: a square of `colour` and what it stands for.
fn swatch(ui: &mut egui::Ui, colour: Color32, text: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 1.0, colour);
    ui.label(RichText::new(text).weak().size(11.0));
    ui.add_space(6.0);
}

/// A figure with a small label over it and its unit after it.
fn tile(ui: &mut egui::Ui, label: &str, value: String, unit: &str) -> egui::Response {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(label).weak().size(11.0));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.label(RichText::new(value).strong().size(15.0));
                ui.label(RichText::new(unit).size(11.0));
            });
        })
        .response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> Mode {
        Mode::from_name(s).unwrap()
    }

    #[test]
    fn air_times_read_in_the_largest_sensible_unit() {
        assert_eq!(air_time(43.21), "43.2 s");
        assert_eq!(air_time(143.0), "2 min 23 s");
        assert_eq!(air_time(5879.0), "1 h 38 min");
    }

    #[test]
    fn family_switch_keeps_the_rate() {
        let p = Profile::W3000;
        // 32.8 bit/s: CSK's nearest is csk256-1/2 at 37.5 (0.6 dB away), and
        // back again lands where it started.
        assert_eq!(
            nearest_mode(Family::Csk, Some(m("ds64-1/2")), p),
            Some(m("csk256-1/2"))
        );
        assert_eq!(
            nearest_mode(Family::DsPsk, Some(m("csk256-1/2")), p),
            Some(m("ds64-1/2"))
        );
        // DS4 3/4 is 787.5 bit/s on 3k; csk8-3/4 is 675 (0.67 dB under).
        assert_eq!(
            nearest_mode(Family::Csk, Some(m("ds4-3/4")), p),
            Some(m("csk8-3/4"))
        );
        // Faster than any CSK mode: the fastest CSK mode (DS1 3/4 is 3150 bit/s on 3k).
        assert_eq!(
            nearest_mode(Family::Csk, Some(m("ds1-3/4")), p),
            Some(m("csk8-3/4"))
        );
        assert_eq!(nearest_mode(Family::Csk, None, p), Some(m("csk8-3/4")));
    }

    #[test]
    fn the_waterfall_id_goes_after_the_vox_lead_and_ahead_of_the_signal() {
        let mut p = TxPanel {
            call: "vk2abc".into(),
            text: true,
            cq_packet: true,
            ..TxPanel::default()
        };
        let rms = |a: &[f32]| (a.iter().map(|v| v * v).sum::<f32>() / a.len() as f32).sqrt();
        for vox in [false, true] {
            p.vox = vox;
            // The session number in the header comes from the clock, so two builds
            // either side of a second boundary differ: build the plain one again
            // after, and use the pair only if the clock did not tick between.
            let (plain, with) = loop {
                p.wfid = false;
                let plain = p.build(48_000).unwrap().audio;
                p.wfid = true;
                let with = p.build(48_000).unwrap().audio;
                p.wfid = false;
                if p.build(48_000).unwrap().audio == plain {
                    break (plain, with);
                }
            };
            p.wfid = true;
            // The banner's seconds are what the timeline says, and the rest is the
            // same audio as without it, after it.
            let banner_n = with.len() - plain.len();
            assert!((banner_n as f64 / 48_000.0 - p.wfid_seconds()).abs() < 1e-4);
            let lead = if vox { 33_600 } else { 0 };
            assert_eq!(with[..lead], plain[..lead]);
            assert_eq!(with[lead + banner_n..], plain[lead..]);
            // Tones, not silence, where it goes (the last half second is its gap).
            assert!(rms(&with[lead..lead + banner_n - 24_000]) > 0.02);
        }
        // A CQ says CQ, and anything else says its mode.
        assert_eq!(p.wfid_words("VK2ABC"), ["VK2ABC", "CQ"]);
        p.cq_packet = false;
        assert_eq!(p.wfid_words("VK2ABC"), ["VK2ABC", "DS8 1/2"]);
    }
}
