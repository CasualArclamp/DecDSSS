// made by CasualArclamp
//! `decdsss`: the command line.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use decdsss_core::{Mode, Profile};

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
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Modes { profile } => modes(profile.as_deref()),
    }
}

fn modes(profile: Option<&str>) -> Result<()> {
    let profiles: Vec<Profile> = match profile {
        Some(s) => match Profile::from_name(s) {
            Some(p) => vec![p],
            None => bail!("unknown profile {s:?}: use 500, 2k4 or 3k"),
        },
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
