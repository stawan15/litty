//! Colour emoji from the system's bitmap emoji font (Apple Color Emoji `sbix` on macOS, Noto
//! Color Emoji `CBDT` on Linux). Only single-codepoint emoji: sequences (ZWJ, skin tones, flags)
//! show their parts.

use crate::font::font_data;
use std::collections::HashMap;
use ttf_parser::{Face, RasterImageFormat};

const FONTS: [&str; 4] = [
    "/System/Library/Fonts/Apple Color Emoji.ttc",
    "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/google-noto-color-emoji-fonts/Noto-COLRv1.ttf",
];

/// Straight-alpha RGBA, `side` × `side` pixels.
pub struct Bitmap {
    pub side: usize,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
pub struct Emoji {
    face: Option<Option<Face<'static>>>,
    cache: HashMap<(char, usize), Option<Bitmap>>,
}

impl Emoji {
    /// The emoji `ch` scaled to `side` pixels, if the font has it as a PNG bitmap.
    pub fn bitmap(&mut self, ch: char, side: usize) -> Option<&Bitmap> {
        if !self.cache.contains_key(&(ch, side)) {
            let bmp = self.load(ch, side);
            self.cache.insert((ch, side), bmp);
        }
        self.cache[&(ch, side)].as_ref()
    }

    fn load(&mut self, ch: char, side: usize) -> Option<Bitmap> {
        let face = self
            .face
            .get_or_insert_with(|| FONTS.iter().find_map(|p| font_data(p)).and_then(|d| Face::parse(d, 0).ok()))
            .as_ref()?;
        let img = face.glyph_raster_image(face.glyph_index(ch)?, side.min(u16::MAX as usize) as u16)?;
        if img.format != RasterImageFormat::PNG {
            return None;
        }
        let (w, h, rgba) = decode_png(img.data)?;
        Some(Bitmap { side, rgba: scale(&rgba, w, h, side) })
    }
}

fn decode_png(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(data);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

/// Area-average `src` (w × h, straight alpha) down or up to a `side` × `side` square.
fn scale(src: &[u8], w: usize, h: usize, side: usize) -> Vec<u8> {
    let mut out = vec![0u8; side * side * 4];
    for dy in 0..side {
        let (y0, y1) = (dy * h / side, ((dy + 1) * h / side).max(dy * h / side + 1).min(h));
        for dx in 0..side {
            let (x0, x1) = (dx * w / side, ((dx + 1) * w / side).max(dx * w / side + 1).min(w));
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &src[(y * w + x) * 4..][..4];
                    // Weight colour by alpha so transparent pixels don't darken the edge.
                    let al = p[3] as u32;
                    (r, g, b, a, n) = (r + p[0] as u32 * al, g + p[1] as u32 * al, b + p[2] as u32 * al, a + al, n + 1);
                }
            }
            let o = (dy * side + dx) * 4;
            if a > 0 {
                out[o..o + 4].copy_from_slice(&[(r / a) as u8, (g / a) as u8, (b / a) as u8, (a / n) as u8]);
            }
        }
    }
    out
}
