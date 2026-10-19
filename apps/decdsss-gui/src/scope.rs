// made by CasualArclamp
//! The instrument panels: dark faces, a hairline frame, the title at the top
//! left and a reading at the top right, one muted hue per quantity. The look
//! is DecSAT's DVB-CID card (`apps/decsat-gui/src/side_panel.rs`).

use decdsss_phy::Iq;
use decdsss_phy::rx::acq::Surface;
use decdsss_phy::rx::header::Finger;
use decdsss_phy::rx::preamble::DelayProfile;
use eframe::egui::{
    Align, Align2, Color32, ColorImage, Context, FontId, Layout, Painter, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2,
};

/// One hue a quantity, kept muted.
pub mod col {
    use eframe::egui::Color32;
    /// Data.
    pub const TRACE: Color32 = Color32::from_rgb(86, 182, 236);
    /// In lock; the prompt correlator.
    pub const LOCK: Color32 = Color32::from_rgb(98, 200, 140);
    /// Early and late correlators; waiting; a detection.
    pub const MARK: Color32 = Color32::from_rgb(232, 176, 72);
    /// Something wrong.
    pub const BAD: Color32 = Color32::from_rgb(214, 96, 88);
    pub const GRID: Color32 = Color32::from_gray(48);
    pub const AXIS: Color32 = Color32::from_gray(76);
    pub const REF: Color32 = Color32::from_gray(128);
}

/// An inferno-like colour for t in 0..1.
pub fn palette(t: f32) -> Color32 {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [0, 0, 4]),
        (0.2, [40, 11, 84]),
        (0.4, [101, 21, 110]),
        (0.6, [159, 42, 99]),
        (0.8, [237, 105, 37]),
        (1.0, [252, 255, 164]),
    ];
    let t = t.clamp(0.0, 1.0);
    let k = STOPS
        .windows(2)
        .position(|w| t <= w[1].0)
        .unwrap_or(STOPS.len() - 2);
    let ((t0, c0), (t1, c1)) = (STOPS[k], STOPS[k + 1]);
    let f = (t - t0) / (t1 - t0);
    let mix = |a: u8, b: u8| (f32::from(a) + f * (f32::from(b) - f32::from(a))).round() as u8;
    Color32::from_rgb(mix(c0[0], c1[0]), mix(c0[1], c1[1]), mix(c0[2], c1[2]))
}

/// An instrument panel `size` big: a dark face, a hairline frame, the title at
/// the top left and a reading at the top right. Returns the painter and the
/// area left for the plot.
pub fn panel(ui: &mut Ui, size: Vec2, title: &str, reading: &str) -> (Painter, Rect) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let v = ui.visuals();
    painter.rect_filled(rect, 3.0, v.extreme_bg_color);
    painter.rect_stroke(
        rect,
        3.0,
        (1.0, v.widgets.noninteractive.bg_stroke.color),
        StrokeKind::Inside,
    );
    let font = FontId::proportional(10.5);
    let weak = v.weak_text_color();
    let strong = v.strong_text_color();
    let tw = painter
        .layout_no_wrap(title.to_string(), font.clone(), weak)
        .size()
        .x;
    let rw = painter
        .layout_no_wrap(reading.to_string(), font.clone(), strong)
        .size()
        .x;
    // A reading that does not fit beside the title goes on a line of its own.
    let two_lines = !reading.is_empty() && tw + rw + 22.0 > size.x;
    painter.text(
        rect.left_top() + vec2(7.0, 5.0),
        Align2::LEFT_TOP,
        title,
        font.clone(),
        weak,
    );
    let mut top = 21.0;
    if two_lines {
        painter.text(
            rect.left_top() + vec2(7.0, 17.0),
            Align2::LEFT_TOP,
            reading,
            font,
            strong,
        );
        top += 12.0;
    } else {
        painter.text(
            rect.right_top() + vec2(-7.0, 5.0),
            Align2::RIGHT_TOP,
            reading,
            font,
            strong,
        );
    }
    let area = Rect::from_min_max(rect.min + vec2(7.0, top), rect.max - vec2(7.0, 7.0));
    (painter, area)
}

fn small(painter: &Painter, at: Pos2, align: Align2, text: &str, color: Color32) {
    painter.text(at, align, text, FontId::proportional(9.5), color);
}

/// A link margin's colour: red below 0 dB, amber within 1 dB, else green
/// (DecSAT's `margin_colour`, `apps/decsat-gui/src/side_panel.rs`).
pub fn margin_colour(margin_db: f32) -> Color32 {
    if margin_db < 0.0 {
        col::BAD
    } else if margin_db < 1.0 {
        col::MARK
    } else {
        col::LOCK
    }
}

/// What the margin bar shows.
pub struct Margin {
    /// The mode, as the header names it.
    pub mode: String,
    /// Eb/N0 now, dB; `None` before there is a reading or between segments.
    pub ebn0_db: Option<f32>,
    /// The same as an in-band SNR, dB.
    pub snr_db: Option<f32>,
    /// The Eb/N0 the mode needs, dB, and whether that was measured.
    pub need_db: f32,
    pub measured: bool,
    /// The station's segment has ended (no reading until the next).
    pub between: bool,
}

/// Eb/N0 bar scale, dB: fixed, so the bar does not jump as the reading moves.
const MARGIN_LO: f32 = 0.0;
const MARGIN_HI: f32 = 20.0;

/// The mode big with its link margin beside it, and under them an Eb/N0 bar
/// on a fixed scale with a tick at the Eb/N0 the mode needs. The layout is
/// DecSAT's MODCOD-and-margin card (`dvb_card` in
/// `apps/decsat-gui/src/side_panel.rs`), with Eb/N0 for its Es/N0.
pub fn margin_bar(ui: &mut Ui, m: &Margin, width: f32) {
    let margin = m.ebn0_db.map(|e| e - m.need_db);
    let colour = margin.map_or(col::REF, margin_colour);
    let need = if m.measured {
        format!("needs {:.1} dB", m.need_db)
    } else {
        format!("needs ≈ {:.1} dB", m.need_db)
    };
    let resp = ui
        .horizontal(|ui| {
            ui.set_width(width);
            ui.label(
                RichText::new(&m.mode)
                    .monospace()
                    .size(22.0)
                    .color(col::LOCK),
            );
            if let Some(v) = margin {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{v:+.1} dB"))
                            .monospace()
                            .size(17.0)
                            .color(colour),
                    );
                });
            }
        })
        .response;
    let about = if m.measured {
        "The Eb/N0 it needs is the lowest at which every frame arrived over AWGN \
         through the whole receiver (DESIGN.md, Verification)."
    } else {
        "This mode has not been measured on this profile: 4.0 dB is assumed, \
         the figure most measured modes came out at."
    };
    resp.on_hover_text(format!(
        "Margin: the Eb/N0 now less the Eb/N0 this mode needs. Below 0 dB the \
         measured runs lost frames; amber is within 1 dB of that. A fading \
         channel needs more than the AWGN figure.\n\n{about}\n\nEb/N0 is the \
         symbol SNR (the last 1.5 s) less 10·log10 of the information bits a \
         symbol, pilots counted as air time. CSK measures the main path only, \
         so with multipath it reads low."
    ));

    let (rect, _) = ui.allocate_exact_size(vec2(width, 16.0), Sense::hover());
    let bar = Rect::from_min_max(rect.min + vec2(0.0, 3.0), rect.max - vec2(0.0, 3.0));
    let x_of = |db: f32| {
        bar.left() + bar.width() * ((db - MARGIN_LO) / (MARGIN_HI - MARGIN_LO)).clamp(0.0, 1.0)
    };
    let p = ui.painter();
    p.rect_filled(bar, 2.0, ui.visuals().extreme_bg_color);
    if let Some(e) = m.ebn0_db {
        p.rect_filled(
            Rect::from_min_max(bar.min, pos2(x_of(e), bar.max.y)),
            2.0,
            colour,
        );
    }
    let x = x_of(m.need_db);
    p.rect_filled(
        Rect::from_min_max(pos2(x - 1.0, rect.top()), pos2(x + 1.0, rect.bottom())),
        0.0,
        ui.visuals().strong_text_color(),
    );

    ui.horizontal(|ui| {
        ui.set_width(width);
        let left = match (m.ebn0_db, m.snr_db) {
            (Some(e), Some(s)) => format!("Eb/N0 {e:.1} dB · in-band SNR {s:+.1} dB"),
            _ if m.between => "no reading between segments".into(),
            _ => "Eb/N0 not measured yet".into(),
        };
        ui.label(RichText::new(left).size(12.0).weak());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(need).size(12.0).weak());
        });
    });
}

/// A scatter of complex points with axes, a unit circle and the ideal points
/// as crosses; points fade with age, the newest marked. The reading is the
/// mean error against the nearest ideal point as an Es/N0.
pub fn constellation(
    ui: &mut Ui,
    title: &str,
    pts: &[Iq],
    ideal: &[(f32, f32)],
    size: Vec2,
    color: Color32,
) {
    const FULL: f32 = 1.8;
    let reading = if pts.len() < 8 {
        String::new()
    } else {
        let err: f32 = pts
            .iter()
            .map(|z| {
                ideal
                    .iter()
                    .map(|&(x, y)| (z.re - x).powi(2) + (z.im - y).powi(2))
                    .fold(f32::MAX, f32::min)
            })
            .sum();
        format!(
            "{:.1} dB",
            10.0 * (pts.len() as f32 / err.max(1e-9)).log10()
        )
    };
    let (painter, area) = panel(ui, size, title, &reading);
    let r = area.width().min(area.height()) / 2.0;
    let o = area.center();
    let k = r / FULL;
    let at = |re: f32, im: f32| {
        pos2(
            o.x + re.clamp(-FULL, FULL) * k,
            o.y - im.clamp(-FULL, FULL) * k,
        )
    };
    painter.line_segment([at(-FULL, 0.0), at(FULL, 0.0)], (1.0, col::AXIS));
    painter.line_segment([at(0.0, -FULL), at(0.0, FULL)], (1.0, col::AXIS));
    painter.circle_stroke(o, k, (1.0, col::GRID));
    for &(x, y) in ideal {
        let p = at(x, y);
        let s = 4.0;
        painter.line_segment([p - vec2(s, s), p + vec2(s, s)], (1.0, col::REF));
        painter.line_segment([p - vec2(s, -s), p + vec2(s, -s)], (1.0, col::REF));
    }
    let n = pts.len();
    for (i, z) in pts.iter().enumerate() {
        let age = (i + 1) as f32 / n as f32;
        let newest = i + 1 == n;
        painter.circle_filled(
            at(z.re, z.im),
            if newest { 2.8 } else { 1.6 },
            if newest {
                Color32::WHITE
            } else {
                color.gamma_multiply(0.12 + 0.88 * age * age)
            },
        );
    }
}

/// The QPSK points of a unit-amplitude symbol.
pub const QPSK: [(f32, f32); 4] = [
    (0.707, 0.707),
    (-0.707, 0.707),
    (-0.707, -0.707),
    (0.707, -0.707),
];
/// The four phases of a QPSK symbol times the conjugate of the last.
pub const QUADRANTS: [(f32, f32); 4] = [(1.0, 0.0), (0.0, 1.0), (-1.0, 0.0), (0.0, -1.0)];

/// The early, prompt and late correlators on the ideal correlation triangle;
/// the reading is the timing discriminator (E − L)/(E + L).
pub fn epl(ui: &mut Ui, e: [f32; 3], size: Vec2) {
    const SPAN: f32 = 1.25;
    let disc = (e[0] - e[2]) / (e[0] + e[2]).max(1e-6);
    let reading = if e[1] > 0.0 {
        format!("E−L {disc:+.2}")
    } else {
        String::new()
    };
    let (painter, area) = panel(ui, size, "Timing", &reading);
    let plot = Rect::from_min_max(area.min, area.max - vec2(0.0, 14.0));
    let x = |t: f32| plot.center().x + t / SPAN * plot.width() / 2.0;
    let y = |v: f32| plot.bottom() - v.clamp(0.0, 1.2) / 1.2 * plot.height();
    painter.line_segment(
        [
            pos2(plot.left(), plot.bottom()),
            pos2(plot.right(), plot.bottom()),
        ],
        (1.0, col::AXIS),
    );
    // The ideal triangle.
    let tri = [(-1.0, 0.0), (0.0, 1.0), (1.0, 0.0)];
    for w in tri.windows(2) {
        painter.line_segment(
            [pos2(x(w[0].0), y(w[0].1)), pos2(x(w[1].0), y(w[1].1))],
            (1.0, col::GRID),
        );
    }
    for (t, v, c, label) in [
        (-0.25, e[0], col::MARK, "E"),
        (0.0, e[1], col::LOCK, "P"),
        (0.25, e[2], col::MARK, "L"),
    ] {
        painter.line_segment([pos2(x(t), y(0.0)), pos2(x(t), y(v))], Stroke::new(2.5, c));
        painter.circle_filled(pos2(x(t), y(v)), 3.0, c);
        small(
            &painter,
            pos2(x(t), plot.bottom() + 2.0),
            Align2::CENTER_TOP,
            label,
            col::REF,
        );
    }
    small(
        &painter,
        pos2(plot.left(), plot.bottom() + 2.0),
        Align2::LEFT_TOP,
        "−1 chip",
        col::AXIS,
    );
    small(
        &painter,
        pos2(plot.right(), plot.bottom() + 2.0),
        Align2::RIGHT_TOP,
        "+1",
        col::AXIS,
    );
}

/// How a strip chart is drawn: how many points fit across, its colour and the
/// smallest span the vertical scale may shrink to.
#[derive(Clone, Copy)]
pub struct Strip {
    pub cap: usize,
    pub color: Color32,
    pub min_span: f32,
}

/// A strip chart of a quantity over time: auto-scaled, the latest value as the
/// reading.
pub fn strip(ui: &mut Ui, title: &str, reading: &str, v: &[f32], style: Strip, size: Vec2) {
    let Strip {
        cap,
        color,
        min_span,
    } = style;
    let (painter, area) = panel(ui, size, title, reading);
    if v.len() < 2 {
        return;
    }
    let (mut lo, mut hi) = v
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    if hi - lo < min_span {
        let mid = 0.5 * (hi + lo);
        lo = mid - 0.5 * min_span;
        hi = mid + 0.5 * min_span;
    }
    let step = area.width() / (cap.max(2) - 1) as f32;
    let x0 = area.right() - (v.len() - 1) as f32 * step;
    let pts: Vec<Pos2> = v
        .iter()
        .enumerate()
        .map(|(i, &y)| {
            pos2(
                x0 + i as f32 * step,
                area.bottom() - (y - lo) / (hi - lo) * area.height(),
            )
        })
        .collect();
    for w in pts.windows(2) {
        painter.line_segment([w[0], w[1]], Stroke::new(1.3, color));
    }
    small(
        &painter,
        area.left_bottom(),
        Align2::LEFT_BOTTOM,
        &format!("{lo:.0}"),
        col::AXIS,
    );
    small(
        &painter,
        area.left_top(),
        Align2::LEFT_TOP,
        &format!("{hi:.0}"),
        col::AXIS,
    );
}

/// CSK: the correlator bank over the last symbols, one row each (newest at the
/// top), brightness the square root of the power against code number. The
/// decided codes show as a bright trace.
pub fn bank(ui: &mut Ui, rows: &[Vec<f32>], size: Vec2) {
    let reading = rows
        .last()
        .map_or(String::new(), |r| format!("{} codes", r.len()));
    let (painter, area) = panel(ui, size, "Walsh bank", &reading);
    let n = rows.len();
    if n == 0 {
        return;
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let ch = area.height() / super_rows() as f32;
    let cw = area.width() / cols as f32;
    for (j, row) in rows.iter().rev().enumerate() {
        let top = area.top() + j as f32 * ch;
        for (i, &v) in row.iter().enumerate() {
            let t = v.max(0.0).sqrt();
            if t < 0.05 {
                continue;
            }
            painter.rect_filled(
                Rect::from_min_size(
                    pos2(area.left() + i as f32 * cw, top),
                    vec2(cw.max(1.0), ch.max(1.0)),
                ),
                0.0,
                palette(t),
            );
        }
    }
}

fn super_rows() -> usize {
    decdsss_phy::rx::payload::LIVE_ROWS
}

/// CSK: the newest correlator row as bars, the decided code marked.
pub fn bank_bars(ui: &mut Ui, row: Option<&Vec<f32>>, size: Vec2) {
    let Some(row) = row else {
        let _ = panel(ui, size, "Last symbol", "");
        return;
    };
    let (best, _) = row.iter().enumerate().fold(
        (0usize, 0.0f32),
        |a, (i, &v)| if v > a.1 { (i, v) } else { a },
    );
    let second = row
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != best)
        .map(|(_, &v)| v)
        .fold(0.0f32, f32::max);
    let reading = format!(
        "code {best} +{:.0} dB",
        10.0 * (1.0 / second.max(1e-6)).log10()
    );
    let (painter, area) = panel(ui, size, "Last symbol", &reading);
    let cw = area.width() / row.len() as f32;
    for (i, &v) in row.iter().enumerate() {
        let h = v.max(0.0).sqrt() * area.height();
        let c = if i == best {
            col::LOCK
        } else {
            col::TRACE.gamma_multiply(0.7)
        };
        painter.rect_filled(
            Rect::from_min_max(
                pos2(area.left() + i as f32 * cw, area.bottom() - h),
                pos2(area.left() + (i as f32 + 1.0) * cw, area.bottom()),
            ),
            0.0,
            c,
        );
    }
}

/// The delay profile (rake): power against lag in chips, the fingers marked.
pub fn rake(ui: &mut Ui, profile: &[f32], fingers: &[Finger], size: Vec2) {
    let reading = format!(
        "{} finger{}",
        fingers.len(),
        if fingers.len() == 1 { "" } else { "s" }
    );
    let (painter, area) = panel(ui, size, "Delay profile", &reading);
    let plot = Rect::from_min_max(area.min, area.max - vec2(0.0, 12.0));
    let lo = DelayProfile::LAG0 as f32;
    let hi = DelayProfile::lag(DelayProfile::LAGS - 1) as f32;
    let x = |lag: f32| plot.left() + (lag - lo) / (hi - lo) * plot.width();
    // Chip ticks (every 2 chips, every 4 when narrow) and the reference at 0.
    let every = if plot.width() > 300.0 { 2 } else { 4 };
    for c in (lo.ceil() as i32..=hi.floor() as i32).filter(|c| c % every == 0) {
        let xx = x(c as f32);
        painter.line_segment(
            [pos2(xx, plot.bottom()), pos2(xx, plot.bottom() + 3.0)],
            (1.0, col::AXIS),
        );
        small(
            &painter,
            pos2(xx, plot.bottom() + 3.0),
            Align2::CENTER_TOP,
            &c.to_string(),
            col::AXIS,
        );
    }
    painter.line_segment(
        [pos2(x(0.0), plot.top()), pos2(x(0.0), plot.bottom())],
        (1.0, col::GRID),
    );
    painter.line_segment(
        [
            pos2(plot.left(), plot.bottom()),
            pos2(plot.right(), plot.bottom()),
        ],
        (1.0, col::AXIS),
    );
    if profile.len() >= 2 {
        let mx = profile.iter().copied().fold(1e-30f32, f32::max);
        let pts: Vec<Pos2> = profile
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                pos2(
                    x(DelayProfile::lag(i) as f32),
                    plot.bottom() - (p / mx).max(0.0) * plot.height() * 0.95,
                )
            })
            .collect();
        for w in pts.windows(2) {
            painter.line_segment([w[0], w[1]], Stroke::new(1.3, col::TRACE));
        }
    }
    for f in fingers {
        let xx = x(f.lag as f32);
        painter.line_segment(
            [pos2(xx, plot.top()), pos2(xx, plot.bottom())],
            Stroke::new(1.5, col::LOCK),
        );
        small(
            &painter,
            pos2(xx + 3.0, plot.top() + 1.0),
            Align2::LEFT_TOP,
            &format!("{:+.1}", f.lag),
            col::LOCK,
        );
    }
}

/// The chips of the last symbol: as received (the real part as a trace, the
/// carrier off and the scrambler still on) above, after the scrambler is taken
/// off as a scatter, below.
pub fn chip_scope(ui: &mut Ui, raw: &[Iq], despread: &[Iq], size: Vec2) {
    let reading = format!("{} chips", despread.len().max(raw.len()));
    let (painter, area) = panel(ui, size, "Chip scope", &reading);
    let split = area.top() + area.height() * 0.42;
    let top = Rect::from_min_max(area.min, pos2(area.right(), split - 3.0));
    let bot = Rect::from_min_max(pos2(area.left(), split + 3.0), area.max);
    // Raw chips: real part.
    if raw.len() >= 2 {
        let mx = raw.iter().map(|z| z.re.abs()).fold(1e-9f32, f32::max);
        painter.line_segment(
            [
                pos2(top.left(), top.center().y),
                pos2(top.right(), top.center().y),
            ],
            (1.0, col::GRID),
        );
        let n = raw.len();
        let pts: Vec<Pos2> = raw
            .iter()
            .enumerate()
            .map(|(i, z)| {
                pos2(
                    top.left() + i as f32 / (n - 1) as f32 * top.width(),
                    top.center().y - z.re / mx * top.height() * 0.48,
                )
            })
            .collect();
        for w in pts.windows(2) {
            painter.line_segment([w[0], w[1]], Stroke::new(1.0, col::TRACE));
        }
    }
    // Despread chips: scatter, normalised to their mean amplitude.
    if !despread.is_empty() {
        let mean = despread.iter().map(|z| z.norm()).sum::<f32>() / despread.len() as f32;
        let k = bot.height().min(bot.width()) / 2.0 / (2.2 * mean.max(1e-9));
        let c = bot.center();
        painter.line_segment(
            [pos2(bot.left(), c.y), pos2(bot.right(), c.y)],
            (1.0, col::GRID),
        );
        painter.line_segment(
            [pos2(c.x, bot.top()), pos2(c.x, bot.bottom())],
            (1.0, col::GRID),
        );
        for z in despread {
            let p = pos2(c.x + z.re * k, c.y - z.im * k);
            if bot.contains(p) {
                painter.circle_filled(p, 1.4, col::LOCK.gamma_multiply(0.7));
            }
        }
    }
}

/// The soft bits of the last codeword: one column a bit (decimated to the
/// width), height the confidence, colour the sign.
pub fn soft_bits(ui: &mut Ui, llrs: &[f32], size: Vec2) {
    let mean = if llrs.is_empty() {
        0.0
    } else {
        llrs.iter().map(|v| v.abs()).sum::<f32>() / llrs.len() as f32
    };
    let reading = format!("{} bits, mean |LLR| {:.1}", llrs.len(), mean);
    let (painter, area) = panel(ui, size, "Soft bits (last codeword)", &reading);
    if llrs.is_empty() {
        return;
    }
    let mx = llrs.iter().map(|v| v.abs()).fold(1e-9f32, f32::max);
    let cols = area.width().floor().max(1.0) as usize;
    let mid = area.center().y;
    for c in 0..cols {
        let a = c * llrs.len() / cols;
        let b = ((c + 1) * llrs.len() / cols).max(a + 1).min(llrs.len());
        let v = llrs[a..b].iter().sum::<f32>() / (b - a) as f32;
        let h = (v.abs() / mx).sqrt() * area.height() * 0.5;
        let x = area.left() + c as f32;
        let color = if v >= 0.0 { col::TRACE } else { col::MARK };
        painter.line_segment([pos2(x, mid - h * v.signum()), pos2(x, mid)], (1.0, color));
    }
    painter.line_segment(
        [pos2(area.left(), mid), pos2(area.right(), mid)],
        (1.0, col::GRID),
    );
}

/// Which data frames of a transmission are in as themselves, and how many
/// distinct frames in all (data or coded) of the `map.len()` needed.
pub fn frame_map(ui: &mut Ui, map: &[bool], have: usize, size: Vec2) {
    let coded = have.saturating_sub(map.iter().filter(|&&b| b).count());
    let reading = if coded > 0 {
        format!("{have} / {} needed (+{coded} coded)", map.len())
    } else {
        format!("{have} / {} needed", map.len())
    };
    let (painter, area) = panel(ui, size, "Frames", &reading);
    if map.is_empty() {
        return;
    }
    let w = area.width() / map.len() as f32;
    for (i, &ok) in map.iter().enumerate() {
        painter.rect_filled(
            Rect::from_min_max(
                pos2(area.left() + i as f32 * w + 0.5, area.top()),
                pos2(area.left() + (i as f32 + 1.0) * w - 0.5, area.bottom()),
            ),
            1.0,
            if ok { col::LOCK } else { col::GRID },
        );
    }
}

/// The preamble: per period, the correlation power with sequence A (blue) and
/// with the end marker B (amber).
pub fn preamble_bars(ui: &mut Ui, periods: &[(f32, f32)], size: Vec2) {
    let (painter, area) = panel(
        ui,
        size,
        "Preamble: A … A B B",
        &format!("{} periods", periods.len()),
    );
    if periods.is_empty() {
        return;
    }
    let mx = periods
        .iter()
        .map(|p| p.0.max(p.1))
        .fold(1e-30f32, f32::max);
    let w = area.width() / periods.len() as f32;
    for (i, &(a, b)) in periods.iter().enumerate() {
        let x = area.left() + i as f32 * w;
        for (k, (v, c)) in [(a, col::TRACE), (b, col::MARK)].into_iter().enumerate() {
            let h = (v / mx).sqrt() * area.height();
            let half = w * 0.5;
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(x + k as f32 * half + 0.5, area.bottom() - h),
                    pos2(x + (k as f32 + 1.0) * half - 0.5, area.bottom()),
                ),
                0.0,
                c,
            );
        }
    }
}

/// A texture the acquisition heatmap is drawn into.
#[derive(Default)]
pub struct HeatTexture {
    handle: Option<TextureHandle>,
}

impl HeatTexture {
    /// Draws the search surface: frequency up, code phase across, brightness
    /// the summed power against the noise (1 = noise, ≥ `hot` = white).
    pub fn show(&mut self, ui: &mut Ui, ctx: &Context, s: &Surface, hot: f32, size: Vec2) {
        let reading = if s.data.is_empty() {
            String::new()
        } else {
            let peak = s.data.iter().copied().fold(0.0f32, f32::max);
            let f = s.freq0_hz + s.peak_row as f64 * s.freq_step_hz;
            format!("peak {peak:.1}× noise at {f:+.1} Hz")
        };
        let (painter, area) = panel(ui, size, "Search: frequency × code phase", &reading);
        if s.rows == 0 || s.cols == 0 {
            return;
        }
        let pixels: Vec<Color32> = (0..s.rows)
            .rev()
            .flat_map(|r| (0..s.cols).map(move |c| (r, c)))
            .map(|(r, c)| {
                let v = s.data[r * s.cols + c];
                palette(((v - 1.0) / (hot - 1.0)).clamp(0.0, 1.0).sqrt())
            })
            .collect();
        let img = ColorImage::new([s.cols, s.rows], pixels);
        let opts = TextureOptions::NEAREST;
        match &mut self.handle {
            Some(h) => h.set(img, opts),
            None => self.handle = Some(ctx.load_texture("search-heat", img, opts)),
        }
        if let Some(h) = &self.handle {
            painter.image(
                h.id(),
                area,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        // The peak.
        let px = area.left() + (s.peak_col as f32 + 0.5) / s.cols as f32 * area.width();
        let py = area.bottom() - (s.peak_row as f32 + 0.5) / s.rows as f32 * area.height();
        painter.circle_stroke(pos2(px, py), 6.0, Stroke::new(1.2, col::MARK));
        small(
            &painter,
            area.left_bottom(),
            Align2::LEFT_BOTTOM,
            &format!("{:+.0} Hz", s.freq0_hz),
            col::REF,
        );
        small(
            &painter,
            area.left_top(),
            Align2::LEFT_TOP,
            &format!("{:+.0} Hz", s.freq0_hz + s.rows as f64 * s.freq_step_hz),
            col::REF,
        );
    }
}
