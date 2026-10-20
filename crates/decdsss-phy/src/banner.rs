// made by CasualArclamp
//! The waterfall ID: the callsign and mode written on the spectrum.
//!
//! Before the signal, a few seconds of audio whose spectrogram *is* the text, the way
//! fldigi's "waterfall ID" works: anyone watching the band sees `VK2ABC DS8 1/2`
//! drawn in the waterfall and knows who is on and in what mode before the preamble
//! has finished. It is not a data signal and nothing decodes it; it is the
//! signal's name card.
//!
//! **How it is drawn.** The text is a 5×7 dot-matrix bitmap. Each bitmap *column* is
//! a tone (frequency left to right as on every waterfall) and each bitmap *row* is a
//! slice of time of [`ROW_S`], in which the tones of that row's lit pixels sound
//! together. A waterfall scrolls with the newest line at the top (this program's does;
//! fldigi's and WSJT-X's do as far as is known, not checked here), so the bottom row
//! of the text goes out first and the top row last.
//!
//! **Where.** Inside the band the signal will occupy ([`Band::edges_hz`], less a
//! [`GUARD_HZ`] each side), so a banner in one slot of the 500 Hz profile does not
//! touch its neighbours. Tones are [`MAX_PITCH_HZ`] apart at most; when the text is
//! too wide for the band at [`MIN_PITCH_HZ`] (a callsign and a mode on one line in
//! 500 Hz) the parts go one to a line instead (and even then "DS8 1/2" is 41 columns
//! at 12 Hz in a 500 Hz slot: it reads, if fuzzily, in a 4096-point spectrum).
//!
//! **Level.** Every tone has the same amplitude, so every lit pixel is as bright as
//! the next, set so the busiest row has an RMS of `level`. A comb of tones has a
//! higher crest factor than the signal does, though (measured: 9.8-11.2 dB for the
//! banners the tests draw, 7.7-7.9 dB, peak 2.4-2.5 times the RMS, for 40 000 chips of
//! the signal at any profile), and a transmitter is set for the signal's peaks. So
//! the audio is scaled down until its peak is no more than [`PEAK_PER_RMS`] times
//! `level`, the signal's own, and never past [`MAX_PEAK`]. The tones keep a fixed
//! phase (Newman's, `π k²/K`, which keeps the crest factor of a comb of tones low:
//! the busiest row of "VK2ABC DS8 1/2" peaked 10.3 dB over its RMS with them and
//! 18.8 dB with every tone at phase zero). Switching a pixel on or off takes [`RAMP_S`] of a raised cosine, so
//! the edges do not click.

use decdsss_core::Band;
use std::f64::consts::{PI, TAU};

/// Seconds one row of the bitmap lasts. A waterfall that draws ten lines a second
/// gives a row two lines; this program's gives it twelve.
pub const ROW_S: f64 = 0.2;

/// Seconds a tone takes to switch on or off (a raised-cosine ramp).
pub const RAMP_S: f64 = 0.03;

/// Seconds of silence after the banner, before whatever follows.
pub const GAP_S: f64 = 0.5;

/// Tones are never closer than this, Hz: about the width of two bins of the 4096-point
/// spectrum this program draws at 48 kHz (11.7 Hz a bin), so neighbouring columns stay
/// apart. Text that would need less goes on two lines.
pub const MIN_PITCH_HZ: f64 = 16.0;

/// Tones are never further apart than this, Hz, so a short text stays a compact
/// picture in a wide band instead of being stretched across it.
pub const MAX_PITCH_HZ: f64 = 32.0;

/// How far inside the band's edges the outermost tones sit, Hz.
pub const GUARD_HZ: f64 = 10.0;

/// The audio's peak is kept to this many times `level`: about the signal's own peak
/// (2.4-2.5 times its RMS, measured over 40 000 chips at each profile).
pub const PEAK_PER_RMS: f32 = 2.5;

/// The peak the audio is never allowed past, whatever the level (full scale is 1).
pub const MAX_PEAK: f32 = 0.9;

/// Pixels across and down a character; one blank column separates characters.
const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;

/// A 5×7 character, top row first, the leftmost pixel the highest of the five bits.
/// Anything this table lacks is drawn as `?`.
fn glyph(c: char) -> [u8; GLYPH_H] {
    match c.to_ascii_uppercase() {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '/' => [
            0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '+' => [
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        ' ' => [0; GLYPH_H],
        _ => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100,
        ],
    }
}

/// Columns a line of `chars` characters takes: five a character and one between.
fn line_cols(chars: usize) -> usize {
    (chars * (GLYPH_W + 1)).saturating_sub(1)
}

/// The bitmap of the banner as it is read: rows top to bottom, each `cols` wide,
/// and the spacing of the tones that draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Bitmap {
    /// Lit pixels, top row first.
    pub rows: Vec<Vec<bool>>,
    /// Columns (tones) across.
    pub cols: usize,
    /// Hz between neighbouring columns.
    pub pitch_hz: f64,
    /// Audio frequency of the middle of the picture, Hz (the band's centre).
    pub centre_hz: f64,
}

impl Bitmap {
    /// Audio frequency of column `k`, Hz.
    pub fn freq_hz(&self, k: usize) -> f64 {
        self.centre_hz + (k as f64 - (self.cols as f64 - 1.0) / 2.0) * self.pitch_hz
    }

    /// The most pixels lit in one row.
    pub fn busiest_row(&self) -> usize {
        self.rows
            .iter()
            .map(|r| r.iter().filter(|&&p| p).count())
            .max()
            .unwrap_or(0)
    }
}

/// Hz between tones for a picture `cols` wide in `band`.
fn pitch_for(cols: usize, band: Band) -> f64 {
    let room = band.profile.bandwidth_hz() - 2.0 * GUARD_HZ;
    if cols <= 1 {
        MAX_PITCH_HZ
    } else {
        (room / (cols - 1) as f64).min(MAX_PITCH_HZ)
    }
}

/// The lines `parts` are written in: all on one, with spaces between, if the band
/// has room for that at [`MIN_PITCH_HZ`], else one part to a line.
pub fn lines(parts: &[&str], band: Band) -> Vec<String> {
    let one = parts.join(" ");
    if parts.len() < 2 || pitch_for(line_cols(one.chars().count()), band) >= MIN_PITCH_HZ {
        return vec![one];
    }
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// The banner for `parts` in `band` as a bitmap. Lines are centred on one another
/// and separated by a blank row.
pub fn bitmap(parts: &[&str], band: Band) -> Bitmap {
    let lines = lines(parts, band);
    let cols = lines
        .iter()
        .map(|l| line_cols(l.chars().count()))
        .max()
        .unwrap_or(0);
    let mut rows: Vec<Vec<bool>> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            rows.push(vec![false; cols]);
        }
        let chars: Vec<char> = line.chars().collect();
        let indent = (cols - line_cols(chars.len())) / 2;
        for r in 0..GLYPH_H {
            let mut row = vec![false; cols];
            for (n, &c) in chars.iter().enumerate() {
                let bits = glyph(c)[r];
                for x in 0..GLYPH_W {
                    row[indent + n * (GLYPH_W + 1) + x] = ((bits >> (GLYPH_W - 1 - x)) & 1) == 1;
                }
            }
            rows.push(row);
        }
    }
    Bitmap {
        rows,
        cols,
        pitch_hz: pitch_for(cols, band),
        centre_hz: band.centre_hz(),
    }
}

/// Seconds the banner for `parts` takes, the gap after it included.
pub fn seconds(parts: &[&str], band: Band) -> f64 {
    let map = bitmap(parts, band);
    if map.busiest_row() == 0 {
        0.0
    } else {
        map.rows.len() as f64 * ROW_S + RAMP_S + GAP_S
    }
}

/// The audio of the banner for `parts` (the callsign and the mode, say) in `band`
/// at `rate` Hz, then [`GAP_S`] of silence. `level` is the signal's RMS: the busiest
/// row has that RMS or, if its peak would pass the signal's, less. Empty if there
/// is nothing to write.
pub fn banner(parts: &[&str], band: Band, rate: f64, level: f32) -> Vec<f32> {
    let map = bitmap(parts, band);
    let busiest = map.busiest_row();
    if busiest == 0 {
        return Vec::new();
    }
    // Time order: the bottom row first (the newest line of a waterfall is the top one).
    let rows: Vec<&Vec<bool>> = map.rows.iter().rev().collect();
    let row_n = (ROW_S * rate).round() as usize;
    let half = ((RAMP_S * rate / 2.0).round() as usize).max(1);
    let ramp_n = 2 * half;
    let total = rows.len() * row_n + ramp_n;
    // Row `r` is samples `edge(r) .. edge(r + 1)`; the ramps straddle the edges.
    let edge = |r: usize| half + r * row_n;
    let mut out = vec![0.0f32; total];
    // Each tone's mean square is a²/2, `busiest` of them make the level.
    let amp = f64::from(level) * (2.0 / busiest as f64).sqrt();
    for k in 0..map.cols {
        let lit = |r: isize| -> f64 {
            // The silence before the first row and after the last counts as unlit.
            usize::try_from(r)
                .ok()
                .and_then(|r| rows.get(r))
                .map_or(0.0, |row| f64::from(u8::from(row[k])))
        };
        if (0..rows.len() as isize).all(|r| lit(r) == 0.0) {
            continue;
        }
        let w = TAU * map.freq_hz(k) / rate;
        let phase0 = PI * (k * k) as f64 / map.cols as f64;
        for j in 0..=rows.len() {
            let (before, after) = (lit(j as isize - 1), lit(j as isize));
            // The ramp across edge `j` ...
            if before != 0.0 || after != 0.0 {
                let from = edge(j) - half;
                run(&mut out[from..from + ramp_n], from, w, phase0, |n| {
                    let x = (n - from) as f64 / ramp_n as f64;
                    amp * (before + (after - before) * (0.5 - 0.5 * (PI * x).cos()))
                });
            }
            // ... and the flat part of the row after it.
            if j < rows.len() && after != 0.0 {
                let (from, to) = (edge(j) + half, edge(j + 1) - half);
                run(&mut out[from..to], from, w, phase0, |_| amp);
            }
        }
    }
    // The comb's crest factor is higher than the signal's: keep its peak to theirs.
    let limit = (PEAK_PER_RMS * level).min(MAX_PEAK);
    let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > limit {
        let k = limit / peak;
        out.iter_mut().for_each(|v| *v *= k);
    }
    out.extend(std::iter::repeat_n(0.0, (GAP_S * rate).round() as usize));
    out
}

/// Adds the tone `amp(n) · cos(phase0 + w n)` to `out`, whose first sample is
/// sample `from` of the whole. The tone is a phasor turned by `w` a sample (a
/// complex multiply, much cheaper than a cosine each); it starts exactly at `from`
/// each time, so skipping the rows where a tone is off costs it nothing. `f64`
/// keeps the phasor true over the few thousand samples of a run.
fn run(out: &mut [f32], from: usize, w: f64, phase0: f64, amp: impl Fn(usize) -> f64) {
    let (mut s, mut c) = (phase0 + w * from as f64).sin_cos();
    let (ws, wc) = w.sin_cos();
    for (i, v) in out.iter_mut().enumerate() {
        *v += (amp(from + i) * c) as f32;
        (s, c) = (s * wc + c * ws, c * wc - s * ws);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdsss_core::{Profile, Slot};

    const RATE: f64 = 48_000.0;

    fn wide() -> Band {
        Band::mid(Profile::W2400)
    }

    fn narrow() -> Band {
        Band::new(Profile::N500, Slot::High)
    }

    /// The picture as ASCII art, top row first.
    fn art(map: &Bitmap) -> Vec<String> {
        map.rows
            .iter()
            .map(|r| r.iter().map(|&p| if p { '#' } else { '.' }).collect())
            .collect()
    }

    #[test]
    fn a_character_is_drawn_as_its_glyph() {
        let map = bitmap(&["A"], wide());
        assert_eq!(
            art(&map),
            [
                ".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"
            ]
        );
        // Two characters are five columns each and one apart.
        let map = bitmap(&["10"], wide());
        assert_eq!(map.cols, 11);
        assert_eq!(map.rows[0], art_row("..#..", ".###."));
    }

    /// A row from two glyph rows with the blank column between.
    fn art_row(a: &str, b: &str) -> Vec<bool> {
        format!("{a}.{b}").chars().map(|c| c == '#').collect()
    }

    #[test]
    fn every_character_a_callsign_or_mode_can_have_has_a_glyph() {
        let unknown = glyph('\u{1}');
        for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/- +.".chars() {
            assert_ne!(glyph(c), unknown, "{c:?} is drawn as ?");
        }
        // Distinct characters look distinct (the zero has its slash).
        let all: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/-+.".chars().collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(glyph(*a), glyph(*b), "{a} and {b}");
            }
        }
        assert_eq!(glyph('k'), glyph('K'), "lower case is drawn as capitals");
    }

    #[test]
    fn it_goes_on_one_line_where_it_fits_and_two_where_it_does_not() {
        let parts = ["VK2ABC", "DS8 1/2"];
        assert_eq!(lines(&parts, wide()), ["VK2ABC DS8 1/2"]);
        assert_eq!(lines(&parts, Band::mid(Profile::W3000)), ["VK2ABC DS8 1/2"]);
        assert_eq!(lines(&parts, narrow()), ["VK2ABC", "DS8 1/2"]);
        // The widest the modem sends: nine characters of callsign and CSK256 3/4.
        let long = ["VK2ABC/QRP", "CSK256 3/4"];
        assert_eq!(lines(&long, wide()).len(), 1);
        assert_eq!(lines(&long, narrow()).len(), 2);
        // The tones stay in the band, at least the guard from its edges.
        for (parts, band) in [
            (&parts[..], wide()),
            (&parts[..], narrow()),
            (&long[..], wide()),
        ] {
            let map = bitmap(parts, band);
            let (lo, hi) = band.edges_hz();
            assert!(map.freq_hz(0) >= lo + GUARD_HZ - 1e-9, "{band:?}");
            assert!(
                map.freq_hz(map.cols - 1) <= hi - GUARD_HZ + 1e-9,
                "{band:?}"
            );
            assert!(map.pitch_hz <= MAX_PITCH_HZ + 1e-9);
        }
        // Two lines: seven rows, a blank one, seven rows.
        assert_eq!(bitmap(&parts, narrow()).rows.len(), 15);
        assert_eq!(bitmap(&parts, wide()).rows.len(), 7);
    }

    /// What a receiver of the audio sees: for each row of the picture, in the order
    /// it was sent, the amplitude of each column's tone in the middle of the row,
    /// found with a Hann-windowed single-bin DFT over the flat part of the row.
    fn read_back(audio: &[f32], map: &Bitmap) -> Vec<Vec<f64>> {
        let row_n = (ROW_S * RATE).round() as usize;
        let half = (RAMP_S * RATE / 2.0).round() as usize;
        let flat = row_n - 2 * half;
        let hann: Vec<f64> = (0..flat)
            .map(|i| 0.5 - 0.5 * (TAU * (i as f64 + 0.5) / flat as f64).cos())
            .collect();
        let norm: f64 = hann.iter().sum();
        (0..map.rows.len())
            .map(|r| {
                let start = half + r * row_n + half;
                (0..map.cols)
                    .map(|k| {
                        let w = TAU * map.freq_hz(k) / RATE;
                        let (mut re, mut im) = (0.0, 0.0);
                        for (i, h) in hann.iter().enumerate() {
                            let x = f64::from(audio[start + i]) * h;
                            let (s, c) = (w * (start + i) as f64).sin_cos();
                            re += x * c;
                            im += x * s;
                        }
                        2.0 * re.hypot(im) / norm
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_spectrogram_reads_back_as_the_text_newest_line_first() {
        for (parts, band) in [
            (&["VK2ABC", "DS8 1/2"][..], wide()),
            (&["VK2ABC", "DS8 1/2"][..], narrow()),
            (&["JA1XYZ/P", "CSK64 3/4"][..], Band::mid(Profile::W3000)),
        ] {
            let map = bitmap(parts, band);
            let audio = banner(parts, band, RATE, 0.2);
            let seen = read_back(&audio, &map);
            // Every lit pixel is as bright as the next, whatever the scaling made it.
            let mut lit: Vec<f64> = Vec::new();
            for (r, row) in seen.iter().enumerate() {
                for (k, &a) in row.iter().enumerate() {
                    if map.rows[map.rows.len() - 1 - r][k] {
                        lit.push(a);
                    }
                }
            }
            lit.sort_by(f64::total_cmp);
            let amp = lit[lit.len() / 2];
            assert!(amp > 0.01, "{band:?}: tones of {amp}");
            // Sent bottom row first, so the sent order is the picture upside down.
            for (r, row) in seen.iter().enumerate() {
                let want = &map.rows[map.rows.len() - 1 - r];
                for (k, &a) in row.iter().enumerate() {
                    if want[k] {
                        assert!(
                            (a - amp).abs() < 0.1 * amp,
                            "{band:?} row {r} col {k}: {a} vs {amp}"
                        );
                    } else {
                        assert!(a < 0.05 * amp, "{band:?} row {r} col {k}: {a} vs {amp}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_level_is_the_signals_at_most_and_the_peak_no_higher() {
        for (parts, band) in [
            (&["VK2ABC", "DS8 1/2"][..], wide()),
            (&["VK2ABC", "DS8 1/2"][..], narrow()),
            (&["VK2ABC/QRP", "CSK256 3/4"][..], Band::mid(Profile::W3000)),
        ] {
            let map = bitmap(parts, band);
            let audio = banner(parts, band, RATE, 0.2);
            let row_n = (ROW_S * RATE).round() as usize;
            let half = (RAMP_S * RATE / 2.0).round() as usize;
            // RMS over the flat part of each row (rows are sent bottom first).
            let rms: Vec<f64> = (0..map.rows.len())
                .map(|r| {
                    let s = &audio[half + r * row_n + half..half + (r + 1) * row_n - half];
                    (s.iter().map(|v| f64::from(v * v)).sum::<f64>() / s.len() as f64).sqrt()
                })
                .collect();
            let top = rms.iter().copied().fold(0.0, f64::max);
            let peak = audio.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            // Never more power than the signal, nor a higher peak.
            assert!(top < 0.2 * 1.04, "{band:?}: busiest row {top}");
            assert!(peak <= PEAK_PER_RMS * 0.2 * 1.0001, "{band:?} peak {peak}");
            assert!(top > 0.12, "{band:?}: busiest row only {top}");
            // For the record (cargo test -- --nocapture).
            println!(
                "{band:?}: busiest row {top:.3} rms of 0.2, peak {peak:.3}, {:.1} dB down on the signal",
                20.0 * (0.2 / top).log10()
            );
        }
    }

    #[test]
    fn it_has_the_length_it_says_and_ends_in_silence() {
        for (parts, band) in [
            (&["VK2ABC", "DS8 1/2"][..], wide()),
            (&["VK2ABC", "DS8 1/2"][..], narrow()),
        ] {
            let audio = banner(parts, band, RATE, 0.2);
            let secs = audio.len() as f64 / RATE;
            assert!(
                (secs - seconds(parts, band)).abs() < 1.0 / RATE + 1e-9,
                "{secs}"
            );
            let gap = (GAP_S * RATE) as usize;
            assert!(audio[audio.len() - gap..].iter().all(|&v| v == 0.0));
            // Ramps in and out: no step at either end.
            assert!(audio[0].abs() < 0.01 && audio[audio.len() - gap - 1].abs() < 0.01);
        }
        assert!(banner(&[], wide(), RATE, 0.2).is_empty());
        assert!(banner(&[""], wide(), RATE, 0.2).is_empty());
        assert_eq!(seconds(&[], wide()), 0.0);
    }

    #[test]
    fn it_stays_in_the_band() {
        use rustfft::{FftPlanner, num_complex::Complex};
        // The power spectrum of the whole banner: all but a sliver is inside the band
        // the signal will fill (the ramps' skirts are what is outside).
        for (parts, band) in [
            (&["VK2ABC", "DS8 1/2"][..], wide()),
            (&["VK2ABC", "DS8 1/2"][..], narrow()),
        ] {
            let audio = banner(parts, band, RATE, 0.2);
            let n = audio.len().next_power_of_two();
            let mut buf: Vec<Complex<f64>> = audio
                .iter()
                .map(|&v| Complex::new(f64::from(v), 0.0))
                .chain(std::iter::repeat(Complex::new(0.0, 0.0)))
                .take(n)
                .collect();
            FftPlanner::new().plan_fft_forward(n).process(&mut buf);
            let (lo, hi) = band.edges_hz();
            let (mut inside, mut all) = (0.0, 0.0);
            for (i, c) in buf.iter().enumerate().take(n / 2) {
                let f = i as f64 * RATE / n as f64;
                all += c.norm_sqr();
                if f >= lo && f <= hi {
                    inside += c.norm_sqr();
                }
            }
            let outside_db = 10.0 * (1.0 - inside / all).log10();
            println!("{band:?}: {outside_db:.1} dB of the banner's power outside the band");
            assert!(outside_db < -25.0, "{band:?}: {outside_db}");
        }
    }
}
