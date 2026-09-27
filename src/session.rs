//! Session restore: the windows, tabs, splits and working directories open when litty quit,
//! reopened at the next launch. Programs are not restored, only where they ran.
//!
//! One line per tab in `<cache>/litty/session`:  `<window group> <focused pane> <layout>`, where a
//! layout is `p<dir>` (a pane; the directory percent-encoded, `-` if unknown) or
//! `v<ratio>(<layout>,<layout>)` / `h…` for panes side by side / stacked.

use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Layout {
    Pane(Option<String>),
    Split { vertical: bool, ratio: f32, a: Box<Layout>, b: Box<Layout> },
}

impl Layout {
    /// Directory of the first pane (the one a new tab starts with).
    pub fn first_dir(&self) -> Option<&str> {
        match self {
            Layout::Pane(dir) => dir.as_deref(),
            Layout::Split { a, .. } => a.first_dir(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct TabState {
    /// Tabs with the same group share a window (on macOS: one native tab group).
    pub group: usize,
    /// Index of the focused pane among the layout's panes, left to right.
    pub active: usize,
    pub layout: Layout,
}

fn path() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache.join("litty/session"))
}

pub fn save(tabs: &[TabState]) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, encode(tabs));
}

/// The saved session, removed so that a session that fails to start is not retried forever.
pub fn take() -> Vec<TabState> {
    let Some(path) = path() else { return Vec::new() };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    decode(&text)
}

pub fn clear() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Tabs closed during this run, newest last, for Reopen Closed Tab.
static CLOSED: std::sync::Mutex<Vec<TabState>> = std::sync::Mutex::new(Vec::new());

pub fn closed(tab: TabState) {
    let mut c = CLOSED.lock().unwrap();
    if c.len() == 10 {
        c.remove(0);
    }
    c.push(tab);
}

pub fn reopen() -> Option<TabState> {
    CLOSED.lock().unwrap().pop()
}

pub fn has_closed() -> bool {
    !CLOSED.lock().unwrap().is_empty()
}

pub fn encode(tabs: &[TabState]) -> String {
    fn layout(l: &Layout, out: &mut String) {
        match l {
            Layout::Pane(dir) => {
                out.push('p');
                match dir {
                    Some(d) => d.bytes().for_each(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b'~' => out.push(b as char),
                        _ => out.push_str(&format!("%{b:02X}")),
                    }),
                    None => out.push('-'),
                }
            }
            Layout::Split { vertical, ratio, a, b } => {
                out.push_str(&format!("{}{ratio:.3}(", if *vertical { 'v' } else { 'h' }));
                layout(a, out);
                out.push(',');
                layout(b, out);
                out.push(')');
            }
        }
    }
    let mut out = String::new();
    for t in tabs {
        out.push_str(&format!("{} {} ", t.group, t.active));
        layout(&t.layout, &mut out);
        out.push('\n');
    }
    out
}

/// Lines that don't parse are skipped: a damaged file loses only those tabs.
pub fn decode(text: &str) -> Vec<TabState> {
    fn layout(s: &mut &[u8], depth: u32) -> Option<Layout> {
        let (&kind, rest) = s.split_first()?;
        *s = rest;
        match kind {
            b'p' => {
                let end = s.iter().position(|&b| b == b',' || b == b')').unwrap_or(s.len());
                let (raw, rest) = s.split_at(end);
                *s = rest;
                if raw == b"-" {
                    return Some(Layout::Pane(None));
                }
                let mut dir = Vec::new();
                let mut i = 0;
                while i < raw.len() {
                    let hex = raw.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
                    match (raw[i], hex) {
                        (b'%', Some(b)) => (dir.push(b), i += 3),
                        (b, _) => (dir.push(b), i += 1),
                    };
                }
                Some(Layout::Pane(String::from_utf8(dir).ok().filter(|d| d.starts_with('/'))))
            }
            b'v' | b'h' if depth < 32 => {
                let open = s.iter().position(|&b| b == b'(')?;
                let ratio: f32 = std::str::from_utf8(&s[..open]).ok()?.parse().ok()?;
                *s = &s[open + 1..];
                let a = layout(s, depth + 1)?;
                *s = s.strip_prefix(b",")?;
                let b = layout(s, depth + 1)?;
                *s = s.strip_prefix(b")")?;
                Some(Layout::Split { vertical: kind == b'v', ratio: ratio.clamp(0.05, 0.95), a: Box::new(a), b: Box::new(b) })
            }
            _ => None,
        }
    }
    text.lines()
        .filter_map(|line| {
            let mut it = line.splitn(3, ' ');
            let (group, active) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
            let mut rest = it.next()?.as_bytes();
            let layout = layout(&mut rest, 0).filter(|_| rest.is_empty())?;
            Some(TabState { group, active, layout })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_damage() {
        let pane = |d: Option<&str>| Box::new(Layout::Pane(d.map(String::from)));
        let tabs = vec![
            TabState { group: 0, active: 0, layout: Layout::Pane(Some("/home/me/my project (1),x".into())) },
            TabState {
                group: 0,
                active: 2,
                layout: Layout::Split { vertical: true, ratio: 0.3, a: pane(None), b: Box::new(Layout::Split { vertical: false, ratio: 0.5, a: pane(Some("/tmp")), b: pane(Some("/ไทย")) }) },
            },
            TabState { group: 1, active: 0, layout: Layout::Pane(None) },
        ];
        let text = encode(&tabs);
        assert_eq!(decode(&text), tabs);
        let damaged = format!("{text}1 0 v0.5(p/a\nx y z\n2 0 prelative\n");
        let back = decode(&damaged);
        assert_eq!(back.len(), 4, "broken lines are skipped");
        assert_eq!(back[3].layout, Layout::Pane(None), "only absolute directories are used");
    }
}
