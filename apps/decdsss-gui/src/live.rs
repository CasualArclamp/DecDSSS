// made by CasualArclamp
//! A live voice transmission from the microphone: speech is encoded as it is
//! spoken, framed, modulated a block at a time and played to the output (the radio)
//! with no gap, for as long as the PTT is held.
//!
//! Two threads. The *capture* thread reads the microphone, brings it to 8 kHz,
//! levels it, encodes it with Codec2 and queues the finished modem frames. The
//! *modulator* thread plays the preamble and header, then takes up to `depth`
//! frames from the queue for each block (a filler for any that is not ready, so the
//! signal never stops) and plays it; the output's own pace times everything, as the
//! block is chosen about a second before it goes out. Letting go sends what is
//! left and an end frame; abort cuts the audio at once.

use crate::radio::Radio;
use decdsss_audio::channels::downmix_to_mono;
use decdsss_audio::{InputOptions, InputStream, OutputOptions, OutputStream};
use decdsss_codec2::Mode as Codec;
use decdsss_core::{Band, Mode, Profile};
use decdsss_dsp::Iq;
use decdsss_dsp::resample::Resampler;
use decdsss_link::live::{END, LiveEncoder};
use decdsss_link::voice::ptype_of;
use decdsss_phy::banner;
use decdsss_phy::framing::Header;
use decdsss_phy::tx::{LiveTx, vox_lead};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What a live transmission is set up with.
#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub profile: Profile,
    pub mode: Mode,
    pub depth: usize,
    pub call: String,
    pub session: u8,
    pub codec: Codec,
    /// RMS level of the audio to the radio.
    pub level: f32,
    /// A tone ahead of the signal, to key a radio on VOX.
    pub vox: bool,
    /// The callsign and mode written on the waterfall ahead of the signal.
    pub wfid: bool,
    pub out_device: Option<String>,
    pub out_rate: u32,
    pub mic_device: Option<String>,
    /// The radio's keying: held down for as long as the signal goes out.
    pub radio: Radio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The preamble and header are going out; nothing of the speech yet.
    Starting,
    /// Speech is going out.
    OnAir,
    /// The PTT was let go: the rest and the end frame are going out.
    Ending,
    Done,
}

/// What the display shows of a live transmission.
#[derive(Clone, Debug)]
pub struct LiveStatus {
    pub phase: Phase,
    /// Seconds since keying up.
    pub elapsed: f64,
    /// Seconds from keying up to the start of the speech.
    pub lead_s: f64,
    /// Speech spoken and not yet sent, seconds.
    pub backlog_s: f64,
    pub blocks: usize,
    pub fillers: usize,
    /// Recent microphone peak, 0–1.
    pub mic_peak: f32,
    pub error: Option<String>,
}

struct Shared {
    frames: VecDeque<Vec<u8>>,
    /// The capture thread has queued everything, the end frame last.
    capture_done: bool,
    blocks: usize,
    fillers: usize,
    mic_peak: f32,
    /// Seconds of speech in the queue.
    queued_s: f64,
    modulator_done: bool,
    error: Option<String>,
}

pub struct LiveTxSession {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    started: Instant,
    lead_s: f64,
    handles: Vec<JoinHandle<()>>,
}

/// Speech frames are levelled to this peak (of full scale).
const AGC_TARGET: f32 = 0.4;

impl LiveTxSession {
    /// Opens the microphone and the output and starts sending. Errors (no such
    /// device, a callsign with no code) come back here.
    pub fn start(cfg: &LiveConfig) -> Result<LiveTxSession, String> {
        let ptype = ptype_of(cfg.codec).ok_or("this codec is not a voice mode here")?;
        let header = Header {
            profile: cfg.profile,
            mode: cfg.mode,
            depth: cfg.depth,
            ptype,
            call: cfg.call.clone(),
            session: ptype.fit_session(cfg.session),
            frame0: 0,
            nblocks: 4095,
            total_frames: 0,
        };
        let tx = LiveTx::new(header, f64::from(cfg.out_rate), cfg.level)
            .ok_or("cannot build the header (check the callsign)")?;
        let frame_bytes = tx.layout().frame_bytes();
        let enc = LiveEncoder::new(cfg.codec, frame_bytes).ok_or("Codec2 cannot start")?;
        let wfid_s = if cfg.wfid {
            let said = cfg.mode.to_string();
            banner::seconds(&[&cfg.call, &said], Band::mid(cfg.profile))
        } else {
            0.0
        };
        let lead_s = tx.lead_seconds() + if cfg.vox { 0.7 } else { 0.0 } + wfid_s;
        let frame_s = enc.frame_seconds();
        let shared = Arc::new(Mutex::new(Shared {
            frames: VecDeque::new(),
            capture_done: false,
            blocks: 0,
            fillers: 0,
            mic_peak: 0.0,
            queued_s: 0.0,
            modulator_done: false,
            error: None,
        }));
        let (stop, abort) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let mut handles = Vec::new();
        {
            let (shared, stop, abort, ready) = (
                shared.clone(),
                stop.clone(),
                abort.clone(),
                ready_tx.clone(),
            );
            let device = cfg.mic_device.clone();
            handles.push(std::thread::spawn(move || {
                capture(device, enc, shared, stop, abort, ready);
            }));
        }
        {
            let (shared, abort, ready) = (shared.clone(), abort.clone(), ready_tx);
            let cfg = cfg.clone();
            handles.push(std::thread::spawn(move || {
                modulate(&cfg, frame_s, tx, shared, abort, ready);
            }));
        }
        // Both threads say whether their device opened.
        for _ in 0..2 {
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    abort.store(true, Ordering::Relaxed);
                    return Err(e);
                }
                Err(_) => {
                    abort.store(true, Ordering::Relaxed);
                    return Err("the audio devices did not open".into());
                }
            }
        }
        Ok(LiveTxSession {
            shared,
            stop,
            abort,
            started: Instant::now(),
            lead_s,
            handles,
        })
    }

    /// The PTT was let go: finish the speech, send the end frame and stop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Cut the transmission now.
    pub fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    pub fn status(&self) -> LiveStatus {
        let g = self.shared.lock().unwrap();
        let elapsed = self.started.elapsed().as_secs_f64();
        let phase = if g.modulator_done || self.handles.iter().all(JoinHandle::is_finished) {
            Phase::Done
        } else if self.stop.load(Ordering::Relaxed) {
            Phase::Ending
        } else if elapsed < self.lead_s {
            Phase::Starting
        } else {
            Phase::OnAir
        };
        LiveStatus {
            phase,
            elapsed,
            lead_s: self.lead_s,
            backlog_s: g.queued_s,
            blocks: g.blocks,
            fillers: g.fillers,
            mic_peak: g.mic_peak,
            error: g.error.clone(),
        }
    }
}

impl Drop for LiveTxSession {
    fn drop(&mut self) {
        // Never leave a transmitter keyed behind a window that is gone.
        self.abort.store(true, Ordering::Relaxed);
    }
}

fn fail(shared: &Arc<Mutex<Shared>>, e: String) {
    shared.lock().unwrap().error.get_or_insert(e);
}

fn capture(
    device: Option<String>,
    mut enc: LiveEncoder,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let mut input = match InputStream::open(&InputOptions {
        device,
        channels: Some(1),
        ..InputOptions::default()
    }) {
        Ok(i) => {
            let _ = ready.send(Ok(()));
            i
        }
        Err(e) => {
            let _ = ready.send(Err(format!("cannot open the microphone: {e}")));
            return;
        }
    };
    let fmt = input.format();
    let ch = fmt.channels.max(1);
    let mut resampler = Resampler::new(f64::from(fmt.sample_rate), 8000.0, 3500.0, 4000.0);
    let frame_s = enc.frame_seconds();
    let (mut env, mut gain) = (0.0f32, 1.0f32);
    let mut out: Vec<Iq> = Vec::new();
    loop {
        if abort.load(Ordering::Relaxed) {
            return;
        }
        if input.is_dead() {
            fail(&shared, "the microphone stopped".into());
            break;
        }
        let frames = input.available_frames().min(8192);
        if frames == 0 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let mono = downmix_to_mono(&input.read(frames), ch);
        let iq: Vec<Iq> = mono.iter().map(|&v| Iq::new(v, 0.0)).collect();
        out.clear();
        resampler.process(&iq, &mut out);
        // Level the speech: a peak follower with a slow release, the gain to bring
        // it to a steady level, limited both ways.
        let peak = out.iter().fold(0.0f32, |m, z| m.max(z.re.abs()));
        env = peak.max(env * 0.995);
        let want = (AGC_TARGET / env.max(1e-4)).clamp(0.25, 30.0);
        gain += 0.1 * (want - gain);
        let pcm: Vec<i16> = out
            .iter()
            .map(|z| (z.re * gain * 32767.0).clamp(-32767.0, 32767.0) as i16)
            .collect();
        let made = enc.push(&pcm);
        let mut g = shared.lock().unwrap();
        g.mic_peak = (g.mic_peak * 0.8).max(peak.min(1.0));
        for f in made {
            g.queued_s += frame_s;
            g.frames.push_back(f);
        }
        drop(g);
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    // Let go: the rest of the speech and the end frame.
    let rest = enc.finish();
    let mut g = shared.lock().unwrap();
    for f in rest {
        g.queued_s += frame_s;
        g.frames.push_back(f);
    }
    g.capture_done = true;
}

fn modulate(
    cfg: &LiveConfig,
    frame_s: f64,
    mut tx: LiveTx,
    shared: Arc<Mutex<Shared>>,
    abort: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let mut out = match OutputStream::open(&OutputOptions {
        device: cfg.out_device.clone(),
        sample_rate: Some(cfg.out_rate),
        channels: Some(1),
        buffer: Duration::from_secs(2),
        start_threshold: Duration::from_millis(100),
    }) {
        Ok(o) => o,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot open the output: {e}")));
            return;
        }
    };
    let fmt = out.format();
    if fmt.sample_rate != cfg.out_rate {
        let _ = ready.send(Err(format!(
            "the output runs at {} Hz, not {} Hz",
            fmt.sample_rate, cfg.out_rate
        )));
        return;
    }
    let _ = ready.send(Ok(()));
    let ch = fmt.channels.max(1);
    // Key the transmitter before the first sample (declared after `out`, so
    // that any way out of here lets go of the key before the audio is cut). A
    // rig that will not key ends the transmission, with the reason.
    let hold = match cfg.radio.hold() {
        Ok(h) => h,
        Err(e) => {
            fail(&shared, e);
            abort.store(true, Ordering::Relaxed);
            shared.lock().unwrap().modulator_done = true;
            return;
        }
    };
    // For checking: what is played, as a WAV (DECDSSS_LIVE_WAV=path).
    let mut played: Vec<f32> = Vec::new();
    let keep = std::env::var_os("DECDSSS_LIVE_WAV").is_some();
    // Plays audio in tenth-of-a-second pieces, looking at the abort flag between.
    let mut play = |audio: &[f32]| -> bool {
        if keep {
            played.extend_from_slice(audio);
        }
        for piece in audio.chunks(cfg.out_rate as usize / 10) {
            if abort.load(Ordering::Relaxed) {
                return false;
            }
            let mut block = Vec::with_capacity(piece.len() * ch);
            for &v in piece {
                block.extend(std::iter::repeat_n(v, ch));
            }
            if let Err(e) = out.write_blocking(&block, Duration::from_secs(5)) {
                fail(&shared, e.to_string());
                return false;
            }
        }
        true
    };
    let mut alive = true;
    if cfg.vox {
        alive = play(&vox_lead(f64::from(cfg.out_rate), cfg.level, 0.4, 0.3));
    }
    if alive && cfg.wfid {
        let said = cfg.mode.to_string();
        alive = play(&banner::banner(
            &[&cfg.call, &said],
            Band::mid(cfg.profile),
            f64::from(cfg.out_rate),
            cfg.level,
        ));
    }
    if alive {
        alive = play(&tx.lead());
    }
    let frame_bytes = tx.layout().frame_bytes();
    let mut ending = false;
    while alive && !ending {
        // The next block, from what is ready (a filler for the rest, so the signal
        // never stops): chosen now, about a second before it goes out.
        let mut frames = Vec::with_capacity(cfg.depth);
        {
            let mut g = shared.lock().unwrap();
            while frames.len() < cfg.depth {
                let Some(f) = g.frames.pop_front() else { break };
                if f.first() == Some(&END) {
                    ending = true;
                } else {
                    g.queued_s = (g.queued_s - frame_s).max(0.0);
                }
                frames.push(f);
                if ending {
                    break;
                }
            }
            while frames.len() < cfg.depth {
                frames.push(filler_frame(frame_bytes));
                g.fillers += usize::from(!ending);
            }
            g.blocks += 1;
        }
        alive = play(&tx.block(&frames));
    }
    if alive {
        // The talker let go (not a STOP: that cuts the signal off): the postamble
        // tells the other end the stream is over.
        alive = play(&tx.eot());
    }
    if alive {
        alive = play(&tx.finish());
    }
    if alive {
        out.drain(Duration::from_secs(10));
    }
    // Played out: the tail time, then let go. Stopped or failed: `hold` lets go
    // at once as it is dropped.
    if alive && let Some(h) = hold {
        h.finish();
    }
    let stats = out.stats();
    if stats.underruns > 0 {
        // The signal had gaps: the other end will have lost blocks.
        fail(
            &shared,
            format!(
                "the output ran dry {} times: part of the transmission was broken",
                stats.underruns
            ),
        );
    }
    if let Some(path) = std::env::var_os("DECDSSS_LIVE_WAV") {
        let _ = decdsss_audio::wav::write(std::path::Path::new(&path), &played, cfg.out_rate);
    }
    shared.lock().unwrap().modulator_done = true;
}

fn filler_frame(frame_bytes: usize) -> Vec<u8> {
    let mut f = vec![0u8; frame_bytes];
    f[0] = decdsss_link::live::FILLER;
    f
}
