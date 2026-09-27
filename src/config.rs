//! Optional settings from `$XDG_CONFIG_HOME/litty/config` (default `~/.config/litty/config`).
//! litty works without the file; every line is `key = value`, `#` starts a comment:
//!
//!   theme = dark | light | auto    (auto follows the system appearance at startup)
//!   font-size = 14                 (points)
//!   cursor = block | bar | underline
//!   cursor-blink = true | false
//!   tray = true | false            (macOS menu-bar hamster)
//!   font = JetBrains Mono          (a family name, or a path to the regular .ttf/.otf)
//!   padding = 10                   (points around the text)
//!   scrollback = 20000             (lines kept per pane, up to 200000)
//!   foreground = #c0caf5           (also background, selection, cursor-color, color0 .. color15)
//!   keybind = ctrl+shift+t = new-tab   (`= none` passes the keys to the program instead)

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

pub struct Config {
    pub light: bool,
    /// Points; when set it wins over the zoom remembered from the last session.
    pub font_size: Option<f32>,
    /// 0 block, 1 underline, 2 bar.
    pub cursor: u8,
    pub cursor_blink: bool,
    pub tray: bool,
    /// A font family name or file path.
    pub font: Option<String>,
    /// Points.
    pub padding: Option<f32>,
    pub scrollback: usize,
    /// Colour overrides: 0..=15 are the ANSI colours, then FOREGROUND, BACKGROUND, SELECTION, CURSOR.
    pub colors: Vec<(usize, u32)>,
    pub keybinds: Vec<Keybind>,
}

pub const FOREGROUND: usize = 16;
pub const BACKGROUND: usize = 17;
pub const SELECTION: usize = 18;
pub const CURSOR: usize = 19;

/// A key combination and the action it runs (`None`: send the keys to the program).
#[derive(Debug, PartialEq)]
pub struct Keybind {
    /// SHIFT | ALT | CTRL | SUPER as in `kitty`.
    pub mods: u8,
    /// Lowercase character, or a key name such as "enter", "up", "f5".
    pub key: String,
    pub action: Option<String>,
}

pub const ACTIONS: &[&str] = &[
    "copy", "paste", "copy-output", "new-tab", "new-window", "close", "split-right", "split-down", "find", "clear",
    "zoom-in", "zoom-out", "zoom-reset", "next-tab", "prev-tab", "next-pane", "prev-pane", "prev-prompt", "next-prompt",
    "toggle-zoom", "record", "update",
];

const DEFAULT: Config = Config {
    light: false,
    font_size: None,
    cursor: 0,
    cursor_blink: false,
    tray: true,
    font: None,
    padding: None,
    scrollback: 20_000,
    colors: Vec::new(),
    keybinds: Vec::new(),
};

/// "#rrggbb" or "rrggbb".
fn hex_color(v: &str) -> Option<u32> {
    let h = v.strip_prefix('#').unwrap_or(v);
    (h.len() == 6).then(|| u32::from_str_radix(h, 16).ok()).flatten()
}

/// "ctrl+shift+t = new-tab"
fn keybind(v: &str) -> Option<Keybind> {
    let (combo, action) = v.split_once('=')?;
    let action = action.trim();
    let action = match action {
        "none" => None,
        a if ACTIONS.contains(&a) => Some(a.to_string()),
        _ => return None,
    };
    let (mut mods, mut key) = (0, None);
    for part in combo.trim().split('+').map(|p| p.trim().to_lowercase()) {
        match part.as_str() {
            "shift" => mods |= crate::kitty::SHIFT,
            "alt" | "option" | "opt" => mods |= crate::kitty::ALT,
            "ctrl" | "control" => mods |= crate::kitty::CTRL,
            "super" | "cmd" | "command" => mods |= crate::kitty::SUPER,
            "" => key = Some("+".to_string()),
            _ if key.is_none() => key = Some(part),
            _ => return None,
        }
    }
    Some(Keybind { mods, key: key?, action })
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("litty/config"))
}

fn system_is_dark() -> bool {
    let out = |cmd: &str, args: &[&str]| Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase());
    if cfg!(target_os = "macos") {
        out("defaults", &["read", "-g", "AppleInterfaceStyle"]).is_some_and(|s| s.contains("dark"))
    } else {
        out("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"]).is_none_or(|s| !s.contains("light") && !s.contains("default"))
    }
}

/// A `#` starts a comment at the start of a line or as a word of its own ("16 # points"); inside
/// a value ("#1a1b26") it is kept.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let at = (0..b.len()).find(|&i| {
        b[i] == b'#' && line[..i].trim().is_empty() || (b[i] == b'#' && b[i - 1].is_ascii_whitespace() && b.get(i + 1).is_none_or(u8::is_ascii_whitespace))
    });
    &line[..at.unwrap_or(b.len())]
}

pub fn parse(text: &str) -> Config {
    let mut c = DEFAULT;
    for line in text.lines() {
        let line = strip_comment(line).trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim().trim_matches('"'));
        match key {
            "theme" => c.light = value == "light" || (value == "auto" && !system_is_dark()),
            "font-size" => {
                if let Ok(pt) = value.parse::<f32>() {
                    c.font_size = Some(pt.clamp(6.0, 48.0));
                }
            }
            "cursor" => c.cursor = match value {
                "underline" => 1,
                "bar" => 2,
                _ => 0,
            },
            "cursor-blink" => c.cursor_blink = value == "true",
            "tray" => c.tray = value != "false",
            "font" if !value.is_empty() => c.font = Some(value.to_string()),
            "padding" => c.padding = value.parse::<f32>().ok().map(|p| p.clamp(0.0, 100.0)),
            "scrollback" => c.scrollback = value.parse::<usize>().map_or(c.scrollback, |n| n.min(200_000)),
            "foreground" | "background" | "selection" | "cursor-color" => {
                let slot = match key {
                    "foreground" => FOREGROUND,
                    "background" => BACKGROUND,
                    "selection" => SELECTION,
                    _ => CURSOR,
                };
                c.colors.extend(hex_color(value).map(|col| (slot, col)));
            }
            k if k.starts_with("color") => {
                if let (Some(n), Some(col)) = (k[5..].parse::<usize>().ok().filter(|&n| n < 16), hex_color(value)) {
                    c.colors.push((n, col));
                }
            }
            "keybind" => c.keybinds.extend(keybind(value)),
            _ => {}
        }
    }
    c
}

pub fn get() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| path().and_then(|p| std::fs::read_to_string(p).ok()).map_or(DEFAULT, |t| parse(&t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_keys_and_ignores_the_rest() {
        let c = parse("# comment\ntheme = light\nfont-size = 16 # points\ncursor = \"bar\"\ncursor-blink = true\ntray = false\nunknown = 1\nnonsense\n");
        assert!(c.light && c.font_size == Some(16.0) && c.cursor == 2 && c.cursor_blink && !c.tray);
        let c = parse("font-size = 400\ntheme = dark");
        assert!(!c.light && c.font_size == Some(48.0));
    }

    #[test]
    fn fonts_colours_and_keybinds() {
        let c = parse("font = JetBrains Mono\npadding = 4\nscrollback = 999999\nbackground = #101010\ncolor1 = ff0000\ncolor16 = #ffffff\nforeground = nope\nkeybind = ctrl+shift+t = new-tab\nkeybind = cmd+d = none\nkeybind = ctrl+q = explode\nkeybind = alt++ = zoom-in\n");
        assert_eq!((c.font.as_deref(), c.padding, c.scrollback), (Some("JetBrains Mono"), Some(4.0), 200_000));
        assert_eq!(c.colors, vec![(BACKGROUND, 0x101010), (1, 0xff0000)]);
        let k = |mods, key: &str, action: Option<&str>| Keybind { mods, key: key.into(), action: action.map(Into::into) };
        use crate::kitty::{ALT, CTRL, SHIFT, SUPER};
        assert_eq!(c.keybinds, vec![k(CTRL | SHIFT, "t", Some("new-tab")), k(SUPER, "d", None), k(ALT, "+", Some("zoom-in"))]);
    }
}
