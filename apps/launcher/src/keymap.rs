//! The keyboard keys that drive the game's stand-in pad, saved as
//! `VICTORY_KEYMAP` in the settings file. The game reads it in
//! `crates/victory-hle/src/modules/gameinput/keymap.rs`; the control names
//! and defaults here are the same as there.
//!
//! Keys are named as the page sees them (`KeyboardEvent.code`: where the key
//! is, whatever the layout prints on it), and saved as the virtual-key code
//! of that place on a US keyboard, which is how the game reads letters.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::settings_file;

const VAR: &str = "VICTORY_KEYMAP";

/// Every control of the stand-in pad and its default key.
pub(crate) const CONTROLS: [(&str, &str); 24] = [
    ("a", "Enter"),
    ("b", "Backspace"),
    ("x", "KeyX"),
    ("y", "KeyY"),
    ("menu", "Escape"),
    ("view", "Tab"),
    ("up", "ArrowUp"),
    ("down", "ArrowDown"),
    ("left", "ArrowLeft"),
    ("right", "ArrowRight"),
    ("lb", "KeyQ"),
    ("rb", "KeyE"),
    ("ls", "KeyF"),
    ("rs", "KeyH"),
    ("lt", "KeyZ"),
    ("rt", "KeyC"),
    ("ls_up", "KeyW"),
    ("ls_down", "KeyS"),
    ("ls_left", "KeyA"),
    ("ls_right", "KeyD"),
    ("rs_up", "KeyI"),
    ("rs_down", "KeyK"),
    ("rs_left", "KeyJ"),
    ("rs_right", "KeyL"),
];

/// Keys other than letters, digits and the numeric keypad. F1 and F3 are
/// the layer's own menu and panel, so they are not offered.
const NAMED_KEYS: [(&str, u8); 37] = [
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("CapsLock", 0x14),
    ("Escape", 0x1B),
    ("Space", 0x20),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("ArrowLeft", 0x25),
    ("ArrowUp", 0x26),
    ("ArrowRight", 0x27),
    ("ArrowDown", 0x28),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("ShiftLeft", 0xA0),
    ("ShiftRight", 0xA1),
    ("ControlLeft", 0xA2),
    ("ControlRight", 0xA3),
    ("AltLeft", 0xA4),
    ("AltRight", 0xA5),
    ("Semicolon", 0xBA),
    ("Equal", 0xBB),
    ("Comma", 0xBC),
    ("Minus", 0xBD),
    ("Period", 0xBE),
    ("Slash", 0xBF),
    ("Backquote", 0xC0),
    ("BracketLeft", 0xDB),
    ("Backslash", 0xDC),
    ("BracketRight", 0xDD),
    ("Quote", 0xDE),
    ("NumpadAdd", 0x6B),
    ("NumpadSubtract", 0x6D),
    ("NumpadMultiply", 0x6A),
    ("NumpadDivide", 0x6F),
];

pub(crate) fn code_to_vk(code: &str) -> Option<u8> {
    let one = |rest: &str, lo: u8, hi: u8, base: u8| {
        let [c] = rest.as_bytes() else { return None };
        (lo..=hi).contains(c).then(|| base + (c - lo))
    };
    if let Some(rest) = code.strip_prefix("Key") {
        return one(rest, b'A', b'Z', b'A');
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        return one(rest, b'0', b'9', b'0');
    }
    if let Some(rest) = code.strip_prefix("Numpad")
        && let Some(vk) = one(rest, b'0', b'9', 0x60)
    {
        return Some(vk);
    }
    NAMED_KEYS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|&(_, vk)| vk)
}

pub(crate) fn vk_to_code(vk: u8) -> Option<String> {
    match vk {
        b'A'..=b'Z' => Some(format!("Key{}", vk as char)),
        b'0'..=b'9' => Some(format!("Digit{}", vk as char)),
        0x60..=0x69 => Some(format!("Numpad{}", vk - 0x60)),
        _ => NAMED_KEYS
            .iter()
            .find(|(_, v)| *v == vk)
            .map(|(c, _)| (*c).to_owned()),
    }
}

/// Control name to key code: the defaults with the saved changes over them.
pub(crate) fn from_settings(text: &str) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = CONTROLS
        .iter()
        .map(|(c, k)| ((*c).to_owned(), (*k).to_owned()))
        .collect();
    let spec = settings_file::entries(text)
        .filter(|(k, _)| *k == VAR)
        .map(|(_, v)| v)
        .last()
        .unwrap_or_default();
    for (control, key) in spec.split(',').filter_map(|p| p.split_once('=')) {
        let code = u8::from_str_radix(key.trim(), 16).ok().and_then(vk_to_code);
        if let (Some(slot), Some(code)) = (map.get_mut(control.trim()), code) {
            *slot = code;
        }
    }
    map
}

/// The `VICTORY_KEYMAP` value for `map`: every control, in a fixed order.
pub(crate) fn to_value(map: &BTreeMap<String, String>) -> Result<String, String> {
    let mut pairs = Vec::new();
    for (control, default) in CONTROLS {
        let code = map.get(control).map_or(default, String::as_str);
        let vk = code_to_vk(code).ok_or_else(|| format!("{code} cannot be used for a control"))?;
        pairs.push(format!("{control}={vk:02X}"));
    }
    if let Some(unknown) = map.keys().find(|k| !CONTROLS.iter().any(|(c, _)| c == k)) {
        return Err(format!("No control is named {unknown}"));
    }
    Ok(pairs.join(","))
}

pub(crate) fn load(menu_file: &Path) -> BTreeMap<String, String> {
    from_settings(&settings_file::read(menu_file))
}

pub(crate) fn save(menu_file: &Path, map: &BTreeMap<String, String>) -> Result<(), String> {
    let value = to_value(map)?;
    settings_file::set(menu_file, &[(VAR, value)]).map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_virtual_keys_round_trip() {
        for code in [
            "KeyA",
            "KeyZ",
            "Digit0",
            "Numpad7",
            "Space",
            "ShiftLeft",
            "Quote",
        ] {
            let vk = code_to_vk(code).unwrap();
            assert_eq!(vk_to_code(vk).as_deref(), Some(code));
        }
        assert_eq!(code_to_vk("KeyW"), Some(b'W'));
        assert_eq!(code_to_vk("F1"), None);
        assert_eq!(code_to_vk("KeyAB"), None);
    }

    #[test]
    fn the_defaults_are_the_games() {
        let map = from_settings("");
        assert_eq!(map["a"], "Enter");
        assert_eq!(map["ls_up"], "KeyW");
        let value = to_value(&map).unwrap();
        assert!(value.starts_with("a=0D,b=08,x=58,y=59,menu=1B,"));
        assert!(value.ends_with("rs_left=4A,rs_right=4C"));
    }

    #[test]
    fn saved_keys_come_back() {
        let mut map = from_settings("");
        map.insert("a".into(), "Space".into());
        map.insert("lt".into(), "ControlLeft".into());
        let text = format!("VICTORY_MENU_FPS=60\n{VAR}={}\n", to_value(&map).unwrap());
        assert_eq!(from_settings(&text), map);
    }

    #[test]
    fn unknown_keys_and_controls_are_refused() {
        let mut map = from_settings("");
        map.insert("a".into(), "F1".into());
        assert!(to_value(&map).is_err());
        let mut map = from_settings("");
        map.insert("turbo".into(), "KeyT".into());
        assert!(to_value(&map).is_err());
    }
}
