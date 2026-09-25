//! Cmd+Shift+R: record a pane as an asciinema v2 cast (`litty-<date>-<time>.cast` on the Desktop,
//! else in the home directory), to replay with `asciinema play` or embed on a web page.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct Recorder {
    out: BufWriter<File>,
    pub path: PathBuf,
    t0: Instant,
    /// Trailing bytes of an unfinished UTF-8 character, kept for the next chunk.
    partial: Vec<u8>,
}

impl Recorder {
    pub fn start(cols: usize, rows: usize) -> std::io::Result<Recorder> {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
        let dir = Some(home.join("Desktop")).filter(|d| d.is_dir()).unwrap_or(home);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let path = dir.join(format!("litty-{}.cast", local_stamp(now as i64)));
        let mut out = BufWriter::new(File::create(&path)?);
        let shell = std::env::var("SHELL").unwrap_or_default();
        writeln!(out, r#"{{"version": 2, "width": {cols}, "height": {rows}, "timestamp": {now}, "title": "litty", "env": {{"TERM": "xterm-256color", "SHELL": {}}}}}"#, json(&shell))?;
        Ok(Recorder { out, path, t0: Instant::now(), partial: Vec::new() })
    }

    /// Output from the shell. Only whole UTF-8 characters are written; invalid bytes become U+FFFD.
    pub fn output(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        let keep = incomplete_tail(&self.partial);
        let text = String::from_utf8_lossy(&self.partial[..self.partial.len() - keep]).into_owned();
        self.partial.drain(..self.partial.len() - keep);
        if !text.is_empty() {
            self.event("o", &text);
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.event("r", &format!("{cols}x{rows}"));
    }

    fn event(&mut self, kind: &str, data: &str) {
        let _ = writeln!(self.out, "[{:.6}, \"{kind}\", {}]", self.t0.elapsed().as_secs_f64(), json(data));
    }

    pub fn finish(mut self) -> PathBuf {
        let _ = self.out.flush();
        self.path
    }
}

/// Length of an unfinished UTF-8 sequence at the end of `b` (0 when it ends on a whole character).
fn incomplete_tail(b: &[u8]) -> usize {
    for back in 1..=3.min(b.len()) {
        let c = b[b.len() - back];
        if c & 0xC0 != 0x80 {
            let need = match c {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            return if need > back { back } else { 0 };
        }
    }
    0
}

fn json(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// "20260925-143012" in local time.
fn local_stamp(secs: i64) -> String {
    // SAFETY: localtime_r only writes the struct we pass.
    let mut tm: nix::libc::tm = unsafe { std::mem::zeroed() };
    unsafe { nix::libc::localtime_r(&secs, &mut tm) };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_utf8_between_chunks_and_escapes_json() {
        assert_eq!(incomplete_tail("aก".as_bytes()), 0);
        assert_eq!(incomplete_tail(&"aก".as_bytes()[..2]), 1);
        assert_eq!(incomplete_tail(&"aก".as_bytes()[..3]), 2);
        assert_eq!(json("a\"b\\\n\x1b[1m"), r#""a\"b\\\n\u001b[1m""#);

        let dir = std::env::temp_dir().join(format!("litty-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.cast");
        let mut r = Recorder { out: BufWriter::new(File::create(&path).unwrap()), path: path.clone(), t0: Instant::now(), partial: Vec::new() };
        let thai = "ก".as_bytes();
        r.output(&[b'x', thai[0]]);
        r.output(&thai[1..]);
        r.resize(80, 24);
        let text = std::fs::read_to_string(r.finish()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].ends_with(r#", "o", "x"]"#), "{text}");
        assert!(lines[1].ends_with(r#", "o", "ก"]"#), "{text}");
        assert!(lines[2].ends_with(r#", "r", "80x24"]"#), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
