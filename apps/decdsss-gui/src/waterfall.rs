// made by CasualArclamp
//! The band view's waterfall as a ring of rows in one texture: a new spectrum row
//! arrives about 60 times a second and is written into the next row of the
//! texture (a one-row upload), and the picture is drawn from the ring with the
//! newest row at the top. Nothing is rebuilt per frame, so it scrolls at the
//! display's own rate.

use crate::scope::palette;
use eframe::egui::{self, Color32, ColorImage, Painter, Rect, TextureHandle, TextureOptions, pos2};

/// Rows kept: 15 s at 60 rows a second.
pub const HISTORY: usize = 900;

pub struct Waterfall {
    tex: TextureHandle,
    /// Bins a row.
    width: usize,
    /// The ring row the next spectrum row goes to.
    head: usize,
    /// A slowly falling record of the highest level seen, dB, so a clean cable
    /// (no noise to speak of) does not blow the colours out.
    peak: f32,
}

impl Waterfall {
    pub fn new(ctx: &egui::Context, width: usize) -> Waterfall {
        let blank = ColorImage::new([width, HISTORY], vec![Color32::BLACK; width * HISTORY]);
        Waterfall {
            tex: ctx.load_texture("waterfall", blank, TextureOptions::LINEAR),
            width,
            head: 0,
            peak: -120.0,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    /// Writes spectrum rows (dB per bin, with the noise floor at the time) into
    /// the ring.
    pub fn push(&mut self, rows: &[(f32, Vec<f32>)]) {
        for (floor, row) in rows {
            let hi_row = row.iter().copied().fold(-200.0f32, f32::max);
            self.peak = hi_row.max(self.peak - 0.02);
            // From just under the floor to 26 dB over it, or to the recent peaks
            // where those are higher.
            let lo = floor - 3.0;
            let hi = (floor + 26.0).max(self.peak + 3.0);
            let pixels: Vec<Color32> = row
                .iter()
                .take(self.width)
                .map(|&d| palette((d - lo) / (hi - lo)))
                .collect();
            if pixels.len() == self.width {
                let img = ColorImage::new([self.width, 1], pixels);
                self.tex
                    .set_partial([0, self.head], img, TextureOptions::LINEAR);
                self.head = (self.head + 1) % HISTORY;
            }
        }
    }

    /// Draws the ring into `plot`, newest at the top.
    pub fn paint(&self, painter: &Painter, plot: Rect) {
        let h = HISTORY as f32;
        let split = plot.height() * self.head as f32 / h;
        let a = self.head as f32 / h;
        // Rows 0..head are the newest, written upward in the texture from the top, so
        // they are drawn flipped (the last written first); the older rows, head..END,
        // continue below them.
        if self.head > 0 {
            painter.image(
                self.tex.id(),
                Rect::from_min_max(plot.min, pos2(plot.right(), plot.top() + split)),
                Rect::from_min_max(pos2(0.0, a), pos2(1.0, 0.0)),
                Color32::WHITE,
            );
        }
        if self.head < HISTORY {
            painter.image(
                self.tex.id(),
                Rect::from_min_max(pos2(plot.left(), plot.top() + split), plot.max),
                Rect::from_min_max(pos2(0.0, 1.0), pos2(1.0, a)),
                Color32::WHITE,
            );
        }
    }
}
