// made by CasualArclamp
//! `decdsss`: the command line.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use decdsss_audio::wav;
use decdsss_chan::{Channel, Watterson};
use decdsss_core::{Mode, Profile};
use decdsss_link::{Assembler, SendSpec, container, segments};
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
        /// Send the whole thing this many times (a receiver fills its gaps).
        #[arg(long, default_value_t = 1)]
        repeat: usize,
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
        output: PathBuf,
    },
    /// Receive: decode a WAV recording and save the files in it.
    Rx {
        input: PathBuf,
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
        Command::Tx {
            file,
            call,
            profile,
            mode,
            depth,
            resync,
            repeat,
            session,
            rate,
            level,
            lead,
            tail,
            output,
        } => tx(
            &file, &call, &profile, &mode, depth, resync, repeat, session, rate, level, lead, tail,
            &output,
        ),
        Command::Rx {
            input,
            profile,
            out_dir,
            verbose,
        } => rx(&input, &profile, &out_dir, verbose),
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
    repeat: usize,
    session: Option<u8>,
    rate: u32,
    level: f32,
    lead: f64,
    tail: f64,
    output: &Path,
) -> Result<()> {
    let profile = parse_profile(profile)?;
    let mode = Mode::from_name(mode).with_context(|| format!("unknown mode {mode:?}"))?;
    let call = normalise_call(call).with_context(|| {
        format!("{call:?} is not a callsign this modem can send (A-Z, 0-9 and /, up to 9)")
    })?;
    if !decdsss_phy::framing::DEPTHS.contains(&depth) {
        bail!("depth must be 1, 2, 4 or 8");
    }
    let data = std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let stream = container::pack(&name, &data);
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
        ptype: PayloadType::File,
        resync_s: resync,
    };
    let segs = segments(&spec, &stream).map_err(anyhow::Error::msg)?;
    let layout = Layout::new(profile, mode, depth);
    let mut chips = Vec::new();
    for _ in 0..repeat.max(1) {
        for s in &segs {
            chips.extend(segment_chips(s).context("cannot build the header")?);
        }
    }
    let audio = chips_to_audio(&chips, profile, f64::from(rate), level, lead, tail);
    let peak = wav::write(output, &audio, rate)?;
    println!(
        "{}: {} bytes → {} frames of {} bytes in {} segment(s), {} {} depth {depth}",
        name,
        data.len(),
        segs.iter().map(|s| s.frames.len()).sum::<usize>(),
        layout.frame_bytes(),
        segs.len(),
        profile.label(),
        mode,
    );
    println!(
        "{:.1} s of audio at {rate} Hz → {} (peak {:.2}, own code {})",
        audio.len() as f64 / f64::from(rate),
        output.display(),
        peak,
        own_code(&call).unwrap_or(0),
    );
    Ok(())
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

fn rx(input: &Path, profile: &str, out_dir: &Path, verbose: bool) -> Result<()> {
    let profiles: Vec<Profile> = if profile == "all" {
        Profile::ALL.to_vec()
    } else {
        profile
            .split(',')
            .map(parse_profile)
            .collect::<Result<_>>()?
    };
    std::fs::create_dir_all(out_dir)?;
    let rate = f64::from(wav::sample_rate(input)?);
    let mut mr = MultiReceiver::new(&profiles, rate);
    let mut asm = Assembler::new();
    let mut seen = 0u64;
    let mut files = 0;
    let mut headers = 0;
    let mut frames_ok = 0usize;
    let mut frames_bad = 0usize;
    let mut open: std::collections::BTreeMap<(String, u8), ()> = Default::default();
    wav::stream(input, 8192, |chunk| {
        let r = &mut mr;
        seen += chunk.len() as u64;
        let t = seen as f64 / rate;
        for tagged in r.push(chunk) {
            let p = tagged.profile.label();
            match tagged.event {
                Event::Acquired(d) => {
                    if verbose {
                        println!(
                            "[{t:7.1}s] {p}: preamble found ({:+.1} Hz, {:.0} dB over threshold)",
                            d.freq_hz, d.margin_db
                        );
                    }
                }
                Event::PreambleLocked(l) => {
                    if verbose {
                        println!(
                            "[{t:7.1}s] {p}: preamble locked (carrier {:+.2} Hz, clock {:+.0} ppm)",
                            l.freq_hz,
                            (4.0 / l.step - 1.0) * 1e6
                        );
                    }
                }
                Event::Lost(why) => {
                    if verbose {
                        println!("[{t:7.1}s] {p}: lost it: {why}");
                    }
                }
                Event::Header { header, snr_db, .. } => {
                    headers += 1;
                    open.insert((header.call.clone(), header.session), ());
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
                    frames_ok += ok;
                    frames_bad += result.frames.len() - ok;
                    if verbose || ok < result.frames.len() {
                        println!(
                            "[{t:7.1}s] {p}: {} block {}: {ok}/{} frames, SNR {:.0} dB, carrier {:+.2} Hz",
                            header.call,
                            result.index,
                            result.frames.len(),
                            result.snr_db,
                            result.freq_hz
                        );
                    }
                    if let Some(f) = asm.push(&header, &result) {
                        let path = unique(out_dir, &sanitize(&f.name));
                        match std::fs::write(&path, &f.data) {
                            Ok(()) => {
                                files += 1;
                                println!(
                                    "[{t:7.1}s] FILE {} ({} bytes) from {} → {}",
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
    })?;
    println!(
        "{} header(s), {frames_ok} frames decoded, {frames_bad} lost, {files} file(s) complete",
        headers
    );
    for (call, session) in open.keys() {
        if let Some(p) = asm.progress(call, *session)
            && p.total > 0
            && p.have < p.total
        {
            println!(
                "{call} session {session}: {}/{} frames, not complete",
                p.have, p.total
            );
        }
    }
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
