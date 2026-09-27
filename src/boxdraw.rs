//! Box-drawing (U+2500..257F) and block elements (U+2580..259F) drawn to fit the cell exactly,
//! so lines join across cells regardless of the font's metrics.

use crate::render::Renderer;

/// (left, right, up, down) arm weights for U+2500..=U+257F: 0 none, 1 light, 2 heavy, 3 double.
/// Dashed lines are drawn solid and rounded corners square; diagonals are left to the font.
#[rustfmt::skip]
const BOX: [[u8; 4]; 128] = [
    [1,1,0,0],[2,2,0,0],[0,0,1,1],[0,0,2,2],[1,1,0,0],[2,2,0,0],[0,0,1,1],[0,0,2,2],
    [1,1,0,0],[2,2,0,0],[0,0,1,1],[0,0,2,2],[0,1,0,1],[0,2,0,1],[0,1,0,2],[0,2,0,2],
    [1,0,0,1],[2,0,0,1],[1,0,0,2],[2,0,0,2],[0,1,1,0],[0,2,1,0],[0,1,2,0],[0,2,2,0],
    [1,0,1,0],[2,0,1,0],[1,0,2,0],[2,0,2,0],[0,1,1,1],[0,2,1,1],[0,1,2,1],[0,1,1,2],
    [0,1,2,2],[0,2,2,1],[0,2,1,2],[0,2,2,2],[1,0,1,1],[2,0,1,1],[1,0,2,1],[1,0,1,2],
    [1,0,2,2],[2,0,2,1],[2,0,1,2],[2,0,2,2],[1,1,0,1],[2,1,0,1],[1,2,0,1],[2,2,0,1],
    [1,1,0,2],[2,1,0,2],[1,2,0,2],[2,2,0,2],[1,1,1,0],[2,1,1,0],[1,2,1,0],[2,2,1,0],
    [1,1,2,0],[2,1,2,0],[1,2,2,0],[2,2,2,0],[1,1,1,1],[2,1,1,1],[1,2,1,1],[2,2,1,1],
    [1,1,2,1],[1,1,1,2],[1,1,2,2],[2,1,2,1],[1,2,2,1],[2,1,1,2],[1,2,1,2],[2,2,2,1],
    [2,2,1,2],[2,1,2,2],[1,2,2,2],[2,2,2,2],[1,1,0,0],[2,2,0,0],[0,0,1,1],[0,0,2,2],
    [3,3,0,0],[0,0,3,3],[0,3,0,1],[0,1,0,3],[0,3,0,3],[3,0,0,1],[1,0,0,3],[3,0,0,3],
    [0,3,1,0],[0,1,3,0],[0,3,3,0],[3,0,1,0],[1,0,3,0],[3,0,3,0],[0,3,1,1],[0,1,3,3],
    [0,3,3,3],[3,0,1,1],[1,0,3,3],[3,0,3,3],[3,3,0,1],[1,1,0,3],[3,3,0,3],[3,3,1,0],
    [1,1,3,0],[3,3,3,0],[3,3,1,1],[1,1,3,3],[3,3,3,3],[0,1,0,1],[1,0,0,1],[1,0,1,0],
    [0,1,1,0],[0,0,0,0],[0,0,0,0],[0,0,0,0],[1,0,0,0],[0,0,1,0],[0,1,0,0],[0,0,0,1],
    [2,0,0,0],[0,0,2,0],[0,2,0,0],[0,0,0,2],[1,2,0,0],[0,0,1,2],[2,1,0,0],[0,0,2,1],
];

/// Half of a group's thickness (in px) on one side of the centre line.
fn half(w: u8, lt: isize) -> isize {
    match w {
        0 => 0,
        1 => (lt + 1) / 2,
        2 => lt,
        _ => (3 * lt + 1) / 2,
    }
}

impl Renderer {
    /// Draw `ch` into the cell at (x, y) if it is a box/block character. Returns false to let
    /// the font handle it.
    pub(crate) fn draw_special(&mut self, ch: char, x: usize, y: usize, cw: usize, chh: usize, fg: u32, bg: u32) -> bool {
        match ch as u32 {
            c @ 0x2500..=0x257F if BOX[(c - 0x2500) as usize] != [0; 4] => {
                let [l, r, u, d] = BOX[(c - 0x2500) as usize];
                let lt = (cw / 8).max(1) as isize;
                let (cx, cy) = ((cw / 2) as isize, (chh / 2) as isize);
                self.box_arms(x, y, (cw, chh), (cx, cy), lt, [l, r], [u, d], false, fg);
                self.box_arms(x, y, (chh, cw), (cy, cx), lt, [u, d], [l, r], true, fg);
                true
            }
            c @ 0x2580..=0x259F => {
                self.block(c, x, y, cw, chh, fg, bg);
                true
            }
            // Powerline's solid arrows, drawn to fill the whole cell so prompt segments join up.
            c @ (0xE0B0 | 0xE0B2) => {
                for r in 0..chh {
                    // Arrow width on this row, from the pixel row's centre.
                    let tip = 1.0 - ((2 * r + 1) as f32 - chh as f32).abs() / chh as f32;
                    let reach = tip * cw as f32;
                    let full = (reach as usize).min(cw);
                    // A solid run from the flat side, then one antialiased edge pixel.
                    let edge = crate::render::mix_color(fg, bg, (reach.fract() * 100.0) as u32);
                    if c == 0xE0B0 {
                        self.fill(x, y + r, full, 1, fg);
                        if full < cw {
                            self.fill(x + full, y + r, 1, 1, edge);
                        }
                    } else {
                        self.fill(x + cw - full, y + r, full, 1, fg);
                        if full < cw {
                            self.fill(x + cw - full - 1, y + r, 1, 1, edge);
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Draw the two arms of one axis. Coordinates are (along, perpendicular) to that axis;
    /// `transposed` maps them back to (y, x).
    #[allow(clippy::too_many_arguments)]
    fn box_arms(&mut self, x: usize, y: usize, len: (usize, usize), centre: (isize, isize), lt: isize, arms: [u8; 2], perp: [u8; 2], transposed: bool, fg: u32) {
        let (ca, cp) = centre;
        let ph = perp.iter().map(|&w| half(w, lt)).max().unwrap_or(0);
        let perp_double = perp.contains(&3);
        for (i, &w) in arms.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let dir: isize = if i == 0 { -1 } else { 1 };
            let other = arms[1 - i];
            // Lines of this arm: (offset from centre, thickness).
            let lines: &[(isize, isize)] = match w {
                1 => &[(0, lt)],
                2 => &[(0, 2 * lt)],
                _ => &[(-lt, lt), (lt, lt)],
            };
            for &(off, t) in lines {
                let near = if ph == 0 {
                    ca
                } else if w == 3 && perp_double {
                    let side = off.signum();
                    let same_side = (side < 0 && perp[0] > 0) || (side > 0 && perp[1] > 0);
                    if same_side {
                        ca + dir * (lt / 2)
                    } else if other > 0 {
                        ca
                    } else {
                        // Outer line of a corner runs out to the far edge of the vertical pair.
                        ca - dir * ((3 * lt + 1) / 2)
                    }
                } else {
                    ca - dir * ph
                };
                let (a0, a1) = if dir < 0 { (0, near) } else { (near, len.0 as isize) };
                let p0 = cp + off - t / 2;
                let (a0, a1, p0, p1) = (a0.max(0), a1.min(len.0 as isize), p0.max(0), (p0 + t).min(len.1 as isize));
                if a1 <= a0 || p1 <= p0 {
                    continue;
                }
                let (a0, a1, p0, p1) = (a0 as usize, a1 as usize, p0 as usize, p1 as usize);
                if transposed {
                    self.fill(x + p0, y + a0, p1 - p0, a1 - a0, fg);
                } else {
                    self.fill(x + a0, y + p0, a1 - a0, p1 - p0, fg);
                }
            }
        }
    }

    fn block(&mut self, c: u32, x: usize, y: usize, cw: usize, chh: usize, fg: u32, bg: u32) {
        // Rectangles in eighths of the cell: (x0, y0, x1, y1).
        let (e, f) = (|n: usize| n * cw / 8, |n: usize| n * chh / 8);
        let mut rects: Vec<(usize, usize, usize, usize)> = Vec::new();
        match c {
            0x2580 => rects.push((0, 0, 8, 4)),
            0x2581..=0x2588 => rects.push((0, 8 - (c - 0x2580) as usize, 8, 8)),
            0x2589..=0x258F => rects.push((0, 0, 8 - (c - 0x2588) as usize, 8)),
            0x2590 => rects.push((4, 0, 8, 8)),
            0x2591..=0x2593 => {
                let a = (c - 0x2590) as u32 * 64 - 1; // 25%, 50%, 75%
                let mix = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
                let color = (mix(fg >> 16 & 255, bg >> 16 & 255) << 16) | (mix(fg >> 8 & 255, bg >> 8 & 255) << 8) | mix(fg & 255, bg & 255);
                self.fill(x, y, cw, chh, color);
            }
            0x2594 => rects.push((0, 0, 8, 1)),
            0x2595 => rects.push((7, 0, 8, 8)),
            _ => {
                // Quadrants U+2596..259F as (upper-left, upper-right, lower-left, lower-right).
                let q: [bool; 4] = match c {
                    0x2596 => [false, false, true, false],
                    0x2597 => [false, false, false, true],
                    0x2598 => [true, false, false, false],
                    0x2599 => [true, false, true, true],
                    0x259A => [true, false, false, true],
                    0x259B => [true, true, true, false],
                    0x259C => [true, true, false, true],
                    0x259D => [false, true, false, false],
                    0x259E => [false, true, true, false],
                    _ => [false, true, true, true],
                };
                for (i, on) in q.into_iter().enumerate() {
                    if on {
                        let (qx, qy) = ((i % 2) * 4, (i / 2) * 4);
                        rects.push((qx, qy, qx + 4, qy + 4));
                    }
                }
            }
        }
        for (x0, y0, x1, y1) in rects {
            self.fill(x + e(x0), y + f(y0), e(x1) - e(x0), f(y1) - f(y0), fg);
        }
    }
}
