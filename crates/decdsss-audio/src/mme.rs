// made by CasualArclamp
//! The MME audio API (WinMM: `waveIn*` / `waveOut*`), which cpal does not offer.
//!
//! MME is the oldest Windows audio interface. It takes plain PCM in whatever rate and
//! channel count you ask for (Windows converts to the device's own format underneath), so
//! the usual modem rates just work, and every device appears under one short name.
//!
//! # How a stream runs
//!
//! Each stream owns a ring of small buffers (*blocks*, 20 ms each) that the driver fills
//! (capture) or plays (playback), and one worker thread. The driver signals a Win32 event
//! whenever a block finishes; the thread wakes, hands the finished blocks to the same
//! lock-free [`Capturer`] / [`Renderer`] halves the cpal path uses (see [`crate::stream`]),
//! and gives them back to the driver. So the callback rules in that module's header apply
//! here too: nothing in the worker loop locks or allocates.
//!
//! Samples are 16-bit PCM on the wire, converted to and from `f32` in the worker.
//!
//! # Rust notes
//!
//! * `windows::Win32::Media::Audio` is a thin `unsafe` binding to the C API: every call
//!   returns a `u32` status (`MMSYSERR_NOERROR` = 0) that [`check`] turns into a `Result`.
//! * A driver keeps *pointers* to the `WAVEHDR`s and sample buffers we give it, so they must
//!   not move while queued. Each lives on the heap (`Box`, `Vec`), which keeps its address
//!   when the owning [`Block`] is moved around.
//! * Raw handles are not `Send`, so [`InCtx`] / [`OutCtx`] carry an `unsafe impl Send`: a
//!   wave handle may be used from any thread, and only the worker touches the blocks.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::{ErrorKind, Sample};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    CALLBACK_EVENT, HWAVEIN, HWAVEOUT, WAVE_FORMAT_PCM, WAVE_FORMAT_QUERY, WAVE_MAPPER,
    WAVEFORMATEX, WAVEHDR, WAVEINCAPSW, WAVEOUTCAPSW, WAVERR_BADFORMAT, WHDR_DONE, waveInAddBuffer,
    waveInClose, waveInGetDevCapsW, waveInGetNumDevs, waveInMessage, waveInOpen,
    waveInPrepareHeader, waveInReset, waveInStart, waveInStop, waveInUnprepareHeader, waveOutClose,
    waveOutGetDevCapsW, waveOutGetErrorTextW, waveOutGetNumDevs, waveOutMessage, waveOutOpen,
    waveOutPause, waveOutPrepareHeader, waveOutReset, waveOutRestart, waveOutUnprepareHeader,
    waveOutWrite,
};
use windows::Win32::Media::Multimedia::{
    DRV_QUERYFUNCTIONINSTANCEID, DRV_QUERYFUNCTIONINSTANCEIDSIZE, DRVM_MAPPER_PREFERRED_GET,
};
use windows::Win32::Media::{
    MMSYSERR_ALLOCATED, MMSYSERR_BADDEVICEID, MMSYSERR_NODRIVER, MMSYSERR_NOERROR,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetEvent, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PSTR};

use crate::device::{ConfigRange, DeviceInfo, channel_cost, pick_by_name, rate_cost};
use crate::stream::{Capturer, ErrorSink, Renderer};
use crate::{AudioFormat, Direction, Error, Result, WORKING_RATE};

/// Length of one driver block, milliseconds.
const BLOCK_MS: u32 = 20;
/// Blocks in flight on capture (160 ms of slack for a busy machine).
const IN_BLOCKS: usize = 8;
/// Blocks in flight on playback: the delay between the renderer and the speaker.
const OUT_BLOCKS: usize = 6;
/// Rates probed when listing what a device accepts.
const RATES: [u32; 10] = [
    8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 96000,
];
const HDR_SIZE: u32 = size_of::<WAVEHDR>() as u32;

/// One MME device: its index in the driver's list (or `WAVE_MAPPER`, the system default).
#[derive(Clone, Debug)]
pub(crate) struct MmeDevice {
    pub index: u32,
    pub name: String,
    pub id: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn error_text(code: u32) -> String {
    let mut buf = [0u16; 160];
    // SAFETY: the buffer outlives the call and its length is passed with it.
    let ok = unsafe { waveOutGetErrorTextW(code, &mut buf) } == MMSYSERR_NOERROR;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if ok && end > 0 {
        String::from_utf16_lossy(&buf[..end])
    } else {
        format!("code {code}")
    }
}

fn mme_error(what: &str, code: u32) -> cpal::Error {
    let kind = match code {
        MMSYSERR_ALLOCATED => ErrorKind::DeviceBusy,
        WAVERR_BADFORMAT => ErrorKind::UnsupportedConfig,
        MMSYSERR_NODRIVER | MMSYSERR_BADDEVICEID => ErrorKind::DeviceNotAvailable,
        _ => ErrorKind::DeviceNotAvailable,
    };
    cpal::Error::with_message(kind, format!("MME {what}: {}", error_text(code)))
}

/// `Ok` for `MMSYSERR_NOERROR`, else the driver's own message.
fn check(what: &str, code: u32) -> Result<()> {
    if code == MMSYSERR_NOERROR {
        Ok(())
    } else {
        Err(Error::Audio(mme_error(what, code)))
    }
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

fn device_count(direction: Direction) -> u32 {
    // SAFETY: no arguments; plain query.
    unsafe {
        match direction {
            Direction::Input => waveInGetNumDevs(),
            Direction::Output => waveOutGetNumDevs(),
        }
    }
}

fn utf16_until_nul(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// The device's name. MME truncates it to 31 characters (`MAXPNAMELEN`), so a long one such
/// as "CABLE Output (VB-Audio Virtual Cable)" arrives cut short; matching by a unique part of
/// the name (as everywhere in this crate) still works.
fn device_name(direction: Direction, index: u32) -> Option<String> {
    // SAFETY: the caps struct is plain data written by the call; its size is passed with it.
    unsafe {
        match direction {
            Direction::Input => {
                let mut c = WAVEINCAPSW::default();
                (waveInGetDevCapsW(index as usize, &mut c, size_of::<WAVEINCAPSW>() as u32)
                    == MMSYSERR_NOERROR)
                    // A field of a packed struct is copied out, never borrowed.
                    .then(|| utf16_until_nul(&{ c.szPname }))
            }
            Direction::Output => {
                let mut c = WAVEOUTCAPSW::default();
                (waveOutGetDevCapsW(index as usize, &mut c, size_of::<WAVEOUTCAPSW>() as u32)
                    == MMSYSERR_NOERROR)
                    .then(|| utf16_until_nul(&{ c.szPname }))
            }
        }
    }
}

/// The endpoint's stable instance id (survives reboots), if the driver gives one.
fn instance_id(direction: Direction, index: u32) -> Option<String> {
    let mut bytes = 0u32;
    let handle = index as usize as *mut c_void;
    // SAFETY: the device index stands in for a handle in these two driver messages; the
    // size and buffer pointers are valid for the call.
    let size_ok = unsafe {
        match direction {
            Direction::Input => waveInMessage(
                Some(HWAVEIN(handle)),
                DRV_QUERYFUNCTIONINSTANCEIDSIZE,
                Some(&mut bytes as *mut u32 as usize),
                None,
            ),
            Direction::Output => waveOutMessage(
                Some(HWAVEOUT(handle)),
                DRV_QUERYFUNCTIONINSTANCEIDSIZE,
                &mut bytes as *mut u32 as usize,
                0,
            ),
        }
    } == MMSYSERR_NOERROR;
    if !size_ok || bytes == 0 || bytes > 4096 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let ptr = buf.as_mut_ptr() as usize;
    // SAFETY: as above; `buf` holds `bytes` bytes.
    let ok = unsafe {
        match direction {
            Direction::Input => waveInMessage(
                Some(HWAVEIN(handle)),
                DRV_QUERYFUNCTIONINSTANCEID,
                Some(ptr),
                Some(bytes as usize),
            ),
            Direction::Output => waveOutMessage(
                Some(HWAVEOUT(handle)),
                DRV_QUERYFUNCTIONINSTANCEID,
                ptr,
                bytes as usize,
            ),
        }
    } == MMSYSERR_NOERROR;
    let id = utf16_until_nul(&buf);
    (ok && !id.is_empty()).then_some(id)
}

/// Index of the system default device of this direction, if there are any devices.
fn default_index(direction: Direction, count: u32) -> Option<u32> {
    if count == 0 {
        return None;
    }
    let (mut dev, mut status) = (u32::MAX, 0u32);
    let mapper = WAVE_MAPPER as usize as *mut c_void;
    // SAFETY: the mapper "handle" and out-pointers are valid for the call.
    let ok = unsafe {
        match direction {
            Direction::Input => waveInMessage(
                Some(HWAVEIN(mapper)),
                DRVM_MAPPER_PREFERRED_GET,
                Some(&mut dev as *mut u32 as usize),
                Some(&mut status as *mut u32 as usize),
            ),
            Direction::Output => waveOutMessage(
                Some(HWAVEOUT(mapper)),
                DRVM_MAPPER_PREFERRED_GET,
                &mut dev as *mut u32 as usize,
                &mut status as *mut u32 as usize,
            ),
        }
    } == MMSYSERR_NOERROR;
    // Windows lists the default device first when it cannot say which one it is.
    Some(if ok && dev < count { dev } else { 0 })
}

/// Every device of `direction`, in the driver's order.
pub(crate) fn devices(direction: Direction) -> Vec<MmeDevice> {
    (0..device_count(direction))
        .filter_map(|index| {
            Some(MmeDevice {
                index,
                name: device_name(direction, index)?,
                id: instance_id(direction, index),
            })
        })
        .collect()
}

/// The 16-bit PCM wave format for `rate` Hz and `channels`.
fn wave_format(rate: u32, channels: u16) -> WAVEFORMATEX {
    let block_align = channels * 2;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Whether the device would open at this format (a dry run: nothing is opened).
fn accepts(direction: Direction, index: u32, rate: u32, channels: u16) -> bool {
    let wf = wave_format(rate, channels);
    // SAFETY: `wf` outlives the call; with WAVE_FORMAT_QUERY no handle is returned.
    let r = unsafe {
        match direction {
            Direction::Input => waveInOpen(None, index, &wf, None, None, WAVE_FORMAT_QUERY),
            Direction::Output => waveOutOpen(None, index, &wf, None, None, WAVE_FORMAT_QUERY),
        }
    };
    r == MMSYSERR_NOERROR
}

/// The formats the device accepts: mono and stereo at the common rates.
fn supported(direction: Direction, index: u32) -> Vec<AudioFormat> {
    let mut out = Vec::new();
    for channels in [1u16, 2] {
        for rate in RATES {
            if accepts(direction, index, rate, channels) {
                out.push(AudioFormat::new(rate, usize::from(channels)));
            }
        }
    }
    out
}

/// The format closest to what was asked: channel count first, then rate (exact, higher,
/// lower), as `device::choose_config` does for cpal.
fn pick(formats: &[AudioFormat], want_rate: u32, want_channels: usize) -> Option<AudioFormat> {
    let cost = |f: &AudioFormat| {
        (
            channel_cost(f.channels, want_channels),
            rate_cost(f.sample_rate, want_rate),
        )
    };
    formats.iter().copied().min_by(|a, b| {
        cost(a)
            .partial_cmp(&cost(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Describes every device of `direction` (what `device::list_devices` returns for cpal).
pub(crate) fn list(direction: Direction) -> Vec<DeviceInfo> {
    let devs = devices(direction);
    let default = default_index(direction, device_count(direction));
    devs.into_iter()
        .map(|d| {
            let formats = supported(direction, d.index);
            DeviceInfo {
                is_default: Some(d.index) == default,
                default_format: pick(&formats, WORKING_RATE, 2),
                configs: formats
                    .iter()
                    .map(|f| ConfigRange {
                        channels: f.channels,
                        min_sample_rate: f.sample_rate,
                        max_sample_rate: f.sample_rate,
                        sample_format: "i16".to_string(),
                    })
                    .collect(),
                name: d.name,
                id: d.id,
                host: "MME".to_string(),
                direction,
            }
        })
        .collect()
}

/// Name of the system default device.
pub(crate) fn default_device_name(direction: Direction) -> Option<String> {
    let devs = devices(direction);
    let index = default_index(direction, device_count(direction))?;
    devs.into_iter().find(|d| d.index == index).map(|d| d.name)
}

/// Finds a device the way `device::find_device` does: `None` is the system default
/// (opened through the wave mapper, so it follows the default as it changes).
pub(crate) fn find_device(direction: Direction, name: Option<&str>) -> Result<MmeDevice> {
    let Some(wanted) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        let name = default_device_name(direction).ok_or(Error::NoDefaultDevice(direction))?;
        return Ok(MmeDevice {
            index: WAVE_MAPPER,
            name,
            id: None,
        });
    };
    let items = devices(direction)
        .into_iter()
        .map(|d| (d.name.clone(), d.id.clone(), d))
        .collect();
    pick_by_name(direction, items, wanted)
}

/// The stream format to open `dev` with: as close to the request as the device allows.
pub(crate) fn choose_format(
    direction: Direction,
    dev: &MmeDevice,
    want_rate: Option<u32>,
    want_channels: Option<usize>,
) -> Result<AudioFormat> {
    let want_ch = want_channels.unwrap_or(2);
    // A rate that was asked for is tried exactly as asked, not only from the common list:
    // Windows converts to the device's own rate, so any rate it can convert opens (44.1 kHz,
    // 48 kHz, 88.2 kHz, 192 kHz, ...) and the stream runs on the caller's time base.
    // Mono and stereo are the channel counts MME is asked for; the wanted one goes first.
    if let Some(rate) = want_rate.filter(|&r| r > 0) {
        let order: [u16; 2] = if want_ch == 1 { [1, 2] } else { [2, 1] };
        for channels in order {
            if accepts(direction, dev.index, rate, channels) {
                return Ok(AudioFormat::new(rate, usize::from(channels)));
            }
        }
    }
    let formats = supported(direction, dev.index);
    pick(&formats, want_rate.unwrap_or(WORKING_RATE), want_ch)
        .ok_or_else(|| Error::NoUsableConfig(dev.name.clone()))
}

// ---------------------------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------------------------

/// A Win32 auto-reset event the driver signals when a block is done.
struct Event(HANDLE);

// SAFETY: a Win32 event handle may be signalled and closed from any thread.
unsafe impl Send for Event {}

impl Event {
    fn new() -> Result<Event> {
        // SAFETY: no attributes, no name; the handle is closed in `Drop`.
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map(Event)
            .map_err(|e| {
                Error::Audio(cpal::Error::with_message(
                    ErrorKind::ResourceExhausted,
                    format!("MME event: {e}"),
                ))
            })
    }

    /// The handle as an integer, to pass to the driver and the worker thread.
    fn raw(&self) -> usize {
        self.0.0 as usize
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and still open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// One driver block: the header the driver reads, and the samples it points at.
struct Block {
    hdr: Box<WAVEHDR>,
    data: Vec<i16>,
}

impl Block {
    fn new(samples: usize) -> Block {
        let mut data = vec![0i16; samples];
        let mut hdr = Box::new(WAVEHDR::default());
        hdr.lpData = PSTR(data.as_mut_ptr().cast::<u8>());
        hdr.dwBufferLength = (samples * 2) as u32;
        Block { hdr, data }
    }

    /// The driver has finished with this block.
    fn done(&self) -> bool {
        self.hdr.dwFlags & WHDR_DONE != 0
    }
}

/// A running MME stream: the worker thread plus what pauses it. Dropping stops it.
pub(crate) struct MmeStream {
    direction: Direction,
    /// The wave handle (`HWAVEIN` / `HWAVEOUT`) as an integer; owned by the worker's context.
    handle: usize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    latency: Duration,
    // Declared last so it closes after the thread is joined (see `Drop`).
    event: Event,
}

impl MmeStream {
    /// Stops capture / playback until [`play`](Self::play).
    pub(crate) fn pause(&self) -> Result<()> {
        // SAFETY: a stale handle (device lost, worker gone) just returns an error.
        let r = unsafe {
            match self.direction {
                Direction::Input => waveInStop(HWAVEIN(self.handle as *mut c_void)),
                Direction::Output => waveOutPause(HWAVEOUT(self.handle as *mut c_void)),
            }
        };
        check("pause", r)
    }

    pub(crate) fn play(&self) -> Result<()> {
        // SAFETY: as in `pause`.
        let r = unsafe {
            match self.direction {
                Direction::Input => waveInStart(HWAVEIN(self.handle as *mut c_void)),
                Direction::Output => waveOutRestart(HWAVEOUT(self.handle as *mut c_void)),
            }
        };
        check("start", r)
    }

    /// Audio already handed to the driver but not yet heard (playback), or the longest the
    /// driver holds captured audio before delivering it.
    pub(crate) fn latency(&self) -> Duration {
        self.latency
    }
}

impl Drop for MmeStream {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        // SAFETY: `self.event` is open until this struct's fields drop, after the join.
        unsafe {
            let _ = SetEvent(self.event.0);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn block_frames(rate: u32) -> usize {
    (rate * BLOCK_MS / 1000).max(1) as usize
}

/// Puts the worker above normal threads so a busy desktop does not starve the blocks.
fn raise_priority() {
    // SAFETY: the pseudo-handle of the calling thread is always valid.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

fn spawn_worker(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .map_err(|e| {
            Error::Audio(cpal::Error::with_message(
                ErrorKind::ResourceExhausted,
                format!("MME worker thread: {e}"),
            ))
        })
}

// -- capture ----------------------------------------------------------------------------------

/// Everything a capture worker owns; closing the device is its `Drop`.
struct InCtx {
    hwi: HWAVEIN,
    blocks: Vec<Block>,
    /// How many of `blocks` have been prepared (only those are unprepared).
    prepared: usize,
}

// SAFETY: a wave handle may be used from any thread; the blocks move with the context and
// only one thread ever holds it.
unsafe impl Send for InCtx {}

impl Drop for InCtx {
    fn drop(&mut self) {
        // SAFETY: reset returns every queued block, after which they can be unprepared and
        // the device closed.
        unsafe {
            waveInReset(self.hwi);
            for b in self.blocks.iter_mut().take(self.prepared) {
                waveInUnprepareHeader(self.hwi, &mut *b.hdr, HDR_SIZE);
            }
            waveInClose(self.hwi);
        }
    }
}

/// Opens `dev` for capture at `fmt` and starts the worker.
pub(crate) fn open_input(
    dev: &MmeDevice,
    fmt: AudioFormat,
    capturer: Capturer,
    sink: ErrorSink,
) -> Result<MmeStream> {
    let event = Event::new()?;
    let wf = wave_format(fmt.sample_rate, fmt.channels as u16);
    let mut hwi = HWAVEIN(std::ptr::null_mut());
    // SAFETY: `wf` and `hwi` outlive the call; the event outlives the stream.
    let r = unsafe {
        waveInOpen(
            Some(&mut hwi),
            dev.index,
            &wf,
            Some(event.raw()),
            None,
            CALLBACK_EVENT,
        )
    };
    check("waveInOpen", r)?;
    // From here `ctx` closes the device on any early return.
    let mut ctx = InCtx {
        hwi,
        blocks: Vec::with_capacity(IN_BLOCKS),
        prepared: 0,
    };
    let samples = block_frames(fmt.sample_rate) * fmt.channels;
    for _ in 0..IN_BLOCKS {
        ctx.blocks.push(Block::new(samples));
        let b = ctx.blocks.last_mut().expect("just pushed");
        // SAFETY: the header and its buffer are heap allocated and stay put while queued.
        unsafe {
            check(
                "waveInPrepareHeader",
                waveInPrepareHeader(hwi, &mut *b.hdr, HDR_SIZE),
            )?;
            ctx.prepared += 1;
            check(
                "waveInAddBuffer",
                waveInAddBuffer(hwi, &mut *b.hdr, HDR_SIZE),
            )?;
        }
    }
    // SAFETY: the device is open with its blocks queued.
    check("waveInStart", unsafe { waveInStart(hwi) })?;

    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, ev) = (Arc::clone(&stop), event.raw());
    let thread = spawn_worker("mme-in", move || run_input(ctx, capturer, sink, stop2, ev))?;
    Ok(MmeStream {
        direction: Direction::Input,
        handle: hwi.0 as usize,
        stop,
        thread: Some(thread),
        latency: Duration::from_millis(u64::from(BLOCK_MS)),
        event,
    })
}

fn run_input(
    mut ctx: InCtx,
    mut cap: Capturer,
    mut sink: ErrorSink,
    stop: Arc<AtomicBool>,
    event: usize,
) {
    raise_priority();
    let event = HANDLE(event as *mut c_void);
    let hwi = ctx.hwi;
    let n = ctx.blocks.len();
    let mut next = 0;
    while !stop.load(Relaxed) {
        // SAFETY: the event is open until the stream is dropped, which joins this thread
        // first. The timeout only bounds how long a stop request waits.
        unsafe {
            WaitForSingleObject(event, 50);
        }
        // Blocks complete in the order they were queued.
        while ctx.blocks[next].done() {
            let b = &mut ctx.blocks[next];
            let got = (b.hdr.dwBytesRecorded as usize / 2).min(b.data.len());
            cap.capture::<i16>(&b.data[..got]);
            // SAFETY: hand the block back to the driver.
            let r = unsafe { waveInAddBuffer(hwi, &mut *b.hdr, HDR_SIZE) };
            if r != MMSYSERR_NOERROR {
                sink.report(mme_error("capture", r));
                return;
            }
            next = (next + 1) % n;
        }
    }
}

// -- playback ---------------------------------------------------------------------------------

/// Everything a playback worker owns; closing the device is its `Drop`.
struct OutCtx {
    hwo: HWAVEOUT,
    blocks: Vec<Block>,
    prepared: usize,
}

// SAFETY: as for `InCtx`.
unsafe impl Send for OutCtx {}

impl Drop for OutCtx {
    fn drop(&mut self) {
        // SAFETY: reset stops playback and returns every queued block.
        unsafe {
            waveOutReset(self.hwo);
            for b in self.blocks.iter_mut().take(self.prepared) {
                waveOutUnprepareHeader(self.hwo, &mut *b.hdr, HDR_SIZE);
            }
            waveOutClose(self.hwo);
        }
    }
}

/// Renders the next block of audio (or silence) from the ring buffer into `b`.
fn fill(b: &mut Block, renderer: &mut Renderer, scratch: &mut [f32]) {
    let scratch = &mut scratch[..b.data.len()];
    renderer.render(scratch);
    for (d, &x) in b.data.iter_mut().zip(scratch.iter()) {
        *d = i16::from_sample(x.clamp(-1.0, 1.0));
    }
}

/// Opens `dev` for playback at `fmt` and starts the worker. The queue starts full of the
/// renderer's silence, so playback is gapless from the first block.
pub(crate) fn open_output(
    dev: &MmeDevice,
    fmt: AudioFormat,
    mut renderer: Renderer,
    sink: ErrorSink,
) -> Result<MmeStream> {
    let event = Event::new()?;
    let wf = wave_format(fmt.sample_rate, fmt.channels as u16);
    let mut hwo = HWAVEOUT(std::ptr::null_mut());
    // SAFETY: as in `open_input`.
    let r = unsafe {
        waveOutOpen(
            Some(&mut hwo),
            dev.index,
            &wf,
            Some(event.raw()),
            None,
            CALLBACK_EVENT,
        )
    };
    check("waveOutOpen", r)?;
    let mut ctx = OutCtx {
        hwo,
        blocks: Vec::with_capacity(OUT_BLOCKS),
        prepared: 0,
    };
    let samples = block_frames(fmt.sample_rate) * fmt.channels;
    let mut scratch = vec![0.0f32; samples];
    for _ in 0..OUT_BLOCKS {
        ctx.blocks.push(Block::new(samples));
        let b = ctx.blocks.last_mut().expect("just pushed");
        // SAFETY: as in `open_input`.
        unsafe {
            check(
                "waveOutPrepareHeader",
                waveOutPrepareHeader(hwo, &mut *b.hdr, HDR_SIZE),
            )?;
            ctx.prepared += 1;
            fill(b, &mut renderer, &mut scratch);
            check("waveOutWrite", waveOutWrite(hwo, &mut *b.hdr, HDR_SIZE))?;
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, ev) = (Arc::clone(&stop), event.raw());
    let thread = spawn_worker("mme-out", move || {
        run_output(ctx, renderer, sink, stop2, ev, scratch)
    })?;
    Ok(MmeStream {
        direction: Direction::Output,
        handle: hwo.0 as usize,
        stop,
        thread: Some(thread),
        latency: Duration::from_millis(u64::from(BLOCK_MS) * OUT_BLOCKS as u64),
        event,
    })
}

fn run_output(
    mut ctx: OutCtx,
    mut renderer: Renderer,
    mut sink: ErrorSink,
    stop: Arc<AtomicBool>,
    event: usize,
    mut scratch: Vec<f32>,
) {
    raise_priority();
    let event = HANDLE(event as *mut c_void);
    let hwo = ctx.hwo;
    let n = ctx.blocks.len();
    let mut next = 0;
    while !stop.load(Relaxed) {
        // SAFETY: as in `run_input`.
        unsafe {
            WaitForSingleObject(event, 50);
        }
        while ctx.blocks[next].done() {
            let b = &mut ctx.blocks[next];
            fill(b, &mut renderer, &mut scratch);
            // SAFETY: the block is prepared and the driver has finished with it.
            let r = unsafe { waveOutWrite(hwo, &mut *b.hdr, HDR_SIZE) };
            if r != MMSYSERR_NOERROR {
                sink.report(mme_error("playback", r));
                return;
            }
            next = (next + 1) % n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wave_format_fields() {
        let wf = wave_format(48_000, 2);
        // Packed struct: copy the fields out before comparing.
        let (tag, ch, rate, avg, align, bits) = (
            wf.wFormatTag,
            wf.nChannels,
            wf.nSamplesPerSec,
            wf.nAvgBytesPerSec,
            wf.nBlockAlign,
            wf.wBitsPerSample,
        );
        assert_eq!(
            (tag, ch, rate, avg, align, bits),
            (1, 2, 48_000, 192_000, 4, 16)
        );
    }

    #[test]
    fn format_choice_prefers_channels_then_rate() {
        let f = |r, c| AudioFormat::new(r, c);
        let all = [f(8000, 1), f(48_000, 1), f(8000, 2), f(48_000, 2)];
        assert_eq!(pick(&all, 48_000, 1), Some(f(48_000, 1)));
        assert_eq!(pick(&all, 8000, 2), Some(f(8000, 2)));
        // Only stereo offered: a mono request takes it (the caller downmixes).
        assert_eq!(pick(&[f(44_100, 2)], 48_000, 1), Some(f(44_100, 2)));
        // Mono-only device, stereo wanted: the nearest is mono.
        assert_eq!(pick(&[f(48_000, 1)], 48_000, 2), Some(f(48_000, 1)));
        assert_eq!(pick(&[], 48_000, 2), None);
    }

    #[test]
    fn block_sizes() {
        assert_eq!(block_frames(48_000), 960);
        assert_eq!(block_frames(8000), 160);
    }

    #[test]
    fn listing_does_not_panic_and_names_are_short() {
        // Works on a machine with or without sound cards.
        for dir in [Direction::Input, Direction::Output] {
            for d in devices(dir) {
                assert!(d.name.chars().count() <= 31, "{}", d.name);
            }
        }
    }
}
