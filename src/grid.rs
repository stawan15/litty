use std::collections::VecDeque;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINE: u8 = 4;
/// Set on the last cell of a row whose text continues on the next row (soft wrap), so resizing
/// can re-join and re-wrap lines.
pub const WRAPPED: u8 = 8;

const HISTORY_CAP: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// A command block from shell integration (OSC 133): prompt, then output, then exit status.
/// Lines are absolute ids (see `Grid::pushed`).
#[derive(Clone, Debug)]
pub struct Mark {
    /// First line of the prompt.
    pub start: u64,
    /// First line of output; set when the command starts running.
    pub out: Option<u64>,
    /// Line where the next prompt began; set when the command finishes.
    pub end: Option<u64>,
    pub exit: Option<i32>,
    pub started: Option<Instant>,
    pub took: Option<Duration>,
}

/// Commands slower than this ask for attention if the window is in the background.
const LONG_COMMAND: Duration = Duration::from_secs(8);

/// `n` consecutive cells sharing one style.
#[derive(Clone, Copy)]
struct Run {
    n: u16,
    fg: u32,
    bg: u32,
    attrs: u8,
}

/// A scrolled-off line stored as text plus style runs: ~10x smaller than a `Vec<Cell>`.
#[derive(Default)]
struct HLine {
    text: String,
    runs: Vec<Run>,
}

impl HLine {
    fn encode(&mut self, cells: &[Cell]) {
        self.text.clear();
        self.runs.clear();
        for c in cells {
            if c.ch != '\0' {
                self.text.push(c.ch);
                self.text.extend(c.comb.iter().filter(|&&m| m != '\0'));
            }
            match self.runs.last_mut() {
                Some(r) if (r.fg, r.bg, r.attrs) == (c.fg, c.bg, c.attrs) => r.n += 1,
                _ => self.runs.push(Run { n: 1, fg: c.fg, bg: c.bg, attrs: c.attrs }),
            }
        }
    }

    fn decode(&self, out: &mut Vec<Cell>) {
        let mut styles = self.runs.iter().flat_map(|r| std::iter::repeat_n((r.fg, r.bg, r.attrs), r.n as usize));
        let mut base = None;
        for ch in self.text.chars() {
            if ch.width() == Some(0) {
                if let Some(cell) = base.and_then(|i| out.get_mut(i)) {
                    let cell: &mut Cell = cell;
                    if let Some(slot) = cell.comb.iter_mut().find(|s| **s == '\0') {
                        *slot = ch;
                    }
                }
                continue;
            }
            let (fg, bg, attrs) = styles.next().unwrap_or((def_fg(), def_bg(), 0));
            base = Some(out.len());
            out.push(Cell { ch, comb: ['\0'; 2], fg, bg, attrs });
            if ch.width() == Some(2) {
                let (fg, bg, attrs) = styles.next().unwrap_or((def_fg(), def_bg(), 0));
                out.push(Cell { ch: '\0', comb: ['\0'; 2], fg, bg, attrs });
            }
        }
    }
}

// Tokyo Night palette.

/// The cursor shape and blinking chosen in the config file (a steady block by default).
fn default_cursor() -> (CursorShape, bool) {
    let c = crate::config::get();
    (
        match c.cursor {
            1 => CursorShape::Underline,
            2 => CursorShape::Bar,
            _ => CursorShape::Block,
        },
        c.cursor_blink,
    )
}

pub fn def_fg() -> u32 {
    crate::theme::theme().fg
}

pub fn def_bg() -> u32 {
    crate::theme::theme().bg
}

pub fn ansi() -> &'static [u32; 16] {
    &crate::theme::theme().ansi
}

fn xterm256(n: u8) -> u32 {
    match n {
        0..=15 => ansi()[n as usize],
        16..=231 => {
            let i = n - 16;
            let f = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
            (f(i / 36) << 16) | (f(i / 6 % 6) << 8) | f(i % 6)
        }
        _ => {
            let v = 8 + (n - 232) as u32 * 10;
            (v << 16) | (v << 8) | v
        }
    }
}

/// Base char plus up to two combining marks (Thai vowels/tone marks). `'\0'` is the spacer
/// half of a double-width char.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub ch: char,
    pub comb: [char; 2],
    pub fg: u32,
    pub bg: u32,
    pub attrs: u8,
}

impl Cell {
    fn blank(fg: u32, bg: u32) -> Self {
        Cell { ch: ' ', comb: ['\0'; 2], fg, bg, attrs: 0 }
    }
}

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cx: usize,
    pub cy: usize,
    pub dirty: Vec<bool>,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// Renderer bookkeeping for incremental drawing of this grid.
    pub drawn_cursor_row: usize,
    pub drawn_scroll: usize,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Mode 1004: report focus in/out to the application.
    pub focus_events: bool,
    /// Mode 2026: when the application began a synchronized update (the renderer holds the frame).
    pub sync_since: Option<std::time::Instant>,
    /// Bytes the terminal must answer back to the application (DSR, DA).
    pub reply: Vec<u8>,
    /// Pending window title from OSC 0/2, taken by the renderer.
    pub title: Option<String>,
    /// Last window title (OSC 0/2) and the shorter tab title (OSC 1, else the window title).
    pub win_title: String,
    pub tab_title: String,
    icon_title_seen: bool,
    /// Working directory reported by the shell (OSC 7), used for new tabs.
    pub cwd: Option<String>,
    /// Pending clipboard contents from OSC 52, taken by the app.
    pub clip: Option<Vec<u8>>,
    /// 0 = off, 1 = press/release (1000), 2 = drag (1002), 3 = any motion (1003).
    pub mouse: u8,
    pub mouse_sgr: bool,
    pub in_alt: bool,
    /// Lines scrolled back from the live screen (0 = live).
    pub scroll: usize,
    /// Total lines ever pushed to history; gives lines a stable absolute id.
    pub pushed: u64,
    /// Selection as (anchor, head), each (absolute line id, column).
    pub sel: Option<((u64, usize), (u64, usize))>,
    history: VecDeque<HLine>,
    pub marks: VecDeque<Mark>,
    /// Find results, sorted: (absolute line id, start column, length in cells).
    pub matches: Vec<(u64, usize, usize)>,
    pub cur_match: usize,
    /// Set when a long command finishes; taken by the app.
    pub attention: bool,
    pen_fg: u32,
    pen_bg: u32,
    pen_rev: bool,
    pen_attrs: u8,
    top: usize,
    bot: usize,
    saved: (usize, usize),
    alt_cells: Vec<Cell>,
    /// Ring-buffer origin: logical row `y` lives at physical row `(off + y) % rows`, so
    /// scrolling the whole screen only moves this offset instead of the cells.
    off: usize,
    alt_off: usize,
}

impl Grid {
    /// Answer a colour query: `OSC <what>;rgb:RRRR/GGGG/BBBB`, ended like the request was.
    fn osc_color_reply(&mut self, what: &str, color: u32, bell: bool) {
        let (r, g, b) = ((color >> 16) & 255, (color >> 8) & 255, color & 255);
        let end = if bell { "\x07" } else { "\x1b\\" };
        self.reply.extend(format!("\x1b]{what};rgb:{:04x}/{:04x}/{:04x}{end}", r * 257, g * 257, b * 257).bytes());
    }

    pub fn new(cols: usize, rows: usize) -> Self {
        let blank = Cell::blank(def_fg(), def_bg());
        Grid {
            cols,
            rows,
            cells: vec![blank; cols * rows],
            cx: 0,
            cy: 0,
            dirty: vec![true; rows],
            cursor_visible: true,
            cursor_shape: default_cursor().0,
            cursor_blink: default_cursor().1,
            drawn_cursor_row: 0,
            drawn_scroll: 0,
            app_cursor: false,
            bracketed_paste: false,
            focus_events: false,
            sync_since: None,
            reply: Vec::new(),
            title: None,
            win_title: String::new(),
            tab_title: String::new(),
            icon_title_seen: false,
            cwd: None,
            clip: None,
            mouse: 0,
            mouse_sgr: false,
            in_alt: false,
            scroll: 0,
            pushed: 0,
            sel: None,
            history: VecDeque::new(),
            marks: VecDeque::new(),
            matches: Vec::new(),
            cur_match: 0,
            attention: false,
            pen_fg: def_fg(),
            pen_bg: def_bg(),
            pen_rev: false,
            pen_attrs: 0,
            top: 0,
            bot: rows,
            saved: (0, 0),
            alt_cells: vec![blank; cols * rows],
            off: 0,
            alt_off: 0,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        // The window system often repeats Resized with an unchanged size; nothing to do then.
        if (cols, rows) == (self.cols, self.rows) {
            self.dirty.fill(true);
            return;
        }
        if !self.in_alt {
            self.reflow(cols, rows);
            return;
        }
        let blank = Cell::blank(def_fg(), def_bg());
        let (old_cols, old_rows) = (self.cols, self.rows);
        // Both screens are resized, so the main screen survives a resize while in the alternate one.
        let copy = |src: &[Cell], off: usize| {
            let mut out = vec![blank; cols * rows];
            for y in 0..rows.min(old_rows) {
                let r = (off + y) % old_rows;
                let n = cols.min(old_cols);
                out[y * cols..y * cols + n].copy_from_slice(&src[r * old_cols..r * old_cols + n]);
            }
            out
        };
        self.cells = copy(&self.cells, self.off);
        self.alt_cells = copy(&self.alt_cells, self.alt_off);
        (self.off, self.alt_off) = (0, 0);
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bot = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.dirty = vec![true; rows];
        self.scroll = self.scroll.min(self.history.len());
    }

    /// Resize the main screen, re-joining soft-wrapped lines and wrapping them to the new width
    /// while keeping the cursor on the same text. Rows that no longer fit at the top move into
    /// scrollback (scrollback itself is not re-wrapped). Marks are remapped to the new rows.
    fn reflow(&mut self, cols: usize, rows: usize) {
        let blank = Cell::blank(def_fg(), def_bg());
        let is_blank = |c: &Cell| c.ch == ' ' && c.bg == def_bg() && c.attrs & UNDERLINE == 0;
        let (old_cols, old_rows) = (self.cols, self.rows);
        let pushed_old = self.pushed;
        let pending = self.cx >= old_cols;
        let cx = self.cx.min(old_cols);

        // Rows in use: up to the last non-blank row or the cursor row.
        let used = (0..old_rows)
            .rev()
            .find(|&y| self.row(y).iter().any(|c| !is_blank(c)))
            .map_or(0, |y| y + 1)
            .max(self.cy + 1);

        let mut out: Vec<Vec<Cell>> = Vec::new();
        let mut row_map = vec![0usize; old_rows];
        let mut cursor = (0usize, 0usize);
        let mut y = 0;
        while y < used {
            let start_y = y;
            let mut cells: Vec<Cell> = Vec::new();
            loop {
                let row = self.row(y);
                cells.extend_from_slice(row);
                let wrapped = row[old_cols - 1].attrs & WRAPPED != 0;
                y += 1;
                if !wrapped || y >= used {
                    break;
                }
            }
            for c in &mut cells {
                c.attrs &= !WRAPPED;
            }
            let cursor_here = (start_y..y).contains(&self.cy);
            let cursor_offset = (self.cy.saturating_sub(start_y)) * old_cols + cx;
            let mut keep = cells.iter().rposition(|c| !is_blank(c)).map_or(0, |i| i + 1);
            if cursor_here {
                keep = keep.max(if pending { cursor_offset } else { cursor_offset + 1 });
            }
            let base = out.len();
            let mut cur: Vec<Cell> = Vec::with_capacity(cols);
            for i in 0..keep {
                let cell = cells[i];
                let wide_base = cell.ch != '\0' && cells.get(i + 1).is_some_and(|n| n.ch == '\0');
                let need = if wide_base { 2 } else { 1 };
                if cur.len() + need > cols && !cur.is_empty() {
                    cur.resize(cols, blank);
                    cur[cols - 1].attrs |= WRAPPED;
                    out.push(std::mem::replace(&mut cur, Vec::with_capacity(cols)));
                }
                if cursor_here && !pending && i == cursor_offset {
                    cursor = (out.len(), cur.len());
                }
                cur.push(cell);
                if cursor_here && pending && i + 1 == cursor_offset {
                    cursor = (out.len(), cur.len());
                }
            }
            cur.resize(cols, blank);
            out.push(cur);
            let line_rows = out.len() - base;
            for r in start_y..y {
                row_map[r] = base + ((r - start_y) * old_cols / cols).min(line_rows - 1);
            }
        }
        let total = out.len();
        // Where a new-row index falls for old rows past the used area.
        let remap = |id: u64| -> u64 {
            if id < pushed_old {
                return id;
            }
            let r = (id - pushed_old) as usize;
            pushed_old + if r < used { row_map[r] as u64 } else { (total + r - used) as u64 }
        };
        for m in self.marks.iter_mut().filter(|m| m.start >= pushed_old) {
            m.start = remap(m.start);
            m.out = m.out.map(remap);
            m.end = m.end.map(remap);
        }

        // Keep the cursor on screen: rows above overflow into scrollback.
        let drop = total.saturating_sub(rows).min(cursor.0);
        for row in &out[..drop] {
            Self::push_line(&mut self.history, &mut self.pushed, &mut self.marks, &mut self.scroll, row);
        }
        let mut cells = Vec::with_capacity(cols * rows);
        for r in drop..drop + rows {
            match out.get(r) {
                Some(row) => cells.extend_from_slice(row),
                None => cells.resize(cells.len() + cols, blank),
            }
        }
        self.cells = cells;
        self.alt_cells = vec![blank; cols * rows];
        (self.off, self.alt_off) = (0, 0);
        (self.cols, self.rows) = (cols, rows);
        (self.top, self.bot) = (0, rows);
        self.cy = (cursor.0 - drop).min(rows - 1);
        self.cx = cursor.1.min(cols);
        self.saved = (self.saved.0.min(cols - 1), self.saved.1.min(rows - 1));
        self.sel = None;
        self.matches.clear();
        self.dirty = vec![true; rows];
        self.scroll = self.scroll.min(self.history.len());
    }

    /// Index into `cells` of logical (row, col).
    fn at(&self, y: usize, x: usize) -> usize {
        let mut r = self.off + y;
        if r >= self.rows {
            r -= self.rows;
        }
        r * self.cols + x
    }

    pub fn row(&self, y: usize) -> &[Cell] {
        let i = self.at(y, 0);
        &self.cells[i..i + self.cols]
    }

    fn push_history(&mut self, r: usize) {
        let i = self.at(r, 0);
        let row = &self.cells[i..i + self.cols];
        Self::push_line(&mut self.history, &mut self.pushed, &mut self.marks, &mut self.scroll, row);
    }

    /// Append `row` to scrollback (associated fn so callers can borrow `cells` at the same time).
    fn push_line(history: &mut VecDeque<HLine>, pushed: &mut u64, marks: &mut VecDeque<Mark>, scroll: &mut usize, row: &[Cell]) {
        // Drop trailing blanks so history stays small for typical short lines.
        let len = row.iter().rposition(|x| !(x.ch == ' ' && x.bg == def_bg() && x.attrs & UNDERLINE == 0)).map_or(0, |i| i + 1);
        // At capacity, recycle the evicted line's allocation.
        let mut line = if history.len() == HISTORY_CAP { history.pop_front().unwrap_or_default() } else { HLine::default() };
        line.encode(&row[..len]);
        history.push_back(line);
        *pushed += 1;
        let base = *pushed - history.len() as u64;
        while marks.front().is_some_and(|m| m.start < base) {
            marks.pop_front();
        }
        if *scroll > 0 {
            *scroll = (*scroll + 1).min(history.len());
        }
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Scroll the view back into history (`delta > 0`) or toward the live screen.
    pub fn scroll_view(&mut self, delta: isize) {
        let max = if self.in_alt { 0 } else { self.history.len() };
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    /// Row `y` of the visible view, which mixes history and live rows while scrolled back.
    pub fn view_row<'a>(&'a self, y: usize, buf: &'a mut Vec<Cell>) -> &'a [Cell] {
        let h = self.history.len();
        let v = h - self.scroll + y;
        if v >= h {
            return self.row(v - h);
        }
        buf.clear();
        self.history[v].decode(buf);
        buf.truncate(self.cols);
        buf.resize(self.cols, Cell::blank(def_fg(), def_bg()));
        buf
    }

    /// Absolute id of visible row `y`.
    pub fn abs_row(&self, y: usize) -> u64 {
        self.pushed - self.scroll as u64 + y as u64
    }

    /// Selected column range `[start, end)` on visible row `y`.
    pub fn sel_cols(&self, y: usize) -> Option<(usize, usize)> {
        let (a, b) = self.sel?;
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let id = self.abs_row(y);
        if id < a.0 || id > b.0 {
            return None;
        }
        Some((if id == a.0 { a.1 } else { 0 }, if id == b.0 { b.1 + 1 } else { self.cols }))
    }

    /// Cells of the line with absolute id `id`, if it is still in history or on screen.
    /// History lines are decoded into `buf` and may be shorter than `cols`.
    pub fn abs_line<'a>(&'a self, id: u64, buf: &'a mut Vec<Cell>) -> Option<&'a [Cell]> {
        let base = self.pushed - self.history.len() as u64;
        if id < base {
            None
        } else if id < self.pushed {
            buf.clear();
            self.history[(id - base) as usize].decode(buf);
            Some(buf)
        } else if ((id - self.pushed) as usize) < self.rows {
            Some(self.row((id - self.pushed) as usize))
        } else {
            None
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        let (a, b) = self.sel?;
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        let mut buf = Vec::new();
        for id in a.0..=b.0 {
            let Some(line) = self.abs_line(id, &mut buf) else { continue };
            let (start, end) = (if id == a.0 { a.1 } else { 0 }, if id == b.0 { b.1 + 1 } else { usize::MAX });
            let mut text = String::new();
            for c in line.iter().take(end).skip(start).filter(|c| c.ch != '\0') {
                text.push(c.ch);
                text.extend(c.comb.iter().filter(|&&m| m != '\0'));
            }
            if id > a.0 {
                out.push('\n');
            }
            out.push_str(text.trim_end_matches(' '));
        }
        Some(out)
    }

    /// Handle OSC 133 semantic prompt marks: A = prompt start, C = command start, D = finished.
    fn semantic_prompt(&mut self, kind: &[u8], rest: &[&[u8]]) {
        let line = self.pushed + self.cy as u64;
        match kind {
            b"A" => self.marks.push_back(Mark { start: line, out: None, end: None, exit: None, started: None, took: None }),
            b"C" => {
                if let Some(m) = self.marks.back_mut().filter(|m| m.out.is_none()) {
                    (m.out, m.started) = (Some(line), Some(Instant::now()));
                }
            }
            b"D" => match self.marks.back_mut() {
                // A prompt that never ran a command (empty Enter): drop it.
                Some(m) if m.out.is_none() => {
                    self.marks.pop_back();
                }
                Some(m) if m.end.is_none() => {
                    m.end = Some(line);
                    m.exit = Some(rest.first().and_then(|c| std::str::from_utf8(c).ok()?.parse().ok()).unwrap_or(0));
                    m.took = m.started.map(|t| t.elapsed());
                    self.attention |= m.took.is_some_and(|t| t >= LONG_COMMAND);
                    self.dirty.fill(true);
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Search history and screen for `query` (case-insensitive unless it has an uppercase letter).
    pub fn find_all(&mut self, query: &str) {
        const MAX: usize = 10_000;
        self.matches.clear();
        self.cur_match = 0;
        if query.is_empty() {
            return;
        }
        let smart_case = query.chars().any(char::is_uppercase);
        let fold = |c: char| if smart_case { c } else { c.to_lowercase().next().unwrap_or(c) };
        let q: Vec<char> = query.chars().map(fold).collect();
        let mut found = Vec::new();
        let mut buf = Vec::new();
        for id in self.pushed - self.history.len() as u64..self.pushed + self.rows as u64 {
            let Some(line) = self.abs_line(id, &mut buf) else { continue };
            let text: Vec<(usize, char)> = line.iter().enumerate().filter(|(_, c)| c.ch != '\0').map(|(x, c)| (x, fold(c.ch))).collect();
            let mut i = 0;
            while i + q.len() <= text.len() && found.len() < MAX {
                if text[i..i + q.len()].iter().map(|t| t.1).eq(q.iter().copied()) {
                    let (start, end) = (text[i].0, text[i + q.len() - 1].0);
                    found.push((id, start, end - start + 1));
                    i += q.len();
                } else {
                    i += 1;
                }
            }
        }
        self.matches = found;
        // Start at the match nearest the bottom, like most find bars.
        self.cur_match = self.matches.len().saturating_sub(1);
    }

    /// Matches on absolute line `id`.
    pub fn row_matches(&self, id: u64) -> &[(u64, usize, usize)] {
        let a = self.matches.partition_point(|m| m.0 < id);
        let b = self.matches.partition_point(|m| m.0 <= id);
        &self.matches[a..b]
    }

    /// Scroll so absolute line `id` is the top row of the view (or as close as possible).
    pub fn scroll_top_to(&mut self, id: u64) {
        let want = self.pushed.saturating_sub(id) as usize;
        self.scroll = want.min(if self.in_alt { 0 } else { self.history.len() });
    }

    /// Scroll so absolute line `id` is near the middle of the view.
    pub fn scroll_to(&mut self, id: u64) {
        self.scroll_top_to(id.saturating_sub(self.rows as u64 / 2));
    }

    /// The http(s) URL under visible cell (`x`, `y`), if any.
    pub fn url_at(&self, y: usize, x: usize) -> Option<String> {
        let mut buf = Vec::new();
        let cells = self.view_row(y, &mut buf);
        let text: Vec<(usize, char)> = cells.iter().enumerate().filter(|(_, c)| c.ch != '\0').map(|(i, c)| (i, c.ch)).collect();
        let k = text.iter().rposition(|t| t.0 <= x)?;
        let delim = |c: char| c.is_whitespace() || "\"'<>()[]{}`|".contains(c);
        if delim(text[k].1) {
            return None;
        }
        let s = (0..k).rev().find(|&i| delim(text[i].1)).map_or(0, |i| i + 1);
        let e = (k..text.len()).find(|&i| delim(text[i].1)).unwrap_or(text.len());
        let url: String = text[s..e].iter().map(|t| t.1).collect();
        let url = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_string())
    }

    /// Cmd+K: wipe screen, scrollback and command blocks.
    pub fn clear_all(&mut self) {
        let blank = Cell::blank(def_fg(), def_bg());
        self.cells.fill(blank);
        self.history.clear();
        self.marks.clear();
        self.matches.clear();
        (self.scroll, self.cx, self.cy, self.sel) = (0, 0, 0, None);
        self.dirty.fill(true);
    }

    /// Select the word (run of same-class characters) at `col` of absolute line `id`.
    pub fn select_word(&mut self, id: u64, col: usize) {
        let mut buf = Vec::new();
        let Some(line) = self.abs_line(id, &mut buf) else { return };
        // 0 = blank, 1 = word character, 2 = other punctuation (selected alone).
        let class = |c: &Cell| match c.ch {
            ' ' | '\0' => 0,
            c if c.is_alphanumeric() || "_-./~:@%+=?#&".contains(c) => 1,
            _ => 2,
        };
        let mut col = col.min(self.cols - 1);
        // A wide character's spacer belongs to the character before it.
        if line.get(col).is_some_and(|c| c.ch == '\0') && col > 0 {
            col -= 1;
        }
        let Some(here) = line.get(col) else {
            self.sel = Some(((id, col), (id, self.cols - 1)));
            return;
        };
        let k = class(here);
        let (mut a, mut b) = (col, col);
        if k != 2 {
            while a > 0 && (class(&line[a - 1]) == k || line[a - 1].ch == '\0') {
                a -= 1;
            }
            while b + 1 < line.len() && (class(&line[b + 1]) == k || line[b + 1].ch == '\0') {
                b += 1;
            }
        }
        if k == 0 && b + 1 >= line.len() {
            b = self.cols - 1;
        }
        self.sel = Some(((id, a), (id, b)));
    }

    /// Select the whole logical line (all soft-wrapped rows) containing absolute line `id`.
    pub fn select_line(&mut self, id: u64) {
        let cols = self.cols;
        let mut buf = Vec::new();
        let base = self.pushed - self.history.len() as u64;
        let mut wrapped_into = |g: &Grid, id: u64| {
            g.abs_line(id, &mut buf).is_some_and(|l| l.len() == cols && l[cols - 1].attrs & WRAPPED != 0)
        };
        let (mut first, mut last) = (id, id);
        while first > base && wrapped_into(self, first - 1) {
            first -= 1;
        }
        while wrapped_into(self, last) {
            last += 1;
        }
        self.sel = Some(((first, 0), (last, cols - 1)));
    }

    /// After the visible screen is cleared, its lines get reused for new output, so command
    /// blocks that pointed at them must go (keeps `marks` sorted and unambiguous).
    fn forget_screen_marks(&mut self) {
        let live = self.pushed;
        self.marks.retain(|m| m.start < live);
        for m in &mut self.marks {
            if m.end.is_some_and(|e| e > live) {
                m.end = Some(live);
            }
        }
    }

    /// The command block containing absolute line `id`, if any.
    pub fn mark_at(&self, id: u64) -> Option<&Mark> {
        let i = self.marks.partition_point(|m| m.start <= id).checked_sub(1)?;
        let m = &self.marks[i];
        (id < m.end.unwrap_or(self.pushed + self.cy as u64 + 1)).then_some(m)
    }

    fn blank(&self) -> Cell {
        Cell::blank(self.pen_fg, self.pen_bg)
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bot - self.top);
        let c = self.cols;
        if !self.in_alt && self.top == 0 {
            for r in 0..n {
                self.push_history(r);
            }
        }
        let blank = self.blank();
        if self.top == 0 && self.bot == self.rows {
            self.off = (self.off + n) % self.rows;
        } else {
            for y in self.top..self.bot - n {
                let (s, d) = (self.at(y + n, 0), self.at(y, 0));
                self.cells.copy_within(s..s + c, d);
            }
        }
        for y in self.bot - n..self.bot {
            let i = self.at(y, 0);
            self.cells[i..i + c].fill(blank);
        }
        self.dirty[self.top..self.bot].fill(true);
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bot - self.top);
        let c = self.cols;
        if self.top == 0 && self.bot == self.rows {
            self.off = (self.off + self.rows - n) % self.rows;
        } else {
            for y in (self.top + n..self.bot).rev() {
                let (s, d) = (self.at(y - n, 0), self.at(y, 0));
                self.cells.copy_within(s..s + c, d);
            }
        }
        let blank = self.blank();
        for y in self.top..self.top + n {
            let i = self.at(y, 0);
            self.cells[i..i + c].fill(blank);
        }
        self.dirty[self.top..self.bot].fill(true);
    }

    fn newline(&mut self) {
        if self.cy + 1 == self.bot {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cy == self.top {
            self.scroll_down(1);
        } else {
            self.cy = self.cy.saturating_sub(1);
        }
    }

    /// Erase linear cell positions `[from, to)` (row * cols + col in logical coordinates).
    fn erase(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let c = self.cols;
        let blank = self.blank();
        let (y0, y1) = (from / c, (to - 1) / c);
        for y in y0..=y1 {
            let a = if y == y0 { from % c } else { 0 };
            let b = if y == y1 { (to - 1) % c + 1 } else { c };
            let i = self.at(y, 0);
            self.cells[i + a..i + b].fill(blank);
        }
        self.dirty[y0..=y1].fill(true);
    }

    fn set_alt(&mut self, on: bool) {
        if on == self.in_alt {
            return;
        }
        self.in_alt = on;
        self.scroll = 0;
        std::mem::swap(&mut self.cells, &mut self.alt_cells);
        std::mem::swap(&mut self.off, &mut self.alt_off);
        if on {
            self.saved = (self.cx, self.cy);
            let blank = Cell::blank(def_fg(), def_bg());
            self.cells.fill(blank);
        } else {
            (self.cx, self.cy) = self.saved;
        }
        self.dirty.fill(true);
    }

    fn sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.reset_pen();
            return;
        }
        let mut it = params.iter();
        while let Some(s) = it.next() {
            if s.len() > 1 {
                // Colon-separated sub-parameters: 38:2::r:g:b, 48:5:n, 4:3.
                match s[0] {
                    38 | 48 => {
                        if let Some(col) = colon_color(s) {
                            if s[0] == 38 { self.pen_fg = col } else { self.pen_bg = col }
                        }
                    }
                    4 if s[1] == 0 => self.pen_attrs &= !UNDERLINE,
                    4 => self.pen_attrs |= UNDERLINE,
                    _ => {}
                }
                continue;
            }
            let c = s[0];
            match c {
                0 => self.reset_pen(),
                1 => self.pen_attrs |= BOLD,
                3 => self.pen_attrs |= ITALIC,
                4 => self.pen_attrs |= UNDERLINE,
                7 => self.pen_rev = true,
                22 => self.pen_attrs &= !BOLD,
                23 => self.pen_attrs &= !ITALIC,
                24 => self.pen_attrs &= !UNDERLINE,
                27 => self.pen_rev = false,
                30..=37 => self.pen_fg = ansi()[(c - 30) as usize],
                39 => self.pen_fg = def_fg(),
                40..=47 => self.pen_bg = ansi()[(c - 40) as usize],
                49 => self.pen_bg = def_bg(),
                90..=97 => self.pen_fg = ansi()[(c - 90) as usize + 8],
                100..=107 => self.pen_bg = ansi()[(c - 100) as usize + 8],
                38 | 48 => {
                    if let Some(col) = extended_color(&mut it.by_ref().map(|s| s[0])) {
                        if c == 38 { self.pen_fg = col } else { self.pen_bg = col }
                    }
                }
                _ => {}
            }
        }
    }

    fn reset_pen(&mut self) {
        (self.pen_fg, self.pen_bg, self.pen_rev, self.pen_attrs) = (def_fg(), def_bg(), false, 0);
    }

    fn set_mode(&mut self, params: &Params, on: bool) {
        for p in params.iter().flatten() {
            match p {
                1 => self.app_cursor = on,
                25 => self.cursor_visible = on,
                1000 => self.mouse = if on { 1 } else { 0 },
                1002 => self.mouse = if on { 2 } else { 0 },
                1003 => self.mouse = if on { 3 } else { 0 },
                1006 => self.mouse_sgr = on,
                47 | 1047 | 1049 => self.set_alt(on),
                2004 => self.bracketed_paste = on,
                1004 => self.focus_events = on,
                2026 => self.sync_since = on.then(std::time::Instant::now),
                _ => {}
            }
        }
    }
}

fn extended_color(it: &mut impl Iterator<Item = u16>) -> Option<u32> {
    match it.next()? {
        5 => Some(xterm256(it.next()? as u8)),
        2 => {
            let (r, g, b) = (it.next()? as u32, it.next()? as u32, it.next()? as u32);
            Some((r << 16) | (g << 8) | b)
        }
        _ => None,
    }
}

fn colon_color(s: &[u16]) -> Option<u32> {
    match s.get(1)? {
        5 => Some(xterm256(*s.get(2)? as u8)),
        2 if s.len() >= 5 => {
            let n = s.len();
            Some(((s[n - 3] as u32) << 16) | ((s[n - 2] as u32) << 8) | s[n - 1] as u32)
        }
        _ => None,
    }
}

/// Path part of a `file://host/path` URI (OSC 7), percent-decoded.
fn file_uri_path(uri: &[u8]) -> Option<String> {
    let rest = uri.strip_prefix(b"file://")?;
    let path = &rest[rest.iter().position(|&b| b == b'/')?..];
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (path[i], path.get(i + 1).and_then(|&b| hex(b)), path.get(i + 2).and_then(|&b| hex(b))) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn base64(input: &[u8]) -> Vec<u8> {
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn raw(params: &Params, i: usize) -> usize {
    params.iter().nth(i).and_then(|p| p.first()).copied().unwrap_or(0) as usize
}

fn param(params: &Params, i: usize, default: usize) -> usize {
    match raw(params, i) {
        0 => default,
        v => v,
    }
}

impl Perform for Grid {
    fn print(&mut self, ch: char) {
        let w = ch.width().unwrap_or(1);
        if w == 0 {
            // Combining mark: attach to the previous cell (or its base if that is a spacer).
            if self.cx == 0 {
                return;
            }
            let mut x = self.cx - 1;
            if self.cells[self.at(self.cy, x)].ch == '\0' && x > 0 {
                x -= 1;
            }
            let i = self.at(self.cy, x);
            let cell = &mut self.cells[i];
            if let Some(slot) = cell.comb.iter_mut().find(|s| **s == '\0') {
                *slot = ch;
            }
            self.dirty[self.cy] = true;
            return;
        }
        if self.cx + w > self.cols {
            let last = self.at(self.cy, self.cols - 1);
            self.cells[last].attrs |= WRAPPED;
            self.cx = 0;
            self.newline();
        }
        let (fg, bg) = if self.pen_rev { (self.pen_bg, self.pen_fg) } else { (self.pen_fg, self.pen_bg) };
        let i = self.at(self.cy, self.cx);
        let attrs = self.pen_attrs;
        self.cells[i] = Cell { ch, comb: ['\0'; 2], fg, bg, attrs };
        if w == 2 {
            self.cells[i + 1] = Cell { ch: '\0', comb: ['\0'; 2], fg, bg, attrs };
        }
        self.dirty[self.cy] = true;
        self.cx += w;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\r' => self.cx = 0,
            0x08 => self.cx = self.cx.min(self.cols - 1).saturating_sub(1),
            b'\t' => self.cx = ((self.cx / 8 + 1) * 8).min(self.cols - 1),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell: bool) {
        match params {
            // Colour queries (neovim, bat, delta ask for the background to pick a theme).
            [code @ (b"10" | b"11" | b"12"), b"?", ..] => {
                let color = if *code == b"11" { def_bg() } else { def_fg() };
                self.osc_color_reply(&String::from_utf8_lossy(code), color, bell);
            }
            [b"4", rest @ ..] => {
                for pair in rest.chunks_exact(2) {
                    if let (Some(n), b"?") = (std::str::from_utf8(pair[0]).ok().and_then(|n| n.parse::<u8>().ok()), pair[1]) {
                        self.osc_color_reply(&format!("4;{n}"), xterm256(n), bell);
                    }
                }
            }
            [b"0" | b"2", title, ..] => {
                let title = String::from_utf8_lossy(title).into_owned();
                if !self.icon_title_seen {
                    self.tab_title = title.clone();
                }
                self.win_title = title.clone();
                self.title = Some(title);
            }
            [b"1", title, ..] => {
                self.icon_title_seen = true;
                self.tab_title = String::from_utf8_lossy(title).into_owned();
            }
            [b"7", uri, ..] => self.cwd = file_uri_path(uri).or(self.cwd.take()),
            [b"52", _, data, ..] if *data != b"?" => self.clip = Some(base64(data)),
            [b"133", kind, rest @ ..] => self.semantic_prompt(kind, rest),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'M' => self.reverse_index(),
            b'D' => self.newline(),
            b'E' => {
                self.cx = 0;
                self.newline();
            }
            b'7' => self.saved = (self.cx, self.cy),
            b'8' => (self.cx, self.cy) = self.saved,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], _: bool, action: char) {
        let (cols, rows) = (self.cols, self.rows);
        // A cursor past the last column is a pending wrap; only sequences that act on the cursor
        // position cancel it (zsh sets colours between the last column and the space that wraps).
        if !matches!(action, 'm' | 'h' | 'l' | 'q') {
            self.cx = self.cx.min(cols - 1);
        }
        let n = param(params, 0, 1);
        match (inter, action) {
            ([b' '], 'q') => {
                // DECSCUSR: 0 the configured default, 1 blinking block, 2 steady block, 3/4 underline, 5/6 bar.
                let style = raw(params, 0);
                (self.cursor_shape, self.cursor_blink) = if style == 0 {
                    default_cursor()
                } else {
                    let shape = match style {
                        3 | 4 => CursorShape::Underline,
                        5 | 6 => CursorShape::Bar,
                        _ => CursorShape::Block,
                    };
                    (shape, matches!(style, 1 | 3 | 5))
                };
            }
            // DECRQM: applications ask whether a mode is supported before relying on it.
            ([b'?', b'$'], 'p') => {
                let mode = raw(params, 0);
                let state = match mode {
                    1 => Some(self.app_cursor),
                    25 => Some(self.cursor_visible),
                    1000 => Some(self.mouse == 1),
                    1002 => Some(self.mouse == 2),
                    1003 => Some(self.mouse == 3),
                    1004 => Some(self.focus_events),
                    1006 => Some(self.mouse_sgr),
                    47 | 1047 | 1049 => Some(self.in_alt),
                    2004 => Some(self.bracketed_paste),
                    2026 => Some(self.sync_since.is_some()),
                    _ => None,
                };
                let code = state.map_or(0, |on| if on { 1 } else { 2 });
                self.reply.extend(format!("\x1b[?{mode};{code}$y").bytes());
            }
            ([b'?'], 'h') => self.set_mode(params, true),
            ([b'?'], 'l') => self.set_mode(params, false),
            ([], 'A') => self.cy = self.cy.saturating_sub(n),
            ([], 'B') => self.cy = (self.cy + n).min(rows - 1),
            ([], 'C') => self.cx = (self.cx + n).min(cols - 1),
            ([], 'D') => self.cx = self.cx.saturating_sub(n),
            ([], 'E') => (self.cx, self.cy) = (0, (self.cy + n).min(rows - 1)),
            ([], 'F') => (self.cx, self.cy) = (0, self.cy.saturating_sub(n)),
            ([], 'G') => self.cx = (n - 1).min(cols - 1),
            ([], 'd') => self.cy = (n - 1).min(rows - 1),
            ([], 'H' | 'f') => {
                self.cy = (n - 1).min(rows - 1);
                self.cx = (param(params, 1, 1) - 1).min(cols - 1);
            }
            ([], 'J') => {
                let cur = self.cy * cols + self.cx;
                match raw(params, 0) {
                    0 => {
                        self.erase(cur, cols * rows);
                        if cur == 0 {
                            self.forget_screen_marks();
                        }
                    }
                    1 => self.erase(0, cur + 1),
                    mode => {
                        self.erase(0, cols * rows);
                        self.forget_screen_marks();
                        if mode == 3 {
                            // Erase saved lines as well.
                            self.history.clear();
                            self.marks.clear();
                            self.scroll = 0;
                        }
                    }
                }
            }
            ([], 'K') => {
                let (start, end) = (self.cy * cols, (self.cy + 1) * cols);
                let cur = start + self.cx;
                match raw(params, 0) {
                    0 => self.erase(cur, end),
                    1 => self.erase(start, cur + 1),
                    _ => self.erase(start, end),
                }
            }
            ([], 'L' | 'M') if (self.top..self.bot).contains(&self.cy) => {
                let top = std::mem::replace(&mut self.top, self.cy);
                if action == 'L' { self.scroll_down(n) } else { self.scroll_up(n) }
                self.top = top;
            }
            ([], 'P') => {
                let start = self.at(self.cy, self.cx);
                let end = start + cols - self.cx;
                let n = n.min(end - start);
                self.cells.copy_within(start + n..end, start);
                let blank = self.blank();
                self.cells[end - n..end].fill(blank);
                self.dirty[self.cy] = true;
            }
            ([], '@') => {
                let start = self.at(self.cy, self.cx);
                let end = start + cols - self.cx;
                let n = n.min(end - start);
                self.cells.copy_within(start..end - n, start + n);
                let blank = self.blank();
                self.cells[start..start + n].fill(blank);
                self.dirty[self.cy] = true;
            }
            ([], 'X') => {
                let cur = self.cy * cols + self.cx;
                self.erase(cur, cur + n.min(cols - self.cx));
            }
            ([], 'S') => self.scroll_up(n),
            ([], 'T') => self.scroll_down(n),
            ([], 'r') => {
                let (top, bot) = (n - 1, param(params, 1, rows).min(rows));
                if top + 1 < bot {
                    (self.top, self.bot) = (top, bot);
                } else {
                    (self.top, self.bot) = (0, rows);
                }
                (self.cx, self.cy) = (0, 0);
            }
            ([], 'm') => self.sgr(params),
            ([], 's') => self.saved = (self.cx, self.cy),
            ([], 'u') => (self.cx, self.cy) = self.saved,
            // DSR 5: "are you there?" (used as a sentinel after other queries).
            ([], 'n') if raw(params, 0) == 5 => self.reply.extend(b"\x1b[0n"),
            // Kitty keyboard protocol query: supported, no enhancements active.
            ([b'?'], 'u') => self.reply.extend(b"\x1b[?0u"),
            ([], 'n') if raw(params, 0) == 6 => {
                self.reply.extend(format!("\x1b[{};{}R", self.cy + 1, self.cx + 1).bytes());
            }
            ([], 'c') if raw(params, 0) == 0 => self.reply.extend(b"\x1b[?6c"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;

    fn feed(g: &mut Grid, s: &str) {
        Parser::new().advance(g, s.as_bytes());
    }

    fn line(g: &Grid, y: usize) -> String {
        g.row(y).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn text_and_newline() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "hi\r\nyo");
        assert_eq!((line(&g, 0), line(&g, 1)), ("hi".into(), "yo".into()));
    }

    #[test]
    fn scrolls_at_bottom() {
        let mut g = Grid::new(5, 2);
        feed(&mut g, "a\r\nb\r\nc");
        assert_eq!((line(&g, 0), line(&g, 1)), ("b".into(), "c".into()));
    }

    #[test]
    fn cursor_move_and_erase() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "hello\x1b[1;3H\x1b[K");
        assert_eq!(line(&g, 0), "he");
        feed(&mut g, "\x1b[2J");
        assert_eq!(line(&g, 0), "");
    }

    #[test]
    fn wraps_at_edge() {
        let mut g = Grid::new(3, 2);
        feed(&mut g, "abcd");
        assert_eq!((line(&g, 0), line(&g, 1)), ("abc".into(), "d".into()));
    }

    #[test]
    fn thai_marks_attach_to_base() {
        let mut g = Grid::new(10, 1);
        feed(&mut g, "ก\u{0e48}า");
        assert_eq!(g.row(0)[0].ch, 'ก');
        assert_eq!(g.row(0)[0].comb[0], '\u{0e48}');
        assert_eq!(g.row(0)[1].ch, 'า');
        assert_eq!(g.cx, 2);
    }

    #[test]
    fn wide_char_takes_two_cells() {
        let mut g = Grid::new(4, 1);
        feed(&mut g, "日a");
        assert_eq!((g.row(0)[0].ch, g.row(0)[1].ch, g.row(0)[2].ch), ('日', '\0', 'a'));
    }

    #[test]
    fn sgr_colors_and_reset() {
        let mut g = Grid::new(4, 1);
        feed(&mut g, "\x1b[31ma\x1b[38;2;1;2;3mb\x1b[0mc");
        assert_eq!(g.row(0)[0].fg, ansi()[1]);
        assert_eq!(g.row(0)[1].fg, 0x010203);
        assert_eq!(g.row(0)[2].fg, def_fg());
    }

    #[test]
    fn scroll_region() {
        let mut g = Grid::new(3, 4);
        feed(&mut g, "a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\n");
        assert_eq!((line(&g, 0), line(&g, 1), line(&g, 2), line(&g, 3)), ("a".into(), "c".into(), "".into(), "d".into()));
    }

    #[test]
    fn alt_screen_restores() {
        let mut g = Grid::new(5, 2);
        feed(&mut g, "main\x1b[?1049hvim\x1b[?1049l");
        assert_eq!(line(&g, 0), "main");
    }

    #[test]
    fn cursor_report() {
        let mut g = Grid::new(5, 3);
        feed(&mut g, "ab\x1b[6n");
        assert_eq!(g.reply, b"\x1b[1;3R");
    }

    #[test]
    fn scrollback_and_view() {
        let mut g = Grid::new(5, 2);
        feed(&mut g, "a\r\nb\r\nc\r\nd");
        assert_eq!(g.history_len(), 2);
        g.scroll_view(2);
        let mut buf = Vec::new();
        assert_eq!(g.view_row(0, &mut buf)[0].ch, 'a');
        assert_eq!(g.view_row(1, &mut buf)[0].ch, 'b');
        g.scroll_view(-1);
        assert_eq!(g.view_row(1, &mut buf)[0].ch, 'c');
        g.scroll_view(-5);
        assert_eq!(g.scroll, 0);
    }

    #[test]
    fn selection_across_history() {
        let mut g = Grid::new(6, 2);
        feed(&mut g, "one\r\ntwo\r\nthree");
        // history: "one"; live: "two", "three"
        let base = g.pushed;
        g.sel = Some(((base - 1, 1), (base + 1, 2)));
        assert_eq!(g.selection_text().unwrap(), "ne\ntwo\nthr");
    }

    #[test]
    fn bold_italic_underline() {
        let mut g = Grid::new(6, 1);
        feed(&mut g, "\x1b[1ma\x1b[3;4mb\x1b[22;23;24mc\x1b[4:3md\x1b[4:0me");
        let a: Vec<u8> = g.row(0).iter().take(5).map(|c| c.attrs).collect();
        assert_eq!(a, vec![BOLD, BOLD | ITALIC | UNDERLINE, 0, UNDERLINE, 0]);
    }

    #[test]
    fn colon_truecolor() {
        let mut g = Grid::new(4, 1);
        feed(&mut g, "\x1b[38:2::10:20:30ma\x1b[48:2:1:2:3mb\x1b[38:5:196mc");
        assert_eq!((g.row(0)[0].fg, g.row(0)[1].bg, g.row(0)[2].fg), (0x0a141e, 0x010203, 0xff0000));
    }

    #[test]
    fn mouse_modes_and_osc() {
        let mut g = Grid::new(4, 1);
        feed(&mut g, "\x1b[?1002h\x1b[?1006h");
        assert_eq!((g.mouse, g.mouse_sgr), (2, true));
        feed(&mut g, "\x1b[?1002l");
        assert_eq!(g.mouse, 0);
        feed(&mut g, "\x1b]2;hi there\x07\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(g.title.as_deref(), Some("hi there"));
        assert_eq!(g.clip.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn ring_buffer_keeps_row_order_across_erase() {
        let mut g = Grid::new(4, 3);
        for i in 0..10 {
            feed(&mut g, &format!("{i}\r\n"));
        }
        feed(&mut g, "x");
        assert_eq!((line(&g, 0), line(&g, 1), line(&g, 2)), ("8".into(), "9".into(), "x".into()));
        feed(&mut g, "\x1b[1;1H\x1b[J");
        assert_eq!((line(&g, 0), line(&g, 1), line(&g, 2)), ("".into(), "".into(), "".into()));
        feed(&mut g, "a\r\nb\r\nc\x1bM\x1bM\x1bMz");
        assert_eq!((line(&g, 0), line(&g, 1), line(&g, 2)), (" z".into(), "a".into(), "b".into()));
    }

    #[test]
    fn history_roundtrip_keeps_style_wide_and_marks() {
        let mut g = Grid::new(12, 2);
        feed(&mut g, "\x1b[1;31mก\u{0e48}า\x1b[0m 日x\r\n\r\n\r\n");
        g.scroll_view(2);
        let mut buf = Vec::new();
        let row = g.view_row(0, &mut buf).to_vec();
        assert_eq!((row[0].ch, row[0].comb[0], row[0].attrs, row[0].fg), ('ก', '\u{0e48}', BOLD, ansi()[1]));
        assert_eq!((row[1].ch, row[1].fg), ('า', ansi()[1]));
        assert_eq!((row[3].ch, row[4].ch, row[5].ch), ('日', '\0', 'x'));
        assert_eq!(row[3].fg, def_fg());
    }

    #[test]
    fn command_blocks_from_osc133() {
        let mut g = Grid::new(20, 6);
        feed(&mut g, "\x1b]133;D;0\x07\x1b]133;A\x07$ ls\r\n\x1b]133;C\x07a b\r\n");
        assert_eq!(g.marks.len(), 1);
        assert_eq!((g.marks[0].start, g.marks[0].out), (0, Some(1)));
        feed(&mut g, "\x1b]133;D;2\x07\x1b]133;A\x07$ ");
        assert_eq!((g.marks[0].end, g.marks[0].exit), (Some(2), Some(2)));
        assert_eq!(g.marks.len(), 2);
        assert_eq!(g.mark_at(1).unwrap().start, 0);
        assert_eq!(g.mark_at(2).unwrap().start, 2);
        assert!(g.mark_at(4).is_none());
        // Empty Enter: prompt without a command is dropped by the next D.
        feed(&mut g, "\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        assert_eq!(g.marks.len(), 2);
    }

    #[test]
    fn marks_survive_scrolling_and_expire_with_history() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b]133;A\x07p\r\n\x1b]133;C\x07o1\r\no2\r\no3\x1b]133;D;0\x07");
        let m = g.marks[0].clone();
        assert_eq!((m.start, m.out), (0, Some(1)));
        assert_eq!((g.pushed, m.end), (2, Some(3)));
        let mut buf = Vec::new();
        assert_eq!(g.abs_line(m.start, &mut buf).unwrap()[0].ch, 'p');
    }

    #[test]
    fn find_across_history_and_screen() {
        let mut g = Grid::new(20, 2);
        feed(&mut g, "Hello world\r\nfoo\r\nbar hello\r\nbaz");
        g.find_all("hello");
        assert_eq!(g.matches, vec![(0, 0, 5), (2, 4, 5)]);
        assert_eq!(g.cur_match, 1);
        g.find_all("Hello");
        assert_eq!(g.matches.len(), 1);
        assert_eq!(g.row_matches(0), &[(0, 0, 5)]);
        assert!(g.row_matches(1).is_empty());
        g.scroll_to(0);
        assert_eq!(g.scroll, 2);
    }

    #[test]
    fn url_detection() {
        let mut g = Grid::new(60, 1);
        feed(&mut g, "see (https://a.io/x?y=1). and http://b.c, ok");
        assert_eq!(g.url_at(0, 10).as_deref(), Some("https://a.io/x?y=1"));
        assert_eq!(g.url_at(0, 33).as_deref(), Some("http://b.c"));
        assert_eq!(g.url_at(0, 1), None);
    }

    #[test]
    fn resize_in_alt_screen_keeps_main_screen() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "main\r\nscreen\x1b[?1049hvim");
        g.resize(12, 4);
        feed(&mut g, "\x1b[?1049l");
        assert_eq!((line(&g, 0), line(&g, 1)), ("main".into(), "screen".into()));
    }

    #[test]
    fn clear_screen_drops_marks_on_reused_lines() {
        let mut g = Grid::new(20, 6);
        feed(&mut g, "\x1b]133;A\x07$ a\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        assert_eq!(g.marks.len(), 2);
        feed(&mut g, "\x1b[H\x1b[2J\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        // The old blocks sat on live lines that were just cleared; only the new prompt remains.
        assert_eq!(g.marks.len(), 1);
        assert_eq!(g.marks[0].start, g.pushed);
        assert!(g.mark_at(g.pushed + 3).is_none());
    }

    #[test]
    fn tab_titles_and_cwd() {
        let mut g = Grid::new(20, 2);
        feed(&mut g, "\x1b]2;user@host:~/x\x07\x1b]1;~/x\x07\x1b]2;eza --icons\x07\x1b]1;ls\x07");
        assert_eq!((g.win_title.as_str(), g.tab_title.as_str()), ("eza --icons", "ls"));
        feed(&mut g, "\x1b]7;file://host.local/Users/a%20b/proj\x1b\\");
        assert_eq!(g.cwd.as_deref(), Some("/Users/a b/proj"));
    }

    fn text_rows(g: &Grid) -> Vec<String> {
        (0..g.rows).map(|y| line(g, y)).collect()
    }

    #[test]
    fn soft_wrap_is_flagged_and_hard_newline_is_not() {
        let mut g = Grid::new(4, 3);
        feed(&mut g, "abcdef\r\nxy");
        assert!(g.row(0)[3].attrs & WRAPPED != 0);
        assert!(g.row(1)[3].attrs & WRAPPED == 0);
    }

    #[test]
    fn reflow_narrower_then_wider_restores_lines() {
        let mut g = Grid::new(8, 6);
        feed(&mut g, "abcdefgh\r\nxyz\r\nlonger12");
        g.resize(4, 6);
        assert_eq!(text_rows(&g)[..5], ["abcd", "efgh", "xyz", "long", "er12"]);
        g.resize(8, 6);
        assert_eq!(text_rows(&g)[..3], ["abcdefgh", "xyz", "longer12"]);
    }

    #[test]
    fn reflow_keeps_cursor_on_its_text() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, "$ hello wor");
        // Cursor is after "wor" (col 1 of the wrapped second row).
        assert_eq!((g.cy, g.cx), (1, 1));
        g.resize(6, 4);
        assert_eq!(text_rows(&g)[..2], ["$ hell", "o wor"]);
        assert_eq!((g.cy, g.cx), (1, 5));
        feed(&mut g, "ld");
        assert_eq!(text_rows(&g)[..3], ["$ hell", "o worl", "d"]);
        g.resize(20, 4);
        assert_eq!(line(&g, 0), "$ hello world");
        assert_eq!((g.cy, g.cx), (0, 13));
    }

    #[test]
    fn shrinking_rows_scrolls_top_into_history_and_keeps_cursor_row() {
        let mut g = Grid::new(10, 5);
        feed(&mut g, "one\r\ntwo\r\nthree\r\nfour\r\nfive");
        g.resize(10, 3);
        assert_eq!(text_rows(&g), ["three", "four", "five"]);
        assert_eq!((g.cy, g.cx), (2, 4));
        assert_eq!(g.history_len(), 2);
        let mut buf = Vec::new();
        assert_eq!(g.view_row(0, &mut buf)[0].ch, 't');
        g.scroll_view(2);
        assert_eq!(g.view_row(0, &mut buf)[0].ch, 'o');
    }

    #[test]
    fn growing_rows_keeps_text_at_top() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "a\r\nb");
        g.resize(10, 6);
        assert_eq!(text_rows(&g)[..3], ["a", "b", ""]);
        assert_eq!((g.cy, g.cx), (1, 1));
    }

    #[test]
    fn reflow_remaps_command_marks() {
        let mut g = Grid::new(8, 8);
        feed(&mut g, "\x1b]133;A\x07$ abcdefghij\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        let before: Vec<(u64, Option<u64>)> = g.marks.iter().map(|m| (m.start, m.out)).collect();
        assert_eq!(before, vec![(0, Some(2)), (3, None)]);
        g.resize(5, 8);
        // "$ abcdefghij" now takes 3 rows, so the output line and second prompt move down by one.
        let after: Vec<(u64, Option<u64>)> = g.marks.iter().map(|m| (m.start, m.out)).collect();
        assert_eq!(after, vec![(0, Some(3)), (4, None)]);
        assert_eq!(g.abs_line(3, &mut Vec::new()).unwrap()[0].ch, 'o');
    }

    #[test]
    fn reflow_keeps_wide_chars_whole() {
        let mut g = Grid::new(6, 6);
        feed(&mut g, "ab日本cd");
        g.resize(3, 6);
        let rows = text_rows(&g);
        assert_eq!(rows[0], "ab");
        assert!(rows[1].starts_with('日'), "{rows:?}");
    }

    #[test]
    fn cursor_style_requests() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b[5 q");
        assert_eq!((g.cursor_shape, g.cursor_blink), (CursorShape::Bar, true));
        feed(&mut g, "\x1b[4 q");
        assert_eq!((g.cursor_shape, g.cursor_blink), (CursorShape::Underline, false));
        feed(&mut g, "\x1b[0 q");
        assert_eq!((g.cursor_shape, g.cursor_blink), (CursorShape::Block, false));
    }

    #[test]
    fn word_and_line_selection() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, "foo bar-baz quux");
        // Row 0: "foo bar-ba", row 1: "z quux".
        g.select_word(0, 5);
        assert_eq!(g.selection_text().as_deref(), Some("bar-ba"));
        g.select_word(0, 3);
        assert_eq!(g.sel, Some(((0, 3), (0, 3))));
        g.select_line(1);
        assert_eq!(g.selection_text().as_deref(), Some("foo bar-ba\nz quux"));
    }

    #[test]
    fn clear_all_wipes_everything() {
        let mut g = Grid::new(5, 2);
        feed(&mut g, "a\r\nb\r\nc\r\nd");
        g.clear_all();
        assert_eq!((g.history_len(), line(&g, 0), line(&g, 1), g.cy, g.cx), (0, "".into(), "".into(), 0, 0));
    }

    #[test]
    fn colour_change_keeps_the_pending_wrap() {
        // zsh writes the last column, resets colours, then a space and CR to force the wrap.
        let mut g = Grid::new(5, 3);
        feed(&mut g, "abcd\x1b[31me\x1b[0m\x1b[39m \r");
        assert_eq!((line(&g, 0), g.cx, g.cy), ("abcde".to_string(), 0, 1));
        // A cursor movement does cancel it.
        let mut g = Grid::new(5, 3);
        feed(&mut g, "abcde\x1b[Cf");
        assert_eq!((line(&g, 0), line(&g, 1)), ("abcdf".to_string(), String::new()));
    }

    #[test]
    fn mode_queries_and_focus_and_sync_modes() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b[?1004h\x1b[?2026h");
        assert!(g.focus_events && g.sync_since.is_some());
        feed(&mut g, "\x1b[?2026$p\x1b[?1004$p\x1b[?9999$p");
        assert_eq!(g.reply, b"\x1b[?2026;1$y\x1b[?1004;1$y\x1b[?9999;0$y");
        feed(&mut g, "\x1b[?2026l\x1b[?1004l");
        assert!(!g.focus_events && g.sync_since.is_none());
    }

    /// Developer tool: `CAP=file COLS=80 cargo test replay_capture -- --ignored --nocapture` feeds
    /// bytes recorded from a real program (or `LITTY_LOG`) through the grid and prints the screen.
    #[test]
    #[ignore]
    fn replay_capture() {
        let bytes = std::fs::read(std::env::var("CAP").unwrap()).unwrap();
        let cols = std::env::var("COLS").ok().and_then(|c| c.parse().ok()).unwrap_or(80);
        let mut g = Grid::new(cols, 24);
        Parser::new().advance(&mut g, &bytes);
        for y in 0..g.rows {
            eprintln!("{y:2}|{}", g.row(y).iter().map(|c| c.ch).collect::<String>().trim_end());
        }
        eprintln!("cursor {} {}", g.cx, g.cy);
    }

    #[test]
    fn colour_queries_are_answered() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b]11;?\x07\x1b]10;?\x1b\\\x1b]4;1;?\x07");
        let reply = String::from_utf8(g.reply.clone()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:1a1a/1b1b/2626\x07\x1b]10;rgb:c0c0/caca/f5f5\x1b\\\x1b]4;1;rgb:"), "{reply:?}");
    }

    #[test]
    fn status_report_and_keyboard_protocol_queries() {
        let mut g = Grid::new(10, 3);
        feed(&mut g, "\x1b[5n\x1b[?u");
        assert_eq!(g.reply, b"\x1b[0n\x1b[?0u");
    }
}
