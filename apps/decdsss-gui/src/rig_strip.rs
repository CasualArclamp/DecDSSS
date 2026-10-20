// made by CasualArclamp
//! The rig strip across the top of the window: where the rig's dial is and what
//! mode it is in, read over CAT, with its lamp and whether it is receiving or
//! transmitting. It is there only with CAT on.
//!
//! The program only reads the rig: the frequency and the mode are set on the
//! radio (the readout is for checking that it is on USB, or its data mode, and
//! where).

use crate::radio::{Lamp, Radio, RadioStatus};
use crate::scope::col;
use decdsss_rig::Tuning;
use eframe::egui::{
    self, Align, Color32, FontId, Layout, Margin, RichText, Stroke, TextFormat, Ui,
};
use egui::text::LayoutJob;

/// How big the dial's figures are.
const DIAL_SIZE: f32 = 26.0;

/// The lamp's colour: grey when the program keys nothing, amber while the rig
/// is being found, green when it is ready, red when the transmitter is keyed or
/// the rig is not working.
pub fn lamp_colour(s: &RadioStatus) -> Color32 {
    match (s.keyed, &s.lamp) {
        (true, _) | (_, Lamp::Bad) => col::BAD,
        (_, Lamp::Wait) => col::MARK,
        (_, Lamp::Ok) => col::LOCK,
        (_, Lamp::Off) => Color32::GRAY,
    }
}

/// The dial as one label: the megahertz and kilohertz bright, the hertz
/// dimmer (`egui` has no way to set two colours in one string but a layout
/// job, which is a list of formatted pieces).
fn dial(ui: &mut Ui, head: &str, tail: &str) {
    let piece = |color| TextFormat {
        font_id: FontId::monospace(DIAL_SIZE),
        color,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.append(head, 0.0, piece(Color32::from_gray(232)));
    job.append(tail, 0.0, piece(Color32::from_gray(138)));
    ui.label(job);
}

/// The mode in a box, in the trace colour.
fn mode_chip(ui: &mut Ui, mode: &str) {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, col::TRACE))
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(mode).strong().color(col::TRACE));
        });
}

/// Draw the strip; nothing if CAT is off.
pub fn show(ui: &mut Ui, radio: &Radio) {
    let config = radio.config();
    let s = radio.status();
    if !config.cat || s.lamp == Lamp::Off {
        return;
    }
    let mut hover = format!("{}\n{}", config.summary(), s.text);
    if let Some(why) = &s.tuning_problem {
        hover.push_str(&format!("\nfrequency and mode: {why}"));
    }
    egui::Frame::new()
        .fill(Color32::from_gray(15))
        .stroke(Stroke::new(1.0, col::GRID))
        .corner_radius(5.0)
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                ui.label(RichText::new("●").color(lamp_colour(&s)));
                ui.label(RichText::new(config.rig.model.name()).monospace().weak());
                match (&s.lamp, &s.tuning) {
                    (Lamp::Ok, Some(t)) => reading(ui, t),
                    (Lamp::Ok, None) => {
                        dial(ui, "--.---", ".---");
                        let why = s.tuning_problem.as_deref().unwrap_or("asking the rig…");
                        ui.add(egui::Label::new(RichText::new(why).weak()).truncate());
                    }
                    (Lamp::Wait, _) => {
                        ui.label(RichText::new("looking for the rig…").weak());
                    }
                    _ => {
                        ui.add(egui::Label::new(RichText::new(&s.text).color(col::BAD)).truncate());
                    }
                }
                if s.lamp == Lamp::Ok {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (word, colour) = if s.keyed {
                            ("TX", col::BAD)
                        } else {
                            ("RX", col::LOCK)
                        };
                        ui.label(RichText::new(word).strong().color(colour));
                    });
                }
            });
        })
        .response
        .on_hover_text(hover);
}

/// What the rig said: the dial, its unit and the mode.
fn reading(ui: &mut Ui, t: &Tuning) {
    let (head, tail) = t.split();
    dial(ui, &head, &tail);
    ui.label(RichText::new("MHz").weak().size(11.0));
    mode_chip(ui, &t.mode);
}
