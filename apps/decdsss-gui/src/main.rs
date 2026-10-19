// made by CasualArclamp
//! `decdsss-gui`: the DecDSSS desktop window.

mod app;
mod demo;
mod engine;
mod scope;
mod spectrum;
mod tx;

use app::{App, Automation, SourceKind};
use eframe::egui;
use std::path::PathBuf;

const USAGE: &str = "usage: decdsss-gui [--demo | --wav FILE] [--speed X] [--profile 500,2k4,3k] \
[--out-dir DIR] [--after SECS] [--screenshot OUT.png] [--window-size WxH] [--tx]";

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let mut source = SourceKind::Device;
    let mut wav = None;
    let mut speed = 1.0;
    let mut out_dir = None;
    let mut profiles = None;
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
    let autostart = matches!(source, SourceKind::Demo | SourceKind::Wav);
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
            let mut app = App::new(auto, source, wav, speed, out_dir, profiles);
            if autostart {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}
