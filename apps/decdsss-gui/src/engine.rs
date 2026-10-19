// made by CasualArclamp
//! The engine: one worker thread that owns the receivers. It reads audio from
//! a sound card, a WAV file or the built-in demo, feeds the spectrum and the
//! receivers, gathers frames into files, and publishes what the UI shows into
//! a shared state the UI polls. The UI never touches a receiver.

use crate::demo;
use crate::spectrum::Spectrum;
use decdsss_audio::{InputOptions, InputStream, OutputOptions, OutputStream, wav};
use decdsss_core::Profile;
use decdsss_link::{Assembler, ReceivedFile};
use decdsss_phy::framing::PayloadType;
use decdsss_phy::rx::{Event, MultiReceiver, Snapshot};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Columns the acquisition preview is reduced to.
const SEARCH_COLS: usize = 192;
/// Log lines kept.
const LOG_MAX: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A sound card or virtual cable; `None` is the system default input.
    Device(Option<String>),
    Wav(PathBuf),
    Demo,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub source: Source,
    pub profiles: Vec<Profile>,
    pub out_dir: PathBuf,
    /// Faster than real time for files and the demo.
    pub speed: f64,
    /// Log the search too: preambles found, locked or lost.
    pub details: bool,
}

pub enum Cmd {
    Start(Config),
    Stop,
    /// Plays audio (already at `rate`) on the output device.
    Transmit {
        audio: Vec<f32>,
        rate: u32,
        device: Option<String>,
    },
    Quit,
}

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

/// How far a transmission has got, for the progress bars.
#[derive(Clone, Debug)]
pub struct Transfer {
    pub call: String,
    pub session: u8,
    pub have: usize,
    pub total: usize,
    pub done: bool,
    /// Which frames are in.
    pub map: Vec<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct TxState {
    pub active: bool,
    pub seconds: f64,
    pub played: f64,
    pub error: Option<String>,
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
    pub transfers: Vec<Transfer>,
    /// Pictures received or arriving, oldest first.
    pub pictures: Vec<Arc<PictureView>>,
    pub tx: TxState,
    /// Bumped whenever something the UI draws changed.
    pub version: u64,
}

#[derive(Clone)]
pub struct SpectrumView {
    pub psd: Vec<f32>,
    pub floor_db: f32,
    pub bin_hz: f64,
    /// Waterfall rows, oldest first.
    pub rows: Vec<Vec<f32>>,
    pub row_version: u64,
}

pub struct Engine {
    pub shared: Arc<Mutex<Shared>>,
    cmd: Sender<Cmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    pub fn spawn() -> Engine {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (cmd, rx) = mpsc::channel();
        let s2 = shared.clone();
        let handle = std::thread::Builder::new()
            .name("decdsss-engine".into())
            .spawn(move || worker(s2, rx))
            .expect("engine thread");
        Engine {
            shared,
            cmd,
            handle: Some(handle),
        }
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
    rate: f64,
    mr: MultiReceiver,
    spectrum: Spectrum,
    asm: Assembler,
    out_dir: PathBuf,
    details: bool,
    samples: u64,
    last_publish: Instant,
    row_version: u64,
    transfers: BTreeMap<(String, u8), Transfer>,
    /// Pictures by (call, session), oldest first, with the bytes of the file
    /// they were last drawn from.
    pictures: Vec<((String, u8), usize, Arc<PictureView>)>,
    picture_version: u64,
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
    if !complete && at.is_some_and(|i| r.pictures[i].1 == data.len()) {
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

fn worker(shared: Arc<Mutex<Shared>>, rx: Receiver<Cmd>) {
    let mut run: Option<Run> = None;
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
            Some(Cmd::Transmit {
                audio,
                rate,
                device,
            }) => transmit(&shared, audio, rate, device),
            None => {}
        }
        let Some(r) = run.as_mut() else { continue };
        if !step(&shared, r) {
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
    let profiles = if cfg.profiles.is_empty() {
        Profile::ALL.to_vec()
    } else {
        cfg.profiles.clone()
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
        g.version += 1;
    }
    log(shared, format!("Listening on {what}, {} Hz", rate as u32));
    Ok(Run {
        reader,
        rate,
        mr: MultiReceiver::new(&profiles, rate),
        spectrum: Spectrum::new(rate),
        asm: Assembler::new(),
        out_dir: cfg.out_dir.clone(),
        details: cfg.details,
        samples: 0,
        last_publish: Instant::now(),
        row_version: 0,
        transfers: BTreeMap::new(),
        pictures: Vec::new(),
        picture_version: 0,
    })
}

/// Reads what is available, processes it, publishes. False when a finite
/// source has run out.
fn step(shared: &Arc<Mutex<Shared>>, r: &mut Run) -> bool {
    let chunk: Vec<f32> = match &mut r.reader {
        Reader::Device(input) => {
            if input.is_dead() {
                log(shared, "The input device stopped.".into());
                return false;
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
        let peak = chunk.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        r.samples += chunk.len() as u64;
        if r.spectrum.push(&chunk) {
            r.row_version += 1;
        }
        let t = r.samples as f64 / r.rate;
        for tagged in r.mr.push(&chunk) {
            handle_event(shared, r, t, tagged.profile, tagged.event);
        }
        shared.lock().unwrap().level = peak;
    }
    publish(shared, r, false);
    true
}

fn handle_event(shared: &Arc<Mutex<Shared>>, r: &mut Run, t: f64, profile: Profile, event: Event) {
    let p = profile.label();
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
            // Progress comes from the assembler: distinct frames in against the
            // frames needed (any that many rebuild the file).
            if let Some(p) = r.asm.progress(&header.call, header.session) {
                if let Some(tr) = r.transfers.get_mut(&(header.call.clone(), header.session)) {
                    tr.have = p.have;
                    tr.total = p.need;
                    tr.map = p.data_held;
                }
                if p.ptype == PayloadType::Picture {
                    update_picture(r, &header.call, header.session, finished.as_ref());
                }
            }
            if let Some(f) = finished {
                if let Some(tr) = r.transfers.get_mut(&(f.call.clone(), f.session)) {
                    tr.done = true;
                }
                save(shared, r, t, f);
            }
        }
        Event::SegmentEnd { .. } => {}
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
        rows: r.spectrum.rows.iter().cloned().collect(),
        row_version: r.row_version,
    };
    let transfers: Vec<Transfer> = r.transfers.values().cloned().collect();
    let pictures: Vec<Arc<PictureView>> = r.pictures.iter().map(|(.., v)| v.clone()).collect();
    let mut g = shared.lock().unwrap();
    g.snapshots = snaps;
    g.spectrum = Some(view);
    g.transfers = transfers;
    g.pictures = pictures;
    g.seconds = r.samples as f64 / r.rate;
    g.version += 1;
}

/// Plays audio on the output device without blocking the receiver.
fn transmit(shared: &Arc<Mutex<Shared>>, audio: Vec<f32>, rate: u32, device: Option<String>) {
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
        };
        g.version += 1;
    }
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
            let mut i = 0;
            while i < total {
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
            Ok(())
        })();
        let mut g = shared.lock().unwrap();
        g.tx.active = false;
        g.tx.error = result.err();
        g.version += 1;
    });
}
