use ab_glyph::{Font as _, FontRef, GlyphId, PxScale, ScaleFont};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Regular, bold, italic, bold-italic. An empty path means the face is missing; bold is then
/// synthesized and italic falls back to regular.
struct Family {
    files: [&'static str; 4],
    index: [u32; 4],
}

/// Maple Mono NF (a rounded monospace font with Nerd Font icons) if installed in a user font
/// directory; it is preferred over the system fonts below.
fn user_family() -> Option<&'static Family> {
    static USER: OnceLock<Option<Family>> = OnceLock::new();
    USER.get_or_init(|| {
        let home = std::env::var("HOME").ok()?;
        let dir = ["Library/Fonts", ".local/share/fonts", ".fonts"]
            .iter()
            .map(|d| format!("{home}/{d}"))
            .find(|d| std::path::Path::new(&format!("{d}/MapleMono-NF-Regular.ttf")).exists())?;
        // Leaked once: the paths live for the whole run.
        let path = |style: &str| -> &'static str { Box::leak(format!("{dir}/MapleMono-NF-{style}.ttf").into_boxed_str()) };
        Some(Family { files: [path("Regular"), path("Bold"), path("Italic"), path("BoldItalic")], index: [0; 4] })
    })
    .as_ref()
}

const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";

const FAMILIES: &[Family] = &[
    Family { files: [MENLO; 4], index: [0, 1, 2, 3] },
    Family { files: ["/System/Library/Fonts/Monaco.ttf", "", "", ""], index: [0; 4] },
    Family {
        files: [
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Oblique.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-BoldOblique.ttf",
        ],
        index: [0; 4],
    },
    Family {
        files: [
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono-Oblique.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono-BoldOblique.ttf",
        ],
        index: [0; 4],
    },
    Family {
        files: [
            "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Bold.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Italic.ttf",
            "/usr/share/fonts/liberation/LiberationMono-BoldItalic.ttf",
        ],
        index: [0; 4],
    },
    Family {
        files: [
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Italic.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-BoldItalic.ttf",
        ],
        index: [0; 4],
    },
];

// Loaded lazily, in order, only when a glyph is missing from the primary font.
const FALLBACK: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Ayuthaya.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansThai-Regular.ttf",
    "/usr/share/fonts/truetype/tlwg/Loma.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

/// Placement of a rasterized glyph relative to the pen on the baseline.
pub struct Metrics {
    pub xmin: i32,
    /// Distance from the baseline up to the bottom of the bitmap (negative for descenders).
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

pub struct Glyph {
    pub m: Metrics,
    pub bmp: Vec<u8>,
}

/// Font file contents, read once per path and kept for the process lifetime so faces can borrow
/// them ('static) and zooming (which rebuilds `Fonts`) never re-reads or duplicates a file.
fn font_data(path: &str) -> Option<&'static [u8]> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static [u8]>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(d) = cache.get(path) {
        return Some(d);
    }
    let data: &'static [u8] = Box::leak(std::fs::read(path).ok()?.into_boxed_slice());
    cache.insert(path.to_string(), data);
    Some(data)
}

/// One font face at a fixed pixel size. ab_glyph reads outlines lazily, so a large Nerd Font
/// costs only its file size (fontdue parsed every glyph up front: ~50 MB extra).
struct Face {
    font: FontRef<'static>,
    /// For OpenType shaping (ligatures).
    shaper: Option<rustybuzz::Face<'static>>,
    scale: PxScale,
}

impl Face {
    fn load(path: &str, index: u32, px: f32) -> Option<Face> {
        let data = font_data(path)?;
        let font = FontRef::try_from_slice_and_index(data, index).ok()?;
        // `px` is the em size; PxScale is the ascent-to-descent height.
        let scale = PxScale::from(px * font.height_unscaled() / font.units_per_em()?);
        Some(Face { font, shaper: rustybuzz::Face::from_slice(data, index), scale })
    }

    fn has(&self, ch: char) -> bool {
        self.font.glyph_id(ch).0 != 0
    }

    fn advance(&self, id: GlyphId) -> f32 {
        self.font.as_scaled(self.scale).h_advance(id)
    }

    fn rasterize(&self, ch: char, embolden: bool) -> Glyph {
        self.rasterize_id(self.font.glyph_id(ch), embolden)
    }

    fn rasterize_id(&self, id: GlyphId, embolden: bool) -> Glyph {
        let advance_width = self.advance(id);
        let glyph = id.with_scale(self.scale);
        let Some(outline) = self.font.outline_glyph(glyph) else {
            return Glyph { m: Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width }, bmp: Vec::new() };
        };
        let b = outline.px_bounds();
        let (w, h) = (b.width() as usize, b.height() as usize);
        let mut bmp = vec![0u8; w * h];
        outline.draw(|x, y, c| {
            if (x as usize) < w && (y as usize) < h {
                bmp[y as usize * w + x as usize] = (c * 255.0).round() as u8;
            }
        });
        let mut m = Metrics { xmin: b.min.x as i32, ymin: -(b.max.y as i32), width: w, height: h, advance_width };
        if embolden && w > 0 {
            // Synthetic bold: smear one pixel to the right.
            let mut out = vec![0u8; (w + 1) * h];
            for r in 0..h {
                for c in 0..=w {
                    let cur = if c < w { bmp[r * w + c] } else { 0 };
                    let prev = if c > 0 { bmp[r * w + c - 1] } else { 0 };
                    out[r * (w + 1) + c] = cur.max(prev);
                }
            }
            (bmp, m.width) = (out, w + 1);
        }
        Glyph { m, bmp }
    }
}

pub struct Fonts {
    family: &'static Family,
    faces: [Option<Face>; 4],
    tried: [bool; 4],
    fallbacks: Vec<Face>,
    next_fallback: usize,
    px: f32,
    ascii: [Vec<Option<Glyph>>; 4],
    other: HashMap<(char, u8), Glyph>,
    by_id: HashMap<(u16, u8), Glyph>,
    /// Whether runs of symbols are shaped so the font's ligatures apply (Maple Mono).
    pub ligatures: bool,
    pub cell_w: usize,
    pub cell_h: usize,
    pub ascent: i32,
}

impl Fonts {
    pub fn new(px: f32) -> Self {
        let (family, regular) = user_family()
            .into_iter()
            .chain(FAMILIES.iter())
            .find_map(|f| Face::load(f.files[0], f.index[0], px).map(|face| (f, face)))
            .expect("no monospace font found");
        let scaled = regular.font.as_scaled(regular.scale);
        Fonts {
            cell_w: regular.advance(regular.font.glyph_id('M')).round() as usize,
            cell_h: (scaled.ascent() - scaled.descent()).ceil() as usize,
            ascent: scaled.ascent().round() as i32,
            family,
            faces: [Some(regular), None, None, None],
            tried: [true, false, false, false],
            fallbacks: Vec::new(),
            next_fallback: 0,
            px,
            ascii: std::array::from_fn(|_| (0..128).map(|_| None).collect()),
            other: HashMap::new(),
            by_id: HashMap::new(),
            ligatures: user_family().is_some_and(|u| std::ptr::eq(u, family)),
        }
    }

    /// `style` is the cell's BOLD | ITALIC bits.
    pub fn glyph(&mut self, ch: char, style: u8) -> &Glyph {
        let s = style as usize & 3;
        if (ch as usize) < 128 {
            if self.ascii[s][ch as usize].is_none() {
                self.ascii[s][ch as usize] = Some(self.raster(ch, style & 3));
            }
            return self.ascii[s][ch as usize].as_ref().unwrap();
        }
        if !self.other.contains_key(&(ch, style & 3)) {
            let g = self.raster(ch, style & 3);
            self.other.insert((ch, style & 3), g);
        }
        &self.other[&(ch, style & 3)]
    }

    fn ensure_face(&mut self, s: usize) {
        if !self.tried[s] {
            self.tried[s] = true;
            let f = self.family;
            if !f.files[s].is_empty() {
                self.faces[s] = Face::load(f.files[s], f.index[s], self.px);
            }
        }
    }

    /// Shape a run of characters with the styled face and return one glyph id per character,
    /// or None if the run can't be shaped 1:1 (missing glyphs, or the font merged clusters).
    pub fn shape(&mut self, text: &[char], style: u8) -> Option<Vec<u16>> {
        let s = style as usize & 3;
        self.ensure_face(s);
        let face = self.faces[s].as_ref().or(self.faces[0].as_ref())?;
        if !text.iter().all(|&c| face.has(c)) {
            return None;
        }
        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(&text.iter().collect::<String>());
        buf.set_direction(rustybuzz::Direction::LeftToRight);
        let out = rustybuzz::shape(face.shaper.as_ref()?, &[], buf);
        let infos = out.glyph_infos();
        (infos.len() == text.len()).then(|| infos.iter().map(|i| i.glyph_id as u16).collect())
    }

    /// A glyph by id (from `shape`), in the same face `shape` used.
    pub fn glyph_by_id(&mut self, id: u16, style: u8) -> &Glyph {
        let s = style as usize & 3;
        if !self.by_id.contains_key(&(id, s as u8)) {
            self.ensure_face(s);
            let (face, genuine) = match &self.faces[s] {
                Some(f) => (f, true),
                None => (self.faces[0].as_ref().unwrap(), s == 0),
            };
            let g = face.rasterize_id(GlyphId(id), s & 1 != 0 && !genuine);
            self.by_id.insert((id, s as u8), g);
        }
        &self.by_id[&(id, s as u8)]
    }

    fn raster(&mut self, ch: char, style: u8) -> Glyph {
        let (px, s) = (self.px, style as usize);
        self.ensure_face(s);
        let bold = style & 1 != 0;
        // Use the styled face if it exists; otherwise regular (bold gets synthesized).
        let (face, genuine) = match &self.faces[s] {
            Some(f) => (f, true),
            None => (self.faces[0].as_ref().unwrap(), s == 0),
        };
        if face.has(ch) {
            return face.rasterize(ch, bold && !genuine);
        }
        if let Some(f) = self.fallbacks.iter().find(|f| f.has(ch)) {
            return f.rasterize(ch, bold);
        }
        while self.next_fallback < FALLBACK.len() {
            let path = FALLBACK[self.next_fallback];
            self.next_fallback += 1;
            if let Some(f) = Face::load(path, 0, px) {
                let hit = f.has(ch);
                self.fallbacks.push(f);
                if hit {
                    return self.fallbacks.last().unwrap().rasterize(ch, bold);
                }
            }
        }
        self.faces[0].as_ref().unwrap().rasterize(ch, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menlo_faces_are_in_style_order() {
        if !std::path::Path::new(MENLO).exists() {
            return;
        }
        // Compare ink coverage of a glyph: bold faces must be heavier than their regular pair,
        // and italic must differ in shape from upright.
        let ink = |i: u32| {
            let g = Face::load(MENLO, i, 40.0).unwrap().rasterize('l', false);
            (g.bmp.iter().map(|&v| v as u32).sum::<u32>(), g.m.xmin, g.m.width)
        };
        let [r, b, i, bi] = [ink(0), ink(1), ink(2), ink(3)];
        assert!(b.0 > r.0 && bi.0 > i.0, "bold heavier: {r:?} {b:?} {i:?} {bi:?}");
        assert!(i != r && bi != b, "italic differs: {r:?} {b:?} {i:?} {bi:?}");
    }

    #[test]
    fn latin1_symbols_use_unicode_cmap() {
        // Menlo's Mac Roman cmap subtable once made U+00B7 render as a summation sign.
        let mut f = Fonts::new(28.0);
        let dot = f.glyph('\u{b7}', 0).bmp.clone();
        let sum = f.glyph('\u{2211}', 0).bmp.clone();
        assert_ne!(dot, sum);
    }

    #[test]
    fn bold_glyph_differs_from_regular() {
        let mut f = Fonts::new(28.0);
        let reg = f.glyph('a', 0).bmp.clone();
        let bold = f.glyph('a', 1).bmp.clone();
        assert_ne!(reg, bold);
    }

    #[test]
    fn cell_is_sane_and_thai_marks_have_zero_advance() {
        let mut f = Fonts::new(28.0);
        assert!((10..30).contains(&f.cell_w) && (20..50).contains(&f.cell_h));
        // Depends on a Thai font (Ayuthaya on macOS) being installed; CI's Linux image has none.
        if cfg!(target_os = "macos") {
            assert_eq!(f.glyph('\u{0e48}', 0).m.advance_width, 0.0);
        }
    }

    #[test]
    fn maple_ligatures_change_glyphs() {
        let mut f = Fonts::new(28.0);
        if !f.ligatures {
            return; // Maple Mono not installed
        }
        let chars: Vec<char> = "a => b".chars().collect();
        let ids = f.shape(&chars, 0).expect("shape");
        let face = f.faces[0].as_ref().unwrap();
        let plain: Vec<u16> = chars.iter().map(|&c| face.font.glyph_id(c).0).collect();
        assert_eq!(ids.len(), plain.len());
        // '=' and '>' are replaced by ligature parts; letters and spaces are untouched.
        assert_ne!(ids[2..4], plain[2..4]);
        assert_eq!((ids[0], ids[5]), (plain[0], plain[5]));
    }
}
