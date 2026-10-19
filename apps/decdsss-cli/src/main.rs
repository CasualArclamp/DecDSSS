// made by CasualArclamp
//! `decdsss`: the command line.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use decdsss_audio::{InputOptions, InputStream, OutputOptions, OutputStream, wav};
use decdsss_chan::{Channel, Watterson};
use decdsss_core::{Mode, Profile};
use decdsss_link::{Assembler, SendSpec, container, max_stream_bytes, plan};
use decdsss_phy::codes::{normalise_call, own_code};
use decdsss_phy::framing::{Layout, PayloadType};
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{chips_to_audio, segment_chips};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "decdsss",
    version,
    about = "Narrowband DSSS / CSK modem for amateur radio"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the mode ladder with rates for each bandwidth profile.
    Modes {
        /// Only this profile: 500, 2k4 or 3k.
        #[arg(long)]
        profile: Option<String>,
    },
    /// List the sound cards (inputs and outputs) with their rates.
    Devices,
    /// Turn a file into a transmission (a WAV of audio to feed a radio).
    Tx {
        /// The file to send.
        file: PathBuf,
        /// Your callsign (also picks the spreading code your data rides on).
        #[arg(long)]
        call: String,
        /// Bandwidth profile: 500, 2k4 or 3k.
        #[arg(long, default_value = "2k4")]
        profile: String,
        /// Mode, for example ds8-1/2, ds4-2/3, csk64-1/2 (see `decdsss modes`).
        #[arg(long, default_value = "ds8-1/2")]
        mode: String,
        /// Codewords per block: 1, 2, 4 or 8. Deeper rides out longer fades.
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Seconds of payload between preambles (late joiners wait this long).
        #[arg(long, default_value_t = 60.0)]
        resync: f64,
        /// Coded frames sent with the data frames, in percent of them: any
        /// frames, in any number of passes, rebuild the file once as many have
        /// arrived as there are data frames.
        #[arg(long, default_value_t = 25.0)]
        parity: f64,
        /// Passes to send; each later pass sends new coded frames.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Send the file as a picture: scaled and compressed to a progressive JPEG
        /// that fits --air seconds (a pass), drawn on the receiver as it arrives.
        #[arg(long)]
        picture: bool,
        /// With --picture, seconds of signal one pass may take.
        #[arg(long, default_value_t = 120.0)]
        air: f64,
        /// With --picture, the lowest JPEG quality (1-100) to accept before the
        /// picture is made smaller instead.
        #[arg(long, default_value_t = 30)]
        min_quality: u8,
        /// With --picture, also write the JPEG as sent to this file.
        #[arg(long)]
        save_jpeg: Option<PathBuf>,
        /// Session number 0-63, to tell transmissions apart.
        #[arg(long)]
        session: Option<u8>,
        /// Sample rate of the WAV, Hz.
        #[arg(long, default_value_t = 48000)]
        rate: u32,
        /// RMS level of the audio, 0-1.
        #[arg(long, default_value_t = 0.2)]
        level: f32,
        /// Seconds of silence before (for VOX or a keying delay).
        #[arg(long, default_value_t = 1.0)]
        lead: f64,
        /// Seconds of silence after.
        #[arg(long, default_value_t = 1.0)]
        tail: f64,
        /// The WAV to write.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Play the audio on this output device (a name or part of one; "default"
        /// for the system default). You key the radio.
        #[arg(long)]
        play: Option<String>,
    },
    /// Receive: decode a WAV recording and save the files in it.
    Rx {
        /// The recording to decode (omit with --device).
        input: Option<PathBuf>,
        /// Listen on this input device instead of a file ("default" for the
        /// system default).
        #[arg(long)]
        device: Option<String>,
        /// With --device, stop after this many seconds (default: until Ctrl-C).
        #[arg(long)]
        duration: Option<f64>,
        /// Profiles to listen on: all, or a comma-separated list of 500, 2k4, 3k.
        #[arg(long, default_value = "all")]
        profile: String,
        /// Where received files go.
        #[arg(long, default_value = ".")]
        out_dir: PathBuf,
        /// Show every block, not just headers and files.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Measure a mode: frame loss against Eb/N0 through the whole receiver.
    Sweep {
        #[arg(long, default_value = "2k4")]
        profile: String,
        #[arg(long)]
        mode: String,
        /// Codewords per block.
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Blocks per segment.
        #[arg(long, default_value_t = 2)]
        blocks: u16,
        /// Segments sent back to back (a long file).
        #[arg(long, default_value_t = 1)]
        segments: usize,
        /// Eb/N0 range start, end and step, dB.
        #[arg(long, num_args = 3, value_names = ["FROM", "TO", "STEP"], allow_negative_numbers = true)]
        ebn0: Vec<f64>,
        /// Transmissions per point.
        #[arg(long, default_value_t = 12)]
        trials: usize,
        /// Watterson fading: good, moderate, poor or flutter (default none).
        #[arg(long)]
        fading: Option<String>,
        /// Carrier offset around which the trials are spread, Hz.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        offset: f64,
        /// Clock error around which the trials are spread, ppm.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        clock_ppm: f64,
    },
    /// Run a WAV through the HF channel simulator.
    Channel {
        input: PathBuf,
        output: PathBuf,
        /// SNR in the profile's bandwidth, dB (omit for no noise).
        #[arg(long, allow_negative_numbers = true)]
        snr: Option<f64>,
        /// The profile the SNR is stated in: 500, 2k4 or 3k.
        #[arg(long, default_value = "2k4")]
        profile: String,
        /// Watterson fading: good, moderate, poor or flutter.
        #[arg(long)]
        fading: Option<String>,
        /// Frequency offset, Hz.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        offset: f64,
        /// Frequency drift, Hz per second.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        drift: f64,
        /// Sound-card clock error, ppm.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        clock_ppm: f64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Modes { profile } => modes(profile.as_deref()),
        Command::Devices => devices(),
        Command::Tx {
            file,
            call,
            profile,
            mode,
            depth,
            resync,
            parity,
            repeat,
            picture,
            air,
            min_quality,
            save_jpeg,
            session,
            rate,
            level,
            lead,
            tail,
            output,
            play,
        } => tx(
            &file,
            &call,
            &profile,
            &mode,
            depth,
            resync,
            parity,
            repeat,
            picture.then_some(PictureOpts {
                air,
                min_quality,
                save_jpeg,
            }),
            session,
            rate,
            level,
            lead,
            tail,
            output.as_deref(),
            play.as_deref(),
        ),
        Command::Rx {
            input,
            device,
            duration,
            profile,
            out_dir,
            verbose,
        } => rx(
            input.as_deref(),
            device.as_deref(),
            duration,
            &profile,
            &out_dir,
            verbose,
        ),
        Command::Sweep {
            profile,
            mode,
            depth,
            blocks,
            segments,
            ebn0,
            trials,
            fading,
            offset,
            clock_ppm,
        } => sweep(
            &profile,
            &mode,
            depth,
            blocks,
            segments,
            &ebn0,
            trials,
            fading.as_deref(),
            offset,
            clock_ppm,
        ),
        Command::Channel {
            input,
            output,
            snr,
            profile,
            fading,
            offset,
            drift,
            clock_ppm,
            seed,
        } => channel(
            &input,
            &output,
            snr,
            &profile,
            fading.as_deref(),
            offset,
            drift,
            clock_ppm,
            seed,
        ),
    }
}

fn parse_profile(s: &str) -> Result<Profile> {
    Profile::from_name(s).with_context(|| format!("unknown profile {s:?}: use 500, 2k4 or 3k"))
}

fn modes(profile: Option<&str>) -> Result<()> {
    let profiles: Vec<Profile> = match profile {
        Some(s) => vec![parse_profile(s)?],
        None => Profile::ALL.to_vec(),
    };
    for p in profiles {
        let (lo, hi) = p.edges_hz();
        println!(
            "{} — {} chip/s, {:.0}–{:.0} Hz, chip {:.2} ms",
            p.label(),
            p.chip_rate(),
            lo,
            hi,
            p.chip_s() * 1e3
        );
        println!(
            "  {:<12} {:>9} {:>11} {:>10} {:>11} {:>13}",
            "mode", "symbol/s", "coded bit/s", "info bit/s", "chips/bit", "SNR − Eb/N0"
        );
        for m in Mode::all() {
            let voice = if Mode::voice(p) == Some(m) {
                "  voice"
            } else {
                ""
            };
            println!(
                "  {:<12} {:>9.2} {:>11.1} {:>10.1} {:>11.1} {:>10.1} dB{voice}",
                m.name(),
                m.symbol_rate(p),
                m.coded_bit_rate(p),
                m.info_bit_rate(p),
                m.chips_per_bit(p),
                m.snr_offset_db(p),
            );
        }
        println!();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn tx(
    file: &Path,
    call: &str,
    profile: &str,
    mode: &str,
    depth: usize,
    resync: f64,
    parity: f64,
    repeat: usize,
    picture: Option<PictureOpts>,
    session: Option<u8>,
    rate: u32,
    level: f32,
    lead: f64,
    tail: f64,
    output: Option<&Path>,
    play: Option<&str>,
) -> Result<()> {
    if output.is_none() && play.is_none() {
        bail!("give -o FILE.wav to write the audio and/or --play DEVICE to play it");
    }
    let profile = parse_profile(profile)?;
    let mode = Mode::from_name(mode).with_context(|| format!("unknown mode {mode:?}"))?;
    let call = normalise_call(call).with_context(|| {
        format!("{call:?} is not a callsign this modem can send (A-Z, 0-9 and /, up to 9)")
    })?;
    if !decdsss_phy::framing::DEPTHS.contains(&depth) {
        bail!("depth must be 1, 2, 4 or 8");
    }
    let mut name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    // A session number from the clock unless one is given.
    let session = session.unwrap_or_else(|| {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        (t % 64) as u8
    });
    let spec = SendSpec {
        profile,
        mode,
        depth,
        call: call.clone(),
        session: session & 63,
        ptype: if picture.is_some() {
            PayloadType::Picture
        } else {
            PayloadType::File
        },
        resync_s: resync,
        parity: parity / 100.0,
        passes: repeat.max(1),
    };
    let data = match &picture {
        Some(opts) => {
            let (picture_name, jpeg) = prepare_picture(file, &spec, opts)?;
            name = picture_name;
            jpeg
        }
        None => std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?,
    };
    let stream = container::pack(&name, &data);
    let plan = plan(&spec, &stream).map_err(anyhow::Error::msg)?;
    let segs = &plan.segments;
    let layout = Layout::new(profile, mode, depth);
    let mut chips = Vec::new();
    for s in segs {
        chips.extend(segment_chips(s).context("cannot build the header")?);
    }
    let audio = chips_to_audio(&chips, profile, f64::from(rate), level, lead, tail);
    println!(
        "{}: {} bytes → {} data + {} coded frames in the first pass ({} pass(es), {} frames of {} bytes in all, {} segment(s)), {} {} depth {depth}",
        name,
        data.len(),
        plan.data_frames,
        plan.coded_first_pass,
        plan.passes,
        plan.frames(),
        layout.frame_bytes(),
        segs.len(),
        profile.label(),
        mode,
    );
    let secs = audio.len() as f64 / f64::from(rate);
    if let Some(output) = output {
        let peak = wav::write(output, &audio, rate)?;
        println!(
            "{secs:.1} s of audio at {rate} Hz → {} (peak {peak:.2}, own code {})",
            output.display(),
            own_code(&call).unwrap_or(0),
        );
    }
    if let Some(dev) = play {
        play_audio(&audio, rate, dev)?;
    }
    Ok(())
}

/// What `tx --picture` was asked for.
struct PictureOpts {
    air: f64,
    min_quality: u8,
    save_jpeg: Option<PathBuf>,
}

/// Loads a picture and compresses it to the largest progressive JPEG that one
/// pass of `spec` carries in `opts.air` seconds. Returns the name to send it
/// under and the JPEG.
fn prepare_picture(file: &Path, spec: &SendSpec, opts: &PictureOpts) -> Result<(String, Vec<u8>)> {
    let src = decdsss_image::Source::open(file).map_err(anyhow::Error::msg)?;
    let stem = file
        .file_stem()
        .map_or_else(|| "picture".into(), |s| s.to_string_lossy().into_owned());
    let name = format!("{stem}.jpg");
    // The air time of a pass: the container's own bytes come out of the budget.
    let one_pass = SendSpec {
        passes: 1,
        ..spec.clone()
    };
    let budget =
        max_stream_bytes(&one_pass, opts.air).saturating_sub(container::pack(&name, &[]).len());
    if budget < 200 {
        bail!(
            "{:.0} s of {} {} carries only {budget} bytes: ask for more with --air or use a faster mode",
            opts.air,
            spec.profile.label(),
            spec.mode
        );
    }
    let enc = decdsss_image::encode_auto(&src.image, budget, 1 << 20, opts.min_quality)
        .map_err(anyhow::Error::msg)?;
    println!(
        "picture {}×{} → {}×{} progressive JPEG, quality {}, {} bytes (room for {budget} in {:.0} s)",
        src.image.width(),
        src.image.height(),
        enc.width,
        enc.height,
        enc.quality,
        enc.bytes.len(),
        opts.air
    );
    if let Some(path) = &opts.save_jpeg {
        std::fs::write(path, &enc.bytes)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok((name, enc.bytes))
}

fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string();
    if clean.is_empty() {
        "received.bin".into()
    } else {
        clean
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
        .unwrap()
}

/// Everything `rx` keeps while it listens, whatever the audio comes from.
struct RxRun {
    mr: MultiReceiver,
    rate: f64,
    seen: u64,
    asm: Assembler,
    out_dir: PathBuf,
    verbose: bool,
    files: usize,
    headers: usize,
    frames_ok: usize,
    frames_bad: usize,
    open: std::collections::BTreeSet<(String, u8)>,
}

impl RxRun {
    fn new(profiles: &[Profile], rate: f64, out_dir: &Path, verbose: bool) -> RxRun {
        RxRun {
            mr: MultiReceiver::new(profiles, rate),
            rate,
            seen: 0,
            asm: Assembler::new(),
            out_dir: out_dir.to_path_buf(),
            verbose,
            files: 0,
            headers: 0,
            frames_ok: 0,
            frames_bad: 0,
            open: Default::default(),
        }
    }

    fn push(&mut self, chunk: &[f32]) {
        self.seen += chunk.len() as u64;
        let t = self.seen as f64 / self.rate;
        for tagged in self.mr.push(chunk) {
            let p = tagged.profile.label();
            match tagged.event {
                Event::Acquired(d) => {
                    if self.verbose {
                        println!(
                            "[{t:7.1}s] {p}: preamble found ({:+.1} Hz, {:.0} dB over threshold)",
                            d.freq_hz, d.margin_db
                        );
                    }
                }
                Event::PreambleLocked(l) => {
                    if self.verbose {
                        println!(
                            "[{t:7.1}s] {p}: preamble locked (carrier {:+.2} Hz, clock {:+.0} ppm)",
                            l.freq_hz,
                            (4.0 / l.step - 1.0) * 1e6
                        );
                    }
                }
                Event::Lost(why) => {
                    if self.verbose {
                        println!("[{t:7.1}s] {p}: lost it: {why}");
                    }
                }
                Event::Header { header, snr_db, .. } => {
                    self.headers += 1;
                    self.open.insert((header.call.clone(), header.session));
                    println!(
                        "[{t:7.1}s] {p}: {} sending {} depth {}, session {}, frames {}.. of {} ({} blocks), symbol SNR {:.0} dB",
                        header.call,
                        header.mode,
                        header.depth,
                        header.session,
                        header.frame0,
                        header.total_frames,
                        header.nblocks,
                        snr_db
                    );
                }
                Event::Block { header, result, .. } => {
                    let ok = result.frames.iter().filter(|f| f.is_some()).count();
                    self.frames_ok += ok;
                    self.frames_bad += result.frames.len() - ok;
                    if self.verbose || ok < result.frames.len() {
                        println!(
                            "[{t:7.1}s] {p}: {} block {}: {ok}/{} frames, SNR {:.0} dB, carrier {:+.2} Hz",
                            header.call,
                            result.index,
                            result.frames.len(),
                            result.snr_db,
                            result.freq_hz
                        );
                    }
                    if let Some(f) = self.asm.push(&header, &result) {
                        let path = unique(&self.out_dir, &sanitize(&f.name));
                        match std::fs::write(&path, &f.data) {
                            Ok(()) => {
                                self.files += 1;
                                let pic = (f.ptype == PayloadType::Picture)
                                    .then(|| decdsss_image::decode(&f.data))
                                    .flatten()
                                    .map_or(String::new(), |i| {
                                        format!(", picture {}×{}", i.width(), i.height())
                                    });
                                println!(
                                    "[{t:7.1}s] FILE {} ({} bytes{pic}) from {} → {}",
                                    f.name,
                                    f.data.len(),
                                    f.call,
                                    path.display()
                                );
                            }
                            Err(e) => println!("cannot write {}: {e}", path.display()),
                        }
                    }
                }
                Event::SegmentEnd { .. } => {}
            }
        }
    }

    fn summary(&self) {
        println!(
            "{} header(s), {} frames decoded, {} lost, {} file(s) complete",
            self.headers, self.frames_ok, self.frames_bad, self.files
        );
        for (call, session) in &self.open {
            if let Some(p) = self.asm.progress(call, *session)
                && p.have < p.need
            {
                println!(
                    "{call} session {session}: {} of the {} frames needed, not complete",
                    p.have, p.need
                );
                // A picture so far: what has arrived, drawn as far as it goes.
                if p.ptype == PayloadType::Picture
                    && let Some(front) = self.asm.front(call, *session)
                    && let Some(part) = decdsss_image::decode_partial(&front.data)
                    && part.scans > 0
                {
                    let path = unique(&self.out_dir, &format!("{call}-{session}-partial.png"));
                    match decdsss_image::save_png(&part.image, &path) {
                        Ok(()) => println!(
                            "  its picture so far ({} of {} bytes, {} scans) → {}",
                            front.data.len(),
                            front.total,
                            part.scans,
                            path.display()
                        ),
                        Err(e) => println!("  cannot write the partial picture: {e}"),
                    }
                }
            }
        }
    }
}

fn rx(
    input: Option<&Path>,
    device: Option<&str>,
    duration: Option<f64>,
    profile: &str,
    out_dir: &Path,
    verbose: bool,
) -> Result<()> {
    let profiles: Vec<Profile> = if profile == "all" {
        Profile::ALL.to_vec()
    } else {
        profile
            .split(',')
            .map(parse_profile)
            .collect::<Result<_>>()?
    };
    std::fs::create_dir_all(out_dir)?;
    match (input, device) {
        (Some(path), None) => {
            let rate = f64::from(wav::sample_rate(path)?);
            let mut run = RxRun::new(&profiles, rate, out_dir, verbose);
            wav::stream(path, 8192, |chunk| run.push(chunk))?;
            run.summary();
            Ok(())
        }
        (None, Some(dev)) => {
            let mut stream = InputStream::open(&InputOptions {
                device: device_name(dev),
                channels: Some(1),
                ..InputOptions::default()
            })?;
            let fmt = stream.format();
            println!(
                "listening on {} ({fmt}){}",
                stream.device_name(),
                duration.map_or(", Ctrl-C to stop".to_string(), |d| format!(" for {d:.0} s"))
            );
            let mut run = RxRun::new(&profiles, f64::from(fmt.sample_rate), out_dir, verbose);
            let started = std::time::Instant::now();
            while duration.is_none_or(|d| started.elapsed().as_secs_f64() < d) {
                if stream.is_dead() {
                    bail!("the input device stopped");
                }
                let frames = stream.available_frames().min(8192);
                if frames == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                let mono =
                    decdsss_audio::channels::downmix_to_mono(&stream.read(frames), fmt.channels);
                run.push(&mono);
            }
            run.summary();
            Ok(())
        }
        _ => bail!("give either a WAV file or --device NAME"),
    }
}

/// "default" (or empty) means the system default device.
fn device_name(dev: &str) -> Option<String> {
    let d = dev.trim();
    (!d.is_empty() && !d.eq_ignore_ascii_case("default")).then(|| d.to_string())
}

fn devices() -> Result<()> {
    for (label, list) in [
        ("Inputs", decdsss_audio::list_input_devices()?),
        ("Outputs", decdsss_audio::list_output_devices()?),
    ] {
        println!("{label}:");
        if list.is_empty() {
            println!("  (none)");
        }
        for d in list {
            let fmt = d
                .default_format
                .map_or(String::new(), |f| format!("  [{f}]"));
            println!(
                "  {}{}{fmt}",
                if d.is_default { "* " } else { "  " },
                d.name
            );
        }
    }
    Ok(())
}

/// Plays mono audio on an output device, blocking until it has played.
fn play_audio(audio: &[f32], rate: u32, dev: &str) -> Result<()> {
    let mut out = OutputStream::open(&OutputOptions {
        device: device_name(dev),
        sample_rate: Some(rate),
        channels: Some(1),
        ..OutputOptions::default()
    })?;
    let fmt = out.format();
    if fmt.sample_rate != rate {
        bail!(
            "{} runs at {} Hz but the audio was made for {rate} Hz: pass --rate {}",
            out.device_name(),
            fmt.sample_rate,
            fmt.sample_rate
        );
    }
    println!("playing on {} ({fmt})", out.device_name());
    let ch = fmt.channels.max(1);
    for block in audio.chunks(rate as usize / 10) {
        let mut frames = Vec::with_capacity(block.len() * ch);
        for &v in block {
            frames.extend(std::iter::repeat_n(v, ch));
        }
        out.write_blocking(&frames, std::time::Duration::from_secs(5))?;
    }
    out.drain(std::time::Duration::from_secs(10));
    println!("done");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn channel(
    input: &Path,
    output: &Path,
    snr: Option<f64>,
    profile: &str,
    fading: Option<&str>,
    offset: f64,
    drift: f64,
    clock_ppm: f64,
    seed: u64,
) -> Result<()> {
    let profile = parse_profile(profile)?;
    let fading = match fading {
        Some(n) => Some(Watterson::from_name(n).with_context(|| format!("unknown fading {n:?}"))?),
        None => None,
    };
    let (audio, rate) = wav::read(input)?;
    // The SNR is referred to the signal while it is on: RMS between the first
    // and last sample above 5 % of the peak.
    let peak = audio.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let (a, b) = {
        let thr = 0.05 * peak;
        let a = audio.iter().position(|v| v.abs() > thr).unwrap_or(0);
        let b = audio.iter().rposition(|v| v.abs() > thr).unwrap_or(0);
        (a, b.max(a + 1))
    };
    let rms = (audio[a..b]
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum::<f64>()
        / (b - a) as f64)
        .sqrt();
    let ch = Channel {
        snr_db: snr,
        bandwidth_hz: profile.bandwidth_hz(),
        signal_rms: rms,
        offset_hz: offset,
        drift_hz_per_s: drift,
        fading,
        clock_ppm,
        seed,
    };
    let out = ch.apply(&audio, f64::from(rate));
    wav::write(output, &out, rate)?;
    println!(
        "{} → {} (signal RMS {:.3}, SNR {} in {} Hz)",
        input.display(),
        output.display(),
        rms,
        snr.map_or("none".into(), |s| format!("{s:.1} dB")),
        profile.bandwidth_hz()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn sweep(
    profile: &str,
    mode: &str,
    depth: usize,
    blocks: u16,
    segments: usize,
    ebn0: &[f64],
    trials: usize,
    fading: Option<&str>,
    offset: f64,
    clock_ppm: f64,
) -> Result<()> {
    use decdsss_phy::sim::{Scenario, run_many, snr_for_ebn0};
    let profile = parse_profile(profile)?;
    let mode = Mode::from_name(mode).with_context(|| format!("unknown mode {mode:?}"))?;
    if !decdsss_phy::framing::DEPTHS.contains(&depth) {
        bail!("depth must be 1, 2, 4 or 8");
    }
    let fading = match fading {
        Some(n) => Some(Watterson::from_name(n).with_context(|| format!("unknown fading {n:?}"))?),
        None => None,
    };
    let [from, to, step] = match ebn0 {
        [a, b, c] => [*a, *b, *c],
        _ => bail!("--ebn0 FROM TO STEP is required, for example --ebn0 2 6 0.5"),
    };
    if step <= 0.0 || to < from {
        bail!("--ebn0 needs FROM <= TO and a positive STEP");
    }
    let layout = Layout::new(profile, mode, depth);
    println!(
        "{} {} depth {depth}: {} frame(s) of {} bytes a transmission, {:.0} s of payload, {trials} transmissions a point, {}",
        profile.label(),
        mode,
        usize::from(blocks) * depth,
        layout.frame_bytes(),
        layout.seconds() * f64::from(blocks),
        fading.map_or("AWGN".to_string(), |_| "Watterson fading".into()),
    );
    println!("  Eb/N0     in-band SNR   headers   frame loss");
    let mut x = from;
    while x <= to + 1e-9 {
        let t = run_many(
            &Scenario {
                profile,
                mode,
                depth,
                nblocks: blocks,
                segments,
                ebn0_db: x,
                fading,
                offset_hz: offset,
                clock_ppm,
                seed: 1000,
            },
            trials,
        );
        println!(
            "  {x:5.1} dB  {:8.1} dB   {:>3}/{:<3}   {:6.2} %  ({}/{} frames)",
            snr_for_ebn0(&layout, x),
            t.headers,
            t.trials,
            t.fer() * 100.0,
            t.frames_ok,
            t.frames_total
        );
        x += step;
    }
    Ok(())
}
