// made by CasualArclamp
//! `decdsss-gui`: the DecDSSS desktop window.
// A release build on Windows is a GUI program, so double-clicking it opens no
// console window; `attach_console` gives it the terminal it was started from, if
// any, so `--version`, `--write-demo` and the messages of the unattended options
// still print there.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod demo;
mod engine;
mod monitor;
mod prefs;
mod scope;
mod spectrum;
mod tx;
mod voice;
mod waterfall;

use app::{App, Automation, SourceKind};
use eframe::egui;
use std::path::PathBuf;

const USAGE: &str = "usage: decdsss-gui [--demo | --wav FILE] [--speed X] [--profile 500,2k4,3k] \
[--device NAME] [--out-dir DIR] [--after SECS] [--screenshot OUT.png] [--window-size WxH] [--tx] [--tx-file FILE] [--record-for SECS [--mic NAME]] [--view rx|tx|both] [--monitor OUTPUT] | --write-demo OUT.wav";

/// Attaches to the console of the process that started this one (a terminal), if
/// there is one; harmless when output is redirected or there is no terminal.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // SAFETY: a plain Win32 call with the documented "parent process" id.
    unsafe {
        AttachConsole(0xFFFF_FFFF);
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}

fn main() -> eframe::Result {
    attach_console();
    let mut args = std::env::args().skip(1);
    let mut source = SourceKind::Device;
    let mut wav = None;
    let mut speed = 1.0;
    let mut out_dir = None;
    let mut profiles = None;
    let mut device: Option<String> = None;
    let mut auto = Automation::default();
    let mut size = [1500.0, 900.0];
    while let Some(a) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("decdsss-gui: {name} needs a value\n{USAGE}");
                std::process::exit(2)
            })
        };
        match a.as_str() {
            "--version" => {
                println!("decdsss-gui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--demo" => source = SourceKind::Demo,
            "--write-demo" => {
                // The demo signal as a WAV (open it with --wav, or in the cli's rx):
                // no window.
                let path = PathBuf::from(value("--write-demo"));
                let d = demo::make();
                match decdsss_audio::wav::write(&path, &d.audio, demo::RATE) {
                    Ok(_) => println!(
                        "{} s of {} Hz audio → {}
{}",
                        d.audio.len() as u32 / demo::RATE,
                        demo::RATE,
                        path.display(),
                        d.what
                    ),
                    Err(e) => {
                        eprintln!("decdsss-gui: cannot write {}: {e}", path.display());
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }
            "--device" => {
                source = SourceKind::Device;
                device = Some(value("--device"));
            }
            "--wav" => {
                source = SourceKind::Wav;
                wav = Some(PathBuf::from(value("--wav")));
            }
            "--speed" => speed = value("--speed").parse().unwrap_or(1.0),
            "--out-dir" => out_dir = Some(PathBuf::from(value("--out-dir"))),
            "--profile" => {
                let list = value("--profile");
                let mut on = [false; 3];
                for name in list.split(',') {
                    match decdsss_core::Profile::from_name(name) {
                        Some(p) => {
                            on[decdsss_core::Profile::ALL
                                .iter()
                                .position(|q| *q == p)
                                .unwrap_or(0)] = true
                        }
                        None => {
                            eprintln!("decdsss-gui: unknown profile {name:?}\n{USAGE}");
                            std::process::exit(2);
                        }
                    }
                }
                profiles = Some(on);
            }
            "--after" => auto.after = value("--after").parse().ok(),
            "--screenshot" => auto.screenshot = Some(PathBuf::from(value("--screenshot"))),
            "--tx" => auto.open_tx = true,
            "--tx-file" => auto.tx_file = Some(PathBuf::from(value("--tx-file"))),
            "--record-for" => auto.record_for = value("--record-for").parse().ok(),
            "--mic" => auto.mic = Some(value("--mic")),
            "--view" => auto.view = Some(value("--view")),
            "--monitor" => auto.monitor = Some(value("--monitor")),
            "--tx-play" => auto.tx_play = Some(value("--tx-play")),
            "--ptt-for" => auto.ptt_for = value("--ptt-for").parse().ok(),
            "--tx-stop" => auto.tx_stop = value("--tx-stop").parse().ok(),
            "--window-size" => {
                let v = value("--window-size");
                if let Some((w, h)) = v.split_once('x')
                    && let (Ok(w), Ok(h)) = (w.parse::<f32>(), h.parse::<f32>())
                {
                    size = [w, h];
                }
            }
            other => {
                eprintln!("decdsss-gui: unknown option {other:?}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    // A screenshot run needs something to look at and a time to take it.
    if auto.screenshot.is_some() && auto.after.is_none() {
        auto.after = Some(20.0);
    }
    let autostart = matches!(source, SourceKind::Demo | SourceKind::Wav) || device.is_some();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([980.0, 600.0])
            .with_title("DecDSSS"),
        ..Default::default()
    };
    eframe::run_native(
        "DecDSSS",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // egui's proportional font has no arrows or ●; its bundled monospace
            // font does, so let text fall back to it.
            let mut fonts = egui::FontDefinitions::default();
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.push("Hack".into());
            }
            cc.egui_ctx.set_fonts(fonts);
            let mut app = App::new(auto, source, wav, speed, out_dir, profiles, device);
            if autostart {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}
