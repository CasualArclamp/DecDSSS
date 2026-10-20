// made by CasualArclamp
//! The rig strip across the top of the window: where the rig's dial is and what
//! mode it is in, read over CAT, with its lamp and whether it is receiving or
//! transmitting. It is there only with CAT on.
//!
//! The dial and the mode are also the rig's controls, as on a radio's own
//! panel:
//!
//! - **Scroll over a digit** and the rig tunes by that digit's place: a click
//!   of the wheel on the kilohertz digit is 1 kHz.
//! - **Type digits while over a digit** and they replace that digit and
//!   everything below it, a place at a time (Enter sets it early, Backspace
//!   takes a digit back, Esc or leaving the dial gives up).
//! - **Right-click a digit** and it and every digit below it go to zero.
//! - **Click the mode** and the rig's other modes slide out to its right; click
//!   one to put the rig in it and they slide back.
//!
//! None of it is offered while the rig is keyed. The window only ever retunes
//! when the operator does one of these; it never does so by itself.

use crate::dial::{self, Entry};
use crate::radio::{Lamp, Radio, RadioStatus};
use crate::scope::col;
use decdsss_rig::{Model, Tuning};
use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, Event, FontId, Id, Key, Layout, Margin, Pos2, Rect,
    RichText, Sense, Stroke, StrokeKind, TextFormat, TextStyle, Ui, Vec2, emath::easing, pos2,
    vec2,
};
use egui::text::LayoutJob;

/// How big the dial's figures are.
const DIAL_SIZE: f32 = 26.0;

/// How long the modes take to slide out or back, seconds.
const SLIDE_TIME: f32 = 0.3;
/// How far a mode slides while it is revealed, points: the row is wiped out
/// from behind the chip that was clicked and also travels this far to the right.
const SLIDE_TRAVEL: f32 = 28.0;
/// The space between the modes in the row, points.
const CHIP_GAP: f32 = 6.0;

/// What the dial says when it is hovered.
const DIAL_HELP: &str = "Scroll over a digit to tune by that digit.\n\
Type digits while over one to enter a frequency from that digit down \
(Enter sets it, Backspace takes one back, Esc cancels).\n\
Right-click a digit to zero it and the digits below it.";

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

/// The dial as one label, for when the rig has not said where it is: the
/// megahertz and kilohertz bright, the hertz dimmer (`egui` has no way to set
/// two colours in one string but a layout job, which is a list of formatted
/// pieces).
fn blank_dial(ui: &mut Ui, head: &str, tail: &str) {
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
                // The rig's own account of itself is under the lamp and its
                // name; the dial has its help of its own.
                ui.label(RichText::new("●").color(lamp_colour(&s)))
                    .on_hover_text(hover.clone());
                ui.label(RichText::new(config.rig.model.name()).monospace().weak())
                    .on_hover_text(hover.clone());
                match (&s.lamp, &s.tuning) {
                    (Lamp::Ok, Some(t)) => reading(ui, radio, &s, t, config.rig.model),
                    (Lamp::Ok, None) => {
                        blank_dial(ui, "--.---", ".---");
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
        });
}

/// What the rig said: the dial, its unit and the mode, with the controls.
fn reading(ui: &mut Ui, radio: &Radio, s: &RadioStatus, t: &Tuning, model: Model) {
    // Nothing is turned while the rig is on the air.
    let live = !s.keyed;
    dial_widget(ui, radio, t.hz, live);
    ui.label(RichText::new("MHz").weak().size(11.0));
    mode_picker(ui, radio, model, &t.mode, live);
    if let Some(why) = &s.tune_problem {
        ui.add(egui::Label::new(RichText::new(why).color(col::BAD).size(11.0)).truncate());
    }
}

/// What the user did to the dial this frame.
enum Act {
    Digit(u8),
    Back,
    Enter,
    Escape,
}

/// The dial: nine digits, each its own target for the wheel and the keyboard.
/// `shown_hz` is where the rig says it is (what the display shows unless a
/// frequency is being typed); a new frequency goes to the rig through `radio`.
fn dial_widget(ui: &mut Ui, radio: &Radio, shown_hz: u64, live: bool) {
    let id = Id::new("rig_dial");
    let (entry_id, wheel_id) = (id.with("entry"), id.with("wheel"));
    let font = FontId::monospace(DIAL_SIZE);
    let glyph = ui
        .painter()
        .layout_no_wrap("0".into(), font.clone(), Color32::WHITE)
        .size();
    // The two dots between the groups take less than a digit.
    let dot = glyph.x * 0.6;
    let size = vec2(glyph.x * dial::PLACES as f32 + dot * 2.0, glyph.y);
    // Sensing clicks as well as hovers is what takes the keyboard from a text
    // box when the dial is clicked.
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    // The cell of the digit at `place`: the dots come after the megahertz and
    // the kilohertz.
    let cell = |place: usize| -> Rect {
        let from_left = dial::PLACES - 1 - place;
        let dots = usize::from(from_left >= 3) + usize::from(from_left >= 6);
        let x = rect.left() + from_left as f32 * glyph.x + dots as f32 * dot;
        Rect::from_min_size(pos2(x, rect.top()), glyph)
    };
    #[cfg(test)]
    ui.data_mut(|d| {
        d.insert_temp(
            id.with("cells"),
            (0..dial::PLACES).map(cell).collect::<Vec<Rect>>(),
        )
    });
    // A pointer between two cells, over a dot, belongs to the nearer digit.
    let hover_place = response.hover_pos().filter(|_| live).and_then(|p: Pos2| {
        (0..dial::PLACES).find(|&place| cell(place).expand2(vec2(dot / 2.0, 0.0)).contains(p))
    });

    let mut entry: Option<Entry> = ui.data(|d| d.get_temp::<Option<Entry>>(entry_id)).flatten();
    // A frequency half typed is dropped when the pointer leaves, or the rig
    // goes on the air.
    if !live || (entry.is_some() && !response.hovered()) {
        entry = None;
    }
    let mut set: Option<u64> = None;

    // Typing, unless a text box has the keyboard.
    if live && response.hovered() && !ui.ctx().egui_wants_keyboard_input() {
        let acts: Vec<Act> = ui.input(|i| {
            i.events
                .iter()
                .flat_map(|event| match event {
                    Event::Text(text) => text
                        .chars()
                        .filter_map(|c| c.to_digit(10))
                        .map(|d| Act::Digit(d as u8))
                        .collect(),
                    Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => match key {
                        Key::Backspace => vec![Act::Back],
                        Key::Enter => vec![Act::Enter],
                        Key::Escape => vec![Act::Escape],
                        _ => vec![],
                    },
                    _ => vec![],
                })
                .collect()
        });
        for act in acts {
            match act {
                Act::Digit(d) => {
                    if entry.is_none() {
                        entry = hover_place.map(|place| Entry::begin(shown_hz, place));
                    }
                    if let Some(typing) = entry.as_mut()
                        && typing.push(d)
                    {
                        // The last place is in: that is the frequency.
                        set = Some(typing.hz);
                        entry = None;
                    }
                }
                Act::Back => {
                    if let Some(typing) = entry.as_mut()
                        && !typing.back()
                    {
                        entry = None;
                    }
                }
                Act::Enter => {
                    if let Some(typing) = entry.take() {
                        set = Some(typing.hz);
                    }
                }
                Act::Escape => entry = None,
            }
            if set.is_some() {
                break;
            }
        }
    }

    // The wheel: one click is one step of the digit it is over. A wheel with
    // finer steps (a trackpad) is added up until it is a click's worth.
    if let (Some(place), true) = (hover_place, set.is_none()) {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            let per_click = ui.ctx().options(|o| o.input_options.line_scroll_speed);
            let mut wheel: f32 = ui.data(|d| d.get_temp(wheel_id)).unwrap_or(0.0) + dy;
            let clicks = (wheel / per_click).trunc();
            wheel -= clicks * per_click;
            ui.data_mut(|d| d.insert_temp(wheel_id, wheel));
            // Scrolling up is up in frequency.
            if clicks != 0.0 {
                entry = None;
                set = Some(dial::step(shown_hz, place, clicks as i64));
            }
            // Nothing under the dial is to scroll too.
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }
    } else if hover_place.is_none() {
        ui.data_mut(|d| d.remove_temp::<f32>(wheel_id));
    }

    // A right click zeroes the digit and all those to its right.
    if let (Some(place), true) = (hover_place, response.secondary_clicked()) {
        entry = None;
        set = Some(dial::clear_from(shown_hz, place));
    }

    if let Some(hz) = set
        && hz != shown_hz
    {
        // A refusal (the rig is on the air) shows by nothing changing.
        let _ = radio.tune(Some(hz), None);
    }
    // (An `Option` is what `egui`'s memory can hold and also clear.)
    ui.data_mut(|d| d.insert_temp(entry_id, entry.clone()));

    // Drawing: what is typed if anything is, else what the rig says.
    let hz = entry.as_ref().map_or(shown_hz, |e| e.hz);
    let painter = ui.painter_at(rect.expand(3.0));
    let cursor = entry.as_ref().and_then(Entry::cursor);
    for place in (0..dial::PLACES).rev() {
        let r = cell(place);
        let hovered = entry.is_none() && hover_place == Some(place);
        if hovered {
            painter.rect_filled(
                r.expand2(vec2(1.0, 0.0)),
                3.0,
                Color32::from_white_alpha(22),
            );
        }
        let typing_here = entry.is_some() && cursor == Some(place);
        if typing_here {
            painter.rect_stroke(
                r.expand2(vec2(1.0, 0.0)),
                3.0,
                Stroke::new(1.0, col::MARK),
                StrokeKind::Inside,
            );
        }
        let colour = if entry.as_ref().is_some_and(|e| e.typed(place)) {
            col::MARK
        } else if dial::is_leading_zero(hz, place) {
            Color32::from_gray(60)
        } else if hovered {
            Color32::WHITE
        } else if place >= 3 {
            Color32::from_gray(232)
        } else {
            Color32::from_gray(138)
        };
        painter.text(
            r.center(),
            Align2::CENTER_CENTER,
            char::from(b'0' + dial::digit(hz, place)),
            font.clone(),
            colour,
        );
    }
    for after in [6, 3] {
        painter.text(
            pos2(cell(after).right() + dot / 2.0, rect.center().y),
            Align2::CENTER_CENTER,
            ".",
            font.clone(),
            Color32::from_gray(if after >= 3 { 160 } else { 138 }),
        );
    }

    if hover_place.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
    }
    response.on_hover_text(if live {
        DIAL_HELP
    } else {
        "The rig is transmitting; the dial is locked."
    });
}

/// How big a mode's chip is.
fn chip_size(galley: &egui::Galley) -> Vec2 {
    galley.size() + vec2(18.0, 4.0)
}

/// A mode's chip: its name in a box, the box filled when `lit`.
fn paint_chip(
    painter: &egui::Painter,
    rect: Rect,
    galley: std::sync::Arc<egui::Galley>,
    colour: Color32,
    lit: bool,
    fade: f32,
) {
    if lit {
        painter.rect_filled(rect, 4.0, colour.gamma_multiply(0.2 * fade));
    }
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, colour.gamma_multiply(fade)),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() / 2.0,
        galley,
        colour.gamma_multiply(fade),
    );
}

/// The mode in a box; clicked, the rig's other modes slide out to its right.
/// One of them clicked puts the rig in it, and they slide back the way they
/// came.
fn mode_picker(ui: &mut Ui, radio: &Radio, model: Model, current: &str, live: bool) {
    let id = Id::new("rig_mode");
    let (open_id, list_id) = (id.with("open"), id.with("list"));
    let font = TextStyle::Body.resolve(ui.style());
    let galley = |ui: &Ui, text: &str, colour: Color32| {
        ui.painter()
            .layout_no_wrap(text.to_string(), font.clone(), colour)
    };

    let mut open: bool = live && ui.data(|d| d.get_temp(open_id)).unwrap_or(false);
    // The others as they were when the row was opened: choosing one makes it
    // the current mode at once, and the row must not reshuffle as it closes.
    let others = |current: &str| -> Vec<String> {
        model
            .modes()
            .iter()
            .filter(|m| **m != current)
            .map(|m| m.to_string())
            .collect()
    };

    let group = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let main_galley = galley(ui, current, col::TRACE);
        let (main_rect, main) = ui.allocate_exact_size(
            chip_size(&main_galley),
            if live { Sense::click() } else { Sense::hover() },
        );
        if main.clicked() {
            open = !open;
            if open {
                ui.data_mut(|d| d.insert_temp(list_id, others(current)));
            }
        }
        if live && main.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // Where the controls are, for the tests that click them.
        #[cfg(test)]
        ui.data_mut(|d| {
            d.insert_temp(id.with("main"), main_rect);
            d.insert_temp(id.with("chips"), Vec::<(String, Rect)>::new());
        });
        let t = ui
            .ctx()
            .animate_bool_with_time_and_easing(id, open, SLIDE_TIME, easing::cubic_out);
        paint_chip(
            ui.painter(),
            main_rect,
            main_galley,
            col::TRACE,
            open || main.hovered(),
            1.0,
        );
        main.on_hover_text(if live {
            "Click to choose another mode."
        } else {
            "The rig is transmitting; its mode is locked."
        });
        if t <= 0.0 {
            return main_rect;
        }

        let list: Vec<String> = ui
            .data(|d| d.get_temp(list_id))
            .unwrap_or_else(|| others(current));
        let chips: Vec<_> = list
            .iter()
            .map(|name| {
                let g = galley(ui, name, col::TRACE);
                (name, chip_size(&g), g)
            })
            .collect();
        let full: f32 = chips.iter().map(|(_, s, _)| s.x + CHIP_GAP).sum();
        let (region, _) =
            ui.allocate_exact_size(vec2(full * t, main_rect.height()), Sense::hover());
        let painter = ui.painter_at(region);
        let mut x = region.left() + CHIP_GAP - (1.0 - t) * SLIDE_TRAVEL;
        for (name, size, g) in chips {
            let rect = Rect::from_min_size(pos2(x, region.center().y - size.y / 2.0), size);
            x += size.x + CHIP_GAP;
            // Only the part that has come out can be clicked, and only while
            // the row is opening.
            let visible = rect.intersect(region);
            #[cfg(test)]
            ui.data_mut(|d| {
                let mut v: Vec<(String, Rect)> = d.get_temp(id.with("chips")).unwrap_or_default();
                v.push((name.clone(), visible));
                d.insert_temp(id.with("chips"), v);
            });
            let hovered = open && visible.is_positive() && {
                let r = ui.interact(visible, id.with(name.as_str()), Sense::click());
                if r.clicked() {
                    // Put the rig in it, and send the row back.
                    let _ = radio.tune(None, Some(name));
                    open = false;
                }
                r.hovered()
            };
            if hovered {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            paint_chip(
                &painter,
                rect,
                g,
                col::TRACE,
                hovered || name.as_str() == current,
                t,
            );
        }
        main_rect.union(region)
    });

    // A click anywhere else puts the row away too.
    let away = ui.input(|i| {
        i.pointer.any_click()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !group.inner.contains(p))
    });
    if open && away {
        open = false;
    }
    ui.data_mut(|d| d.insert_temp(open_id, open));
}

#[cfg(test)]
mod tests {
    //! The strip driven headless: `egui` runs a frame from a made-up input (a
    //! wheel turned, keys typed, a click) with no window, and the stand-in rig
    //! is what it ends up tuned to.

    use super::*;
    use decdsss_rig::testing::FakeRig;
    use egui::{MouseWheelUnit, PointerButton, RawInput, TouchPhase};
    use std::time::{Duration, Instant};

    struct Bench {
        ctx: egui::Context,
        radio: Radio,
        rig: FakeRig,
        time: f64,
    }

    impl Bench {
        fn new() -> Self {
            let rig = FakeRig::new(Model::Ft710);
            let radio = Radio::with_fake_rig(&rig, 5, 5);
            let started = Instant::now();
            while radio.status().tuning.is_none() && started.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut bench = Self {
                ctx: egui::Context::default(),
                radio,
                rig,
                time: 0.0,
            };
            bench.frames(&[], 2);
            bench
        }

        /// Run `count` frames, the first with `events`, a sixtieth of a second apart.
        fn frames(&mut self, events: &[Event], count: usize) {
            for n in 0..count {
                let raw = RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 200.0))),
                    time: Some(self.time),
                    events: if n == 0 { events.to_vec() } else { vec![] },
                    ..Default::default()
                };
                self.time += 1.0 / 60.0;
                let radio = self.radio.clone();
                let mut out = self.ctx.run_ui(raw, |ui| show(ui, &radio));
                // No renderer takes the textures; say so rather than panic.
                out.textures_delta.clear();
            }
        }

        fn move_to(&mut self, at: Pos2) {
            self.frames(&[Event::PointerMoved(at)], 2);
        }

        fn click(&mut self, at: Pos2) {
            self.press(at, PointerButton::Primary);
        }

        fn right_click(&mut self, at: Pos2) {
            self.press(at, PointerButton::Secondary);
        }

        fn press(&mut self, at: Pos2, button: PointerButton) {
            self.move_to(at);
            for pressed in [true, false] {
                self.frames(
                    &[Event::PointerButton {
                        pos: at,
                        button,
                        pressed,
                        modifiers: Default::default(),
                    }],
                    2,
                );
            }
        }

        fn wheel(&mut self, lines: f32) {
            self.frames(
                &[Event::MouseWheel {
                    unit: MouseWheelUnit::Line,
                    delta: vec2(0.0, lines),
                    phase: TouchPhase::Move,
                    modifiers: Default::default(),
                }],
                30,
            );
        }

        fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                self.frames(&[Event::Text(c.to_string())], 1);
            }
        }

        /// A key pressed and let go (`egui` ignores a second press of a key
        /// it thinks is still down).
        fn key(&mut self, key: Key) {
            for pressed in [true, false] {
                self.frames(
                    &[Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: Default::default(),
                    }],
                    1,
                );
            }
        }

        fn cell(&self, place: usize) -> Pos2 {
            let cells: Vec<Rect> = self
                .ctx
                .data(|d| d.get_temp(Id::new("rig_dial").with("cells")))
                .expect("the dial was drawn");
            cells[place].center()
        }

        fn chips(&self) -> Vec<(String, Rect)> {
            self.ctx
                .data(|d| d.get_temp(Id::new("rig_mode").with("chips")))
                .unwrap_or_default()
        }

        fn main_chip(&self) -> Pos2 {
            let rect: Rect = self
                .ctx
                .data(|d| d.get_temp(Id::new("rig_mode").with("main")))
                .expect("the mode was drawn");
            rect.center()
        }

        fn settle(&self, what: impl Fn() -> bool) {
            let started = Instant::now();
            while !what() && started.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(what());
        }
    }

    const OFF_DIAL: Pos2 = pos2(900.0, 150.0);

    #[test]
    fn the_wheel_over_a_digit_tunes_by_that_digit() {
        let mut b = Bench::new();
        // The kilohertz digit: up one click is 1 kHz, down two is one under where it began.
        b.move_to(b.cell(3));
        b.wheel(1.0);
        b.settle(|| b.rig.hz() == 14_098_000);
        b.wheel(-2.0);
        b.settle(|| b.rig.hz() == 14_096_000);
        // The megahertz digit; the dial shows what it was turned to.
        b.move_to(b.cell(6));
        b.wheel(3.0);
        b.settle(|| b.rig.hz() == 17_096_000);
        assert_eq!(b.radio.status().tuning.map(|t| t.hz), Some(17_096_000));
        assert!(b.rig.keyings().is_empty());
    }

    #[test]
    fn a_right_click_zeroes_the_digit_and_those_to_its_right() {
        let mut b = Bench::new();
        // 14.097.000: from the hundreds of kilohertz down (0, 9, 7) is cleared.
        b.right_click(b.cell(5));
        b.settle(|| b.rig.hz() == 14_000_000);
        // Over the megahertz digit it goes too: 14 becomes 10.
        b.right_click(b.cell(6));
        b.settle(|| b.rig.hz() == 10_000_000);
        // Over a digit that is already zero it changes nothing, and tunes nothing.
        b.right_click(b.cell(2));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(b.rig.hz(), 10_000_000);
        // Off the dial, nothing.
        b.right_click(OFF_DIAL);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(b.rig.hz(), 10_000_000);
        assert!(b.rig.keyings().is_empty());
    }

    #[test]
    fn the_wheel_off_the_dial_tunes_nothing() {
        let mut b = Bench::new();
        b.move_to(OFF_DIAL);
        b.wheel(2.0);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(b.rig.hz(), 14_097_000);
    }

    #[test]
    fn digits_typed_over_a_digit_replace_it_and_those_below() {
        let mut b = Bench::new();
        // From the tens of megahertz, 0 7 0 7 4 and Enter.
        b.move_to(b.cell(7));
        b.type_text("07074");
        // Typed but not set: the rig has not been touched.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(b.rig.hz(), 14_097_000);
        b.key(Key::Enter);
        b.settle(|| b.rig.hz() == 7_074_000);
        // The last place typed sets it with no Enter.
        b.move_to(b.cell(3));
        b.type_text("5123");
        b.settle(|| b.rig.hz() == 7_075_123);
    }

    #[test]
    fn a_frequency_half_typed_is_given_up_on_escape_or_leaving() {
        let mut b = Bench::new();
        b.move_to(b.cell(6));
        b.type_text("2");
        b.key(Key::Escape);
        b.move_to(b.cell(6));
        b.type_text("3");
        b.move_to(OFF_DIAL);
        b.key(Key::Enter);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(b.rig.hz(), 14_097_000);
        // Backspace takes a digit back.
        b.move_to(b.cell(6));
        b.type_text("9");
        b.key(Key::Backspace);
        b.type_text("8");
        b.key(Key::Enter);
        b.settle(|| b.rig.hz() == 18_000_000);
    }

    #[test]
    fn clicking_the_mode_slides_the_others_out_and_choosing_one_slides_them_back() {
        let mut b = Bench::new();
        assert!(b.chips().is_empty(), "closed to begin with");
        b.click(b.main_chip());
        // Part way out, then all the way.
        b.frames(&[], 8);
        let part = b.chips();
        assert!(!part.is_empty());
        b.frames(&[], 40);
        let out = b.chips();
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        let want: Vec<&str> = Model::Ft710
            .modes()
            .iter()
            .copied()
            .filter(|m| *m != "USB-D")
            .collect();
        assert_eq!(names, want, "every other mode, in order, to the right");
        let main = b.main_chip();
        assert!(out.iter().all(|(_, r)| r.left() > main.x));
        // Slid out: a chip moved right as the row opened.
        assert!(out[0].1.left() >= part[0].1.left());
        // Choose LSB.
        let lsb = out.iter().find(|(n, _)| n == "LSB").unwrap().1;
        b.click(lsb.center());
        b.settle(|| b.rig.mode().name() == "LSB");
        // Slid back: shut again, and the main chip now says LSB.
        b.frames(&[], 60);
        assert!(b.chips().is_empty());
        assert_eq!(b.radio.status().tuning.map(|t| t.mode), Some("LSB".into()));
        // The row opens again from the new mode, without it.
        b.click(b.main_chip());
        b.frames(&[], 40);
        let names: Vec<String> = b.chips().into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"USB-D".to_string()) && !names.contains(&"LSB".to_string()));
        // A click elsewhere puts it away.
        b.click(OFF_DIAL);
        b.frames(&[], 60);
        assert!(b.chips().is_empty());
        assert_eq!(b.rig.mode().name(), "LSB");
    }

    #[test]
    fn nothing_is_turned_while_the_rig_is_on_the_air() {
        let mut b = Bench::new();
        b.radio.test_ptt(true);
        b.settle(|| b.rig.transmitting());
        b.settle(|| b.radio.status().keyed);
        b.frames(&[], 3);
        b.move_to(b.cell(3));
        b.wheel(2.0);
        b.click(b.main_chip());
        b.frames(&[], 40);
        assert!(b.chips().is_empty());
        b.radio.test_ptt(false);
        assert_eq!(b.rig.hz(), 14_097_000);
        assert_eq!(b.rig.mode().name(), "USB-D");
    }
}
