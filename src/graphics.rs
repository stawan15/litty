//! Images through the Kitty graphics protocol (`ESC _ G <keys> ; <base64> ESC \`): PNG, RGB and
//! RGBA, sent directly (chunked), from a file, or from a temporary file. An image is placed at the
//! cursor and anchored to that line of text, so it scrolls with it and is freed when the line
//! leaves scrollback. Each pane keeps at most `QUOTA` bytes of pixels; the oldest go first.
//! Not supported: shared memory, cropping, z-index below text, Unicode placeholders.
//! https://sw.kovidgoyal.net/kitty/graphics-protocol/

use std::collections::HashMap;

/// Decoded pixels kept per pane.
const QUOTA: usize = 64 << 20;
/// Largest escape sequence accepted (chunks of one image add up to at most this).
const MAX_PAYLOAD: usize = 96 << 20;
/// Largest image side in pixels.
const MAX_SIDE: usize = 10_000;

/// A piece of PTY output.
pub enum Piece<'a> {
    Text(&'a [u8]),
    Apc(&'a [u8]),
}

/// Splits PTY output into ordinary bytes (for the VT parser) and APC strings (`ESC _ … ESC \`),
/// which the VT parser would drop. Keeps its state across reads.
#[derive(Default)]
pub struct ApcSplit {
    /// Inside an APC string: its bytes so far (None once it grew past the limit).
    apc: Option<Option<Vec<u8>>>,
    /// The previous read ended in ESC (outside an APC) or ESC inside one.
    esc: bool,
}

impl ApcSplit {
    pub fn split(&mut self, mut input: &[u8], mut out: impl FnMut(Piece)) {
        let text = |out: &mut dyn FnMut(Piece), t: &[u8]| {
            if !t.is_empty() {
                out(Piece::Text(t));
            }
        };
        while !input.is_empty() {
            match &mut self.apc {
                None => {
                    if std::mem::take(&mut self.esc) {
                        if input[0] == b'_' {
                            self.apc = Some(Some(Vec::new()));
                            input = &input[1..];
                            continue;
                        }
                        text(&mut out, b"\x1b");
                    }
                    // Hand over everything up to an ESC that starts an APC (or might: at the end).
                    let mut from = 0;
                    loop {
                        let Some(i) = input[from..].iter().position(|&b| b == 0x1b).map(|i| i + from) else {
                            text(&mut out, input);
                            return;
                        };
                        match input.get(i + 1) {
                            Some(b'_') => {
                                text(&mut out, &input[..i]);
                                self.apc = Some(Some(Vec::new()));
                                input = &input[i + 2..];
                                break;
                            }
                            Some(_) => from = i + 1,
                            None => {
                                text(&mut out, &input[..i]);
                                self.esc = true;
                                return;
                            }
                        }
                    }
                }
                Some(buf) => {
                    // The string ends at ESC \ (or BEL, which some programs use).
                    let esc_before = self.esc;
                    let end = input.iter().enumerate().position(|(i, &b)| b == 0x07 || (b == b'\\' && if i == 0 { esc_before } else { input[i - 1] == 0x1b }));
                    let body = &input[..end.unwrap_or(input.len())];
                    if let Some(b) = buf {
                        b.extend_from_slice(body);
                        if b.len() > MAX_PAYLOAD {
                            *buf = None;
                        }
                    }
                    let Some(e) = end else {
                        self.esc = input.last() == Some(&0x1b);
                        return;
                    };
                    let mut done = self.apc.take().flatten().unwrap_or_default();
                    // Drop the ESC of the terminator (it may have come in the previous read).
                    if input[e] == b'\\' && done.last() == Some(&0x1b) {
                        done.pop();
                    }
                    self.esc = false;
                    if !done.is_empty() {
                        out(Piece::Apc(&done));
                    }
                    input = &input[e + 1..];
                }
            }
        }
    }
}

pub struct Image {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    /// For evicting the oldest first.
    serial: u64,
}

/// Where an image is shown: anchored to an absolute line id (see `Grid::pushed`).
pub struct Placement {
    pub image: u32,
    pub id: u32,
    pub line: u64,
    pub col: usize,
    pub rows: usize,
    /// Pixel size to draw at (natural size, or scaled to the requested cells).
    pub px: (usize, usize),
    /// On the alternate screen.
    pub alt: bool,
    /// The pixels scaled to `px`, made on first draw (None when drawn at natural size).
    pub scaled: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct Graphics {
    pub images: HashMap<u32, Image>,
    pub placements: Vec<Placement>,
    /// A chunked transmission in progress: its keys and the data so far (each chunk is its own
    /// base64 string, decoded as it arrives).
    pending: Option<(Keys, Vec<u8>)>,
    /// Image numbers (I=) to the ids litty gave them.
    numbers: HashMap<u32, u32>,
    serial: u64,
}

/// The keys of one command (`a=T,f=100,…`); absent numbers are 0.
#[derive(Clone, Default, Debug)]
pub struct Keys {
    pub action: u8,
    format: u32,
    medium: u8,
    compressed: bool,
    pub id: u32,
    number: u32,
    pub placement: u32,
    src_w: usize,
    src_h: usize,
    cols: usize,
    rows: usize,
    more: bool,
    quiet: u8,
    pub no_move: bool,
    delete: u8,
}

impl Keys {
    fn parse(s: &[u8]) -> Keys {
        let mut k = Keys { action: b't', format: 32, medium: b'd', delete: b'a', ..Keys::default() };
        for kv in s.split(|&b| b == b',') {
            let [key, b'=', value @ ..] = kv else { continue };
            let n = || std::str::from_utf8(value).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let c = value.first().copied().unwrap_or(0);
            match key {
                b'a' => k.action = c,
                b'f' => k.format = n() as u32,
                b't' => k.medium = c,
                b'o' => k.compressed = c == b'z',
                b'i' => k.id = n() as u32,
                b'I' => k.number = n() as u32,
                b'p' => k.placement = n() as u32,
                b's' => k.src_w = n() as usize,
                b'v' => k.src_h = n() as usize,
                b'c' => k.cols = n().min(1000) as usize,
                b'r' => k.rows = n().min(1000) as usize,
                b'm' => k.more = c == b'1',
                b'q' => k.quiet = n() as u8,
                b'C' => k.no_move = c == b'1',
                b'd' => k.delete = c,
                _ => {}
            }
        }
        k
    }
}

/// What a command asks of the terminal after it ran.
pub struct Outcome {
    /// Reply to send back (already an escape sequence), if any.
    pub reply: Option<String>,
    /// A placement to put at the cursor: (image id, keys).
    pub place: Option<(u32, Keys)>,
}

fn base64(data: &[u8]) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in data.iter().filter(|c| !matches!(c, b'=' | b'\n' | b'\r')) {
        acc = acc << 6 | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// A PNG as straight RGBA: (width, height, pixels).
pub fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), &'static str> {
    let mut dec = png::Decoder::new(bytes);
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = dec.read_info().map_err(|_| "EBADPNG:not a PNG")?;
    let (w, h) = (reader.info().width as usize, reader.info().height as usize);
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err("EFBIG:image too large");
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|_| "EBADPNG:bad PNG data")?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => return Err("EBADPNG:unsupported PNG colour type"),
    };
    Ok((w, h, rgba))
}

/// Pixels as straight RGBA from the transmitted bytes.
fn decode(k: &Keys, bytes: Vec<u8>) -> Result<(usize, usize, Vec<u8>), &'static str> {
    let bytes = if k.compressed { miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&bytes, MAX_PAYLOAD).map_err(|_| "EINVAL:bad zlib data")? } else { bytes };
    let (w, h, rgba) = match k.format {
        100 => decode_png(&bytes)?,
        24 | 32 => {
            let (w, h) = (k.src_w, k.src_h);
            let bpp = k.format as usize / 8;
            if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || bytes.len() < w * h * bpp {
                return Err("ENODATA:size and data don't match");
            }
            let rgba = if bpp == 4 { bytes[..w * h * 4].to_vec() } else { bytes[..w * h * 3].chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
            (w, h, rgba)
        }
        _ => return Err("EINVAL:unsupported format"),
    };
    Ok((w, h, rgba))
}

/// The bytes a command refers to: inline, or read from the file whose path was sent.
fn load(k: &Keys, data: Vec<u8>) -> Result<Vec<u8>, &'static str> {
    match k.medium {
        b'd' => Ok(data),
        b'f' | b't' => {
            let path = String::from_utf8(data).map_err(|_| "EINVAL:bad path")?;
            let meta = std::fs::metadata(&path).map_err(|_| "EBADF:can't read the file")?;
            if !meta.is_file() || meta.len() as usize > MAX_PAYLOAD {
                return Err("EBADF:not a regular file");
            }
            let bytes = std::fs::read(&path).map_err(|_| "EBADF:can't read the file")?;
            // A temporary file is removed once read, but only one that says it is one.
            if k.medium == b't' && path.contains("tty-graphics-protocol") {
                let _ = std::fs::remove_file(&path);
            }
            Ok(bytes)
        }
        _ => Err("EINVAL:unsupported transmission medium"),
    }
}

impl Graphics {
    /// Run one APC payload (without the leading `G`). The grid places images and sends replies.
    pub fn command(&mut self, payload: &[u8]) -> Option<Outcome> {
        let rest = payload.strip_prefix(b"G")?;
        let (keys, data) = match rest.iter().position(|&b| b == b';') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &[][..]),
        };
        let mut k = Keys::parse(keys);
        let Some(data) = base64(data) else {
            self.pending = None;
            return Some(Outcome { reply: None, place: None });
        };
        // A chunk continues a transmission: its keys come from the first chunk.
        let data = match self.pending.take() {
            Some((first, mut so_far)) => {
                so_far.extend_from_slice(&data);
                if so_far.len() > MAX_PAYLOAD {
                    return None;
                }
                if k.more {
                    self.pending = Some((first, so_far));
                    return None;
                }
                k = first;
                so_far
            }
            None if k.more && matches!(k.action, b't' | b'T' | b'q') => {
                self.pending = Some((k, data));
                return None;
            }
            None => data,
        };
        let reply = |k: &Keys, msg: &str| {
            let ok = msg == "OK";
            let silent = (ok && k.quiet >= 1) || k.quiet >= 2 || (k.id == 0 && k.number == 0);
            (!silent).then(|| {
                let mut ids = if k.id != 0 { format!("i={}", k.id) } else { format!("I={}", k.number) };
                if k.placement != 0 {
                    ids.push_str(&format!(",p={}", k.placement));
                }
                format!("\x1b_G{ids};{msg}\x1b\\")
            })
        };
        match k.action {
            b't' | b'T' | b'q' => {
                let result = load(&k, data).and_then(|d| decode(&k, d));
                let (w, h, rgba) = match result {
                    Ok(img) => img,
                    Err(e) => return Some(Outcome { reply: reply(&k, e), place: None }),
                };
                // Replies go only to commands that named the image (a=q always does).
                let ok = reply(&k, "OK");
                if k.action == b'q' {
                    return Some(Outcome { reply: ok, place: None });
                }
                if k.id == 0 {
                    k.id = (1u32 << 31) | (self.serial as u32 & 0x7fff_ffff);
                    if k.number != 0 {
                        self.numbers.insert(k.number, k.id);
                    }
                }
                self.serial += 1;
                self.images.insert(k.id, Image { w, h, rgba, serial: self.serial });
                self.enforce_quota(k.id);
                let place = (k.action == b'T').then(|| (k.id, k.clone()));
                Some(Outcome { reply: ok, place })
            }
            b'p' => {
                let id = if k.id == 0 { self.numbers.get(&k.number).copied().unwrap_or(0) } else { k.id };
                if !self.images.contains_key(&id) {
                    return Some(Outcome { reply: reply(&k, "ENOENT:no such image"), place: None });
                }
                Some(Outcome { reply: reply(&k, "OK"), place: Some((id, k)) })
            }
            b'd' => {
                let free = k.delete.is_ascii_uppercase();
                match k.delete.to_ascii_lowercase() {
                    b'i' => {
                        self.placements.retain(|p| p.image != k.id || (k.placement != 0 && p.id != k.placement));
                        if free && !self.placements.iter().any(|p| p.image == k.id) {
                            self.images.remove(&k.id);
                        }
                    }
                    b'n' => {
                        if let Some(id) = self.numbers.get(&k.number).copied() {
                            self.placements.retain(|p| p.image != id);
                            if free {
                                self.images.remove(&id);
                            }
                        }
                    }
                    _ => {
                        self.placements.clear();
                        if free {
                            self.images.clear();
                        }
                    }
                }
                None
            }
            _ => Some(Outcome { reply: reply(&k, "EINVAL:unsupported action"), place: None }),
        }
    }

    /// Show `image` at (line, col); returns the cells it covers (columns, rows).
    pub fn place(&mut self, image: u32, k: &Keys, line: u64, col: usize, cell: (usize, usize), alt: bool) -> (usize, usize) {
        let Some(img) = self.images.get(&image) else { return (0, 0) };
        let (cw, ch) = (cell.0.max(1), cell.1.max(1));
        // Cells asked for (keeping the aspect ratio when only one side is given), else natural size.
        let px = match (k.cols, k.rows) {
            (0, 0) => (img.w, img.h),
            (c, 0) => (c * cw, (c * cw * img.h / img.w.max(1)).max(1)),
            (0, r) => ((r * ch * img.w / img.h.max(1)).max(1), r * ch),
            (c, r) => (c * cw, r * ch),
        };
        let (cols, rows) = (px.0.div_ceil(cw), px.1.div_ceil(ch));
        if k.placement != 0 {
            self.placements.retain(|p| !(p.image == image && p.id == k.placement));
        }
        self.placements.push(Placement { image, id: k.placement, line, col, rows, px, alt, scaled: None });
        (cols, rows)
    }

    /// Drop placements whose lines left scrollback (lines before `first`) and images nobody can
    /// show again (no id the program knows, no placement).
    pub fn forget_before(&mut self, first: u64) {
        let before = self.placements.len();
        self.placements.retain(|p| p.alt || p.line + p.rows as u64 > first);
        if self.placements.len() != before {
            let shown: Vec<u32> = self.placements.iter().map(|p| p.image).collect();
            self.images.retain(|id, _| id & (1 << 31) == 0 || shown.contains(id));
        }
    }

    /// Clear the placements on one screen that overlap lines [from, to).
    pub fn clear_lines(&mut self, alt: bool, from: u64, to: u64) {
        self.placements.retain(|p| p.alt != alt || p.line + p.rows as u64 <= from || p.line >= to);
    }

    fn enforce_quota(&mut self, keep: u32) {
        let mut total: usize = self.images.values().map(|i| i.rgba.len()).sum();
        while total > QUOTA {
            let Some((&id, img)) = self.images.iter().filter(|(id, _)| **id != keep).min_by_key(|(_, i)| i.serial) else { break };
            total -= img.rgba.len();
            self.images.remove(&id);
            self.placements.retain(|p| p.image != id);
        }
    }
}

/// Area-average (or nearest when enlarging) scale of straight RGBA to `w` × `h`.
pub fn scale(src: &[u8], sw: usize, sh: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1).min(sh));
        for x in 0..w {
            let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1).min(sw));
            let mut acc = [0u32; 5];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &src[(sy * sw + sx) * 4..][..4];
                    let a = p[3] as u32;
                    acc = [acc[0] + p[0] as u32 * a, acc[1] + p[1] as u32 * a, acc[2] + p[2] as u32 * a, acc[3] + a, acc[4] + 1];
                }
            }
            if acc[3] > 0 {
                let o = (y * w + x) * 4;
                out[o..o + 4].copy_from_slice(&[(acc[0] / acc[3]) as u8, (acc[1] / acc[3]) as u8, (acc[2] / acc[3]) as u8, (acc[3] / acc[4]) as u8]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let (mut text, mut apcs) = (Vec::new(), Vec::new());
        let mut s = ApcSplit::default();
        for c in chunks {
            s.split(c, |p| match p {
                Piece::Text(t) => text.extend_from_slice(t),
                Piece::Apc(a) => apcs.push(a.to_vec()),
            });
        }
        (text, apcs)
    }

    #[test]
    fn apc_strings_are_taken_out_across_reads() {
        let (text, apcs) = split_all(&[b"a\x1b[1mb\x1b_Gi=1;QUJD\x1b\\c"]);
        assert_eq!((text.as_slice(), apcs), (&b"a\x1b[1mbc"[..], vec![b"Gi=1;QUJD".to_vec()]));
        // Split at every awkward place: after ESC, inside, between ESC and backslash.
        let (text, apcs) = split_all(&[b"x\x1b", b"_Ga=q;", b"AAAA\x1b", b"\\y\x1b", b"]0;t\x07z"]);
        assert_eq!((text.as_slice(), apcs), (&b"xy\x1b]0;t\x07z"[..], vec![b"Ga=q;AAAA".to_vec()]));
    }

    #[test]
    fn transmit_place_query_and_delete() {
        let mut g = Graphics::default();
        // 2x1 RGB image, id 7: red, green.
        let out = g.command(b"Ga=T,f=24,s=2,v=1,i=7;/wAAAP8A").unwrap();
        assert_eq!(out.reply.as_deref(), Some("\x1b_Gi=7;OK\x1b\\"));
        let (id, keys) = out.place.unwrap();
        assert_eq!(g.images[&id].rgba, [255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(g.place(id, &keys, 5, 3, (10, 20), false), (1, 1));
        // Chunked (keys alone first, each chunk padded base64 like chafa sends), no id: no reply;
        // asked for 4 columns: scaled to 40 px wide, aspect kept.
        assert!(g.command(b"Ga=T,f=24,s=2,v=1,c=4,m=1").is_none());
        assert!(g.command(b"Gm=1;/w==").is_none());
        let out = g.command(b"Gm=0;AAAA/wA=").unwrap();
        assert!(out.reply.is_none());
        let (id2, keys) = out.place.unwrap();
        assert_eq!(g.place(id2, &keys, 6, 0, (10, 20), false), (4, 1));
        assert_eq!(g.placements[1].px, (40, 20));
        // Support query: a valid tiny image is OK, a broken one says why.
        assert_eq!(g.command(b"Ga=q,i=31,f=24,s=1,v=1;AAAA").unwrap().reply.as_deref(), Some("\x1b_Gi=31;OK\x1b\\"));
        assert!(g.command(b"Ga=q,i=31,f=100;AAAA").unwrap().reply.unwrap().contains("EBADPNG"));
        assert!(g.command(b"Ga=q,i=1,t=s;AAAA").unwrap().reply.unwrap().contains("EINVAL"));
        // Lines scrolled out of history take their images along (unnamed ones are freed).
        g.forget_before(7);
        assert!(g.placements.is_empty() && g.images.contains_key(&7) && !g.images.contains_key(&id2));
        g.command(b"Ga=d,d=I,i=7");
        assert!(g.images.is_empty());
    }

    #[test]
    fn png_images_decode() {
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 2, 2);
            enc.set_color(png::ColorType::Rgb);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]).unwrap();
        }
        let b64 = |d: &[u8]| {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            d.chunks(3).flat_map(|c| {
                let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
                (0..4).map(move |i| if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] } else { b'=' })
            }).collect::<Vec<u8>>()
        };
        let mut g = Graphics::default();
        let cmd = [b"Ga=t,f=100,i=2;".to_vec(), b64(&png)].concat();
        g.command(&cmd);
        assert_eq!((g.images[&2].w, g.images[&2].h, &g.images[&2].rgba[8..12]), (2, 2, &[0, 0, 255, 255][..]));
    }
}
