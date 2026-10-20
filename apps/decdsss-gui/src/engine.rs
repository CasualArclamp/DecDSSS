// made by CasualArclamp
//! The engine: one worker thread that owns the receivers. It reads audio from
//! a sound card, a WAV file or the built-in demo, feeds the spectrum and the
//! receivers, gathers frames into files, and publishes what the UI shows into
//! a shared state the UI polls. The UI never touches a receiver.

use crate::demo;
use crate::monitor::Monitor;
use crate::player::Player;
use crate::radio::Radio;
use crate::spectrum::Spectrum;
use decdsss_audio::{InputOptions, InputStream, OutputOptions, OutputStream, wav};
use decdsss_codec2::Mode as Codec;
use decdsss_core::Band;
use decdsss_link::{Assembler, ReceivedFile, ReceivedVoice};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver, Snapshot};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Columns the acquisition preview is reduced to.
const SEARCH_COLS: usize = 192;
/// Log lines kept.
const LOG_MAX: usize = 400;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A sound card or virtual cable; `None` is the system default input.
    Device(Option<String>),
    Wav(PathBuf),
    Demo,
    /// A KiwiSDR's audio stream: its address, the dial frequency (kHz), the
    /// sideband, and the name its owner sees.
    Kiwi {
        address: String,
        freq_khz: f64,
        lsb: bool,
        ident: String,
    },
}

#[derive(Clone, Debug)]
pub struct Config {
    pub source: Source,
    pub bands: Vec<Band>,
    pub out_dir: PathBuf,
    /// Faster than real time for files and the demo.
    pub speed: f64,
    /// Log the search too: preambles found, locked or lost.
    pub details: bool,
}

pub enum Cmd {
    Start(Config),
    Stop,
    /// Stops the transmission (or playback) in progress at once.
    StopTransmit,
    /// The output live speech is played on (speakers or headphones).
    Listen {
        device: Option<String>,
    },
    /// Speech (8 kHz) for the speech output, as if it had just been decoded from a
    /// live transmission (`last`: the end of it); for checking the output.
    Speech {
        pcm: Vec<i16>,
        last: bool,
    },
    /// The RF monitor: play what the receiver hears on an output device.
    Monitor {
        on: bool,
        device: Option<String>,
        volume: f32,
    },
    /// Plays audio (already at `rate`) on the output device. `key`: the audio
    /// goes to the radio, so the transmitter is keyed for it (if the CAT and PTT
    /// settings say how); false for audio to the speakers.
    Transmit {
        audio: Vec<f32>,
        rate: u32,
        device: Option<String>,
        key: bool,
    },
    Quit,
}

/// A text message that arrived whole (a CQ call, chat).
#[derive(Clone, Debug)]
pub struct Message {
    /// Seconds into the run it was complete.
    pub at_s: f64,
    pub call: String,
    pub session: u8,
    /// The band it was heard in (the 500 Hz slot, for that profile).
    pub band: String,
    pub text: String,
    /// A CQ packet (a header with the callsign), not a text message.
    pub packet: bool,
    /// Still arriving: `text` is the front that has come without a gap, and the
    /// finished message takes this entry's place.
    pub partial: bool,
}

/// Messages kept for the display.
const MESSAGES_MAX: usize = 200;

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub file: ReceivedFile,
    pub path: PathBuf,
}

/// A picture as far as it has arrived.
#[derive(Clone, Debug)]
pub struct PictureView {
    pub call: String,
    pub session: u8,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// RGBA, transparent where nothing has arrived yet.
    pub rgba: Vec<u8>,
    /// Bytes of the file in, without a gap, and the whole file's.
    pub bytes: usize,
    pub total: usize,
    /// Complete JPEG scans decoded.
    pub scans: usize,
    pub complete: bool,
    /// Changes whenever the picture does, so the UI knows to upload it again.
    pub version: u64,
}

/// Speech as far as it has arrived.
#[derive(Clone, Debug)]
pub struct VoiceView {
    pub call: String,
    pub session: u8,
    pub codec: Codec,
    /// Frames in, frames in all, and which are in.
    pub have: usize,
    pub total: usize,
    pub map: Vec<bool>,
    /// 8 kHz speech, silence where frames are missing.
    pub speech: Arc<Vec<i16>>,
    /// Lowest and highest sample in each column of the waveform, −1…1.
    pub wave: Vec<(f32, f32)>,
    pub seconds: f64,
    pub complete: bool,
    /// A live transmission: the speech so far, still coming unless `complete`.
    pub live: bool,
}

/// How far a transmission has got, for the progress bars.
#[derive(Clone, Debug)]
pub struct Transfer {
    pub call: String,
    pub session: u8,
    pub have: usize,
    pub total: usize,
    pub done: bool,
    /// The station's postamble was heard: it has finished transmitting.
    pub ended: bool,
    /// Which frames are in.
    pub map: Vec<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct TxState {
    pub active: bool,
    pub seconds: f64,
    pub played: f64,
    pub error: Option<String>,
    /// The last transmission was stopped before it was done.
    pub stopped: bool,
}

/// Everything the UI reads.
#[derive(Default)]
pub struct Shared {
    pub running: bool,
    pub status: String,
    pub sample_rate: f64,
    pub seconds: f64,
    /// Peak of the last block of input, 0–1.
    pub level: f32,
    pub snapshots: Vec<Snapshot>,
    pub spectrum: Option<SpectrumView>,
    pub log: VecDeque<String>,
    pub files: Vec<FileEntry>,
    /// Text messages received, oldest first.
    pub messages: Vec<Message>,
    pub transfers: Vec<Transfer>,
    /// Pictures received or arriving, oldest first.
    pub pictures: Vec<Arc<PictureView>>,
    /// Speech received or arriving, oldest first.
    pub voices: Vec<Arc<VoiceView>>,
    pub tx: TxState,
    /// Why the RF monitor stopped, if it did.
    pub monitor_error: Option<String>,
    /// Audio the sound card delivered that the receiver had no room for, as a
    /// share of what it delivered over the last [`DROP_WINDOW`], 0–1. Above zero
    /// the receiver is falling behind and nothing will decode.
    pub dropped_share: f64,
    /// Frames dropped since Start, and losses the driver itself reported.
    pub dropped_frames: u64,
    pub xruns: u64,
    /// Waterfall rows made and not yet taken by the display: the noise floor at
    /// the time, and dB per bin.
    pub new_rows: Vec<(f32, Vec<f32>)>,
    /// Bumped whenever something the UI draws changed.
    pub version: u64,
}

#[derive(Clone)]
pub struct SpectrumView {
    pub psd: Vec<f32>,
    pub floor_db: f32,
    pub bin_hz: f64,
}

pub struct Engine {
    pub shared: Arc<Mutex<Shared>>,
    cmd: Sender<Cmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    pub fn spawn(radio: Radio) -> Engine {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (cmd, rx) = mpsc::channel();
        let s2 = shared.clone();
        let handle = std::thread::Builder::new()
            .name("decdsss-engine".into())
            .spawn(move || worker(s2, rx, radio))
            .expect("engine thread");
        Engine {
            shared,
            cmd,
            handle: Some(handle),
        }
    }

    /// A line for the log, from the window.
    pub fn note(&self, line: String) {
        log(&self.shared, line);
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.cmd.send(c);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// An audio source the worker reads.
enum Reader {
    Device(Box<InputStream>),
    /// A KiwiSDR stream, and how much of its notes and gaps is in the log.
    Kiwi {
        stream: Box<decdsss_kiwi::KiwiStream>,
        notes: usize,
        gaps: u64,
    },
    /// Samples, the position, and when the run started.
    Memory {
        audio: Vec<f32>,
        pos: usize,
        started: Instant,
        speed: f64,
    },
}

struct Run {
    reader: Reader,
    drops: DropWatch,
    rate: f64,
    mr: MultiReceiver,
    spectrum: Spectrum,
    asm: Assembler,
    out_dir: PathBuf,
    details: bool,
    samples: u64,
    last_publish: Instant,
    transfers: BTreeMap<(String, u8), Transfer>,
    /// Pictures by (call, session), oldest first, with the bytes of the file
    /// they were last drawn from.
    pictures: Vec<((String, u8), usize, Arc<PictureView>)>,
    picture_version: u64,
    /// The same for speech: with the frames it was last drawn from.
    voices: Vec<((String, u8), usize, Arc<VoiceView>)>,
    /// The codec of each live transmission heard (from its header's type).
    live_codec: BTreeMap<(String, u8), Codec>,
}

/// How far back the dropped share in the top bar looks.
pub const DROP_WINDOW: Duration = Duration::from_secs(10);

/// Watches the capture stream's counters for audio thrown away because the
/// receiver fell behind (the capture ring was full when the driver delivered).
#[derive(Default)]
struct DropWatch {
    /// (when, frames captured, frames dropped), about ten a second, oldest first.
    marks: VecDeque<(Instant, u64, u64)>,
    share: f64,
    dropped: u64,
    xruns: u64,
}

impl DropWatch {
    /// Takes the stream's counters; returns the log lines for a first loss.
    fn update(&mut self, captured: u64, dropped: u64, xruns: u64) -> Vec<String> {
        let now = Instant::now();
        // `is_none_or` is true for an empty queue, or when the closure is.
        if self
            .marks
            .back()
            .is_none_or(|m| now - m.0 >= Duration::from_millis(100))
        {
            self.marks.push_back((now, captured, dropped));
            while self.marks.len() > 2 && now - self.marks[0].0 > DROP_WINDOW {
                self.marks.pop_front();
            }
        }
        let (_, c0, d0) = self.marks[0];
        self.share = if captured > c0 {
            (dropped - d0) as f64 / (captured - c0) as f64
        } else {
            0.0
        };
        let mut lines = Vec::new();
        if dropped > 0 && self.dropped == 0 {
            lines.push(
                "Audio dropped: the receiver is not keeping up with the sound card, so                  nothing will decode while this lasts. Untick bands you do not need,                  close other heavy programs, or set the input to a lower sample rate."
                    .to_string(),
            );
        }
        if xruns > 0 && self.xruns == 0 {
            lines.push("The sound card driver reported lost input audio.".to_string());
        }
        (self.dropped, self.xruns) = (dropped, xruns);
        lines
    }
}

/// Speech cards kept on the display.
const VOICES_MAX: usize = 6;
/// Columns of a speech waveform.
const WAVE_COLS: usize = 300;

/// Redraws a speech transmission from the frames that have arrived.
fn update_voice(r: &mut Run, call: &str, session: u8) {
    let Some(p) = r.asm.voice_progress(call, session) else {
        return;
    };
    let key = (call.to_string(), session);
    let at = r.voices.iter().position(|(k, ..)| *k == key);
    if at.is_some_and(|i| r.voices[i].1 == p.have) {
        return;
    }
    let Some((speech, _)) = r.asm.voice_speech(call, session) else {
        return;
    };
    let per_col = speech.len().div_ceil(WAVE_COLS).max(1);
    let wave: Vec<(f32, f32)> = speech
        .chunks(per_col)
        .map(|c| {
            let lo = c.iter().copied().min().unwrap_or(0);
            let hi = c.iter().copied().max().unwrap_or(0);
            (f32::from(lo) / 32768.0, f32::from(hi) / 32768.0)
        })
        .collect();
    let view = Arc::new(VoiceView {
        call: call.to_string(),
        session,
        codec: p.codec,
        have: p.have,
        total: p.total,
        map: p.map,
        seconds: speech.len() as f64 / 8000.0,
        speech: Arc::new(speech),
        wave,
        complete: p.have == p.total,
        live: false,
    });
    match at {
        Some(i) => r.voices[i] = (key, p.have, view),
        None => {
            r.voices.push((key, p.have, view));
            if r.voices.len() > VOICES_MAX {
                r.voices.remove(0);
            }
        }
    }
}

/// Redraws a live transmission's card from the speech so far.
fn update_live_view(r: &mut Run, call: &str, session: u8) {
    let Some((speech, done)) = r.asm.live_speech(call, session) else {
        return;
    };
    let speech = speech.to_vec();
    let per_col = speech.len().div_ceil(WAVE_COLS).max(1);
    let wave: Vec<(f32, f32)> = speech
        .chunks(per_col)
        .map(|c| {
            let lo = c.iter().copied().min().unwrap_or(0);
            let hi = c.iter().copied().max().unwrap_or(0);
            (f32::from(lo) / 32768.0, f32::from(hi) / 32768.0)
        })
        .collect();
    let key = (call.to_string(), session);
    let at = r.voices.iter().position(|(k, ..)| *k == key);
    let view = Arc::new(VoiceView {
        call: call.to_string(),
        session,
        codec: if let Some(h) = r.live_codec.get(&key) {
            *h
        } else {
            Codec::C450
        },
        have: 0,
        total: 0,
        map: Vec::new(),
        seconds: speech.len() as f64 / 8000.0,
        speech: Arc::new(speech),
        wave,
        complete: done,
        live: true,
    });
    match at {
        Some(i) => r.voices[i] = (key, 0, view),
        None => {
            r.voices.push((key, 0, view));
            if r.voices.len() > VOICES_MAX {
                r.voices.remove(0);
            }
        }
    }
}

/// Writes a received speech transmission as a WAV and lists it with the files.
fn save_voice(shared: &Arc<Mutex<Shared>>, r: &Run, t: f64, v: ReceivedVoice) {
    let name = clean_name(&format!("{}-{}.wav", v.call, v.session));
    let path = unique(&r.out_dir, &name);
    let audio: Vec<f32> = v.speech.iter().map(|&s| f32::from(s) / 32768.0).collect();
    match wav::write(&path, &audio, 8000) {
        Ok(_) => {
            log(
                shared,
                format!(
                    "[{t:6.1}s] VOICE from {} ({:.1} s of Codec2 {}) saved to {}",
                    v.call,
                    v.speech.len() as f64 / 8000.0,
                    v.codec,
                    path.display()
                ),
            );
            let ptype = decdsss_link::voice::ptype_of(v.codec).unwrap_or(PayloadType::Voice);
            let file = ReceivedFile {
                call: v.call,
                session: v.session,
                ptype,
                name,
                data: v.speech.iter().flat_map(|s| s.to_le_bytes()).collect(),
            };
            let mut g = shared.lock().unwrap();
            g.files.push(FileEntry { file, path });
            g.version += 1;
        }
        Err(e) => log(shared, format!("cannot write {}: {e}", path.display())),
    }
}

/// Pictures kept on the display.
const PICTURES_MAX: usize = 8;

/// Redraws a transmission's picture from the front of its file that has
/// arrived (or from the whole file once it has).
fn update_picture(r: &mut Run, call: &str, session: u8, finished: Option<&ReceivedFile>) {
    let (data, name, total, complete) = match finished {
        Some(f) => (f.data.clone(), f.name.clone(), f.data.len(), true),
        None => match r.asm.front(call, session) {
            Some(fr) => (fr.data, fr.name, fr.total, false),
            None => return,
        },
    };
    let key = (call.to_string(), session);
    let at = r.pictures.iter().position(|(k, ..)| *k == key);
    // Nothing new to draw; and a picture that has arrived whole stays whole: blocks
    // that come after it (the rest of a pass) would otherwise redraw it from the
    // front that arrived without a gap, which is shorter when a data frame was lost
    // and the file was rebuilt from coded ones.
    if !complete && at.is_some_and(|i| r.pictures[i].1 == data.len() || r.pictures[i].2.complete) {
        return;
    }
    let Some(part) = decdsss_image::decode_partial(&data) else {
        return;
    };
    r.picture_version += 1;
    let view = Arc::new(PictureView {
        call: call.to_string(),
        session,
        name,
        width: part.image.width(),
        height: part.image.height(),
        rgba: part.image.into_raw(),
        bytes: data.len(),
        total,
        scans: part.scans,
        complete: complete || part.complete,
        version: r.picture_version,
    });
    match at {
        Some(i) => r.pictures[i] = (key, data.len(), view),
        None => {
            r.pictures.push((key, data.len(), view));
            if r.pictures.len() > PICTURES_MAX {
                r.pictures.remove(0);
            }
        }
    }
}

fn log(shared: &Arc<Mutex<Shared>>, line: String) {
    let mut g = shared.lock().unwrap();
    if g.log.len() >= LOG_MAX {
        g.log.pop_front();
    }
    g.log.push_back(line);
    g.version += 1;
}

fn worker(shared: Arc<Mutex<Shared>>, rx: Receiver<Cmd>, radio: Radio) {
    let mut run: Option<Run> = None;
    let mut monitor = Monitor::default();
    let mut player = Player::default();
    let tx_stop = Arc::new(AtomicBool::new(false));
    loop {
        // Commands: block briefly when idle, poll when running.
        let cmd = if run.is_some() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };
        match cmd {
            Some(Cmd::Quit) => return,
            Some(Cmd::Stop) => {
                run = None;
                let mut g = shared.lock().unwrap();
                g.running = false;
                g.status = "Stopped.".into();
                g.version += 1;
            }
            Some(Cmd::Start(cfg)) => match start(&shared, &cfg) {
                Ok(r) => run = Some(r),
                Err(e) => {
                    run = None;
                    let mut g = shared.lock().unwrap();
                    g.running = false;
                    g.status = format!("Cannot start: {e}");
                    g.version += 1;
                }
            },
            Some(Cmd::Monitor { on, device, volume }) => {
                monitor.configure(on, device, volume);
                shared.lock().unwrap().monitor_error = None;
            }
            Some(Cmd::StopTransmit) => tx_stop.store(true, Ordering::Relaxed),
            Some(Cmd::Listen { device }) => player.configure(device),
            Some(Cmd::Speech { pcm, last }) => {
                if let Some(e) = player.push(&pcm, last) {
                    log(&shared, e);
                }
            }
            Some(Cmd::Transmit {
                audio,
                rate,
                device,
                key,
            }) => transmit(
                &shared,
                audio,
                rate,
                device,
                tx_stop.clone(),
                key.then(|| radio.clone()),
            ),
            None => {}
        }
        // The speech output is fed whether or not a receiver is running.
        player.tick();
        let Some(r) = run.as_mut() else { continue };
        if !step(&shared, r, &mut monitor, &mut player) {
            run = None;
            let mut g = shared.lock().unwrap();
            g.running = false;
            g.status = "Finished.".into();
            g.version += 1;
        }
    }
}

fn start(shared: &Arc<Mutex<Shared>>, cfg: &Config) -> Result<Run, String> {
    std::fs::create_dir_all(&cfg.out_dir).map_err(|e| format!("{}: {e}", cfg.out_dir.display()))?;
    let (reader, rate, what) = match &cfg.source {
        Source::Device(name) => {
            let input = InputStream::open(&InputOptions {
                device: name.clone(),
                channels: Some(1),
                ..InputOptions::default()
            })
            .map_err(|e| e.to_string())?;
            let f = input.format();
            let what = format!("{} ({})", input.device_name(), f);
            (
                Reader::Device(Box::new(input)),
                f64::from(f.sample_rate),
                what,
            )
        }
        Source::Wav(p) => {
            let (audio, rate) = wav::read(p).map_err(|e| e.to_string())?;
            (
                Reader::Memory {
                    audio,
                    pos: 0,
                    started: Instant::now(),
                    speed: cfg.speed.max(0.1),
                },
                f64::from(rate),
                p.display().to_string(),
            )
        }
        Source::Kiwi {
            address,
            freq_khz,
            lsb,
            ident,
        } => {
            let mut k = decdsss_kiwi::Config::new(address, *freq_khz).map_err(|e| e.to_string())?;
            k.lsb = *lsb;
            k.ident = ident.clone();
            log(
                shared,
                format!("Connecting to the KiwiSDR at {}:{}…", k.host, k.port),
            );
            let stream = decdsss_kiwi::KiwiStream::open(&k, Duration::from_secs(10))
                .map_err(|e| e.to_string())?;
            let rate = stream.sample_rate();
            let what = format!(
                "KiwiSDR {}:{}, {freq_khz:.3} kHz {} ({rate:.3} Hz)",
                k.host,
                k.port,
                if *lsb { "LSB" } else { "USB" }
            );
            (
                Reader::Kiwi {
                    stream: Box::new(stream),
                    notes: 0,
                    gaps: 0,
                },
                rate,
                what,
            )
        }
        Source::Demo => {
            let d = demo::make();
            (
                Reader::Memory {
                    audio: d.audio,
                    pos: 0,
                    started: Instant::now(),
                    speed: cfg.speed.max(0.1),
                },
                f64::from(demo::RATE),
                format!("demo: {}", d.what),
            )
        }
    };
    let bands = if cfg.bands.is_empty() {
        Band::ALL.to_vec()
    } else {
        cfg.bands.clone()
    };
    {
        let mut g = shared.lock().unwrap();
        g.running = true;
        g.status = format!("Listening: {what}");
        g.sample_rate = rate;
        g.seconds = 0.0;
        g.snapshots.clear();
        g.spectrum = None;
        g.transfers.clear();
        g.pictures.clear();
        g.voices.clear();
        g.dropped_share = 0.0;
        g.dropped_frames = 0;
        g.xruns = 0;
        g.version += 1;
    }
    log(shared, format!("Listening on {what}, {} Hz", rate as u32));
    Ok(Run {
        reader,
        drops: DropWatch::default(),
        rate,
        mr: MultiReceiver::with_bands(&bands, rate),
        spectrum: Spectrum::new(rate),
        asm: Assembler::new(),
        out_dir: cfg.out_dir.clone(),
        details: cfg.details,
        samples: 0,
        last_publish: Instant::now(),
        transfers: BTreeMap::new(),
        pictures: Vec::new(),
        picture_version: 0,
        voices: Vec::new(),
        live_codec: BTreeMap::new(),
    })
}

/// Reads what is available, processes it, publishes. False when a finite
/// source has run out.
fn step(
    shared: &Arc<Mutex<Shared>>,
    r: &mut Run,
    monitor: &mut Monitor,
    player: &mut Player,
) -> bool {
    let chunk: Vec<f32> = match &mut r.reader {
        Reader::Device(input) => {
            if input.is_dead() {
                log(shared, "The input device stopped.".into());
                return false;
            }
            let st = input.stats();
            for line in r
                .drops
                .update(st.frames_captured, st.dropped_frames, st.xruns)
            {
                log(shared, line);
            }
            let frames = input.available_frames().min(8192);
            if frames == 0 {
                std::thread::sleep(Duration::from_millis(5));
                Vec::new()
            } else {
                // A device may deliver stereo whatever we asked for.
                let ch = input.format().channels;
                decdsss_audio::channels::downmix_to_mono(&input.read(frames), ch)
            }
        }
        Reader::Kiwi {
            stream,
            notes,
            gaps,
        } => {
            let st = stream.status();
            for n in &st.notes[*notes..] {
                log(shared, format!("KiwiSDR: {n}"));
            }
            *notes = st.notes.len();
            if st.gap_samples > *gaps {
                log(
                    shared,
                    format!(
                        "KiwiSDR: {:.0} ms of audio never came (put back as silence, so nothing \
                         after it moved)",
                        1e3 * (st.gap_samples - *gaps) as f64 / r.rate
                    ),
                );
                *gaps = st.gap_samples;
            }
            if stream.is_dead() {
                let why = stream.status().error.unwrap_or_default();
                log(shared, format!("The KiwiSDR stream stopped: {why}"));
                return false;
            }
            let audio = stream.read();
            if audio.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
            audio
        }
        Reader::Memory {
            audio,
            pos,
            started,
            speed,
        } => {
            let due = (started.elapsed().as_secs_f64() * r.rate * *speed) as usize;
            let end = due.min(audio.len()).min(*pos + 4096);
            if *pos >= audio.len() {
                // Let the receivers drain, then stop.
                publish(shared, r, true);
                return false;
            }
            if end <= *pos {
                std::thread::sleep(Duration::from_millis(4));
                Vec::new()
            } else {
                let c = audio[*pos..end].to_vec();
                *pos = end;
                c
            }
        }
    };
    if !chunk.is_empty() {
        if let Some(e) = monitor.feed(&chunk, r.rate) {
            let mut g = shared.lock().unwrap();
            g.monitor_error = Some(e);
            g.version += 1;
        }
        let peak = chunk.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        r.samples += chunk.len() as u64;
        if r.spectrum.push(&chunk) {
            // Straight to the display, each as it is made, so the waterfall
            // scrolls smoothly; the rest of the state is published ten times a
            // second.
            let rows = r.spectrum.take_rows();
            let mut g = shared.lock().unwrap();
            g.new_rows.extend(rows);
            let extra = g.new_rows.len().saturating_sub(600);
            g.new_rows.drain(..extra);
        }
        let t = r.samples as f64 / r.rate;
        for tagged in r.mr.push(&chunk) {
            handle_event(shared, r, player, t, tagged.band, tagged.event);
        }
        shared.lock().unwrap().level = peak;
    }
    publish(shared, r, false);
    true
}

fn handle_event(
    shared: &Arc<Mutex<Shared>>,
    r: &mut Run,
    player: &mut Player,
    t: f64,
    band: Band,
    event: Event,
) {
    let p = band.label();
    match event {
        Event::Acquired(d) if r.details => log(
            shared,
            format!(
                "[{t:6.1}s] {p}: preamble found ({:+.1} Hz, {:.0} dB over threshold)",
                d.freq_hz, d.margin_db
            ),
        ),
        Event::PreambleLocked(l) if r.details => log(
            shared,
            format!(
                "[{t:6.1}s] {p}: preamble locked, carrier {:+.2} Hz, clock {:+.0} ppm",
                l.freq_hz,
                (4.0 / l.step - 1.0) * 1e6
            ),
        ),
        Event::Lost(why) if r.details => log(shared, format!("[{t:6.1}s] {p}: lost it: {why}")),
        Event::Acquired(_) | Event::PreambleLocked(_) | Event::Lost(_) => {}
        Event::Header { header, snr_db, .. } if decdsss_link::cq::is_cq(&header) => {
            // The header is the whole message: no transfer to follow.
            heard_cq(shared, r, t, &p, &header, snr_db);
        }
        Event::Header { header, snr_db, .. } => {
            log(
                shared,
                format!(
                    "[{t:6.1}s] {p}: {} sends {} depth {} (session {}, frames {}.. of {}, {} blocks), symbol SNR {:.0} dB",
                    header.call,
                    header.mode,
                    header.depth,
                    header.session,
                    header.frame0,
                    header.total_frames,
                    header.nblocks,
                    snr_db
                ),
            );
            r.transfers
                .entry((header.call.clone(), header.session))
                .or_insert_with(|| Transfer {
                    call: header.call.clone(),
                    session: header.session,
                    have: 0,
                    total: 0,
                    done: false,
                    ended: false,
                    map: Vec::new(),
                });
        }
        Event::Block { header, result, .. } => {
            let ok = result.frames.iter().filter(|f| f.is_some()).count();
            if ok < result.frames.len() {
                log(
                    shared,
                    format!(
                        "[{t:6.1}s] {p}: {} block {}: {ok}/{} frames, SNR {:.0} dB",
                        header.call,
                        result.index,
                        result.frames.len(),
                        result.snr_db
                    ),
                );
            }
            let finished = r.asm.push(&header, &result);
            if let Some(codec) = decdsss_link::voice::codec_of(header.ptype) {
                if header.total_frames == 0 {
                    // Live speech: played as it comes, and drawn.
                    r.live_codec
                        .insert((header.call.clone(), header.session), codec);
                    for c in r.asm.take_live() {
                        if let Some(e) = player.push(&c.pcm, c.ended) {
                            log(shared, format!("[{t:6.1}s] {e}"));
                        }
                    }
                    update_live_view(r, &header.call, header.session);
                } else {
                    update_voice(r, &header.call, header.session);
                }
                for v in r.asm.take_voice() {
                    save_voice(shared, r, t, v);
                }
            }
            // Progress comes from the assembler: distinct frames in against the
            // frames needed (any that many rebuild the file).
            if let Some(prog) = r.asm.progress(&header.call, header.session) {
                if let Some(tr) = r.transfers.get_mut(&(header.call.clone(), header.session)) {
                    tr.have = prog.have;
                    tr.total = prog.need;
                    tr.map = prog.data_held;
                }
                if prog.ptype == PayloadType::Picture {
                    update_picture(r, &header.call, header.session, finished.as_ref());
                } else if prog.ptype == PayloadType::Text
                    && finished.is_none()
                    && !r.asm.is_done(&header.call, header.session)
                {
                    // The message so far, as the frames come in.
                    update_text(shared, r, t, &p, &header.call, header.session);
                }
            }
            if let Some(f) = finished {
                if let Some(tr) = r.transfers.get_mut(&(f.call.clone(), f.session)) {
                    tr.done = true;
                }
                if f.ptype == PayloadType::Text {
                    save_text(shared, r, t, &p, &f);
                } else {
                    save(shared, r, t, f);
                }
            }
        }
        Event::SegmentEnd { .. } => {}
        Event::EndOfTransmission { header, ratio, .. } => {
            log(
                shared,
                format!(
                    "[{t:6.1}s] {p}: {} has finished (postamble {ratio:.0}x its noise floor)",
                    header.call
                ),
            );
            if let Some(tr) = r.transfers.get_mut(&(header.call.clone(), header.session)) {
                tr.ended = true;
            }
            // A live stream whose end frame was lost ends here instead.
            if let Some(codec) = decdsss_link::voice::codec_of(header.ptype)
                && header.total_frames == 0
            {
                r.asm.end_live(&header.call, header.session);
                r.live_codec
                    .insert((header.call.clone(), header.session), codec);
                for c in r.asm.take_live() {
                    if let Some(e) = player.push(&c.pcm, c.ended) {
                        log(shared, format!("[{t:6.1}s] {e}"));
                    }
                }
                update_live_view(r, &header.call, header.session);
                for v in r.asm.take_voice() {
                    save_voice(shared, r, t, v);
                }
            }
        }
    }
}

fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let c: String = base
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let c = c.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if c.is_empty() {
        "received.bin".into()
    } else {
        c
    }
}

fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem}-{i}{ext}")))
        .find(|p| !p.exists())
        .expect("a free name")
}

/// A received message goes to the message list and the log, and is appended to
/// `messages.log` in the output folder, not saved as a file.
fn save_text(shared: &Arc<Mutex<Shared>>, r: &Run, t: f64, band: &str, f: &ReceivedFile) {
    let text = decdsss_link::text::read(f).unwrap_or_default();
    log(
        shared,
        format!("[{t:6.1}s] TEXT from {} ({band}): {text}", f.call),
    );
    let m = Message {
        at_s: t,
        call: f.call.clone(),
        session: f.session,
        band: band.to_string(),
        text,
        packet: false,
        partial: false,
    };
    push_message(shared, r, m);
}

/// Shows a message as far as it has arrived: the front of it without a gap (the data
/// frames come first in a pass, so it grows frame by frame). One entry per
/// (call, session), updated in place; [`push_message`] swaps in the finished text.
fn update_text(shared: &Arc<Mutex<Shared>>, r: &Run, t: f64, band: &str, call: &str, session: u8) {
    let Some(front) = r.asm.front(call, session) else {
        return;
    };
    let text = decdsss_link::text::read_front(&front.data);
    if text.is_empty() {
        return;
    }
    let mut g = shared.lock().unwrap();
    let at = g
        .messages
        .iter()
        .position(|m| m.partial && m.call == call && m.session == session);
    match at {
        Some(i) if g.messages[i].text == text => return,
        Some(i) => g.messages[i].text = text,
        None => {
            g.messages.push(Message {
                at_s: t,
                call: call.to_string(),
                session,
                band: band.to_string(),
                text,
                packet: false,
                partial: true,
            });
            if g.messages.len() > MESSAGES_MAX {
                g.messages.remove(0);
            }
        }
    }
    g.version += 1;
}

/// A CQ packet was heard: a message "CQ de CALL", once however many copies arrive
/// (copies of one CQ share the station and session).
fn heard_cq(
    shared: &Arc<Mutex<Shared>>,
    r: &Run,
    t: f64,
    band: &str,
    header: &decdsss_phy::framing::Header,
    snr_db: f64,
) {
    log(
        shared,
        format!(
            "[{t:6.1}s] {band}: {}",
            decdsss_link::cq::describe(header, snr_db)
        ),
    );
    let again = shared.lock().unwrap().messages.iter().rev().any(|m| {
        m.call == header.call && m.session == header.session && m.packet && t - m.at_s < 300.0
    });
    if !again {
        let text = format!("CQ de {} (CQ packet)", header.call);
        let m = Message {
            at_s: t,
            call: header.call.clone(),
            session: header.session,
            band: band.to_string(),
            text,
            packet: true,
            partial: false,
        };
        push_message(shared, r, m);
    }
}

/// Adds a message to the list and to `messages.log`.
fn push_message(shared: &Arc<Mutex<Shared>>, r: &Run, m: Message) {
    use std::io::Write;
    let line = format!(
        "[{:7.1}s] {} ({}): {}\n",
        m.at_s,
        m.call,
        m.band,
        m.text.replace('\n', " / ")
    );
    let wrote = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r.out_dir.join("messages.log"))
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(e) = wrote {
        log(shared, format!("cannot write messages.log: {e}"));
    }
    let mut g = shared.lock().unwrap();
    // A message that was showing as it arrived is finished in the same place.
    let partial = g
        .messages
        .iter()
        .position(|p| p.partial && !m.packet && p.call == m.call && p.session == m.session);
    match partial {
        Some(i) => g.messages[i] = m,
        None => g.messages.push(m),
    }
    if g.messages.len() > MESSAGES_MAX {
        g.messages.remove(0);
    }
    g.version += 1;
}

fn save(shared: &Arc<Mutex<Shared>>, r: &Run, t: f64, f: ReceivedFile) {
    let path = unique(&r.out_dir, &clean_name(&f.name));
    match std::fs::write(&path, &f.data) {
        Ok(()) => {
            log(
                shared,
                format!(
                    "[{t:6.1}s] FILE {} ({} bytes) from {} saved to {}",
                    f.name,
                    f.data.len(),
                    f.call,
                    path.display()
                ),
            );
            let mut g = shared.lock().unwrap();
            g.files.push(FileEntry { file: f, path });
            g.version += 1;
        }
        Err(e) => log(shared, format!("cannot write {}: {e}", path.display())),
    }
}

/// Publishes the display state about ten times a second.
fn publish(shared: &Arc<Mutex<Shared>>, r: &mut Run, force: bool) {
    if !force && r.last_publish.elapsed() < Duration::from_millis(100) {
        return;
    }
    r.last_publish = Instant::now();
    let snaps = r.mr.snapshots(SEARCH_COLS);
    let view = SpectrumView {
        psd: r.spectrum.psd.clone(),
        floor_db: r.spectrum.floor_db,
        bin_hz: r.spectrum.bin_hz(),
    };
    let transfers: Vec<Transfer> = r.transfers.values().cloned().collect();
    let pictures: Vec<Arc<PictureView>> = r.pictures.iter().map(|(.., v)| v.clone()).collect();
    let voices: Vec<Arc<VoiceView>> = r.voices.iter().map(|(.., v)| v.clone()).collect();
    let mut g = shared.lock().unwrap();
    g.snapshots = snaps;
    g.spectrum = Some(view);
    g.transfers = transfers;
    g.pictures = pictures;
    g.voices = voices;
    g.seconds = r.samples as f64 / r.rate;
    g.dropped_share = r.drops.share;
    g.dropped_frames = r.drops.dropped;
    g.xruns = r.drops.xruns;
    g.version += 1;
}

/// Plays audio on the output device without blocking the receiver.
fn transmit(
    shared: &Arc<Mutex<Shared>>,
    audio: Vec<f32>,
    rate: u32,
    device: Option<String>,
    stop: Arc<AtomicBool>,
    radio: Option<Radio>,
) {
    {
        let mut g = shared.lock().unwrap();
        if g.tx.active {
            return;
        }
        g.tx = TxState {
            active: true,
            seconds: audio.len() as f64 / f64::from(rate),
            played: 0.0,
            error: None,
            stopped: false,
        };
        g.version += 1;
    }
    stop.store(false, Ordering::Relaxed);
    let shared = shared.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let mut out = OutputStream::open(&OutputOptions {
                device,
                sample_rate: Some(rate),
                channels: Some(1),
                ..OutputOptions::default()
            })
            .map_err(|e| e.to_string())?;
            let fmt = out.format();
            if fmt.sample_rate != rate {
                return Err(format!(
                    "the output device runs at {} Hz, the audio was made for {rate} Hz",
                    fmt.sample_rate
                ));
            }
            let ch = fmt.channels.max(1);
            let total = audio.len();
            // Key the transmitter, with the stream already open so that nothing
            // but the rig's answer and the lead time stands between the key and
            // the first sample. A rig that will not key stops the transmission.
            // Declared after `out`, so a stop or an error lets go of the key
            // first, then cuts the audio.
            let hold = match &radio {
                Some(r) => r.hold()?,
                None => None,
            };
            let mut i = 0;
            while i < total {
                if stop.load(Ordering::Relaxed) {
                    // Dropping the key, then the stream, at the end of this closure.
                    shared.lock().unwrap().tx.stopped = true;
                    return Ok(());
                }
                let n = (rate as usize / 10).min(total - i);
                let mut block = Vec::with_capacity(n * ch);
                for &v in &audio[i..i + n] {
                    block.extend(std::iter::repeat_n(v, ch));
                }
                out.write_blocking(&block, Duration::from_secs(5))
                    .map_err(|e| e.to_string())?;
                i += n;
                let mut g = shared.lock().unwrap();
                g.tx.played = i as f64 / f64::from(rate);
                g.version += 1;
            }
            out.drain(Duration::from_secs(10));
            // The tail time, then let go.
            if let Some(h) = hold {
                h.finish();
            }
            Ok(())
        })();
        let mut g = shared.lock().unwrap();
        g.tx.active = false;
        g.tx.error = result.err();
        g.version += 1;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_rig::testing::FakeRig;

    #[test]
    fn dropped_audio_is_a_share_of_what_was_captured_and_logged_once() {
        let mut w = DropWatch::default();
        assert!(w.update(4410, 0, 0).is_empty());
        assert_eq!(w.share, 0.0);
        // A fifth of the next 44 100 frames thrown away.
        std::thread::sleep(Duration::from_millis(110));
        let lines = w.update(48_510, 8820, 0);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!((w.share - 0.2).abs() < 1e-9, "{}", w.share);
        // Still dropping: no second log line; a driver loss gets its own.
        let lines = w.update(52_920, 9702, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("driver"));
        assert_eq!((w.dropped, w.xruns), (9702, 1));
    }

    /// Plays two seconds of a tone through `transmit` into VB-Audio cable A
    /// (silent) with a stand-in rig: the rig is keyed before the first sample
    /// reaches the card and let go after the last. Needs the cable, so it is run
    /// by hand: `cargo test -p decdsss-gui -- --ignored keyed_around`.
    #[test]
    #[ignore = "needs VB-Audio cable A"]
    fn the_rig_is_keyed_around_the_audio() {
        let rig = FakeRig::new(decdsss_rig::Model::Ft710);
        let radio = Radio::with_fake_rig(&rig, 150, 150);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let audio: Vec<f32> = (0..96_000).map(|i| 0.1 * (i as f32 * 0.13).sin()).collect();
        let started = Instant::now();
        transmit(
            &shared,
            audio,
            48_000,
            Some("CABLE-A Input".into()),
            Arc::new(AtomicBool::new(false)),
            Some(radio),
        );
        // While audio is being played the rig is keyed, from before the first
        // sample (progress is only reported once a piece has been written).
        let mut keyed_when_playing = None;
        while shared.lock().unwrap().tx.active {
            let played = shared.lock().unwrap().tx.played;
            if played > 0.0 && keyed_when_playing.is_none() {
                keyed_when_playing = Some(rig.transmitting());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let g = shared.lock().unwrap();
        assert_eq!(g.tx.error, None);
        assert_eq!(keyed_when_playing, Some(true));
        assert!(!rig.transmitting());
        assert_eq!(rig.keyings(), [true, false]);
        // The lead (0.15 s), two seconds of audio, the tail (0.15 s).
        assert!(
            started.elapsed() >= Duration::from_millis(2300),
            "{:?}",
            started.elapsed()
        );
    }

    /// A rig that will not key stops the transmission before any audio.
    #[test]
    #[ignore = "needs VB-Audio cable A"]
    fn a_rig_that_will_not_key_stops_the_transmission() {
        let rig = FakeRig::new(decdsss_rig::Model::Ft710).refuse(true);
        let radio = Radio::with_fake_rig(&rig, 10, 10);
        let shared = Arc::new(Mutex::new(Shared::default()));
        transmit(
            &shared,
            vec![0.0; 48_000],
            48_000,
            Some("CABLE-A Input".into()),
            Arc::new(AtomicBool::new(false)),
            Some(radio),
        );
        while shared.lock().unwrap().tx.active {
            std::thread::sleep(Duration::from_millis(5));
        }
        let g = shared.lock().unwrap();
        assert!(
            g.tx.error
                .as_deref()
                .is_some_and(|e| e.contains("would not key")),
            "{:?}",
            g.tx.error
        );
        assert_eq!(g.tx.played, 0.0);
        assert!(!rig.transmitting());
    }
}
