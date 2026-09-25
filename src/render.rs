use crate::font::{Fonts, Glyph};
use crate::grid::{ANSI, BOLD, Cell, CursorShape, DEF_BG, Grid, ITALIC, Mark, UNDERLINE};

const SEL_BG: u32 = 0x33467c;
const MATCH_BG: u32 = 0x54432a;
const CUR_MATCH_BG: u32 = 0xe0af68;
const FAIL_TINT: u32 = 0x29212d;

const FAIL_TEXT: u32 = 0xf7768e;
const ACCENT: u32 = 0x7aa2f7;

// Tab bar.
const DIVIDER: u32 = 0x2f334d;
const CURSOR_BAR: u32 = 0xc0caf5;

const BAR_BG: u32 = 0x16161e;
const TAB_TEXT: u32 = 0x565f89;
const TAB_TEXT_ACTIVE: u32 = 0xc0caf5;

const RAIL_TRACK: u32 = 0x1f2130;
const RAIL_THUMB: u32 = 0x414868;
const RAIL_THUMB_ACTIVE: u32 = ACCENT;
const MATCH_TICK: u32 = 0xe0af68;

const BADGE_OK: u32 = 0x565f89;

fn failed(m: &Mark) -> bool {
    m.end.is_some() && m.exit != Some(0)
}

/// "exit 127 · 1.2s": shown after slow or failed commands.
fn badge(m: &Mark) -> Option<String> {
    let secs = m.took?.as_secs_f32();
    let mut parts = Vec::new();
    if failed(m) {
        parts.push(format!("exit {}", m.exit.unwrap_or(1)));
    }
    if secs >= 0.5 {
        parts.push(if secs < 60.0 { format!("{secs:.1}s") } else { format!("{}m{:02}s", secs as u32 / 60, secs as u32 % 60) });
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && y >= self.y as f64 && x < (self.x + self.w) as f64 && y < (self.y + self.h) as f64
    }
}

/// How to draw one pane's grid.
pub struct PaneView<'a> {
    pub rect: Rect,
    /// The pane with keyboard focus; others are dimmed and show an outline cursor.
    pub focused: bool,
    /// False during the "off" half of a blinking cursor.
    pub cursor_on: bool,
    pub find: Option<&'a str>,
    /// Update notice, drawn in the bottom-right corner of the pane.
    pub notice: Option<&'a str>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TabHit {
    Tab(usize),
    Close(usize),
    New,
    None,
}

#[allow(clippy::too_many_arguments)]
fn blend_glyph(fb: &mut [u32], w: usize, clip: [usize; 4], g: &Glyph, ox: usize, oy: usize, ascent: i32, fg: u32) {
    let [cx0, cy0, cx1, cy1] = clip.map(|v| v as i32);
    let gx = ox as i32 + g.m.xmin;
    let gy = oy as i32 + ascent - g.m.ymin - g.m.height as i32;
    let (fr, fgc, fb_) = ((fg >> 16) & 255, (fg >> 8) & 255, fg & 255);
    for row in 0..g.m.height {
        let py = gy + row as i32;
        if py < cy0 || py >= cy1 {
            continue;
        }
        for col in 0..g.m.width {
            let px = gx + col as i32;
            let a = g.bmp[row * g.m.width + col] as u32;
            if a == 0 || px < cx0 || px >= cx1 {
                continue;
            }
            let dst = &mut fb[py as usize * w + px as usize];
            *dst = if a == 255 {
                fg
            } else {
                let mix = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
                (mix(fr, (*dst >> 16) & 255) << 16) | (mix(fgc, (*dst >> 8) & 255) << 8) | mix(fb_, *dst & 255)
            };
        }
    }
}

/// Blend `a` toward `b`: result = a*pct% + b*(100-pct)%.
fn mix_color(a: u32, b: u32, pct: u32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 255, (b >> shift) & 255);
        (x * pct + y * (100 - pct)) / 100
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Characters that can take part in programming ligatures (`=>`, `!=`, `->`, `<=`, ...).
fn is_symbol(c: char) -> bool {
    "=<>-!|&:+*/~#_.?%$\\^@;".contains(c)
}

/// CPU renderer: keeps a persistent framebuffer and repaints only dirty rows.
pub struct Renderer {
    pub fonts: Fonts,
    pub fb: Vec<u32>,
    pub w: usize,
    pub h: usize,
    pub pad: usize,
    /// Height of the tab bar at the top (0 with a single tab).
    pub bar_h: usize,
    tmp: Vec<Cell>,
    /// Rows (first, last + 1) whose pixels changed since the last `take_damage`.
    damage: Option<(usize, usize)>,
    /// Glyphs are only drawn inside this (x0, y0, x1, y1): one that overhangs its cell would leave
    /// pixels behind that no later repaint of the row erases.
    clip: [usize; 4],
}

impl Renderer {
    pub fn new(px: f32, pad: usize) -> Self {
        Renderer {
            fonts: Fonts::new(px),
            fb: Vec::new(),
            w: 0,
            h: 0,
            pad,
            bar_h: 0,
            tmp: Vec::new(),
            damage: None,
            clip: [0; 4],
        }
    }

    /// One "point" in pixels; sizes of the gutter and rail scale with it.
    pub(crate) fn unit(&self) -> usize {
        (self.pad / 10).max(1)
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.fb = vec![DEF_BG; w * h];
        self.damage = Some((0, h));
        self.clip = [0, 0, w, h];
    }

    /// The area panes are laid out in: the window minus padding and the tab bar.
    pub fn area(&self) -> Rect {
        Rect {
            x: self.pad,
            y: self.pad + self.bar_h,
            w: self.w.saturating_sub(2 * self.pad),
            h: self.h.saturating_sub(2 * self.pad + self.bar_h),
        }
    }

    /// Whole cells that fit in `rect` (at least 1x1).
    pub fn grid_size(&self, rect: Rect) -> (usize, usize) {
        ((rect.w / self.fonts.cell_w).max(1), (rect.h / self.fonts.cell_h).max(1))
    }

    /// Forget everything drawn so far (layout changed): the next draws repaint from scratch.
    pub fn clear(&mut self) {
        self.fb.fill(DEF_BG);
        self.damage = Some((0, self.h));
    }

    /// The rows changed since the last call, for presenting only those.
    pub fn take_damage(&mut self) -> Option<(usize, usize)> {
        self.damage.take()
    }

    fn mark(&mut self, y0: usize, y1: usize) {
        let (y0, y1) = (y0.min(self.h), y1.min(self.h));
        if y0 < y1 {
            self.damage = Some(self.damage.map_or((y0, y1), |(a, b)| (a.min(y0), b.max(y1))));
        }
    }

    /// Top-left pixel of the cursor cell of a grid drawn in `rect`.
    pub fn cursor_px(&self, g: &Grid, rect: Rect) -> (usize, usize) {
        (rect.x + g.cx.min(g.cols - 1) * self.fonts.cell_w, rect.y + g.cy * self.fonts.cell_h)
    }

    /// Scrollback position (lines back from live) that centres the view at pixel row `y` of the rail.
    pub fn scroll_for_rail_y(&self, g: &Grid, y: f64) -> usize {
        let a = self.area();
        let (y0, y1) = (a.y as f64, (a.y + a.h).max(a.y + 1) as f64);
        let frac = ((y - y0) / (y1 - y0)).clamp(0.0, 1.0);
        let (hist, rows) = (g.history_len(), g.rows);
        let top = (frac * (hist + rows) as f64) as isize - rows as isize / 2;
        hist - top.clamp(0, hist as isize) as usize
    }

    /// Draw the dirty rows of one pane's grid into `view.rect`.
    pub fn draw_pane(&mut self, g: &mut Grid, view: &PaneView) {
        // While scrolled back, dirty tracking is by view row, so repaint everything.
        if g.scroll != g.drawn_scroll || g.scroll > 0 {
            g.dirty.fill(true);
        }
        g.drawn_scroll = g.scroll;
        if g.drawn_cursor_row < g.rows {
            g.dirty[g.drawn_cursor_row] = true;
        }
        g.dirty[g.cy] = true;
        for y in 0..g.rows {
            if std::mem::take(&mut g.dirty[y]) {
                self.draw_row(g, y, view);
            }
        }
        g.drawn_cursor_row = g.cy;
        if let Some(q) = view.find {
            self.draw_find_bar(g, q, view.rect);
        }
        if let Some(n) = view.notice {
            self.draw_notice(n, view.rect);
        }
    }

    /// Everything outside the panes: divider lines, the rail (single pane only) and the tab bar.
    pub fn draw_chrome(&mut self, dividers: &[Rect], rail: Option<&Grid>, tabs: &[String], active: usize) {
        let u = self.unit();
        for d in dividers {
            let (w, h) = (d.w.min(u), d.h.min(u));
            let (x, y) = (d.x + (d.w - w) / 2, d.y + (d.h - h) / 2);
            // The rect is a gap between panes: draw a hairline through its middle along its length.
            if d.w < d.h { self.fill(x, d.y, w, d.h, DIVIDER) } else { self.fill(d.x, y, d.w, h, DIVIDER) }
        }
        if let Some(g) = rail {
            self.draw_rail(g);
        }
        if self.bar_h > 0 {
            self.draw_tabs(tabs, active);
        }
    }

    fn draw_row(&mut self, g: &Grid, y: usize, view: &PaneView) {
        let (cw, ch) = (self.fonts.cell_w, self.fonts.cell_h);
        let (ox, oy) = (view.rect.x, view.rect.y + y * ch);
        self.clip = [ox, oy, (ox + g.cols * cw).min(self.w), (oy + ch).min(self.h)];
        let id = g.abs_row(y);
        let cursor_row = g.cursor_visible && g.scroll == 0 && y == g.cy;
        // Only the block cursor recolours its cell; other shapes are drawn over the text.
        let has_cursor = cursor_row && view.focused && view.cursor_on && g.cursor_shape == CursorShape::Block;
        let focused = view.focused;
        let cursor_x = g.cx.min(g.cols - 1);
        let sel = g.sel_cols(y);
        let matches = g.row_matches(id);
        let current = g.matches.get(g.cur_match).copied();
        let mark = g.mark_at(id);
        let tint = mark.is_some_and(|m| failed(m) && id >= m.out.unwrap_or(m.start));
        let colors = |x: usize, c: &Cell| {
            let mut fg = c.fg;
            // Bold text uses the bright variant of the 8 base colours.
            if c.attrs & BOLD != 0 {
                if let Some(i) = ANSI[..8].iter().position(|&a| a == fg) {
                    fg = ANSI[i + 8];
                }
            }
            let mut bg = if tint && c.bg == DEF_BG { FAIL_TINT } else { c.bg };
            if let Some(m) = matches.iter().find(|m| x >= m.1 && x < m.1 + m.2) {
                (fg, bg) = if Some(*m) == current { (DEF_BG, CUR_MATCH_BG) } else { (fg, MATCH_BG) };
            }
            let (fg, bg) = if has_cursor && x == cursor_x {
                (c.bg, c.fg)
            } else if sel.is_some_and(|(a, b)| x >= a && x < b) {
                (fg, SEL_BG)
            } else {
                (fg, bg)
            };
            // Unfocused panes are dimmed.
            if focused { (fg, bg) } else { (mix_color(fg, bg, 90), bg) }
        };
        let thick = (ch / 16).clamp(1, 3);
        let mut tmp = std::mem::take(&mut self.tmp);
        let line = g.view_row(y, &mut tmp);

        // Backgrounds first so wide/overhanging glyphs are not painted over by the next cell.
        let cols: Vec<(u32, u32)> = line.iter().enumerate().map(|(x, c)| colors(x, c)).collect();
        let mut x = 0;
        while x < cols.len() {
            let bg = cols[x].1;
            let run = cols[x..].iter().take_while(|c| c.1 == bg).count();
            self.fill(ox + x * cw, oy, run * cw, ch, bg);
            x += run;
        }
        let ligatures = if self.fonts.ligatures { self.shape_ligatures(line) } else { Vec::new() };
        for (x, c) in line.iter().enumerate() {
            let (fg, bg) = cols[x];
            let style = c.attrs & (BOLD | ITALIC);
            if self.draw_special(c.ch, ox + x * cw, oy, cw, ch, fg, bg) {
                // Box and block characters are drawn to fit the cell.
            } else if let Some(&id) = ligatures.get(x).and_then(Option::as_ref) {
                self.glyph_id(id, style, ox + x * cw, oy, fg);
            } else if c.ch != ' ' && c.ch != '\0' {
                self.glyph(c.ch, style, ox + x * cw, oy, fg);
                // Combining marks have zero advance and negative xmin: they expect the pen at
                // the end of the base glyph.
                if c.comb[0] != '\0' {
                    let adv = self.fonts.glyph(c.ch, style).m.advance_width.round() as usize;
                    for &m in c.comb.iter().filter(|&&m| m != '\0') {
                        self.glyph(m, style, ox + x * cw + adv, oy, fg);
                    }
                }
            }
            if c.attrs & UNDERLINE != 0 {
                let uy = (self.fonts.ascent as usize + 2).min(ch - thick);
                self.fill(ox + x * cw, oy + uy, cw, thick, fg);
            }
        }
        if cursor_row {
            let (cx0, u) = (ox + cursor_x * cw, self.unit());
            if !focused {
                // Hollow block so an inactive pane still shows where its cursor is.
                let cell_fg = mix_color(line[cursor_x].fg, DEF_BG, 90);
                self.fill(cx0, oy, cw, u, cell_fg);
                self.fill(cx0, oy + ch - u, cw, u, cell_fg);
                self.fill(cx0, oy, u, ch, cell_fg);
                self.fill(cx0 + cw - u, oy, u, ch, cell_fg);
            } else if view.cursor_on {
                match g.cursor_shape {
                    CursorShape::Block => {}
                    CursorShape::Underline => self.fill(cx0, oy + ch - 2 * u, cw, 2 * u, CURSOR_BAR),
                    CursorShape::Bar => self.fill(cx0, oy, 2 * u, ch, CURSOR_BAR),
                }
            }
        }
        // Exit status / duration on the command line, if there is room.
        if let Some(text) = mark.filter(|m| m.out.is_some_and(|o| o == id + 1)).and_then(badge) {
            let n = text.chars().count();
            if n + 2 <= line.len() && line[line.len() - n - 2..].iter().all(|c| c.ch == ' ' && c.bg == DEF_BG) {
                let color = if mark.is_some_and(failed) { FAIL_TEXT } else { BADGE_OK };
                self.text(&text, ox + (line.len() - n - 1) * cw, oy, color);
            }
        }
        self.tmp = tmp;
        self.clip = [0, 0, self.w, self.h];
    }

    /// Right rail: scroll position over the whole history, with ticks for failed commands
    /// and find matches.
    fn draw_rail(&mut self, g: &Grid) {
        let u = self.unit();
        let rw = 4 * u;
        if self.w < self.pad + rw {
            return;
        }
        let rx = self.w - self.pad + self.pad.saturating_sub(rw) / 2;
        let (y0, y1) = (self.pad + self.bar_h, self.h.saturating_sub(self.pad));
        if y1 <= y0 {
            return;
        }
        let th = y1 - y0;
        self.fill(rx, self.bar_h, rw, self.h - self.bar_h, DEF_BG);
        let hist = g.history_len();
        if hist == 0 {
            return;
        }
        let total = hist + g.rows;
        let at = |line: u64| y0 + ((line - (g.pushed - hist as u64)) as usize * th / total).min(th - 1);
        self.fill(rx, y0, rw, th, RAIL_TRACK);
        for m in g.marks.iter().filter(|m| failed(m) && m.start + hist as u64 >= g.pushed) {
            self.fill(rx, at(m.start), rw, (2 * u).min(th), FAIL_TEXT);
        }
        for &(id, ..) in g.matches.iter().filter(|m| m.0 + hist as u64 >= g.pushed) {
            self.fill(rx, at(id), rw, u, MATCH_TICK);
        }
        let ty = y0 + (hist - g.scroll) * th / total;
        let len = (g.rows * th / total).max(12 * u).min(th);
        let color = if g.scroll > 0 { RAIL_THUMB_ACTIVE } else { RAIL_THUMB };
        // Thumb is drawn inset so ticks stay visible on both sides.
        self.fill(rx + u, ty.min(y1 - len), rw - 2 * u, len, color);
    }

    pub fn tab_bar_height(&self) -> usize {
        self.fonts.cell_h + 6 * self.unit()
    }

    /// Width of each tab and of the "+" button that follows the last tab.
    fn tab_widths(&self, n: usize) -> (usize, usize) {
        let plus = self.bar_h;
        ((self.w.saturating_sub(plus) / n.max(1)).min(26 * self.fonts.cell_w), plus)
    }

    /// What is under pixel `x` in the tab bar.
    pub fn tab_hit(&self, x: usize, n: usize) -> TabHit {
        let (tw, plus) = self.tab_widths(n);
        if tw == 0 {
            return TabHit::None;
        }
        let i = x / tw;
        if i < n {
            // The last two cells of a tab are its close button.
            if x % tw >= tw.saturating_sub(2 * self.fonts.cell_w) { TabHit::Close(i) } else { TabHit::Tab(i) }
        } else if x < n * tw + plus {
            TabHit::New
        } else {
            TabHit::None
        }
    }

    fn draw_tabs(&mut self, titles: &[String], active: usize) {
        let (u, cw, ch) = (self.unit(), self.fonts.cell_w, self.fonts.cell_h);
        let (tw, plus) = self.tab_widths(titles.len());
        self.fill(0, 0, self.w, self.bar_h, BAR_BG);
        let ty = (self.bar_h - ch) / 2;
        for (i, title) in titles.iter().enumerate() {
            let x = i * tw;
            let on = i == active;
            if on {
                self.fill(x, 0, tw, self.bar_h, DEF_BG);
                self.fill(x, 0, tw, 2 * u, ACCENT);
            }
            let room = (tw / cw).saturating_sub(5).max(1);
            let label: String = if title.chars().count() > room {
                title.chars().take(room - 1).chain(['…']).collect()
            } else {
                title.clone()
            };
            let color = if on { TAB_TEXT_ACTIVE } else { TAB_TEXT };
            self.text(&label, x + 2 * cw, ty, color);
            self.text("×", x + tw - 2 * cw, ty, TAB_TEXT);
        }
        let px = titles.len() * tw;
        self.text("+", px + (plus.saturating_sub(cw)) / 2, ty, TAB_TEXT);
    }

    fn draw_find_bar(&mut self, g: &Grid, query: &str, rect: Rect) {
        let (u, cw, ch) = (self.unit(), self.fonts.cell_w, self.fonts.cell_h);
        let status = if g.matches.is_empty() {
            if query.is_empty() { String::new() } else { "no matches".into() }
        } else {
            format!("{}/{}", g.cur_match + 1, g.matches.len())
        };
        let label = format!("find: {query}|  {status}");
        let (bw, bh) = (label.chars().count() * cw + 8 * u, ch + 4 * u);
        let bx = (rect.x + rect.w).saturating_sub(6 * u + bw).max(rect.x);
        let by = rect.y;
        self.fill(bx, by, bw, bh, 0x7aa2f7);
        self.fill(bx + u, by + u, bw - 2 * u, bh - 2 * u, 0x24283b);
        self.text(&label, bx + 4 * u, by + 2 * u, 0xc0caf5);
    }

    fn draw_notice(&mut self, text: &str, rect: Rect) {
        let (u, cw, ch) = (self.unit(), self.fonts.cell_w, self.fonts.cell_h);
        let fit = rect.w.saturating_sub(14 * u) / cw;
        let label: String = text.chars().take(fit).collect();
        let (bw, bh) = (label.chars().count() * cw + 8 * u, ch + 4 * u);
        let bx = (rect.x + rect.w).saturating_sub(6 * u + bw).max(rect.x);
        let by = (rect.y + rect.h).saturating_sub(bh + 2 * u).max(rect.y);
        self.fill(bx, by, bw, bh, 0x565f89);
        self.fill(bx + u, by + u, bw - 2 * u, bh - 2 * u, 0x24283b);
        self.text(&label, bx + 4 * u, by + 2 * u, 0xc0caf5);
    }

    /// Draw a single line of text at pixel (x, y) using the monospace grid.
    fn text(&mut self, s: &str, x: usize, y: usize, color: u32) {
        let cw = self.fonts.cell_w;
        for (i, c) in s.chars().enumerate() {
            if c != ' ' {
                self.glyph(c, 0, x + i * cw, y, color);
            }
        }
    }

    /// For each cell, the glyph id chosen by shaping when a run of ASCII text (same style)
    /// contains adjacent symbol characters; other cells are None and use the normal path.
    fn shape_ligatures(&mut self, line: &[Cell]) -> Vec<Option<u16>> {
        let mut ids = vec![None; line.len()];
        let is_ascii = |c: &Cell| (' '..='~').contains(&c.ch) && c.comb[0] == '\0';
        let mut x = 0;
        while x < line.len() {
            if !is_ascii(&line[x]) {
                x += 1;
                continue;
            }
            let style = line[x].attrs & (BOLD | ITALIC);
            let end = (x..line.len()).find(|&i| !is_ascii(&line[i]) || line[i].attrs & (BOLD | ITALIC) != style).unwrap_or(line.len());
            let run = &line[x..end];
            if run.windows(2).any(|w| is_symbol(w[0].ch) && is_symbol(w[1].ch)) {
                let text: Vec<char> = run.iter().map(|c| c.ch).collect();
                if let Some(shaped) = self.fonts.shape(&text, style) {
                    for (i, id) in shaped.into_iter().enumerate() {
                        ids[x + i] = Some(id);
                    }
                }
            }
            x = end;
        }
        ids
    }

    pub(crate) fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x1 = (x + w).min(self.w);
        if x >= x1 {
            return;
        }
        for py in y..(y + h).min(self.h) {
            let line = &mut self.fb[py * self.w + x..py * self.w + x1];
            if line.iter().any(|&p| p != color) {
                line.fill(color);
                self.mark(py, py + 1);
            }
        }
    }

    fn glyph(&mut self, ch: char, style: u8, ox: usize, oy: usize, fg: u32) {
        let ascent = self.fonts.ascent;
        let g = self.fonts.glyph(ch, style);
        let (y0, h) = (oy as i32 + ascent - g.m.ymin - g.m.height as i32, g.m.height);
        blend_glyph(&mut self.fb, self.w, self.clip, g, ox, oy, ascent, fg);
        self.mark((y0.max(0) as usize).max(self.clip[1]), ((y0 + h as i32).max(0) as usize).min(self.clip[3]));
    }

    /// Draw a glyph chosen by id (from a shaped run) instead of by character.
    fn glyph_id(&mut self, id: u16, style: u8, ox: usize, oy: usize, fg: u32) {
        let ascent = self.fonts.ascent;
        let g = self.fonts.glyph_by_id(id, style);
        let (y0, h) = (oy as i32 + ascent - g.m.ymin - g.m.height as i32, g.m.height);
        blend_glyph(&mut self.fb, self.w, self.clip, g, ox, oy, ascent, fg);
        self.mark((y0.max(0) as usize).max(self.clip[1]), ((y0 + h as i32).max(0) as usize).min(self.clip[3]));
    }
}
