//! Optional settings from `$XDG_CONFIG_HOME/litty/config` (default `~/.config/litty/config`).
//! litty works without the file; every line is `key = value`, `#` starts a comment:
//!
//!   theme = dark | light | auto    (auto follows the system appearance at startup)
//!   font-size = 14                 (points)
//!   cursor = block | bar | underline
//!   cursor-blink = true | false

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
}

const DEFAULT: Config = Config { light: false, font_size: None, cursor: 0, cursor_blink: false };

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

pub fn parse(text: &str) -> Config {
    let mut c = DEFAULT;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
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
        let c = parse("# comment\ntheme = light\nfont-size = 16 # points\ncursor = \"bar\"\ncursor-blink = true\nunknown = 1\nnonsense\n");
        assert!(c.light && c.font_size == Some(16.0) && c.cursor == 2 && c.cursor_blink);
        let c = parse("font-size = 400\ntheme = dark");
        assert!(!c.light && c.font_size == Some(48.0));
    }
}
