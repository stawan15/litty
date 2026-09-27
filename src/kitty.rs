//! Kitty keyboard protocol (progressive enhancement): the key encoding applications ask for with
//! `CSI > flags u`. Legacy encoding is used for everything the flags leave alone.
//! https://sw.kovidgoyal.net/kitty/keyboard-protocol/

pub const DISAMBIGUATE: u8 = 1;
pub const EVENT_TYPES: u8 = 2;
pub const ALTERNATE_KEYS: u8 = 4;
pub const ALL_KEYS: u8 = 8;
pub const TEXT: u8 = 16;

/// A key, as the protocol names it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    /// A key that types text: its unshifted code point (lowercase), and what Shift makes of it.
    Text(u32, Option<u32>),
    /// Enter, Tab, Backspace and Escape: 'u' codes that also have a legacy byte.
    Control(u32),
    /// Arrows, Home, End, F1..F4 (`CSI 1;mods X`) and keys ending in '~' (`CSI n;mods ~`).
    Func(u32, char),
    /// A key with a private-use code (keypad, modifiers, lock keys): only sent with ALL_KEYS.
    Other(u32),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Press = 1,
    Repeat = 2,
    Release = 3,
}

/// Modifier bits as the protocol counts them (the parameter sent is 1 + these).
pub const SHIFT: u8 = 1;
pub const ALT: u8 = 2;
pub const CTRL: u8 = 4;
pub const SUPER: u8 = 8;

/// The bytes for `key`, or None when the legacy encoding applies. `text` is what the key types.
pub fn encode(key: Key, mods: u8, kind: Kind, flags: u8, text: Option<&str>) -> Option<Vec<u8>> {
    let all = flags & ALL_KEYS != 0;
    if kind != Kind::Press && flags & EVENT_TYPES == 0 {
        return (kind == Kind::Release).then(Vec::new); // releases are never sent without the flag
    }
    // Releases of Enter, Tab and Backspace would confuse shells that only asked for event types.
    if kind == Kind::Release && !all && matches!(key, Key::Control(13 | 9 | 127)) {
        return Some(Vec::new());
    }
    let event = kind != Kind::Press;
    let legacy_ok = !all && !event;
    match key {
        // Text keys keep typing text unless modified (Shift alone still types text).
        Key::Text(..) if legacy_ok && (flags & DISAMBIGUATE == 0 || mods & !SHIFT == 0) => return None,
        Key::Control(27) if legacy_ok && flags & DISAMBIGUATE == 0 => return None,
        Key::Control(c) if legacy_ok && c != 27 && (flags & DISAMBIGUATE == 0 || mods == 0) => return None,
        Key::Func(..) if legacy_ok => return None,
        Key::Other(_) if !all => return None,
        _ => {}
    }
    let mut params = String::new();
    if mods != 0 || event {
        params.push_str(&format!(";{}", 1 + mods));
        if event {
            params.push_str(&format!(":{}", kind as u8));
        }
    }
    let (code, suffix) = match key {
        Key::Text(c, shifted) => {
            let mut code = c.to_string();
            if let Some(s) = shifted.filter(|&s| flags & ALTERNATE_KEYS != 0 && mods & SHIFT != 0 && s != c) {
                code.push_str(&format!(":{s}"));
            }
            if let Some(t) = text.filter(|_| all && flags & TEXT != 0 && kind != Kind::Release) {
                let cps: Vec<String> = t.chars().filter(|c| !c.is_control()).map(|c| (c as u32).to_string()).collect();
                if !cps.is_empty() {
                    if params.is_empty() {
                        params.push(';');
                    }
                    params.push_str(&format!(";{}", cps.join(":")));
                }
            }
            (code, 'u')
        }
        Key::Control(c) | Key::Other(c) => (c.to_string(), 'u'),
        Key::Func(1, x) if params.is_empty() => (String::new(), x),
        Key::Func(n, x) => (n.to_string(), x),
    };
    Some(format!("\x1b[{code}{params}{suffix}").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, mods: u8, kind: Kind, flags: u8, text: Option<&str>) -> Option<String> {
        encode(key, mods, kind, flags, text).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn disambiguate_only_changes_ambiguous_keys() {
        let a = Key::Text(97, Some(65));
        assert_eq!(enc(a, 0, Kind::Press, 1, Some("a")), None);
        assert_eq!(enc(a, SHIFT, Kind::Press, 1, Some("A")), None);
        assert_eq!(enc(a, CTRL, Kind::Press, 1, None).as_deref(), Some("\x1b[97;5u"));
        assert_eq!(enc(a, ALT | SHIFT, Kind::Press, 1, None).as_deref(), Some("\x1b[97;4u"));
        assert_eq!(enc(Key::Control(27), 0, Kind::Press, 1, None).as_deref(), Some("\x1b[27u"));
        assert_eq!(enc(Key::Control(13), 0, Kind::Press, 1, None), None);
        assert_eq!(enc(Key::Control(9), SHIFT, Kind::Press, 1, None).as_deref(), Some("\x1b[9;2u"));
        assert_eq!(enc(Key::Func(1, 'A'), CTRL, Kind::Press, 1, None), None);
        // No flags: nothing changes, and releases are dropped.
        assert_eq!(enc(Key::Control(27), 0, Kind::Press, 0, None), None);
        assert_eq!(enc(a, 0, Kind::Release, 0, None).as_deref(), Some(""));
    }

    #[test]
    fn event_types_alternate_keys_and_text() {
        let a = Key::Text(97, Some(65));
        assert_eq!(enc(a, 0, Kind::Release, 1 | 2, None).as_deref(), Some("\x1b[97;1:3u"));
        assert_eq!(enc(a, 0, Kind::Repeat, 1 | 2, Some("a")).as_deref(), Some("\x1b[97;1:2u"));
        assert_eq!(enc(Key::Func(1, 'A'), 0, Kind::Release, 1 | 2, None).as_deref(), Some("\x1b[1;1:3A"));
        assert_eq!(enc(Key::Func(5, '~'), 0, Kind::Repeat, 2, None).as_deref(), Some("\x1b[5;1:2~"));
        assert_eq!(enc(Key::Control(13), 0, Kind::Release, 2, None).as_deref(), Some(""));
        assert_eq!(enc(a, SHIFT, Kind::Press, 1 | 4 | 8, Some("A")).as_deref(), Some("\x1b[97:65;2u"));
        assert_eq!(enc(a, 0, Kind::Press, 8 | 16, Some("a")).as_deref(), Some("\x1b[97;;97u"));
        assert_eq!(enc(a, SHIFT, Kind::Press, 8 | 16, Some("A")).as_deref(), Some("\x1b[97;2;65u"));
        assert_eq!(enc(Key::Control(13), 0, Kind::Press, 8, None).as_deref(), Some("\x1b[13u"));
        assert_eq!(enc(Key::Func(1, 'D'), 0, Kind::Press, 8, None).as_deref(), Some("\x1b[D"));
        assert_eq!(enc(Key::Other(57441), SHIFT, Kind::Press, 8, None).as_deref(), Some("\x1b[57441;2u"));
        assert_eq!(enc(Key::Other(57441), 0, Kind::Press, 1, None), None);
    }
}
