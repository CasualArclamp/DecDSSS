// made by CasualArclamp
//! `decdsss-gui`: the DecDSSS desktop window.
// A release build on Windows is a GUI program, so double-clicking it opens no
// console window; `attach_console` gives it the terminal it was started from, if
// any, so `--version`, `--write-demo` and the messages of the unattended options
// still print there.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cat;
mod demo;
mod dial;
mod engine;
mod live;
mod monitor;
mod player;
mod prefs;
mod radio;
mod rig_strip;
mod scope;
mod spectrum;
mod tx;
mod voice;
mod waterfall;

use app::{App, Automation, SourceKind};
use eframe::egui;
use std::path::PathBuf;

const USAGE: &str = "usage: decdsss-gui [--demo | --wav FILE] [--speed X] [--profile 500,2k4,3k] \
[--device NAME] [--out-dir DIR] [--after SECS] [--screenshot OUT.png] [--window-size WxH] [--tx] [--tx-file FILE] [--tx-text MSG|cq] [--tx-band 500@+500] [--tx-wfid] [--record-for SECS [--mic NAME]] [--view rx|tx|both] [--monitor OUTPUT] [--open-cat] [--fake-rig] | --write-demo OUT.wav";

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
    let mut bands = None;
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
                // A profile, and for 500 Hz optionally one slot (`500@-500`); plain
                // `500` listens on all three.
                let mut on = [false; 5];
                for name in list.split(',') {
                    let (prof, slot) = match name.split_once('@') {
                        Some((p, s)) => (p, decdsss_core::Slot::from_name(s)),
                        None => (name, None),
                    };
                    let Some(p) = decdsss_core::Profile::from_name(prof) else {
                        eprintln!("decdsss-gui: unknown profile {name:?}\n{USAGE}");
                        std::process::exit(2);
                    };
                    for (i, b) in decdsss_core::Band::ALL.iter().enumerate() {
                        if b.profile == p && slot.is_none_or(|s| s == b.slot) {
                            on[i] = true;
                        }
                    }
                }
                bands = Some(on);
            }
            "--after" => auto.after = value("--after").parse().ok(),
            "--screenshot" => auto.screenshot = Some(PathBuf::from(value("--screenshot"))),
            "--tx" => auto.open_tx = true,
            "--open-cat" => auto.open_cat = true,
            "--fake-rig" => auto.fake_rig = true,
            "--tx-file" => auto.tx_file = Some(PathBuf::from(value("--tx-file"))),
            "--tx-text" => auto.tx_text = Some(value("--tx-text")),
            "--tx-band" => auto.tx_band = Some(value("--tx-band")),
            "--tx-wfid" => auto.tx_wfid = true,
            "--record-for" => auto.record_for = value("--record-for").parse().ok(),
            "--mic" => auto.mic = Some(value("--mic")),
            "--view" => auto.view = Some(value("--view")),
            "--monitor" => auto.monitor = Some(value("--monitor")),
            "--tx-play" => auto.tx_play = Some(value("--tx-play")),
            "--listen" => auto.listen = Some(value("--listen")),
            "--listen-test" => auto.listen_test = true,
            "--ptt-for" => auto.ptt_for = value("--ptt-for").parse().ok(),
            "--live-for" => auto.live_for = value("--live-for").parse().ok(),
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
    // The window's own icon (title bar and taskbar while it runs). `include_bytes!`
    // puts the PNG inside the executable at compile time, so nothing is read from
    // disk; the exe's file icon comes from build.rs.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size([980.0, 600.0])
        .with_title(format!("DecDSSS v{}", app::VERSION));
    match eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/decdsss-256.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => eprintln!("decdsss-gui: window icon: {e}"),
    }
    let options = eframe::NativeOptions {
        viewport,
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
            let mut app = App::new(auto, source, wav, speed, out_dir, bands, device);
            if autostart {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}
