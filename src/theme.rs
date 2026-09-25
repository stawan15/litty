//! Colours. The theme is chosen once at startup (see `config`), so cells can store plain RGB.

use std::sync::OnceLock;

pub struct Theme {
    pub fg: u32,
    pub bg: u32,
    pub ansi: [u32; 16],
    pub sel_bg: u32,
    pub match_bg: u32,
    pub cur_match_bg: u32,
    pub match_tick: u32,
    pub fail_tint: u32,
    pub fail_text: u32,
    pub accent: u32,
    pub divider: u32,
    pub cursor_bar: u32,
    pub bar_bg: u32,
    pub tab_text: u32,
    pub tab_text_active: u32,
    pub rail_track: u32,
    pub rail_thumb: u32,
    pub badge_ok: u32,
    /// Background of small floating panels (find bar, update notice).
    pub panel_bg: u32,
}

/// Tokyo Night.
pub const DARK: Theme = Theme {
    fg: 0xc0caf5,
    bg: 0x1a1b26,
    ansi: [
        0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6, 0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7,
        0xbb9af7, 0x7dcfff, 0xc0caf5,
    ],
    sel_bg: 0x33467c,
    match_bg: 0x54432a,
    cur_match_bg: 0xe0af68,
    match_tick: 0xe0af68,
    fail_tint: 0x29212d,
    fail_text: 0xf7768e,
    accent: 0x7aa2f7,
    divider: 0x2f334d,
    cursor_bar: 0xc0caf5,
    bar_bg: 0x16161e,
    tab_text: 0x565f89,
    tab_text_active: 0xc0caf5,
    rail_track: 0x1f2130,
    rail_thumb: 0x414868,
    badge_ok: 0x565f89,
    panel_bg: 0x24283b,
};

/// Tokyo Night Day.
pub const LIGHT: Theme = Theme {
    fg: 0x3760bf,
    bg: 0xe1e2e7,
    ansi: [
        0xb4b5b9, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x6172b0, 0xa1a6c5, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9,
        0x9854f1, 0x007197, 0x3760bf,
    ],
    sel_bg: 0xb7c1e3,
    match_bg: 0xf2dfb5,
    cur_match_bg: 0xe0af68,
    match_tick: 0x8c6c3e,
    fail_tint: 0xf3dfe3,
    fail_text: 0xf52a65,
    accent: 0x2e7de9,
    divider: 0xc4c8da,
    cursor_bar: 0x3760bf,
    bar_bg: 0xd0d5e3,
    tab_text: 0x848cb5,
    tab_text_active: 0x3760bf,
    rail_track: 0xd5d6db,
    rail_thumb: 0xa1a6c5,
    badge_ok: 0x848cb5,
    panel_bg: 0xd0d5e3,
};

static THEME: OnceLock<&'static Theme> = OnceLock::new();

/// Fix the theme for the process (first call wins).
pub fn init(light: bool) {
    let _ = THEME.set(if light { &LIGHT } else { &DARK });
}

pub fn theme() -> &'static Theme {
    THEME.get().copied().unwrap_or(&DARK)
}
