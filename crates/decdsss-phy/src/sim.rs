// made by CasualArclamp
//! End-to-end simulation: a segment of test frames synthesised, put through
//! the channel simulator and fed to the receiver, counting what comes out.
//! The thresholds in the documentation are measured with this, so they
//! include acquisition, the preamble, the header and the payload together.

use crate::framing::{Header, Layout, PayloadType};
use crate::rx::{Event, Receiver};
use crate::tx::{Segment, chips_to_audio, segment_chips};
use decdsss_chan::{Channel, Slips, Watterson};
use decdsss_core::{Mode, Profile};

/// Sample rate of every simulation.
pub const FS: f64 = 48_000.0;
/// RMS level of the transmitted audio.
pub const LEVEL: f32 = 0.2;

/// What to simulate.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub profile: Profile,
    pub mode: Mode,
    pub depth: usize,
    pub nblocks: u16,
    /// Segments sent back to back (a long file); each has `nblocks` blocks.
    pub segments: usize,
    /// Eb/N0 per information bit of a codeword (its CRC counted, the pilots
    /// and padding of the block counted as air time), dB.
    pub ebn0_db: f64,
    pub fading: Option<Watterson>,
    pub offset_hz: f64,
    pub clock_ppm: f64,
    /// The listener's audio path skipping blocks ([`decdsss_chan::slip`]).
    pub slips: Option<Slips>,
    pub seed: u64,
}

/// What came back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// A header was decoded.
    pub header: bool,
    pub frames_total: usize,
    pub frames_ok: usize,
}

/// In-band SNR (in the profile's width) for an Eb/N0 on this layout:
/// SNR = Eb/N0 · depth·k / (B · T_block).
pub fn snr_for_ebn0(layout: &Layout, ebn0_db: f64) -> f64 {
    let k = layout.code().k() as f64;
    let b = layout.profile.bandwidth_hz();
    ebn0_db + 10.0 * (layout.depth as f64 * k / (b * layout.seconds())).log10()
}

/// Test frames: a counting pattern.
pub fn frames(layout: &Layout, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            (0..layout.frame_bytes())
                .map(|b| (i * 31 + b * 7 + 3) as u8)
                .collect()
        })
        .collect()
}

/// Runs one scenario through the whole receiver.
pub fn run(sc: &Scenario) -> Outcome {
    let layout = Layout::new(sc.profile, sc.mode, sc.depth);
    let per = usize::from(sc.nblocks) * sc.depth;
    let segments = sc.segments.max(1);
    let n = per * segments;
    let all = frames(&layout, n);
    let mut chips = Vec::new();
    for i in 0..segments {
        let seg = Segment {
            header: Header {
                profile: sc.profile,
                mode: sc.mode,
                depth: sc.depth,
                ptype: PayloadType::Test,
                call: "VK2ABC".into(),
                session: 1,
                frame0: (i * per) as u16,
                nblocks: sc.nblocks,
                total_frames: n as u16,
            },
            frames: all[i * per..(i + 1) * per].to_vec(),
        };
        chips.extend(segment_chips(&seg).expect("header packs"));
    }
    let audio = chips_to_audio(&chips, sc.profile, FS, LEVEL, 1.0, 2.0);
    let ch = Channel {
        snr_db: Some(snr_for_ebn0(&layout, sc.ebn0_db)),
        offset_hz: sc.offset_hz,
        clock_ppm: sc.clock_ppm,
        fading: sc.fading,
        seed: sc.seed,
        ..Channel::clean(f64::from(LEVEL), sc.profile.bandwidth_hz())
    };
    let audio = ch.apply(&audio, FS);
    let audio = match sc.slips {
        Some(s) => decdsss_chan::slip(&audio, FS, s, sc.seed).0,
        None => audio,
    };
    let mut rx = Receiver::new(sc.profile, FS);
    let mut out = Outcome {
        frames_total: n,
        ..Outcome::default()
    };
    let mut seen = vec![false; n];
    for chunk in audio.chunks(4800) {
        for e in rx.push(chunk) {
            match e {
                Event::Header { .. } => out.header = true,
                Event::Block { result, .. } => {
                    for (i, f) in result.frames.iter().enumerate() {
                        let idx = result.first_frame + i;
                        if idx < n && !seen[idx] && f.as_ref() == Some(&all[idx]) {
                            seen[idx] = true;
                            out.frames_ok += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The sum of several transmissions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub trials: usize,
    /// Transmissions whose header was decoded.
    pub headers: usize,
    pub frames_ok: usize,
    pub frames_total: usize,
}

impl Totals {
    /// Fraction of frames lost (a transmission whose header was never found
    /// loses all its frames).
    pub fn fer(&self) -> f64 {
        1.0 - self.frames_ok as f64 / self.frames_total.max(1) as f64
    }
}

/// Runs `trials` independent transmissions on all cores and adds them up. The
/// carrier offset and clock error are spread over the trials around the base
/// values, so no result depends on one lucky alignment.
pub fn run_many(base: &Scenario, trials: usize) -> Totals {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(trials)
        .max(1);
    let total = std::sync::Mutex::new(Totals {
        trials,
        ..Totals::default()
    });
    std::thread::scope(|sc| {
        for w in 0..workers {
            let total = &total;
            sc.spawn(move || {
                for t in (w..trials).step_by(workers) {
                    let mut s = base.clone();
                    s.seed = base.seed + t as u64;
                    s.offset_hz = base.offset_hz + (t % 7) as f64 * 2.0 - 6.0;
                    s.clock_ppm = base.clock_ppm + (t % 5) as f64 * 15.0 - 30.0;
                    let o = run(&s);
                    let mut g = total.lock().unwrap();
                    g.headers += usize::from(o.header);
                    g.frames_ok += o.frames_ok;
                    g.frames_total += o.frames_total;
                }
            });
        }
    });
    total.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hunts a seed that loses frames at high SNR and prints its blocks.
    #[test]
    #[ignore]
    fn debug_loss() {
        let base = Scenario {
            profile: Profile::W2400,
            mode: Mode::from_name("csk64-1/2").unwrap(),
            depth: 4,
            nblocks: 2,
            segments: 1,
            ebn0_db: 22.0,
            fading: Some(Watterson::GOOD),
            offset_hz: 0.0,
            clock_ppm: 0.0,
            slips: None,
            seed: 1000,
        };
        for (e, t) in [21.0f64, 24.0]
            .iter()
            .flat_map(|&e| (0..24usize).map(move |t| (e, t)))
        {
            let mut base = base.clone();
            base.ebn0_db = e;
            let mut s = base.clone();
            s.seed = base.seed + t as u64;
            s.offset_hz = base.offset_hz + (t % 7) as f64 * 2.0 - 6.0;
            s.clock_ppm = base.clock_ppm + (t % 5) as f64 * 15.0 - 30.0;
            let o = run(&s);
            if o.frames_ok < o.frames_total {
                println!(
                    "ebn0 {e} trial {t}: seed {} offset {} clock {}: {}/{}",
                    s.seed, s.offset_hz, s.clock_ppm, o.frames_ok, o.frames_total
                );
                run_verbose(&s);
                break;
            }
        }
    }

    /// DS4 on the moderate channel loses a block now and then at any SNR: hunts
    /// the seeds.
    #[test]
    #[ignore]
    fn debug_ds4_floor() {
        let mode = std::env::var("DEBUG_MODE").unwrap_or_else(|_| "ds4-2/3".into());
        // `DEBUG_DEPTH`, `DEBUG_NBLOCKS` and `DEBUG_EBN0` change what is hunted.
        let env = |k: &str, d: f64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let base = Scenario {
            profile: Profile::W2400,
            mode: Mode::from_name(&mode).unwrap(),
            depth: env("DEBUG_DEPTH", 2.0) as usize,
            nblocks: env("DEBUG_NBLOCKS", 4.0) as u16,
            segments: 1,
            ebn0_db: env("DEBUG_EBN0", 30.0),
            fading: Some(Watterson::MODERATE),
            offset_hz: 5.0,
            clock_ppm: 10.0,
            slips: None,
            seed: 1000,
        };
        let mut found = 0;
        for t in 0..200usize {
            let mut s = base.clone();
            s.seed = base.seed + t as u64;
            s.offset_hz = base.offset_hz + (t % 7) as f64 * 2.0 - 6.0;
            s.clock_ppm = base.clock_ppm + (t % 5) as f64 * 15.0 - 30.0;
            let o = run(&s);
            if o.frames_ok < o.frames_total {
                println!(
                    "trial {t}: seed {} offset {} clock {}: {}/{}",
                    s.seed, s.offset_hz, s.clock_ppm, o.frames_ok, o.frames_total
                );
                run_verbose(&s);
                found += 1;
                if found == 3 {
                    break;
                }
            }
        }
    }

    fn run_verbose(sc: &Scenario) {
        let layout = Layout::new(sc.profile, sc.mode, sc.depth);
        let n = usize::from(sc.nblocks) * sc.depth;
        let seg = Segment {
            header: Header {
                profile: sc.profile,
                mode: sc.mode,
                depth: sc.depth,
                ptype: PayloadType::Test,
                call: "VK2ABC".into(),
                session: 1,
                frame0: 0,
                nblocks: sc.nblocks,
                total_frames: n as u16,
            },
            frames: frames(&layout, n),
        };
        let chips = segment_chips(&seg).unwrap();
        let audio = chips_to_audio(&chips, sc.profile, FS, LEVEL, 1.0, 2.0);
        let ch = Channel {
            snr_db: Some(snr_for_ebn0(&layout, sc.ebn0_db)),
            offset_hz: sc.offset_hz,
            clock_ppm: sc.clock_ppm,
            fading: sc.fading,
            seed: sc.seed,
            ..Channel::clean(f64::from(LEVEL), sc.profile.bandwidth_hz())
        };
        let audio = ch.apply(&audio, FS);
        // Where the preamble, header and payload are (before any slip).
        let cr = f64::from(sc.profile.chip_rate());
        let pre = (crate::codes::PRE_A_PERIODS + crate::codes::PRE_B_PERIODS) as f64
            * crate::codes::preamble_len(sc.profile) as f64
            / cr;
        let hdr = crate::tx::header_len(crate::tx::header_alphabet_for(sc.mode)) as f64 / cr;
        println!(
            "  preamble 1.00..{:.2} s, header ..{:.2} s",
            1.0 + pre,
            1.0 + pre + hdr
        );
        let audio = match sc.slips {
            Some(s) => {
                let (a, made) = decdsss_chan::slip(&audio, FS, s, sc.seed);
                for m in made {
                    println!("  slip at {:.2} s: {:+}", m.at as f64 / FS, m.samples);
                }
                a
            }
            None => audio,
        };
        let mut rx = Receiver::new(sc.profile, FS);
        let mut pushed = 0usize;
        for chunk in audio.chunks(4800) {
            pushed += chunk.len();
            let now = pushed as f64 / FS;
            for e in rx.push(chunk) {
                match e {
                    Event::Acquired(d) => println!("  {now:.1} s acquired {:.1} Hz", d.freq_hz),
                    Event::PreambleLocked(l) => println!(
                        "  {now:.1} s preamble locked, header at {:.2} s",
                        l.header_start / (cr * crate::rx::frontend::SPS as f64)
                    ),
                    Event::Header {
                        header,
                        snr_db,
                        fingers,
                        ..
                    } => println!(
                        "  {now:.1} s header {} snr {snr_db:.1} fingers {fingers}",
                        header.mode
                    ),
                    Event::Block { result, .. } => println!(
                        "  block {}: ok {:?} iters {:?} snr {:.1} freq {:.2} fingers {:?}",
                        result.index,
                        result
                            .frames
                            .iter()
                            .map(|f| f.is_some())
                            .collect::<Vec<_>>(),
                        result.iterations,
                        result.snr_db,
                        result.freq_hz,
                        result.fingers
                    ),
                    Event::Lost(w) => println!("  {now:.1} s lost: {w}"),
                    _ => {}
                }
            }
        }
    }

    /// Transmissions through audio that slips (a browser into a virtual cable):
    /// prints what the receiver did with each one that lost frames.
    /// `DEBUG_MODE`, `DEBUG_PROFILE` (2k4, 3k, 500), `SLIP_EVERY` (s),
    /// `DEBUG_EBN0`, `DEBUG_DEPTH`, `DEBUG_NBLOCKS`, `DEBUG_TRIALS` change it. `cargo test --release -p
    /// decdsss-phy debug_slips -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn debug_slips() {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
        let profile = match var("DEBUG_PROFILE", "3k").as_str() {
            "2k4" => Profile::W2400,
            "500" => Profile::N500,
            _ => Profile::W3000,
        };
        let base = Scenario {
            profile,
            mode: Mode::from_name(&var("DEBUG_MODE", "csk8-3/4")).unwrap(),
            depth: var("DEBUG_DEPTH", "1").parse().unwrap(),
            nblocks: var("DEBUG_NBLOCKS", "6").parse().unwrap(),
            segments: 1,
            ebn0_db: var("DEBUG_EBN0", "14").parse().unwrap(),
            fading: None,
            offset_hz: 3.0,
            clock_ppm: 0.0,
            slips: Some(decdsss_chan::Slips {
                every_s: var("SLIP_EVERY", "3").parse().unwrap(),
                block: 128,
                max_blocks: 3,
            }),
            seed: 1000,
        };
        let trials: usize = var("DEBUG_TRIALS", "8").parse().unwrap();
        for t in 0..trials {
            let mut s = base.clone();
            s.seed = base.seed + t as u64;
            let o = run(&s);
            println!("trial {t}: {}/{} frames", o.frames_ok, o.frames_total);
            if o.frames_ok < o.frames_total {
                run_verbose(&s);
            }
        }
    }

    /// The transmissions of a `decdsss sweep` point whose header was not decoded,
    /// with what the receiver did in each (the first `DEBUG_SHOW`, 4). The
    /// sweep's scenario, `--fading moderate --offset 1 --clock-ppm 10`, on
    /// `DEBUG_PROFILE` (500), `DEBUG_MODE` (dsc64-1/2), `DEBUG_DEPTH` (2),
    /// `DEBUG_NBLOCKS` (2), `DEBUG_EBN0` (15), `DEBUG_TRIALS` (32). `cargo test
    /// --release -p decdsss-phy debug_header_loss -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn debug_header_loss() {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
        let profile = match var("DEBUG_PROFILE", "500").as_str() {
            "2k4" => Profile::W2400,
            "3k" => Profile::W3000,
            _ => Profile::N500,
        };
        let base = Scenario {
            profile,
            mode: Mode::from_name(&var("DEBUG_MODE", "dsc64-1/2")).unwrap(),
            depth: var("DEBUG_DEPTH", "2").parse().unwrap(),
            nblocks: var("DEBUG_NBLOCKS", "2").parse().unwrap(),
            segments: 1,
            ebn0_db: var("DEBUG_EBN0", "15").parse().unwrap(),
            fading: Some(Watterson::MODERATE),
            offset_hz: 1.0,
            clock_ppm: 10.0,
            slips: None,
            seed: 1000,
        };
        let trials: usize = var("DEBUG_TRIALS", "32").parse().unwrap();
        let show: usize = var("DEBUG_SHOW", "4").parse().unwrap();
        // The trial's offset and clock as `run_many` spreads them.
        let trial = |t: usize| {
            let mut s = base.clone();
            s.seed = base.seed + t as u64;
            s.offset_hz = base.offset_hz + (t % 7) as f64 * 2.0 - 6.0;
            s.clock_ppm = base.clock_ppm + (t % 5) as f64 * 15.0 - 30.0;
            s
        };
        let lost = std::sync::Mutex::new(Vec::new());
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        std::thread::scope(|sc| {
            for w in 0..workers {
                let (lost, trial) = (&lost, &trial);
                sc.spawn(move || {
                    for t in (w..trials).step_by(workers) {
                        if !run(&trial(t)).header {
                            lost.lock().unwrap().push(t);
                        }
                    }
                });
            }
        });
        let mut lost = lost.into_inner().unwrap();
        lost.sort_unstable();
        println!("no header in trials {lost:?} of {trials}");
        for &t in lost.iter().take(show) {
            let s = trial(t);
            println!(
                "trial {t}: offset {} Hz, clock {} ppm",
                s.offset_hz, s.clock_ppm
            );
            run_verbose(&s);
        }
    }

    #[test]
    fn a_strong_signal_gets_everything_through() {
        let sc = Scenario {
            profile: Profile::W2400,
            mode: Mode::from_name("csk64-1/2").unwrap(),
            depth: 2,
            nblocks: 2,
            segments: 1,
            ebn0_db: 12.0,
            fading: None,
            offset_hz: 4.0,
            clock_ppm: 10.0,
            slips: None,
            seed: 1,
        };
        let o = run(&sc);
        assert!(o.header);
        assert_eq!((o.frames_ok, o.frames_total), (4, 4));
    }

    /// A file sent as many segments back to back (a preamble right after the
    /// previous payload) must arrive whole. The search once started its preamble
    /// tracker at the oldest search window, which then held payload: the pattern
    /// test could match there by chance and lock early, losing a whole segment
    /// (4-6 % of frames at any SNR in this scenario).
    #[test]
    fn many_back_to_back_segments_lose_nothing() {
        let t = run_many(
            &Scenario {
                profile: Profile::W2400,
                mode: Mode::from_name("csk64-1/2").unwrap(),
                depth: 1,
                nblocks: 3,
                segments: 6,
                ebn0_db: 11.0,
                fading: None,
                offset_hz: 5.0,
                clock_ppm: 0.0,
                slips: None,
                seed: 2000,
            },
            16,
        );
        assert_eq!(t.frames_ok, t.frames_total, "{t:?}");
    }

    #[test]
    fn ebn0_maps_to_the_expected_snr() {
        // DS8 1/2 on 2k4: nominal offset is −10.6 dB; the exact one differs by
        // the CRC, closing pilot and padding (a fraction of a dB).
        let layout = Layout::new(Profile::W2400, Mode::from_name("ds8-1/2").unwrap(), 4);
        let exact = snr_for_ebn0(&layout, 0.0);
        let nominal = layout.mode.snr_offset_db(layout.profile);
        assert!((exact - nominal).abs() < 0.5, "{exact} vs {nominal}");
    }
}
