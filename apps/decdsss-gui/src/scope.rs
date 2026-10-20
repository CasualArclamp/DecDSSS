// made by CasualArclamp
//! The instrument panels: dark faces, a hairline frame, the title at the top
//! left and a reading at the top right, one muted hue per quantity. The look
//! is DecSAT's DVB-CID card (`apps/decsat-gui/src/side_panel.rs`).

use decdsss_phy::Iq;
use decdsss_phy::rx::acq::Surface;
use decdsss_phy::rx::header::{Finger, HeaderView};
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
    /// Its postamble was heard: there is no next.
    pub ended: bool,
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
            _ if m.ended => "no reading: the transmission has ended".into(),
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

/// The main finger's channel as the pilots measure it, drawn as a path through the
/// I/Q plane (the phasor trace), scaled so the circle is its RMS: a fade is the path
/// swinging through the middle, a carrier error a steady turn. Older points fade.
/// The reading is the newest point's power against the RMS.
pub fn channel_trace(ui: &mut Ui, trace: &[Iq], dt: f64, size: Vec2) {
    const FULL: f32 = 2.2;
    let rms = (trace.iter().map(|z| z.norm_sqr()).sum::<f32>() / trace.len().max(1) as f32).sqrt();
    let reading = match trace.last() {
        Some(z) if rms > 0.0 && trace.len() >= 4 => format!(
            "{:+.1} dB  ·  {:.0} s",
            10.0 * (z.norm_sqr() / (rms * rms)).max(1e-6).log10(),
            trace.len() as f64 * dt
        ),
        _ => String::new(),
    };
    let (painter, area) = panel(ui, size, "Channel (pilots)", &reading);
    if rms <= 0.0 || trace.len() < 2 {
        return;
    }
    let r = area.width().min(area.height()) / 2.0;
    let o = area.center();
    let k = r / FULL;
    let at = |z: &Iq| {
        let (x, y) = (z.re / rms, z.im / rms);
        pos2(
            o.x + x.clamp(-FULL, FULL) * k,
            o.y - y.clamp(-FULL, FULL) * k,
        )
    };
    painter.line_segment([pos2(o.x - r, o.y), pos2(o.x + r, o.y)], (1.0, col::AXIS));
    painter.line_segment([pos2(o.x, o.y - r), pos2(o.x, o.y + r)], (1.0, col::AXIS));
    painter.circle_stroke(o, k, (1.0, col::GRID));
    let n = trace.len();
    for (i, w) in trace.windows(2).enumerate() {
        let age = (i + 2) as f32 / n as f32;
        painter.line_segment(
            [at(&w[0]), at(&w[1])],
            (1.4, col::TRACE.gamma_multiply(0.1 + 0.9 * age * age)),
        );
    }
    painter.circle_filled(at(&trace[n - 1]), 3.0, Color32::WHITE);
}

/// The Doppler spectrum of the channel the pilots measure: the power spectrum of
/// the newest points of the trace (a power of two of them, Hann window), in dB from
/// its peak, over the trace's own Nyquist range (±1/(2·dt): for DS-PSK the pilots',
/// so a channel faster than that folds back in, which is what the plot is for). The
/// estimate's band (`band_hz`, half its noise bandwidth) is marked. The reading is
/// the spread 2σ of what stands above the floor, Watterson's measure.
pub fn doppler(ui: &mut Ui, trace: &[Iq], dt: f64, band_hz: f32, size: Vec2) {
    // The largest power of two not over the points there are, at most 256.
    let have = trace.len().min(256);
    let n = if have == 0 {
        0
    } else {
        1usize << (usize::BITS - 1 - have.leading_zeros())
    };
    if n < 16 || dt <= 0.0 {
        let _ = panel(ui, size, "Doppler", "");
        return;
    }
    let x = &trace[trace.len() - n..];
    let hann = |k: usize| 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / n as f32).cos();
    // A plain DFT: n is at most 256.
    let mut p = vec![0.0f32; n];
    for (m, pm) in p.iter_mut().enumerate() {
        let mut acc = Iq::new(0.0, 0.0);
        for (k, z) in x.iter().enumerate() {
            let a = -std::f32::consts::TAU * (m * k % n) as f32 / n as f32;
            acc += z * Iq::new(a.cos(), a.sin()) * hann(k);
        }
        *pm = acc.norm_sqr();
    }
    // Centred: bin m is frequency (m − n/2)/(n·dt).
    p.rotate_right(n / 2);
    let df = 1.0 / (n as f64 * dt);
    let freq = |m: usize| (m as f64 - (n / 2) as f64) * df;
    let peak = p.iter().copied().fold(1e-30f32, f32::max);
    let reading = match doppler_spread(x, dt) {
        Some(s) => format!("spread ≈ {s:.2} Hz"),
        None => String::new(),
    };
    let (painter, area) = panel(ui, size, "Doppler", &reading);
    let nyq = 0.5 / dt;
    const DB: f32 = 30.0;
    let fx = |f: f64| area.left() + ((f + nyq) / (2.0 * nyq)) as f32 * area.width();
    let fy = |v: f32| {
        let d = (10.0 * (v / peak).max(1e-6).log10()).max(-DB);
        area.top() + (-d / DB) * (area.height() - 12.0)
    };
    painter.line_segment(
        [
            pos2(fx(0.0), area.top()),
            pos2(fx(0.0), area.bottom() - 12.0),
        ],
        (1.0, col::AXIS),
    );
    if band_hz > 0.0 {
        for s in [-1.0, 1.0] {
            let x = fx(s * f64::from(band_hz).min(nyq));
            painter.line_segment(
                [pos2(x, area.top()), pos2(x, area.bottom() - 12.0)],
                (1.0, col::MARK.gamma_multiply(0.7)),
            );
        }
    }
    let pts: Vec<Pos2> = (0..n).map(|m| pos2(fx(freq(m)), fy(p[m]))).collect();
    painter.add(eframe::egui::Shape::line(pts, Stroke::new(1.4, col::TRACE)));
    small(
        &painter,
        pos2(area.left(), area.bottom()),
        Align2::LEFT_BOTTOM,
        &format!("−{nyq:.1} Hz"),
        col::REF,
    );
    small(
        &painter,
        pos2(area.right(), area.bottom()),
        Align2::RIGHT_BOTTOM,
        &format!("+{nyq:.1} Hz"),
        col::REF,
    );
    if band_hz > 0.0 {
        small(
            &painter,
            pos2(area.center().x, area.bottom()),
            Align2::CENTER_BOTTOM,
            &format!("estimate ±{band_hz:.2} Hz"),
            col::MARK,
        );
    }
}

/// The Doppler spread (Watterson's 2σ, Hz) of a channel sampled every `dt` seconds,
/// from the trace's autocorrelation at one and two samples: for a Gaussian Doppler
/// spectrum of standard deviation σ, |R(τ)| = P·exp(−2π²σ²τ²), so
/// |R(2dt)|/|R(dt)| = exp(−6π²σ²dt²) and σ = √(−ln r / 6π²) / dt (derived). White
/// noise in the trace adds to R(0) only, and a carrier offset turns R without
/// changing its size, so neither biases it. `None` where the ratio says nothing (a
/// channel too slow for the trace's length to show, or too fast for its rate).
pub fn doppler_spread(x: &[Iq], dt: f64) -> Option<f64> {
    let r = |k: usize| -> f64 {
        let n = x.len().saturating_sub(k);
        if n == 0 {
            return 0.0;
        }
        let s: Iq = (0..n).map(|i| x[i + k] * x[i].conj()).sum();
        f64::from(s.norm()) / n as f64
    };
    let (r1, r2) = (r(1), r(2));
    if r1 <= 0.0 || r2 <= 0.0 || r2 > r1 || r2 < 0.05 * r1 {
        return None;
    }
    let sigma = (-(r2 / r1).ln() / (6.0 * std::f64::consts::PI.powi(2))).sqrt() / dt;
    Some(2.0 * sigma)
}

/// `dsc`: the main finger's despread power on the pilot's Walsh code, the data's,
/// and the mean of the others (noise and multipath leakage), each as a bar in dB
/// over the others. On one clean path the others hold noise only.
pub fn code_powers(ui: &mut Ui, p: [f32; 3], size: Vec2) {
    let noise = p[2].max(1e-12);
    let db = |v: f32| 10.0 * (v / noise).max(1e-6).log10();
    let reading = if p[2] > 0.0 {
        format!("pilot {:+.1} dB · data {:+.1} dB", db(p[0]), db(p[1]))
    } else {
        String::new()
    };
    let (painter, area) = panel(ui, size, "Walsh codes", &reading);
    let rows = [
        ("pilot (code 0)", db(p[0]), col::LOCK),
        ("data (code 1)", db(p[1]), col::TRACE),
        ("other codes", 0.0, col::REF),
    ];
    let top = rows.iter().map(|r| r.1).fold(10.0f32, f32::max);
    let h = area.height() / 3.0;
    let label_w = 86.0;
    for (i, (name, v, c)) in rows.iter().enumerate() {
        let y = area.top() + i as f32 * h;
        small(
            &painter,
            pos2(area.left(), y + h * 0.5),
            Align2::LEFT_CENTER,
            name,
            col::REF,
        );
        let w = (area.width() - label_w) * (v.max(0.0) / top).max(0.01);
        painter.rect_filled(
            Rect::from_min_max(
                pos2(area.left() + label_w, y + h * 0.25),
                pos2(area.left() + label_w + w, y + h * 0.75),
            ),
            1.0,
            *c,
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

/// A symbol margin this far over the runner-up counts as a clear decision on the
/// display (green rather than amber). A display choice, not a threshold the
/// decoder uses: noise alone gives about 1 dB (see `HeaderView::margin_db`).
const CLEAR_MARGIN_DB: f32 = 3.0;

/// The header strip says "a longer header?" once this share of the points sit
/// at −1 while the median margin is clear (noise alone also puts half the
/// points at −1, but with a margin near 1 dB). Also display choices: in
/// `the_watch_tells_the_alphabet_sent_by_its_phase` (AWGN at 0 dB, moderate
/// fading at +2 dB) the alphabet sent gave 0–3 % and a shorter one 34–41 %,
/// with median margins of 7.5 dB and more.
const LONGER_FLIPS: f32 = 0.2;
const LONGER_MARGIN_DB: f32 = 4.0;
const LONGER_AFTER: usize = 16;

/// The header as it comes in: one bar a symbol across the whole header in the
/// alphabet being tried, its height the decision margin (decided code over the
/// runner-up), the symbols still to come as empty slots, and the level noise
/// alone would give as a line. Green is a clear decision in phase with the
/// symbol before, red a clear one turned round from it (at −1 in the header
/// constellation, a sign of a longer header), amber an unclear one. Replaces a
/// progress bar: it fills at the same rate and shows whether what fills it is a
/// header in this alphabet.
pub fn header_symbols(ui: &mut Ui, h: &HeaderView, since_s: f64, needs_s: f64, size: Vec2) {
    let mut title = format!(
        "Header: CSK{}, symbol {} of {}",
        h.alphabet, h.done, h.symbols
    );
    if !h.failed.is_empty() {
        let tried: Vec<String> = h.failed.iter().map(|l| format!("CSK{l}")).collect();
        title += &format!("  ({} did not decode)", tried.join(", "));
    }
    let mut reading = format!("{:.1} of {:.1} s", since_s.min(needs_s), needs_s);
    if !h.margin_db.is_empty() {
        let mut m = h.margin_db.clone();
        m.sort_by(f32::total_cmp);
        let median = m[m.len() / 2];
        let flipped = h.flipped();
        reading += &format!("  ·  margin {median:.1} dB  ·  {flipped} at −1");
        if h.done >= LONGER_AFTER
            && median >= LONGER_MARGIN_DB
            && flipped as f32 >= LONGER_FLIPS * h.points.len() as f32
        {
            reading += ": a longer header?";
        }
    }
    let (painter, area) = panel(ui, size, &title, &reading);
    if h.symbols == 0 {
        return;
    }
    // The bars on the left, their scale on the right.
    let plot = Rect::from_min_max(area.min, area.max - vec2(44.0, 0.0));
    // Noise alone: the largest of L equal exponentials over the second largest,
    // H_L / (H_L − 1) in the means (one finger).
    let hl: f32 = (1..=h.alphabet.max(2)).map(|k| 1.0 / k as f32).sum();
    let noise_db = 10.0 * (hl / (hl - 1.0)).log10();
    let top_db = h.margin_db.iter().copied().fold(12.0f32, f32::max);
    let y = |db: f32| plot.bottom() - (db.max(0.0) / top_db).min(1.0) * plot.height();
    let w = plot.width() / h.symbols as f32;
    for i in 0..h.symbols {
        let x0 = plot.left() + i as f32 * w;
        let (l, r) = (x0 + 0.5, (x0 + w - 0.5).max(x0 + 1.0));
        let (top, c) = match h.margin_db.get(i) {
            Some(&db) => {
                // The points start at the second symbol (each is symbol × previous).
                let flipped = i
                    .checked_sub(1)
                    .and_then(|j| h.points.get(j))
                    .is_some_and(|z| z.re < 0.0);
                let c = match (db >= CLEAR_MARGIN_DB, flipped) {
                    (false, _) => col::MARK,
                    (true, false) => col::LOCK,
                    (true, true) => col::BAD,
                };
                (y(db), c)
            }
            // Still to come: an empty slot.
            None => (plot.bottom() - 3.0, col::GRID),
        };
        painter.rect_filled(
            Rect::from_min_max(pos2(l, top), pos2(r, plot.bottom())),
            0.0,
            c,
        );
    }
    painter.line_segment(
        [
            pos2(plot.left(), y(noise_db)),
            pos2(plot.right(), y(noise_db)),
        ],
        (1.0, col::REF),
    );
    small(
        &painter,
        pos2(area.right(), y(noise_db)),
        Align2::RIGHT_CENTER,
        &format!("noise {noise_db:.1}"),
        col::REF,
    );
    small(
        &painter,
        pos2(area.right(), plot.top()),
        Align2::RIGHT_TOP,
        &format!("{top_db:.0} dB"),
        col::AXIS,
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_dsp::rng::Rng;

    #[test]
    fn the_doppler_reading_finds_the_watterson_spread_through_noise() {
        // A Gaussian tap of 0.5 Hz spread (moderate), sampled 15 times a second as a
        // DS16 `dsc` chunk is on 2k4, with noise as strong as the tap: about 0.5 Hz.
        let mut rng = Rng::new(7);
        for spread in [0.5, 1.0] {
            let tap = decdsss_chan::tap_process(4000, 15.0, spread, &mut rng);
            let noisy: Vec<Iq> = tap.iter().map(|z| z + rng.cgauss()).collect();
            let s = doppler_spread(&noisy, 1.0 / 15.0).unwrap();
            assert!(
                (s - spread).abs() < 0.2 * spread,
                "{spread} Hz read as {s:.3}"
            );
        }
    }
}
