//! `litty img FILE...`: show PNG and JPEG pictures in the terminal, with no other tools.
//! Pictures are shrunk to fit the window before they are sent (so the terminal never holds a
//! full-size photo), turned upright by their EXIF orientation, and sent with the Kitty graphics
//! protocol, so they also show over ssh when the remote end has litty.

use nix::libc;
use std::io::Write;

pub fn main(files: &[String]) -> i32 {
    if files.is_empty() || files.iter().any(|f| f == "-h" || f == "--help") {
        eprintln!("usage: litty img FILE...   (PNG or JPEG)");
        return 2;
    }
    // SAFETY: isatty and TIOCGWINSZ only read the terminal's state into `ws`.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::isatty(1) } == 0 || unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } != 0 {
        eprintln!("litty img: output is not a terminal");
        return 1;
    }
    // Room for the picture: the window's text area in pixels, less a row for the prompt after it.
    let (cols, rows) = (ws.ws_col as usize, ws.ws_row as usize);
    let (px_w, px_h) = (ws.ws_xpixel as usize, ws.ws_ypixel as usize);
    let cell_h = if rows > 0 { px_h / rows } else { 0 };
    let room = (px_w.max(cols * 8), px_h.saturating_sub(cell_h).max(cell_h));
    let mut status = 0;
    let mut out = std::io::stdout().lock();
    for f in files {
        match load(f).map(|img| fit(img, room)) {
            Ok((w, h, rgba)) => {
                let _ = out.write_all(&encode(w, h, &rgba));
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
            Err(e) => {
                eprintln!("litty img: {f}: {e}");
                status = 1;
            }
        }
    }
    status
}

/// (width, height, straight RGBA) of a PNG or JPEG file.
fn load(path: &str) -> Result<(usize, usize, Vec<u8>), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.starts_with(b"\x89PNG") {
        return crate::graphics::decode_png(&data).map_err(|e| e.split(':').next_back().unwrap_or(e).to_string());
    }
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err("not a PNG or JPEG picture".into());
    }
    use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(20_000).set_max_height(20_000);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(&data[..], options);
    let rgba = dec.decode().map_err(|e| format!("bad JPEG ({e:?})"))?;
    let info = dec.info().ok_or("bad JPEG")?;
    let (w, h) = (info.width as usize, info.height as usize);
    if rgba.len() != w * h * 4 {
        return Err("unsupported JPEG colour format".into());
    }
    Ok(orient((w, h, rgba), dec.exif().map_or(1, |e| orientation(e))))
}

/// The EXIF orientation tag (1 = upright) from a TIFF-structured EXIF block.
fn orientation(tiff: &[u8]) -> u16 {
    let le = tiff.starts_with(b"II");
    let u16_at = |i: usize| tiff.get(i..i + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) });
    let u32_at = |i: usize| tiff.get(i..i + 4).map(|b| if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) });
    let Some(ifd) = u32_at(4).map(|o| o as usize) else { return 1 };
    let n = u16_at(ifd).unwrap_or(0) as usize;
    (0..n.min(64)).map(|i| ifd + 2 + i * 12).find(|&e| u16_at(e) == Some(0x0112)).and_then(|e| u16_at(e + 8)).filter(|o| (1..=8).contains(o)).unwrap_or(1)
}

/// Turn pixels so the picture is upright (EXIF orientations 2–8 are flips and quarter turns).
fn orient((w, h, src): (usize, usize, Vec<u8>), o: u16) -> (usize, usize, Vec<u8>) {
    if o <= 1 {
        return (w, h, src);
    }
    let (nw, nh) = if o >= 5 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; src.len()];
    for y in 0..nh {
        for x in 0..nw {
            // Where the upright pixel (x, y) comes from in the stored picture.
            let (sx, sy) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                _ => (w - 1 - y, x),
            };
            out[(y * nw + x) * 4..][..4].copy_from_slice(&src[(sy * w + sx) * 4..][..4]);
        }
    }
    (nw, nh, out)
}

/// Shrink (never enlarge) to fit `room` pixels, keeping the aspect ratio.
fn fit((w, h, rgba): (usize, usize, Vec<u8>), room: (usize, usize)) -> (usize, usize, Vec<u8>) {
    let scale = (room.0 as f64 / w as f64).min(room.1 as f64 / h as f64);
    if scale >= 1.0 {
        return (w, h, rgba);
    }
    let (nw, nh) = (((w as f64 * scale) as usize).max(1), ((h as f64 * scale) as usize).max(1));
    (nw, nh, crate::graphics::scale(&rgba, w, h, nw, nh))
}

/// The escape sequences that show the picture at the cursor: zlib-compressed RGBA, base64, in
/// 4 KB chunks. Replies are turned off (q=2) so nothing ends up in the shell's input.
fn encode(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let zipped = miniz_oxide::deflate::compress_to_vec_zlib(rgba, 6);
    let mut b64 = Vec::with_capacity(zipped.len().div_ceil(3) * 4);
    for c in zipped.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            b64.push(if i <= c.len() { B64[(n >> (18 - 6 * i) & 63) as usize] } else { b'=' });
        }
    }
    let mut out = Vec::with_capacity(b64.len() + b64.len() / 4096 * 16 + 64);
    let chunks: Vec<&[u8]> = b64.chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        if i == 0 {
            out.extend(format!("\x1b_Ga=T,q=2,f=32,o=z,s={w},v={h},m={more};").as_bytes());
        } else {
            out.extend(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::{ApcSplit, Graphics, Piece};

    #[test]
    fn what_litty_img_sends_is_what_litty_shows() {
        // A 3x2 picture with distinct pixels, through encode → the terminal's own parser.
        let rgba: Vec<u8> = (0..24).map(|i| (i * 10) as u8).collect();
        let (mut split, mut g, mut shown) = (ApcSplit::default(), Graphics::default(), None);
        split.split(&encode(3, 2, &rgba), |p| {
            if let Piece::Apc(a) = p {
                if let Some(out) = g.command(a) {
                    assert!(out.reply.is_none(), "q=2 keeps replies out of the shell");
                    shown = out.place.map(|(id, _)| id);
                }
            }
        });
        let img = &g.images[&shown.expect("placed")];
        assert_eq!((img.w, img.h, &img.rgba), (3, 2, &rgba));
    }

    #[test]
    fn pictures_are_fitted_and_turned_upright() {
        let px = |v: u8| [v, v, v, 255];
        // 2x1: left 1, right 2. Orientation 6 (turned 90° clockwise to be upright) → 1x2 with 1 on top.
        let img = (2, 1, [px(1), px(2)].concat());
        assert_eq!(orient(img.clone(), 6), (1, 2, [px(1), px(2)].concat()));
        assert_eq!(orient(img.clone(), 3), (2, 1, [px(2), px(1)].concat()));
        assert_eq!(fit((400, 200, vec![0; 400 * 200 * 4]), (100, 100)).0, 100);
        assert_eq!(fit(img.clone(), (100, 100)), img, "small pictures are not enlarged");
        // EXIF: big-endian TIFF, one IFD entry: orientation = 6.
        let tiff = [b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0".as_slice(), &[0; 4]].concat();
        assert_eq!(orientation(&tiff), 6);
    }
}
