//! The keys of the keyboard's stand-in gamepad (`victory_hle`'s gameinput
//! module), one per pad control, which the F1 menu can change while the
//! game runs.
//!
//! Each control's key is `VICTORY_KEY_<CONTROL>` if that is set (a virtual
//! key as `0x57`, or a single letter or digit), else the default below,
//! which is the layout the README lists. The values are read once, at the
//! first use, into atomics; the menu stores a new key there at once and
//! saves the variable for the next start. The input thread reads the
//! atomics at every poll (24 loads every 8 ms), so a change shows at the
//! next poll.
//!
//! Only keys the input module reports can be used: 0x08 to 0xFE, no mouse
//! buttons (below 0x08) and no shift, ctrl or alt without a side (0x10 to
//! 0x12; the left and right ones, 0xA0 to 0xA5, are reported). F1 and F3
//! open the settings menu and the stats panel, so they are kept for those.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// A control of the stand-in pad: its variable, its name in the menu and
/// its key when the variable is not set.
pub struct Bind {
    pub var: &'static str,
    pub label: &'static str,
    pub default: u8,
}

/// The controls' places in [`BINDS`], for the input module's tables.
pub mod control {
    pub const MENU: usize = 0;
    pub const VIEW: usize = 1;
    pub const A: usize = 2;
    pub const B: usize = 3;
    pub const X: usize = 4;
    pub const Y: usize = 5;
    pub const UP: usize = 6;
    pub const DOWN: usize = 7;
    pub const LEFT: usize = 8;
    pub const RIGHT: usize = 9;
    pub const LEFT_SHOULDER: usize = 10;
    pub const RIGHT_SHOULDER: usize = 11;
    pub const LEFT_STICK: usize = 12;
    pub const RIGHT_STICK: usize = 13;
    pub const LEFT_TRIGGER: usize = 14;
    pub const RIGHT_TRIGGER: usize = 15;
    pub const LEFT_STICK_UP: usize = 16;
    pub const LEFT_STICK_DOWN: usize = 17;
    pub const LEFT_STICK_LEFT: usize = 18;
    pub const LEFT_STICK_RIGHT: usize = 19;
    pub const RIGHT_STICK_UP: usize = 20;
    pub const RIGHT_STICK_DOWN: usize = 21;
    pub const RIGHT_STICK_LEFT: usize = 22;
    pub const RIGHT_STICK_RIGHT: usize = 23;
}

/// How many controls there are.
pub const COUNT: usize = 24;

/// Every control, in the order of [`control`]. The labels fit the menu's
/// name column (27 characters).
#[rustfmt::skip]
pub const BINDS: [Bind; COUNT] = [
    Bind { var: "VICTORY_KEY_MENU", label: "KEY: MENU (START)", default: 0x1B },          // Esc
    Bind { var: "VICTORY_KEY_VIEW", label: "KEY: VIEW (BACK)", default: 0x09 },           // Tab
    Bind { var: "VICTORY_KEY_A", label: "KEY: A", default: 0x0D },                        // Enter
    Bind { var: "VICTORY_KEY_B", label: "KEY: B", default: 0x08 },                        // Backspace
    Bind { var: "VICTORY_KEY_X", label: "KEY: X", default: b'X' },
    Bind { var: "VICTORY_KEY_Y", label: "KEY: Y", default: b'Y' },
    Bind { var: "VICTORY_KEY_DPAD_UP", label: "KEY: D-PAD UP", default: 0x26 },           // arrows
    Bind { var: "VICTORY_KEY_DPAD_DOWN", label: "KEY: D-PAD DOWN", default: 0x28 },
    Bind { var: "VICTORY_KEY_DPAD_LEFT", label: "KEY: D-PAD LEFT", default: 0x25 },
    Bind { var: "VICTORY_KEY_DPAD_RIGHT", label: "KEY: D-PAD RIGHT", default: 0x27 },
    Bind { var: "VICTORY_KEY_LB", label: "KEY: LEFT BUMPER", default: b'Q' },
    Bind { var: "VICTORY_KEY_RB", label: "KEY: RIGHT BUMPER", default: b'E' },
    Bind { var: "VICTORY_KEY_LS", label: "KEY: LEFT STICK PRESS", default: b'F' },
    Bind { var: "VICTORY_KEY_RS", label: "KEY: RIGHT STICK PRESS", default: b'H' },
    Bind { var: "VICTORY_KEY_LT", label: "KEY: LEFT TRIGGER", default: b'Z' },
    Bind { var: "VICTORY_KEY_RT", label: "KEY: RIGHT TRIGGER", default: b'C' },
    Bind { var: "VICTORY_KEY_LS_UP", label: "KEY: LEFT STICK UP", default: b'W' },
    Bind { var: "VICTORY_KEY_LS_DOWN", label: "KEY: LEFT STICK DOWN", default: b'S' },
    Bind { var: "VICTORY_KEY_LS_LEFT", label: "KEY: LEFT STICK LEFT", default: b'A' },
    Bind { var: "VICTORY_KEY_LS_RIGHT", label: "KEY: LEFT STICK RIGHT", default: b'D' },
    Bind { var: "VICTORY_KEY_RS_UP", label: "KEY: RIGHT STICK UP", default: b'I' },
    Bind { var: "VICTORY_KEY_RS_DOWN", label: "KEY: RIGHT STICK DOWN", default: b'K' },
    Bind { var: "VICTORY_KEY_RS_LEFT", label: "KEY: RIGHT STICK LEFT", default: b'J' },
    Bind { var: "VICTORY_KEY_RS_RIGHT", label: "KEY: RIGHT STICK RIGHT", default: b'L' },
];

/// The keys in use, by control; filled from the variables at the first use.
fn keys() -> &'static [AtomicU8; COUNT] {
    static KEYS: OnceLock<[AtomicU8; COUNT]> = OnceLock::new();
    KEYS.get_or_init(|| {
        std::array::from_fn(|i| {
            let bind = &BINDS[i];
            // a value that is not a usable key is said once and the default kept
            let key = match std::env::var(bind.var) {
                Ok(v) if !v.trim().is_empty() => match parse(&v) {
                    Some(vk) => vk,
                    None => {
                        tracing::warn!(var = bind.var, value = %v, "not a key that can drive the stand-in pad; the default is kept");
                        bind.default
                    }
                },
                _ => bind.default,
            };
            AtomicU8::new(key)
        })
    })
}

/// The key of `control` (a place in [`BINDS`]); 0, which no key reports,
/// for a place out of range.
pub fn key(control: usize) -> u8 {
    keys().get(control).map_or(0, |k| k.load(Ordering::Relaxed))
}

/// Makes `vk` the key of `control` from now on (the menu; it saves the
/// variable itself). Nothing happens for a place out of range or a key that
/// cannot be used.
pub fn set(control: usize, vk: u8) {
    if !usable(vk) {
        return;
    }
    if let Some(k) = keys().get(control) {
        k.store(vk, Ordering::Relaxed);
    }
}

/// Whether the input module reports `vk` and nothing else of ours owns it.
pub fn usable(vk: u8) -> bool {
    vk >= 0x08 && !(0x10..=0x12).contains(&vk) && vk != 0x70 && vk != 0x72 && vk != 0xFF
}

/// A variable's value as a key: `0x57` (hexadecimal), or one letter or digit.
pub fn parse(value: &str) -> Option<u8> {
    let v = value.trim();
    let vk = if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).ok()?
    } else if let [c] = v.as_bytes()
        && c.is_ascii_alphanumeric()
    {
        c.to_ascii_uppercase()
    } else {
        return None;
    };
    usable(vk).then_some(vk)
}

/// The value the menu saves for `vk` (read back by [`parse`]).
pub fn value(vk: u8) -> String {
    format!("0x{vk:02X}")
}

/// A key's name as the menu shows it, in the characters its font has
/// (letters, digits and `. / - > < * : _ = ( ) +`).
pub fn name(vk: u8) -> String {
    let named = match vk {
        0x08 => "BACKSPACE",
        0x09 => "TAB",
        0x0C => "CLEAR",
        0x0D => "ENTER",
        0x13 => "PAUSE",
        0x14 => "CAPS LOCK",
        0x1B => "ESC",
        0x20 => "SPACE",
        0x21 => "PAGE UP",
        0x22 => "PAGE DOWN",
        0x23 => "END",
        0x24 => "HOME",
        0x25 => "LEFT",
        0x26 => "UP",
        0x27 => "RIGHT",
        0x28 => "DOWN",
        0x2C => "PRINT SCREEN",
        0x2D => "INSERT",
        0x2E => "DELETE",
        0x5B => "LEFT WINDOWS",
        0x5C => "RIGHT WINDOWS",
        0x5D => "APPS",
        0x6A => "NUM *",
        0x6B => "NUM +",
        0x6D => "NUM -",
        0x6E => "NUM .",
        0x6F => "NUM /",
        0x90 => "NUM LOCK",
        0x91 => "SCROLL LOCK",
        0xA0 => "LEFT SHIFT",
        0xA1 => "RIGHT SHIFT",
        0xA2 => "LEFT CTRL",
        0xA3 => "RIGHT CTRL",
        0xA4 => "LEFT ALT",
        0xA5 => "RIGHT ALT",
        0xBA => "SEMICOLON",
        0xBB => "=",
        0xBC => "COMMA",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "BACKQUOTE",
        0xDB => "LEFT BRACKET",
        0xDC => "BACKSLASH",
        0xDD => "RIGHT BRACKET",
        0xDE => "QUOTE",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    match vk {
        b'0'..=b'9' | b'A'..=b'Z' => (vk as char).to_string(),
        0x60..=0x69 => format!("NUM {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("KEY {}", value(vk)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_back() {
        for vk in (0..=0xFF_u8).filter(|&vk| usable(vk)) {
            assert_eq!(parse(&value(vk)), Some(vk));
        }
        assert_eq!(parse("w"), Some(b'W'));
        assert_eq!(parse("7"), Some(b'7'));
        assert_eq!(parse(""), None);
        assert_eq!(parse("0x70"), None, "F1 is the menu's");
        assert_eq!(parse("0x01"), None, "mouse buttons are not reported");
        assert_eq!(parse("WW"), None);
    }

    #[test]
    fn defaults_are_usable_and_distinct() {
        for (i, a) in BINDS.iter().enumerate() {
            assert!(usable(a.default), "{}", a.var);
            for b in &BINDS[i + 1..] {
                assert_ne!(a.default, b.default, "{} and {}", a.var, b.var);
                assert_ne!(a.var, b.var);
            }
            assert!(a.label.len() <= 27, "{}", a.label);
        }
    }

    #[test]
    fn names_are_short() {
        for vk in 0..=0xFF_u8 {
            assert!(!name(vk).is_empty() && name(vk).len() <= 14, "{vk:#x}: {}", name(vk));
        }
    }
}
