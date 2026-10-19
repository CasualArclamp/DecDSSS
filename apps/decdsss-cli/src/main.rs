// made by CasualArclamp
//! `decdsss`: the command line.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use decdsss_audio::{InputOptions, InputStream, OutputOptions, OutputStream, wav};
use decdsss_chan::{Channel, Watterson};
use decdsss_codec2::Mode as Codec;
use decdsss_core::{Band, Mode, Profile, Slot};
use decdsss_link::voice::default_codec;
use decdsss_link::{
    Assembler, SendSpec, VoiceSpec, container, cq::plan_cq, max_stream_bytes, plan, plan_text,
    plan_voice, text,
};
use decdsss_phy::codes::{normalise_call, own_code};
use decdsss_phy::framing::{Layout, PayloadType};
use decdsss_phy::rx::{Event, MultiReceiver};
use decdsss_phy::tx::{Segment, chips_to_audio_at, transmission_chips};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

// `Tx` carries every option of a transmission, so it is the large variant; a
// command line is parsed once.
#[allow(clippy::large_enum_variant)]
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
    /// List the serial ports, or check that a rig answers on one. Never keys
    /// the transmitter: it only asks the rig what it is.
    Rig {
        /// The rig: ic7300, ft710 or rigctld (Hamlib's, over TCP).
        #[arg(long, requires = "port")]
        model: Option<String>,
        /// Its serial port (COM7), or for rigctld its address (localhost:4532).
        #[arg(long, requires = "model")]
        port: Option<String>,
        /// The CAT rate to try first (the rig's other rates are tried after).
        #[arg(long)]
        baud: Option<u32>,
        /// The IC-7300's CI-V address, in hexadecimal (94 as it comes).
        #[arg(long, value_parser = parse_hex_byte)]
        civ: Option<u8>,
    },
    /// Turn a file into a transmission (a WAV of audio to feed a radio).
    Tx {
        /// The file to send (not needed with --text or --cq).
        #[arg(required_unless_present_any = ["text", "cq"])]
        file: Option<PathBuf>,
        /// Send this text instead of a file: a message the receiver shows as text.
        #[arg(long, conflicts_with_all = ["file", "picture", "voice", "cq"])]
        text: Option<String>,
        /// Send a CQ packet from --call: a preamble and a header carrying the callsign
        /// and nothing else, in the most robust header the modem has (--mode, --depth,
        /// --parity and --resync do not apply). For a CQ as text, use --text.
        #[arg(long, conflicts_with_all = ["file", "picture", "voice"])]
        cq: bool,
        /// With --cq, copies of the packet sent back to back (1-8); each decodes on
        /// its own, so more copies get through a worse channel.
        #[arg(long, default_value_t = decdsss_link::cq::DEFAULT_COPIES)]
        copies: usize,
        /// Your callsign (also picks the spreading code your data rides on).
        #[arg(long)]
        call: String,
        /// Bandwidth profile: 500, 2k4 or 3k.
        #[arg(long, default_value = "2k4")]
        profile: String,
        /// With --profile 500, where in the passband: -500, 0 or +500 Hz from the
        /// middle. Up to three 500 Hz stations fit side by side in a 2.4 kHz
        /// filter; a receiver on 500 listens on all three.
        #[arg(long, default_value = "0", allow_hyphen_values = true)]
        slot: String,
        /// Mode, for example ds8-1/2, ds4-2/3, csk64-1/2 (see `decdsss modes`).
        /// Default ds8-1/2; for --voice, the voice mode of the profile.
        #[arg(long)]
        mode: Option<String>,
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
        #[arg(long, conflicts_with = "voice")]
        picture: bool,
        /// Send a WAV recording as speech: Codec2 450 on 2k4, 700C on 3k.
        #[arg(long)]
        voice: bool,
        /// With --voice, the Codec2 mode: 450 or 700C (default from the profile).
        #[arg(long)]
        codec: Option<String>,
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
        /// Ahead of the signal, a 1 kHz tone (0.4 s) and a pause (0.3 s) to key a
        /// radio that transmits on VOX.
        #[arg(long)]
        vox: bool,
        /// The WAV to write.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Play the audio on this output device (a name or part of one; "default"
        /// for the system default). The transmitter is keyed as the GUI's CAT / PTT
        /// dialog has it set (see --ptt), VOX if it was never set up.
        #[arg(long)]
        play: Option<String>,
        /// With --play, what keys the transmitter: vox (nothing: the radio is on
        /// VOX or you key it), cat, dtr or rts. Default: the GUI's setting.
        #[arg(long)]
        ptt: Option<String>,
        /// With --play, the rig for CAT (ic7300, ft710 or rigctld); turns CAT on.
        /// Default: the GUI's setting.
        #[arg(long)]
        rig: Option<String>,
        /// The rig's serial port (COM7) or, for rigctld, its address (localhost:4532).
        #[arg(long)]
        rig_port: Option<String>,
        /// The CAT rate to try first.
        #[arg(long)]
        rig_baud: Option<u32>,
        /// The serial port a DTR or RTS key is on (empty: the CAT port).
        #[arg(long)]
        ptt_port: Option<String>,
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
        /// 500 listens on all three of its slots; 500@-500, 500@0 or 500@+500 on one.
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
        Command::Rig {
            model,
            port,
            baud,
            civ,
        } => rig(model.as_deref(), port.as_deref(), baud, civ),
        Command::Tx {
            file,
            text,
            cq,
            copies,
            call,
            profile,
            slot,
            mode,
            depth,
            resync,
            parity,
            repeat,
            picture,
            voice,
            codec,
            air,
            min_quality,
            save_jpeg,
            session,
            rate,
            level,
            lead,
            tail,
            vox,
            output,
            play,
            ptt,
            rig,
            rig_port,
            rig_baud,
            ptt_port,
        } => tx(
            file.as_deref(),
            &call,
            &profile,
            &slot,
            mode.as_deref(),
            depth,
            resync,
            parity,
            repeat,
            if cq {
                Payload::Cq(copies)
            } else if let Some(t) = text {
                Payload::Text(t)
            } else if voice {
                Payload::Voice(VoiceOpts { codec })
            } else if picture {
                Payload::Picture(PictureOpts {
                    air,
                    min_quality,
                    save_jpeg,
                })
            } else {
                Payload::File
            },
            session,
            rate,
            level,
            lead,
            tail,
            vox,
            output.as_deref(),
            play.as_deref(),
            &KeyOpts {
                ptt,
                rig,
                rig_port,
                rig_baud,
                ptt_port,
            },
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

/// Adds a received message to `messages.log` in the output folder: when (seconds into
/// the recording), who, which band, what. A failure is reported, not fatal.
fn append_message(dir: &Path, t: f64, call: &str, band: &str, msg: &str) {
    use std::io::Write;
    let line = format!("[{t:7.1}s] {call} ({band}): {}\n", msg.replace('\n', " / "));
    let r = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("messages.log"))
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = r {
        println!("cannot write messages.log: {e}");
    }
}

fn parse_profile(s: &str) -> Result<Profile> {
    Profile::from_name(s).with_context(|| format!("unknown profile {s:?}: use 500, 2k4 or 3k"))
}

/// The bands to listen on: `all`, or a comma-separated list of profiles, where `500`
/// is all three of its slots and `500@-500`, `500@0`, `500@+500` is one.
fn parse_bands(s: &str) -> Result<Vec<Band>> {
    if s == "all" {
        return Ok(Band::ALL.to_vec());
    }
    let mut out = Vec::new();
    for item in s.split(',') {
        let (name, slot) = match item.split_once('@') {
            Some((n, sl)) => (
                n,
                Some(
                    Slot::from_name(sl)
                        .with_context(|| format!("unknown slot {sl:?}: use -500, 0 or +500"))?,
                ),
            ),
            None => (item, None),
        };
        let profile = parse_profile(name)?;
        for b in Band::ALL {
            if b.profile == profile && slot.is_none_or(|s| s == b.slot) && !out.contains(&b) {
                out.push(b);
            }
        }
    }
    Ok(out)
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
    file: Option<&Path>,
    call: &str,
    profile: &str,
    slot: &str,
    mode: Option<&str>,
    depth: usize,
    resync: f64,
    parity: f64,
    repeat: usize,
    payload: Payload,
    session: Option<u8>,
    rate: u32,
    level: f32,
    lead: f64,
    tail: f64,
    vox: bool,
    output: Option<&Path>,
    play: Option<&str>,
    keying: &KeyOpts,
) -> Result<()> {
    if output.is_none() && play.is_none() {
        bail!("give -o FILE.wav to write the audio and/or --play DEVICE to play it");
    }
    let profile = parse_profile(profile)?;
    let slot = Slot::from_name(slot)
        .with_context(|| format!("unknown slot {slot:?}: use -500, 0 or +500"))?;
    if profile != Profile::N500 && slot != Slot::Mid {
        bail!("slots are for the 500 Hz profile only (the wider profiles have just a middle)");
    }
    let band = Band::new(profile, slot);
    let mode = match (mode, &payload) {
        (Some(m), _) => Mode::from_name(m).with_context(|| format!("unknown mode {m:?}"))?,
        (None, Payload::Voice(_)) => {
            Mode::voice(profile).context("500 Hz is too slow for voice: use --profile 2k4 or 3k")?
        }
        (None, _) => Mode::from_name("ds8-1/2").expect("the default mode exists"),
    };
    let call = normalise_call(call).with_context(|| {
        format!("{call:?} is not a callsign this modem can send (A-Z, 0-9 and /, up to 9)")
    })?;
    if !decdsss_phy::framing::DEPTHS.contains(&depth) {
        bail!("depth must be 1, 2, 4 or 8");
    }
    let mut name = file
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    // A session number from the clock unless one is given.
    let session = session.unwrap_or_else(|| {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        (t % 64) as u8
    });
    let layout = Layout::new(profile, mode, depth);
    let (segs, summary): (Vec<Segment>, String) = if let Payload::Cq(copies) = &payload {
        // The header is the message; the mode, depth and parity do not apply.
        let segs = plan_cq(profile, &call, session & 63, *copies).map_err(anyhow::Error::msg)?;
        let summary = format!(
            "CQ packet from {call}: {} header-only cop{} in the long header, {} (about {:.0} s of signal)",
            segs.len(),
            if segs.len() == 1 { "y" } else { "ies" },
            band.label(),
            decdsss_link::cq::air_seconds(profile, segs.len()),
        );
        (segs, summary)
    } else if let Payload::Text(msg) = &payload {
        // A message: always depth 1 (see `decdsss_link::text`).
        let spec = SendSpec {
            profile,
            mode,
            depth,
            call: call.clone(),
            session: session & 63,
            ptype: PayloadType::Text,
            resync_s: resync,
            parity: parity / 100.0,
            passes: repeat.max(1),
        };
        let plan = plan_text(&spec, msg).map_err(anyhow::Error::msg)?;
        let summary = format!(
            "text {:?}: {} data + {} coded frames in the first pass ({} pass(es), {} frames of {} bytes in all, {} segment(s)), {} {} depth 1",
            text::tidy(msg),
            plan.data_frames,
            plan.coded_first_pass,
            plan.passes,
            plan.frames(),
            Layout::new(profile, mode, 1).frame_bytes(),
            plan.segments.len(),
            band.label(),
            mode,
        );
        (plan.segments, summary)
    } else if let Payload::Voice(v) = &payload {
        let codec = match &v.codec {
            Some(c) => Codec::from_name(c)
                .with_context(|| format!("unknown Codec2 mode {c:?}: use 450 or 700C"))?,
            None => default_codec(profile)
                .context("500 Hz is too slow for voice: use --profile 2k4 or 3k")?,
        };
        let speech = load_speech(file.context("give the WAV to send")?)?;
        let vs = VoiceSpec {
            profile,
            mode,
            depth,
            call: call.clone(),
            session: session & 63,
            codec,
            resync_s: resync,
        };
        let p = plan_voice(&vs, &speech).map_err(anyhow::Error::msg)?;
        let summary = format!(
            "{name}: {:.1} s of speech as Codec2 {codec} → {} frames of {} bytes ({} codec frames each), {} segment(s), {:.0} s of signal; the payload runs at {:.2}× the codec's rate, {} {} depth {depth}",
            p.speech_seconds,
            p.frames,
            layout.frame_bytes(),
            p.per_frame,
            p.segments.len(),
            p.air_seconds,
            p.throughput_ratio,
            profile.label(),
            mode,
        );
        (p.segments, summary)
    } else {
        let spec = SendSpec {
            profile,
            mode,
            depth,
            call: call.clone(),
            session: session & 63,
            ptype: if matches!(payload, Payload::Picture(_)) {
                PayloadType::Picture
            } else {
                PayloadType::File
            },
            resync_s: resync,
            parity: parity / 100.0,
            passes: repeat.max(1),
        };
        let data = match &payload {
            Payload::Picture(opts) => {
                let (picture_name, jpeg) =
                    prepare_picture(file.context("give the picture to send")?, &spec, opts)?;
                name = picture_name;
                jpeg
            }
            _ => {
                let file = file.context("give the file to send")?;
                std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?
            }
        };
        let stream = container::pack(&name, &data);
        let plan = plan(&spec, &stream).map_err(anyhow::Error::msg)?;
        let summary = format!(
            "{}: {} bytes → {} data + {} coded frames in the first pass ({} pass(es), {} frames of {} bytes in all, {} segment(s)), {} {} depth {depth}",
            name,
            data.len(),
            plan.data_frames,
            plan.coded_first_pass,
            plan.passes,
            plan.frames(),
            layout.frame_bytes(),
            plan.segments.len(),
            profile.label(),
            mode,
        );
        (plan.segments, summary)
    };
    let chips = transmission_chips(&segs).context("cannot build the header")?;
    let mut audio = chips_to_audio_at(&chips, band, f64::from(rate), level, lead, tail);
    if vox {
        let mut v = decdsss_phy::tx::vox_lead(f64::from(rate), level, 0.4, 0.3);
        v.extend(audio);
        audio = v;
    }
    println!("{summary}");
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
        play_audio(&audio, rate, dev, &key_config(keying)?)?;
    }
    Ok(())
}

/// The command line's say in what keys the transmitter for `--play`.
struct KeyOpts {
    ptt: Option<String>,
    rig: Option<String>,
    rig_port: Option<String>,
    rig_baud: Option<u32>,
    ptt_port: Option<String>,
}

/// The CAT and PTT settings the GUI's dialog keeps in its preferences file
/// (`%APPDATA%\DecDSSS\prefs.txt`, the same `key=value` lines the GUI reads);
/// the defaults (no keying) if there is none.
fn gui_rig_config() -> decdsss_rig::Config {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    });
    let text = base
        .and_then(|b| std::fs::read_to_string(b.join("DecDSSS").join("prefs.txt")).ok())
        .unwrap_or_default();
    let map: std::collections::BTreeMap<&str, &str> = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    decdsss_rig::Config::from_lookup(|k| map.get(k).copied())
}

/// The GUI's settings with the command line's changes on top, checked.
fn key_config(o: &KeyOpts) -> Result<decdsss_rig::Config> {
    apply_key_opts(gui_rig_config(), o)
}

fn apply_key_opts(mut c: decdsss_rig::Config, o: &KeyOpts) -> Result<decdsss_rig::Config> {
    if let Some(m) = &o.rig {
        c.rig.model = decdsss_rig::Model::parse(m)
            .with_context(|| format!("no rig {m:?}: ic7300, ft710 or rigctld"))?;
        c.rig.baud = c.rig.model.default_baud();
        c.cat = true;
    }
    if let Some(p) = &o.rig_port {
        c.rig.port = p.clone();
        c.cat = true;
    }
    if let Some(b) = o.rig_baud {
        c.rig.baud = b;
    }
    if let Some(p) = &o.ptt_port {
        c.ptt_port = p.clone();
    }
    if let Some(m) = &o.ptt {
        c.method = decdsss_rig::Method::parse(m)
            .with_context(|| format!("no PTT method {m:?}: vox, cat, dtr or rts"))?;
    }
    c.check().map_err(anyhow::Error::msg)?;
    Ok(c)
}

/// What `tx` sends.
enum Payload {
    /// A message.
    Text(String),
    /// A CQ packet of this many copies (see `decdsss_link::cq`).
    Cq(usize),
    File,
    Picture(PictureOpts),
    Voice(VoiceOpts),
}

/// What `tx --voice` was asked for.
struct VoiceOpts {
    codec: Option<String>,
}

/// Loads a recording as 8 kHz mono speech, its peak brought to 80 % of full scale
/// (at most 30 dB of gain, so a silent file stays quiet).
fn load_speech(file: &Path) -> Result<Vec<i16>> {
    let (audio, rate) =
        wav::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    decdsss_link::voice::speech_from_audio(&audio, f64::from(rate))
        .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))
}

/// Writes 8 kHz speech as a WAV in `dir` under a name not already taken.
fn write_speech(dir: &Path, name: &str, speech: &[i16]) -> Result<PathBuf> {
    let path = unique(dir, name);
    let audio: Vec<f32> = speech.iter().map(|&v| f32::from(v) / 32768.0).collect();
    wav::write(&path, &audio, 8000)?;
    Ok(path)
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
    /// Text messages received.
    messages: usize,
    headers: usize,
    frames_ok: usize,
    frames_bad: usize,
    open: std::collections::BTreeSet<(String, u8)>,
    /// CQ packets already logged, by station and session (a CQ has several copies).
    cq_heard: std::collections::BTreeSet<(String, u8)>,
}

impl RxRun {
    fn new(bands: &[Band], rate: f64, out_dir: &Path, verbose: bool) -> RxRun {
        RxRun {
            mr: MultiReceiver::with_bands(bands, rate),
            rate,
            seen: 0,
            asm: Assembler::new(),
            out_dir: out_dir.to_path_buf(),
            verbose,
            files: 0,
            messages: 0,
            headers: 0,
            frames_ok: 0,
            frames_bad: 0,
            open: Default::default(),
            cq_heard: Default::default(),
        }
    }

    fn push(&mut self, chunk: &[f32]) {
        self.seen += chunk.len() as u64;
        let t = self.seen as f64 / self.rate;
        for tagged in self.mr.push(chunk) {
            let p = tagged.band.label();
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
                Event::Header { header, snr_db, .. } if decdsss_link::cq::is_cq(&header) => {
                    // A CQ packet is its header: nothing follows to open or wait for.
                    self.headers += 1;
                    println!(
                        "[{t:7.1}s] {p}: {}",
                        decdsss_link::cq::describe(&header, snr_db)
                    );
                    // Copies of one CQ are logged once.
                    if self.cq_heard.insert((header.call.clone(), header.session)) {
                        self.messages += 1;
                        append_message(&self.out_dir, t, &header.call, &p, "CQ (CQ packet)");
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
                    let file = self.asm.push(&header, &result);
                    for v in self.asm.take_voice() {
                        match write_speech(
                            &self.out_dir,
                            &format!("{}-{}.wav", v.call, v.session),
                            &v.speech,
                        ) {
                            Ok(path) => {
                                self.files += 1;
                                println!(
                                    "[{t:7.1}s] VOICE from {} ({:.1} s of Codec2 {} speech) → {}",
                                    v.call,
                                    v.speech.len() as f64 / 8000.0,
                                    v.codec,
                                    path.display()
                                );
                            }
                            Err(e) => println!("cannot write the speech: {e}"),
                        }
                    }
                    if let Some(f) = file.as_ref().filter(|f| f.ptype == PayloadType::Text) {
                        let msg = text::read(f).unwrap_or_default();
                        println!("[{t:7.1}s] TEXT from {} ({p}): {msg}", f.call);
                        self.messages += 1;
                        append_message(&self.out_dir, t, &f.call, &p, &msg);
                    } else if let Some(f) = file {
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
                Event::EndOfTransmission { header, ratio, .. } => {
                    println!(
                        "[{t:7.1}s] {p}: {} has finished (postamble {ratio:.0}x its noise floor)",
                        header.call
                    );
                }
            }
        }
    }

    fn summary(&self) {
        println!(
            "{} header(s), {} frames decoded, {} lost, {} file(s) complete, {} message(s)",
            self.headers, self.frames_ok, self.frames_bad, self.files, self.messages
        );
        for (call, session) in &self.open {
            if let Some(vp) = self.asm.voice_progress(call, *session)
                && vp.have < vp.total
                && let Some((speech, _)) = self.asm.voice_speech(call, *session)
            {
                println!(
                    "{call} session {session}: {} of the {} speech frames, not complete",
                    vp.have, vp.total
                );
                match write_speech(
                    &self.out_dir,
                    &format!("{call}-{session}-partial.wav"),
                    &speech,
                ) {
                    Ok(path) => println!(
                        "  the speech so far ({:.1} s, silence where frames are missing) → {}",
                        speech.len() as f64 / 8000.0,
                        path.display()
                    ),
                    Err(e) => println!("  cannot write the partial speech: {e}"),
                }
            }
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
    let bands = parse_bands(profile)?;
    std::fs::create_dir_all(out_dir)?;
    match (input, device) {
        (Some(path), None) => {
            let rate = f64::from(wav::sample_rate(path)?);
            let mut run = RxRun::new(&bands, rate, out_dir, verbose);
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
            let mut run = RxRun::new(&bands, f64::from(fmt.sample_rate), out_dir, verbose);
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

fn parse_hex_byte(s: &str) -> Result<u8, String> {
    u8::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("{s}: {e}"))
}

/// `decdsss rig`: the serial ports; with a model and a port, whether the rig is
/// there. Nothing is keyed.
fn rig(model: Option<&str>, port: Option<&str>, baud: Option<u32>, civ: Option<u8>) -> Result<()> {
    let (Some(model), Some(port)) = (model, port) else {
        println!("Serial ports:");
        let ports = decdsss_rig::ports();
        if ports.is_empty() {
            println!("  (none)");
        }
        for p in ports {
            println!("  {}  {}", p.name, p.description);
        }
        println!("Rigs: ic7300, ft710, rigctld (any Hamlib rig; start rigctld first)");
        return Ok(());
    };
    let Some(parsed) = decdsss_rig::Model::parse(model) else {
        bail!("no rig {model:?}: ic7300, ft710 or rigctld");
    };
    let mut settings = decdsss_rig::Settings::new(parsed, port);
    if let Some(b) = baud {
        settings.baud = b;
    }
    if let Some(c) = civ {
        settings.civ = c;
    }
    let found = decdsss_rig::connect(&settings).map_err(anyhow::Error::msg)?;
    let rate = if found.baud > 0 {
        format!(" at {} baud", found.baud)
    } else {
        String::new()
    };
    println!("{} on {port}{rate}: {}", parsed.name(), found.answer);
    Ok(())
}

/// Plays mono audio on an output device, blocking until it has played.
fn play_audio(audio: &[f32], rate: u32, dev: &str, keying: &decdsss_rig::Config) -> Result<()> {
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
    // Ctrl-C ends the loop below like any error, so the key is let go (a killed
    // process would leave the transmitter on).
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&interrupted);
        // A second handler in one process would fail to be set; that is no loss.
        let _ = ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed));
    }
    // Key the transmitter with the stream open, so that only the rig's answer
    // and the lead time stand between the key and the first sample. Declared
    // after `out`: any way out of here lets go of the key first.
    let hold = if keying.keys() {
        let ptt = Arc::new(decdsss_rig::Ptt::open(keying.clone()));
        let hold = ptt.hold().map_err(anyhow::Error::msg)?;
        println!("transmitter keyed ({})", keying.summary());
        Some(hold)
    } else {
        None
    };
    println!("playing on {} ({fmt})", out.device_name());
    let ch = fmt.channels.max(1);
    for block in audio.chunks(rate as usize / 10) {
        if interrupted.load(Ordering::Relaxed) {
            bail!("interrupted: the transmitter is let go");
        }
        let mut frames = Vec::with_capacity(block.len() * ch);
        for &v in block {
            frames.extend(std::iter::repeat_n(v, ch));
        }
        out.write_blocking(&frames, std::time::Duration::from_secs(5))?;
    }
    out.drain(std::time::Duration::from_secs(10));
    if let Some(h) = hold {
        h.finish();
        println!("transmitter let go");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> KeyOpts {
        KeyOpts {
            ptt: None,
            rig: None,
            rig_port: None,
            rig_baud: None,
            ptt_port: None,
        }
    }

    #[test]
    fn with_nothing_set_the_radio_is_not_keyed() {
        let c = apply_key_opts(decdsss_rig::Config::default(), &opts()).unwrap();
        assert!(!c.keys());
    }

    #[test]
    fn the_command_line_changes_what_the_dialog_had() {
        let c = apply_key_opts(
            decdsss_rig::Config::default(),
            &KeyOpts {
                ptt: Some("cat".into()),
                rig: Some("IC-7300".into()),
                rig_port: Some("COM9".into()),
                rig_baud: Some(19_200),
                ..opts()
            },
        )
        .unwrap();
        assert!(c.cat && c.keys());
        assert_eq!(
            (c.rig.model, c.rig.port.as_str(), c.rig.baud),
            (decdsss_rig::Model::Ic7300, "COM9", 19_200)
        );
        // A line with no port to be on is refused before anything is opened.
        for ptt in ["rts", "nonsense"] {
            let e = apply_key_opts(
                decdsss_rig::Config::default(),
                &KeyOpts {
                    ptt: Some(ptt.into()),
                    ..opts()
                },
            );
            assert!(e.is_err(), "{ptt}");
        }
    }
}
