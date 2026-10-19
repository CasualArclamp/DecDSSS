// made by CasualArclamp
// The size ladder, the fit-to-budget bisection and the source loading follow
// COFDMtv's `cofdmtv-pix` (same author); the encoder here is pure Rust.
//! Pictures for DecDSSS: a picture is scaled to a size the air time allows,
//! compressed to a progressive JPEG with the highest quality that fits a byte
//! budget, and decoded as far as it has arrived while the rest is on the way.
//!
//! A progressive JPEG sends the whole picture coarse first (the DC of each
//! component: one value per 8×8 block, an eighth of the resolution) and then
//! the AC coefficients from low to high frequency, so a listener sees the
//! picture early and watch it sharpen. The receive side feeds the first bytes
//! of the file (the frames come in order, see `decdsss-link`) to
//! [`decode_partial`].

// The picture types, for callers that build or show pictures.
use image::{DynamicImage, ImageFormat, ImageReader};
pub use image::{Rgb, RgbImage, RgbaImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::io::Cursor;
use std::path::Path;

/// Smallest side of a picture sent, pixels.
pub const MIN_SIDE: u32 = 16;
/// Largest side of a picture sent, pixels.
pub const MAX_SIDE: u32 = 1024;
/// Scans per component: the DC, then the AC coefficients 1…63 in `SCANS - 1`
/// equal bands. More scans show more in the first half of the file and cost
/// about 3 % of the file each (measured on a 192×144 photo: 6 show 22.8 dB at
/// half the file against 21.4 dB for 4, for 6 % more bytes in all).
pub const SCANS: u8 = 6;

/// A picture loaded for sending.
#[derive(Clone)]
pub struct Source {
    /// Upright (EXIF orientation applied), alpha composited onto white.
    pub image: RgbImage,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Source, String> {
        use image::ImageDecoder;
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let orientation = decoder.orientation().map_err(|e| e.to_string())?;
        let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
        img.apply_orientation(orientation);
        Self::from_dynamic(img)
    }

    pub fn from_rgba(img: RgbaImage) -> Result<Source, String> {
        Self::from_dynamic(DynamicImage::ImageRgba8(img))
    }

    fn from_dynamic(img: DynamicImage) -> Result<Source, String> {
        if img.width() < MIN_SIDE || img.height() < MIN_SIDE {
            return Err(format!(
                "{}×{} pixels: pictures need at least {MIN_SIDE} on each side",
                img.width(),
                img.height()
            ));
        }
        let img = flatten(img);
        // Nothing sent is bigger than this, and every size tried is scaled from
        // the source, so keep it small enough to scale quickly.
        let (w, h) = fitted_size(img.width(), img.height(), SOURCE_PIXELS);
        let image = if (w, h) == img.dimensions() {
            img
        } else {
            image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3)
        };
        Ok(Source { image })
    }
}

/// Pixels a loaded source is reduced to at most.
const SOURCE_PIXELS: u32 = 1 << 20;

/// Composite onto white and drop the alpha channel.
fn flatten(img: DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let a = u16::from(p[3]);
        let mix = |c: u8| ((u16::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])])
    })
}

/// The size of a `width`×`height` picture scaled to at most `max_pixels`, each
/// side within [`MIN_SIDE`]…[`MAX_SIDE`], keeping the aspect ratio as far as
/// those limits allow. Never enlarges.
pub fn fitted_size(width: u32, height: u32, max_pixels: u32) -> (u32, u32) {
    let (w, h) = (f64::from(width), f64::from(height));
    let mut scale = (f64::from(max_pixels) / (w * h)).sqrt().min(1.0);
    scale = scale
        .min(f64::from(MAX_SIDE) / w)
        .min(f64::from(MAX_SIDE) / h);
    let side = |v: f64| ((v * scale).floor() as u32).clamp(MIN_SIDE, MAX_SIDE);
    (side(w), side(h))
}

/// Scale for sending (Lanczos; never enlarges).
pub fn scale(img: &RgbImage, max_pixels: u32) -> RgbImage {
    let (w, h) = fitted_size(img.width(), img.height(), max_pixels);
    if (w, h) == img.dimensions() {
        return img.clone();
    }
    image::imageops::resize(img, w, h, image::imageops::FilterType::Lanczos3)
}

/// A compressed picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    /// Quality used, 1…100.
    pub quality: u8,
    pub width: u32,
    pub height: u32,
}

/// Compress to a progressive JPEG (4:2:0, optimised Huffman tables) at one
/// quality.
pub fn encode_at(img: &RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let (w, h) = img.dimensions();
    let (w16, h16) = (
        u16::try_from(w).map_err(|_| "picture too wide")?,
        u16::try_from(h).map_err(|_| "picture too tall")?,
    );
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, quality.clamp(1, 100));
    enc.set_progressive_scans(SCANS);
    enc.set_optimized_huffman_tables(true);
    enc.set_sampling_factor(SamplingFactor::F_2_2);
    enc.encode(img.as_raw(), w16, h16, ColorType::Rgb)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Compress with the highest quality whose file fits `budget` bytes (bisection
/// over 1…100; size grows with quality).
pub fn encode_to_fit(img: &RgbImage, budget: usize) -> Result<Encoded, String> {
    let (width, height) = img.dimensions();
    let done = |bytes: Vec<u8>, quality| Encoded {
        bytes,
        quality,
        width,
        height,
    };
    let best = encode_at(img, 100)?;
    if best.len() <= budget {
        return Ok(done(best, 100));
    }
    let worst = encode_at(img, 1)?;
    if worst.len() > budget {
        return Err(format!(
            "{width}×{height}: {} bytes even at the lowest quality, more than {budget}",
            worst.len()
        ));
    }
    let (mut lo, mut hi, mut found) = (1u8, 100u8, worst);
    let mut q_found = 1;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        let bytes = encode_at(img, mid)?;
        if bytes.len() <= budget {
            lo = mid;
            found = bytes;
            q_found = mid;
        } else {
            hi = mid;
        }
    }
    Ok(done(found, q_found))
}

/// The largest size whose picture fits `budget` bytes at a quality of at least
/// `min_quality`, or, if none does, the smallest size that fits at all. Sizes go
/// down from `max_pixels` in steps of √2 in the pixel count (about 19 % on each
/// side); the picture gets worse with size at a fixed budget, so the search
/// bisects them.
pub fn encode_auto(
    src: &RgbImage,
    budget: usize,
    max_pixels: u32,
    min_quality: u8,
) -> Result<Encoded, String> {
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    let mut pixels = f64::from(max_pixels);
    while pixels >= 256.0 {
        let d = fitted_size(src.width(), src.height(), pixels as u32);
        if sizes.last() != Some(&d) {
            sizes.push(d);
        }
        pixels /= std::f64::consts::SQRT_2;
    }
    sizes.reverse();
    let at = |i: usize| {
        let (w, h) = sizes[i];
        let img = if (w, h) == src.dimensions() {
            src.clone()
        } else {
            image::imageops::resize(src, w, h, image::imageops::FilterType::Lanczos3)
        };
        encode_to_fit(&img, budget)
    };
    // The first size that is not good enough (fails, or only at a quality
    // below the minimum) ends the good ones.
    let (mut lo, mut hi) = (0, sizes.len());
    let mut best = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match at(mid) {
            Ok(e) if e.quality >= min_quality => {
                best = Some(e);
                lo = mid + 1;
            }
            _ => hi = mid,
        }
    }
    match best {
        Some(e) => Ok(e),
        // Nothing reaches the quality: the smallest size, as good as it gets.
        None => at(0),
    }
}

/// Write a picture as PNG.
pub fn save_png(img: &RgbaImage, path: &Path) -> Result<(), String> {
    img.save_with_format(path, ImageFormat::Png)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Decode a whole received picture (JPEG, PNG, WebP, …) for display.
pub fn decode(bytes: &[u8]) -> Option<RgbaImage> {
    image::load_from_memory(bytes).ok().map(|i| i.to_rgba8())
}

/// What the markers of the first bytes of a JPEG say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub progressive: bool,
    /// Where each complete scan's data ends (the offset of the next marker).
    pub scan_ends: Vec<usize>,
    /// Bytes ran out inside a scan (rather than between scans).
    pub in_scan: bool,
    /// The end-of-image marker is there: the file is whole.
    pub complete: bool,
}

/// Walk the marker segments of the beginning `head` of a JPEG file. `None`
/// until the frame header (which has the size) is in.
pub fn scan_info(head: &[u8]) -> Option<Info> {
    if head.len() < 4 || head[..2] != [0xFF, 0xD8] {
        return None;
    }
    let mut info = Info {
        width: 0,
        height: 0,
        progressive: false,
        scan_ends: Vec::new(),
        in_scan: false,
        complete: false,
    };
    let mut have_frame = false;
    let mut at = 2;
    while at + 1 < head.len() {
        if head[at] != 0xFF {
            return have_frame.then_some(info);
        }
        let marker = head[at + 1];
        match marker {
            // Fill byte before a marker.
            0xFF => {
                at += 1;
                continue;
            }
            0xD9 => {
                info.complete = true;
                break;
            }
            0x01 | 0xD0..=0xD8 => {
                at += 2;
                continue;
            }
            _ => {}
        }
        let Some(len) = head.get(at + 2..at + 4) else {
            break;
        };
        let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        if len < 2 || at + 2 + len > head.len() {
            // Cut inside a segment header.
            break;
        }
        match marker {
            // Frame headers (baseline, extended, progressive, lossless …), not
            // DHT (C4), JPG (C8) or DAC (CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if len < 8 {
                    return None;
                }
                info.progressive = marker == 0xC2;
                info.height = u32::from(u16::from_be_bytes([head[at + 5], head[at + 6]]));
                info.width = u32::from(u16::from_be_bytes([head[at + 7], head[at + 8]]));
                have_frame = info.width > 0 && info.height > 0;
            }
            _ => {}
        }
        at += 2 + len;
        if marker == 0xDA {
            // Entropy-coded data up to the next marker that is not a restart or a
            // stuffed 0xFF.
            let mut i = at;
            let end = loop {
                match head[i..].iter().position(|&b| b == 0xFF) {
                    None => break None,
                    Some(p) => {
                        i += p;
                        match head.get(i + 1) {
                            None => break None,
                            Some(0x00 | 0xD0..=0xD7) => i += 2,
                            Some(_) => break Some(i),
                        }
                    }
                }
            };
            match end {
                Some(e) => {
                    info.scan_ends.push(e);
                    at = e;
                }
                None => {
                    info.in_scan = true;
                    break;
                }
            }
        }
    }
    have_frame.then_some(info)
}

/// A picture decoded as far as its file has arrived.
#[derive(Debug, Clone)]
pub struct Partial {
    /// The whole picture as far as it is known; transparent where nothing has
    /// arrived yet.
    pub image: RgbaImage,
    /// Complete scans in (a progressive file has [`SCANS`] per component, 18 in
    /// all for the three).
    pub scans: usize,
    pub progressive: bool,
    /// The file is whole.
    pub complete: bool,
}

/// Decode the beginning `head` of a JPEG: the picture as far as it goes, coarse
/// when the file is progressive and only the first scans are in. `None` before
/// the size is known (the first few hundred bytes), and for data that is not
/// a JPEG. Before the first scan has any data the picture is blank.
pub fn decode_partial(head: &[u8]) -> Option<Partial> {
    let info = scan_info(head)?;
    let blank = || RgbaImage::new(info.width, info.height);
    let decode = |bytes: &[u8]| {
        image::load_from_memory_with_format(bytes, ImageFormat::Jpeg)
            .ok()
            .map(|d| d.to_rgba8())
    };
    let mk = |image| {
        Some(Partial {
            image,
            scans: info.scan_ends.len(),
            progressive: info.progressive,
            complete: info.complete,
        })
    };
    if info.complete {
        return mk(decode(head).unwrap_or_else(blank));
    }
    // With the end-of-image marker put on the cut: the scan that was in
    // progress gives what it has. Failing that, the complete scans only.
    let with_eoi = |end: usize| {
        let mut v = head[..end].to_vec();
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    };
    let mut image = decode(&with_eoi(head.len()));
    if image.is_none()
        && let Some(&end) = info.scan_ends.last()
    {
        image = decode(&with_eoi(end));
    }
    mk(image.unwrap_or_else(blank))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test picture with gradients, edges and detail at several scales.
    fn test_image(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let (u, v) = (x as f32, y as f32);
            let fy = v / h as f32;
            let r = 128.0
                + 50.0 * (u * 0.11 + v * 0.07).sin()
                + 40.0 * (u * 0.37 - v * 0.23).sin()
                + 20.0 * (u * 0.9).sin();
            let g = 255.0 * fy + 30.0 * (v * 0.5).sin() * (u * 0.3).cos();
            let b = if (x / 16 + y / 16) % 2 == 0 {
                200.0
            } else {
                60.0
            } + 25.0 * (u * 0.6 + v * 0.45).sin();
            image::Rgb([r, g, b].map(|c| c.clamp(0.0, 255.0) as u8))
        })
    }

    fn psnr(a: &RgbaImage, b: &RgbImage) -> f64 {
        assert_eq!(a.dimensions(), b.dimensions());
        let mut se = 0.0;
        let mut n = 0.0;
        for (pa, pb) in a.pixels().zip(b.pixels()) {
            for c in 0..3 {
                let d = f64::from(pa.0[c]) - f64::from(pb.0[c]);
                se += d * d;
                n += 1.0;
            }
        }
        10.0 * (255.0 * 255.0 / (se / n).max(1e-9)).log10()
    }

    #[test]
    fn sizes_respect_the_limits() {
        assert_eq!(fitted_size(4000, 3000, 1 << 20), (1024, 768));
        assert_eq!(fitted_size(4000, 3000, 1 << 14), (147, 110));
        assert_eq!(fitted_size(100, 50, 1 << 20), (100, 50));
        assert_eq!(fitted_size(5000, 20, 1 << 14), (1024, 16));
    }

    #[test]
    fn fits_the_budget_with_the_highest_quality() {
        let img = test_image(160, 120);
        let e = encode_to_fit(&img, 3000).unwrap();
        assert!(e.bytes.len() <= 3000, "{}", e.bytes.len());
        assert!(e.quality < 100);
        assert!(
            encode_at(&img, e.quality + 1).unwrap().len() > 3000,
            "q {} is the highest that fits",
            e.quality
        );
        // Progressive (start of frame 2) and decodes whole.
        assert!(e.bytes.windows(2).any(|w| w == [0xFF, 0xC2]));
        let back = decode(&e.bytes).expect("decodes");
        assert_eq!(back.dimensions(), (160, 120));
        assert!(psnr(&back, &img) > 20.0);
        assert!(encode_to_fit(&img, 100).is_err());
    }

    #[test]
    fn auto_picks_the_largest_good_size() {
        let img = test_image(800, 600);
        let e = encode_auto(&img, 6000, 1 << 20, 30).unwrap();
        assert!(e.quality >= 30 && e.bytes.len() <= 6000, "{}", e.quality);
        let pixels = e.width * e.height;
        assert!(pixels < 1 << 20);
        // The next size up would not have made the quality.
        let bigger = scale(
            &img,
            (f64::from(pixels) * std::f64::consts::SQRT_2 * 1.05) as u32,
        );
        assert!(
            bigger.dimensions() == (e.width, e.height)
                || encode_to_fit(&bigger, 6000).is_ok_and(|b| b.quality < 30),
            "a larger picture would have fitted at q >= 30"
        );
        // The tiny budget lands on a smaller size, not an error.
        let small = encode_auto(&img, 1500, 1 << 20, 30).unwrap();
        assert!(small.width * small.height <= pixels && small.bytes.len() <= 1500);
        // A minimum quality that only a tiny picture meets: the picture is
        // small, not an error.
        let tiny = encode_auto(&img, 3000, 1 << 20, 100).unwrap();
        assert!(tiny.bytes.len() <= 3000 && tiny.width * tiny.height < small.width * small.height);
    }

    #[test]
    fn the_scans_are_found() {
        let img = test_image(160, 120);
        let e = encode_at(&img, 40).unwrap();
        let info = scan_info(&e).unwrap();
        assert_eq!((info.width, info.height), (160, 120));
        assert!(info.progressive && info.complete && !info.in_scan);
        // Three components, the DC scan and the AC bands each.
        assert_eq!(info.scan_ends.len(), 3 * usize::from(SCANS));
        assert!(info.scan_ends.windows(2).all(|w| w[0] < w[1]));
        // Cut inside the tables: no size yet.
        assert!(scan_info(&e[..30]).is_none());
        assert!(scan_info(b"not a jpeg at all").is_none());
    }

    #[test]
    fn a_cut_short_file_draws_the_whole_picture_and_sharpens() {
        let img = test_image(192, 144);
        let e = encode_at(&img, 60).unwrap();
        let info = scan_info(&e).unwrap();
        // Every cut that has the size gives a picture of that size.
        let mut last = 0.0;
        let mut sharpened = 0;
        for cut in (info.scan_ends[0]..e.len()).step_by(37) {
            let p = decode_partial(&e[..cut]).expect("size known");
            assert_eq!(p.image.dimensions(), (192, 144));
            assert!(!p.complete || cut == e.len());
            if p.scans >= 3 {
                // The DC of all three components is in: the picture is there,
                // coarse, and (a few dB of slack for where a scan is cut) gets
                // better as more arrives.
                let q = psnr(&p.image, &img);
                assert!(q > last - 1.5, "cut {cut}: {q} after {last}");
                if q > last + 0.2 {
                    sharpened += 1;
                }
                last = last.max(q);
            }
        }
        assert!(sharpened >= 5, "{sharpened}");
        let whole = decode_partial(&e).unwrap();
        assert!(whole.complete && whole.scans == info.scan_ends.len());
        assert!(psnr(&whole.image, &img) >= last - 0.05);
        // A third of the file already shows the whole picture.
        let third = decode_partial(&e[..e.len() / 3]).unwrap();
        assert!(
            psnr(&third.image, &img) > 15.0,
            "{}",
            psnr(&third.image, &img)
        );
    }

    #[test]
    fn before_the_first_scan_the_picture_is_blank() {
        let img = test_image(64, 64);
        let e = encode_at(&img, 50).unwrap();
        let info = scan_info(&e).unwrap();
        // The size is in once the frame header has arrived; find the first scan.
        let sos = e.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        let p = decode_partial(&e[..sos + 4]).expect("size known");
        assert_eq!(p.image.dimensions(), (64, 64));
        assert!(p.image.pixels().all(|px| px.0[3] == 0));
        assert_eq!(p.scans, 0);
        assert!(info.scan_ends[0] > sos);
    }
}
