//! The keyboard and mouse controls: which keys and mouse buttons press which of
//! the pad's inputs, what the mouse's motion moves in each mode, and its speed.
//! Read from `victory-controls.txt` (written with the defaults at the first
//! start, beside `victory-settings.txt`), and read again whenever it is saved.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER, RIGHT_STICK, UP,
    VIEW, X, Y,
};

/// The file's name.
const NAME: &str = "victory-controls.txt";

/// One of the pad's inputs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PadInput {
    Button(u32),
    LeftTrigger,
    RightTrigger,
    /// A stick pushed one way: the right stick or the left; x and y -1, 0 or 1 (up positive).
    Stick { right: bool, x: i8, y: i8 },
}

impl PadInput {
    /// Presses this input on `pad`; opposite ways of a stick cancel.
    pub(super) fn press(self, pad: &mut PadState) {
        match self {
            PadInput::Button(bit) => pad.buttons |= bit,
            PadInput::LeftTrigger => pad.left_trigger = 1.0,
            PadInput::RightTrigger => pad.right_trigger = 1.0,
            PadInput::Stick { right, x, y } => {
                let (sx, sy) = if right { (&mut pad.right_x, &mut pad.right_y) } else { (&mut pad.left_x, &mut pad.left_y) };
                *sx = (*sx + f32::from(x)).clamp(-1.0, 1.0);
                *sy = (*sy + f32::from(y)).clamp(-1.0, 1.0);
            }
        }
    }
}

const fn stick(right: bool, x: i8, y: i8) -> PadInput {
    PadInput::Stick { right, x, y }
}

/// The pad's inputs by name, in the order the file lists them.
const PAD_INPUTS: [(&str, PadInput); 28] = [
    ("A", PadInput::Button(A)),
    ("B", PadInput::Button(B)),
    ("X", PadInput::Button(X)),
    ("Y", PadInput::Button(Y)),
    ("LB", PadInput::Button(LEFT_SHOULDER)),
    ("RB", PadInput::Button(RIGHT_SHOULDER)),
    ("LT", PadInput::LeftTrigger),
    ("RT", PadInput::RightTrigger),
    ("L3", PadInput::Button(LEFT_STICK)),
    ("R3", PadInput::Button(RIGHT_STICK)),
    ("VIEW", PadInput::Button(VIEW)),
    ("MENU", PadInput::Button(MENU)),
    ("DPAD UP", PadInput::Button(UP)),
    ("DPAD DOWN", PadInput::Button(DOWN)),
    ("DPAD LEFT", PadInput::Button(LEFT)),
    ("DPAD RIGHT", PadInput::Button(RIGHT)),
    ("LEFT STICK UP", stick(false, 0, 1)),
    ("LEFT STICK DOWN", stick(false, 0, -1)),
    ("LEFT STICK LEFT", stick(false, -1, 0)),
    ("LEFT STICK RIGHT", stick(false, 1, 0)),
    ("RIGHT STICK UP", stick(true, 0, 1)),
    ("RIGHT STICK DOWN", stick(true, 0, -1)),
    ("RIGHT STICK LEFT", stick(true, -1, 0)),
    ("RIGHT STICK RIGHT", stick(true, 1, 0)),
    // Other names for the same inputs.
    ("START", PadInput::Button(MENU)),
    ("BACK", PadInput::Button(VIEW)),
    ("LS", PadInput::Button(LEFT_STICK)),
    ("RS", PadInput::Button(RIGHT_STICK)),
];

fn pad_input(name: &str) -> Option<PadInput> {
    PAD_INPUTS.iter().find(|(n, _)| *n == name).map(|&(_, input)| input)
}

/// Mouse buttons, as the host's sample has them.
pub(super) const MOUSE_LEFT: u32 = 0x1;
pub(super) const MOUSE_RIGHT: u32 = 0x2;
pub(super) const MOUSE_MIDDLE: u32 = 0x4;
pub(super) const MOUSE_BACK: u32 = 0x8;
pub(super) const MOUSE_FORWARD: u32 = 0x10;

/// A mouse button, or a notch of the wheel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum MouseInput {
    Button(u32),
    WheelUp,
    WheelDown,
}

/// Mouse inputs by name in the file, and on the F2 page.
const MOUSE_INPUTS: [(&str, &str, MouseInput); 7] = [
    ("LEFT BUTTON", "LEFT", MouseInput::Button(MOUSE_LEFT)),
    ("RIGHT BUTTON", "RIGHT", MouseInput::Button(MOUSE_RIGHT)),
    ("MIDDLE BUTTON", "MIDDLE", MouseInput::Button(MOUSE_MIDDLE)),
    ("BACK BUTTON", "BACK", MouseInput::Button(MOUSE_BACK)),
    ("FORWARD BUTTON", "FWD", MouseInput::Button(MOUSE_FORWARD)),
    ("WHEEL UP", "WHEEL+", MouseInput::WheelUp),
    ("WHEEL DOWN", "WHEEL-", MouseInput::WheelDown),
];

fn mouse_input(name: &str) -> Option<MouseInput> {
    // "MOUSE LEFT", "LEFT CLICK" and "LEFT BUTTON" are one button.
    let name = name.strip_prefix("MOUSE ").unwrap_or(name);
    let name = name.strip_suffix(" CLICK").map_or_else(|| name.to_owned(), |n| format!("{n} BUTTON"));
    let name = if MOUSE_INPUTS.iter().any(|(n, _, _)| *n == name) { name } else { format!("{name} BUTTON") };
    MOUSE_INPUTS.iter().find(|(n, _, _)| *n == name).map(|&(_, _, input)| input)
}

fn mouse_short(input: MouseInput) -> &'static str {
    MOUSE_INPUTS.iter().find(|(_, _, i)| *i == input).map_or("?", |&(_, short, _)| short)
}

/// The virtual keys a key's name stands for (both of a pair for SHIFT, CTRL and ALT).
/// F1, F2 and F3 are VIctory's own and are not offered.
fn key_codes(name: &str) -> Option<Vec<u8>> {
    let one = |vk: u8| Some(vec![vk]);
    match name {
        "ENTER" | "RETURN" => one(0x0D),
        "ESC" | "ESCAPE" => one(0x1B),
        "TAB" => one(0x09),
        "BACKSPACE" => one(0x08),
        "SPACE" => one(0x20),
        "UP" => one(0x26),
        "DOWN" => one(0x28),
        "LEFT" => one(0x25),
        "RIGHT" => one(0x27),
        "SHIFT" => Some(vec![0xA0, 0xA1]),
        "LEFT SHIFT" => one(0xA0),
        "RIGHT SHIFT" => one(0xA1),
        "CTRL" | "CONTROL" => Some(vec![0xA2, 0xA3]),
        "LEFT CTRL" => one(0xA2),
        "RIGHT CTRL" => one(0xA3),
        "ALT" => Some(vec![0xA4, 0xA5]),
        "LEFT ALT" => one(0xA4),
        "RIGHT ALT" => one(0xA5),
        "CAPS LOCK" => one(0x14),
        "INSERT" => one(0x2D),
        "DELETE" => one(0x2E),
        "HOME" => one(0x24),
        "END" => one(0x23),
        "PAGE UP" => one(0x21),
        "PAGE DOWN" => one(0x22),
        n if n.len() == 1 && n.as_bytes()[0].is_ascii_alphanumeric() => one(n.as_bytes()[0]),
        n => {
            if let Some(d) = n.strip_prefix("NUMPAD ").and_then(|d| d.parse::<u8>().ok()).filter(|d| *d <= 9) {
                return one(0x60 + d);
            }
            let f = n.strip_prefix('F').and_then(|f| f.parse::<u8>().ok()).filter(|f| (4..=12).contains(f))?;
            one(0x6F + f)
        }
    }
}

/// A key's short name on the F2 page.
fn key_short(vk: u8) -> String {
    match vk {
        0x0D => "ENTER".into(),
        0x1B => "ESC".into(),
        0x09 => "TAB".into(),
        0x08 => "BKSP".into(),
        0x20 => "SPACE".into(),
        0x26 => "UP".into(),
        0x28 => "DOWN".into(),
        0x25 => "LEFT".into(),
        0x27 => "RIGHT".into(),
        0xA0 => "LSHIFT".into(),
        0xA1 => "RSHIFT".into(),
        0xA2 => "LCTRL".into(),
        0xA3 => "RCTRL".into(),
        0xA4 => "LALT".into(),
        0xA5 => "RALT".into(),
        0x14 => "CAPS".into(),
        0x2D => "INS".into(),
        0x2E => "DEL".into(),
        0x24 => "HOME".into(),
        0x23 => "END".into(),
        0x21 => "PGUP".into(),
        0x22 => "PGDN".into(),
        0x60..=0x69 => format!("NUM{}", vk - 0x60),
        0x73..=0x7B => format!("F{}", vk - 0x6F),
        c => char::from(c).to_string(),
    }
}

/// What the mouse does in one of its modes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct MouseMode {
    /// The stick its motion moves: `Some(true)` the right, `Some(false)` the left.
    pub(super) moves: Option<bool>,
    pub(super) buttons: Vec<(MouseInput, PadInput)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Bindings {
    pub(super) keys: Vec<(u8, PadInput)>,
    pub(super) play: MouseMode,
    pub(super) menu: MouseMode,
    /// Mouse buttons that switch between the modes.
    pub(super) switch: u32,
    /// The keys of the pad's menu button switch the mode too (the title's pause).
    pub(super) pause_switches: bool,
    /// The mouse's speed (`None`: `VICTORY_MOUSE_LOOK`).
    pub(super) speed: Option<f32>,
    /// Lines not understood, as "LINE n: text".
    pub(super) errors: Vec<String>,
}

impl Default for Bindings {
    /// The defaults, with the speed left to `VICTORY_MOUSE_LOOK`.
    fn default() -> Bindings {
        parse(&DEFAULT.replace("SPEED = {speed}", ""))
    }
}

impl Bindings {
    /// Whether `vk` presses the pad's menu button, when that switches the mouse's mode.
    pub(super) fn is_pause_key(&self, vk: u8) -> bool {
        self.pause_switches && self.keys.iter().any(|&(k, input)| k == vk && input == PadInput::Button(MENU))
    }

    /// The pad as the keys held press it.
    pub(super) fn pad(&self, held: impl Fn(u8) -> bool) -> PadState {
        let mut pad = PadState::default();
        for &(vk, input) in &self.keys {
            if held(vk) {
                input.press(&mut pad);
            }
        }
        pad
    }
}

#[derive(Clone, Copy)]
enum Section {
    Keyboard,
    MousePlay,
    MouseMenu,
    Mouse,
}

/// Upper case, `_` and `-` as spaces, one space between words.
fn normal(s: &str) -> String {
    let s = s.to_ascii_uppercase().replace(['_', '-'], " ");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.replace("D PAD", "DPAD")
}

/// The controls a file's text gives. Every input not named is left unbound.
pub(super) fn parse(text: &str) -> Bindings {
    let mut b = Bindings {
        keys: Vec::new(),
        play: MouseMode::default(),
        menu: MouseMode::default(),
        switch: 0,
        pause_switches: false,
        speed: None,
        errors: Vec::new(),
    };
    let mut section = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let bad = |b: &mut Bindings| b.errors.push(format!("LINE {}: {}", n + 1, raw.trim()));
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = match normal(name).as_str() {
                "KEYBOARD" => Some(Section::Keyboard),
                "MOUSE PLAY" => Some(Section::MousePlay),
                "MOUSE MENU" => Some(Section::MouseMenu),
                "MOUSE" => Some(Section::Mouse),
                _ => {
                    bad(&mut b);
                    None
                }
            };
            continue;
        }
        let Some((left, right)) = line.split_once('=') else {
            bad(&mut b);
            continue;
        };
        let left = normal(left);
        let values: Vec<String> = right.split(',').map(normal).filter(|v| !v.is_empty()).collect();
        let ok = match section {
            None => false,
            Some(Section::Keyboard) => match pad_input(&left) {
                Some(input) => values.iter().all(|v| match key_codes(v) {
                    Some(vks) => {
                        b.keys.extend(vks.into_iter().map(|vk| (vk, input)));
                        true
                    }
                    None => false,
                }),
                None => false,
            },
            Some(s @ (Section::MousePlay | Section::MouseMenu)) => {
                let mode = if matches!(s, Section::MousePlay) { &mut b.play } else { &mut b.menu };
                if left == "MOVE" {
                    let moves = match values.as_slice() {
                        [] => Some(None),
                        [v] if v == "NONE" => Some(None),
                        [v] if v == "RIGHT STICK" => Some(Some(true)),
                        [v] if v == "LEFT STICK" => Some(Some(false)),
                        _ => None,
                    };
                    moves.map(|m| mode.moves = m).is_some()
                } else {
                    match pad_input(&left) {
                        Some(input) => values.iter().all(|v| match mouse_input(v) {
                            Some(m) => {
                                mode.buttons.push((m, input));
                                true
                            }
                            None => false,
                        }),
                        None => false,
                    }
                }
            }
            Some(Section::Mouse) => match left.as_str() {
                "SPEED" => match values.first().and_then(|v| v.parse::<f32>().ok()).filter(|s| s.is_finite() && *s >= 0.0) {
                    Some(speed) if values.len() == 1 => {
                        b.speed = Some(speed);
                        true
                    }
                    _ => false,
                },
                "SWITCH MODE" => values.iter().all(|v| match mouse_input(v) {
                    Some(MouseInput::Button(bit)) => {
                        b.switch |= bit;
                        true
                    }
                    _ => false,
                }),
                "PAUSE SWITCHES MODE" => match values.first().map(String::as_str) {
                    Some("YES") if values.len() == 1 => {
                        b.pause_switches = true;
                        true
                    }
                    Some("NO") if values.len() == 1 => {
                        b.pause_switches = false;
                        true
                    }
                    _ => false,
                },
                _ => false,
            },
        };
        if !ok {
            bad(&mut b);
        }
    }
    b
}

/// The file with the defaults, written at the first start.
pub(super) const DEFAULT: &str = "\
# VIctory keyboard and mouse controls. Saved changes apply at once, while the game runs.
# F2 in the game shows what is set. A line that is not understood is left out (F2 counts them).
#
# Each line: a pad input = the keys or mouse buttons that press it, separated by commas.
# Leave the right side empty to have nothing press it. Lines starting with # are notes.
#
# Pad inputs: A, B, X, Y, LB, RB, LT, RT, L3, R3, VIEW, MENU,
#   DPAD UP / DOWN / LEFT / RIGHT, LEFT STICK UP / DOWN / LEFT / RIGHT,
#   RIGHT STICK UP / DOWN / LEFT / RIGHT
# Keys: A to Z, 0 to 9, ENTER, ESC, TAB, BACKSPACE, SPACE, UP, DOWN, LEFT, RIGHT,
#   SHIFT, LEFT SHIFT, RIGHT SHIFT, CTRL, LEFT CTRL, RIGHT CTRL, ALT, LEFT ALT, RIGHT ALT,
#   CAPS LOCK, INSERT, DELETE, HOME, END, PAGE UP, PAGE DOWN, NUMPAD 0 to NUMPAD 9, F4 to F12
#   (F1, F2 and F3 are VIctory's: settings, controls, stats)
#   A letter means the key where a US keyboard has that letter, so the same keys are
#   under your fingers on every layout (W A S D are Z Q S D on an AZERTY keyboard).
# Mouse: LEFT BUTTON, RIGHT BUTTON, MIDDLE BUTTON, BACK BUTTON, FORWARD BUTTON,
#   WHEEL UP, WHEEL DOWN
#
# The mouse has two modes: PLAY (the camera) and MENU (the game's menus).
# MOVE says which stick the mouse's motion moves in a mode: RIGHT STICK, LEFT STICK or NONE.

[KEYBOARD]
A = ENTER
B = BACKSPACE
X = X
Y = Y
LB = Q
RB = E
LT = Z
RT = C
L3 = F
R3 = H
VIEW = TAB
MENU = ESC
DPAD UP = UP
DPAD DOWN = DOWN
DPAD LEFT = LEFT
DPAD RIGHT = RIGHT
LEFT STICK UP = W
LEFT STICK DOWN = S
LEFT STICK LEFT = A
LEFT STICK RIGHT = D
RIGHT STICK UP = I
RIGHT STICK DOWN = K
RIGHT STICK LEFT = J
RIGHT STICK RIGHT = L

[MOUSE PLAY]
MOVE = RIGHT STICK
LT = RIGHT BUTTON
RT = LEFT BUTTON

[MOUSE MENU]
MOVE = LEFT STICK
A = LEFT BUTTON
B = RIGHT BUTTON
LB = BACK BUTTON
RB = FORWARD BUTTON
DPAD UP = WHEEL UP
DPAD DOWN = WHEEL DOWN

[MOUSE]
# How fast the mouse moves a stick. Higher is faster, 0 turns the mouse off.
# Without this line, VICTORY_MOUSE_LOOK in victory-settings.txt sets it.
SPEED = {speed}
# Mouse buttons that switch between PLAY and MENU.
SWITCH MODE = MIDDLE BUTTON
# Whether the keys of MENU (the game's pause) switch the mode too: YES or NO.
PAUSE SWITCHES MODE = YES
";

/// Where the controls are kept: `VICTORY_CONTROLS_FILE`, else beside the settings file
/// (`VICTORY_MENU_FILE`, `VICTORY_HOME`, `%LOCALAPPDATA%\victory`).
pub(super) fn file() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    set("VICTORY_CONTROLS_FILE")
        .or_else(|| set("VICTORY_MENU_FILE").and_then(|f| f.parent().map(|d| d.join(NAME))))
        .or_else(|| set("VICTORY_HOME").map(|h| h.join(NAME)))
        .or_else(|| set("LOCALAPPDATA").map(|b| b.join("victory").join(NAME)))
}

/// When the file was last saved.
pub(super) fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The controls in `path`; with no file there, the defaults, written there first
/// (with `speed`, the speed this start was given).
pub(super) fn load(path: &Path, speed: f32) -> Bindings {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let b = parse(&text);
            for e in &b.errors {
                tracing::warn!(file = %path.display(), "controls: not understood, left out: {e}");
            }
            tracing::info!(file = %path.display(), keys = b.keys.len(), errors = b.errors.len(), "controls read");
            b
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let text = DEFAULT.replace("{speed}", &speed.to_string());
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(path, text.replace('\n', "\r\n")) {
                Ok(()) => tracing::info!(file = %path.display(), "controls: the defaults written"),
                Err(e) => tracing::warn!(file = %path.display(), error = %e, "controls: the defaults not written"),
            }
            parse(&text)
        }
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "controls: not read; the defaults apply");
            Bindings::default()
        }
    }
}

/// The F2 page: a line for each pad input, the keys and mouse inputs that press it.
pub(super) fn page(b: &Bindings, speed: f32) -> Vec<String> {
    let keys_of = |input: PadInput| {
        let mut vks: Vec<u8> = b.keys.iter().filter(|&&(_, i)| i == input).map(|&(vk, _)| vk).collect();
        let mut names = Vec::new();
        for (pair, name) in [((0xA0, 0xA1), "SHIFT"), ((0xA2, 0xA3), "CTRL"), ((0xA4, 0xA5), "ALT")] {
            if vks.contains(&pair.0) && vks.contains(&pair.1) {
                vks.retain(|&v| v != pair.0 && v != pair.1);
                names.push(name.to_owned());
            }
        }
        names.extend(vks.into_iter().map(key_short));
        names
    };
    let mouse_of = |mode: &MouseMode, input: PadInput| {
        mode.buttons.iter().filter(|&&(_, i)| i == input).map(|&(m, _)| mouse_short(m)).collect::<Vec<_>>().join(" ")
    };
    let cell = |s: &str, width: usize| {
        let s: String = s.chars().take(width - 1).collect();
        format!("{s:<width$}")
    };
    let row = |pad: &str, keys: &str, play: &str, menu: &str| {
        format!("{}{}{}{}", cell(pad, 7), cell(keys, 21), cell(play, 15), cell(menu, 15)).trim_end().to_owned()
    };
    let mut out = vec![row("PAD", "KEYBOARD", "MOUSE: PLAY", "MOUSE: MENU"), String::new()];
    for &(name, input) in &PAD_INPUTS[..12] {
        out.push(row(name, &keys_of(input).join(" "), &mouse_of(&b.play, input), &mouse_of(&b.menu, input)));
    }
    // The D-pad and the sticks, a line each: up, left, down, right.
    let ways = |first: usize| [first, first + 2, first + 1, first + 3].map(|i| PAD_INPUTS[i].1);
    for (name, first, stick) in [("D-PAD", 12, None), ("LSTICK", 16, Some(false)), ("RSTICK", 20, Some(true))] {
        let ways = ways(first);
        let keys: Vec<String> =
            ways.iter().map(|&w| keys_of(w).into_iter().next().unwrap_or_else(|| "-".into())).collect();
        let keys = if keys == ["UP", "LEFT", "DOWN", "RIGHT"] { "ARROWS".to_owned() } else { keys.join(" ") };
        let mouse = |mode: &MouseMode| {
            let mut parts: Vec<String> = Vec::new();
            if stick.is_some() && mode.moves == stick {
                parts.push("MOVE".into());
            }
            parts.extend(ways.iter().map(|&w| mouse_of(mode, w)).filter(|s| !s.is_empty()));
            parts.join(" ")
        };
        out.push(row(name, &keys, &mouse(&b.play), &mouse(&b.menu)));
    }
    out.push(String::new());
    let mut switches: Vec<String> = MOUSE_INPUTS
        .iter()
        .filter(|(_, _, m)| matches!(m, MouseInput::Button(bit) if b.switch & bit != 0))
        .map(|(name, _, _)| (*name).to_owned())
        .collect();
    if b.pause_switches {
        switches.push("PAUSE".into());
    }
    out.push(if switches.is_empty() {
        "NOTHING SWITCHES THE MOUSE BETWEEN PLAY AND MENU.".to_owned()
    } else {
        format!("{} SWITCHES THE MOUSE: PLAY / MENU.", switches.join(" OR "))
    });
    out.push(if speed > 0.0 { format!("MOUSE SPEED {speed}. MENU MODE SHOWS AT THE TOP.") } else { "MOUSE OFF (SPEED 0).".to_owned() });
    out.push("EDIT VICTORY-CONTROLS.TXT, BESIDE VICTORY-SETTINGS.TXT.".to_owned());
    out.push(match b.errors.len() {
        0 => "SAVED CHANGES APPLY AT ONCE.".to_owned(),
        1 => "SAVED CHANGES APPLY AT ONCE. 1 LINE NOT UNDERSTOOD.".to_owned(),
        n => format!("SAVED CHANGES APPLY AT ONCE. {n} LINES NOT UNDERSTOOD."),
    });
    out.push("F1 SETTINGS   F2 CONTROLS   F3 STATS".to_owned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_read_cleanly_and_match_the_stand_in_table() {
        let b = Bindings::default();
        assert!(b.errors.is_empty(), "{:?}", b.errors);
        for (vk, bit) in super::super::FALLBACK_BUTTONS {
            assert!(b.keys.contains(&(vk, PadInput::Button(bit))), "{vk:#x}");
        }
        let pad = b.pad(|vk| vk == b'D' || vk == b'I' || vk == b'Z');
        assert_eq!((pad.left_x, pad.right_y, pad.left_trigger), (1.0, 1.0, 1.0));
        assert_eq!(b.pad(|vk| vk == b'A' || vk == b'D').left_x, 0.0, "opposite ways cancel");
        assert_eq!((b.play.moves, b.menu.moves), (Some(true), Some(false)));
        assert_eq!((b.switch, b.pause_switches, b.speed), (MOUSE_MIDDLE, true, None));
        assert!(b.is_pause_key(0x1B) && !b.is_pause_key(0x0D));
        let written = parse(&DEFAULT.replace("{speed}", "5"));
        assert_eq!((written.speed, written.errors.len()), (Some(5.0), 0));
    }

    #[test]
    fn lines_are_read_loosely_and_bad_ones_are_counted() {
        let b = parse(
            "[keyboard]\nA = space, enter\nlb = left_shift\nRT=\nNOPE = Q\nB = NOT A KEY\n\
             [Mouse Play]\nmove = left stick\nA = mouse left, Right Click, wheel up\n\
             [MOUSE]\nspeed = 2.5\nswitch mode = back button, forward\npause switches mode = no\nQ\n[nothing]\n",
        );
        assert!(b.keys.contains(&(0x20, PadInput::Button(A))) && b.keys.contains(&(0x0D, PadInput::Button(A))));
        assert!(b.keys.contains(&(0xA0, PadInput::Button(LEFT_SHOULDER))));
        assert!(!b.keys.iter().any(|&(_, i)| i == PadInput::RightTrigger));
        assert_eq!(b.play.moves, Some(false));
        assert_eq!(
            b.play.buttons,
            [(MouseInput::Button(MOUSE_LEFT), PadInput::Button(A)), (MouseInput::Button(MOUSE_RIGHT), PadInput::Button(A)), (MouseInput::WheelUp, PadInput::Button(A))]
        );
        assert_eq!((b.speed, b.switch, b.pause_switches), (Some(2.5), MOUSE_BACK | MOUSE_FORWARD, false));
        assert_eq!(b.errors.len(), 4, "{:?}", b.errors);
        assert!(b.errors[0].starts_with("LINE 5:"), "{:?}", b.errors);
    }

    #[test]
    fn the_page_fits_the_box_and_says_what_is_set() {
        let b = Bindings::default();
        let lines = page(&b, 5.0);
        let font = |c: char| c.is_ascii_alphanumeric() || " .,:/-()+=*<>_!?".contains(c);
        assert!(lines.iter().all(|l| l.len() <= 58 && l.chars().all(font)), "{lines:#?}");
        assert!(lines.len() <= 27, "{}", lines.len());
        let line = |start: &str| lines.iter().find(|l| l.starts_with(start)).unwrap_or_else(|| panic!("{start}: {lines:#?}"));
        assert!(line("A ").contains("ENTER") && line("A ").contains("LEFT"));
        assert!(line("LT").contains("RIGHT"));
        assert!(line("D-PAD").contains("ARROWS") && line("D-PAD").contains("WHEEL+ WHEEL-"));
        assert!(line("LSTICK").contains("W A S D") && line("RSTICK").contains("I J K L"));
        assert!(line("RSTICK").contains("MOVE") && line("LSTICK").contains("MOVE"));
        assert!(lines.iter().any(|l| l.contains("MIDDLE BUTTON OR PAUSE SWITCHES")));
        let shifted = parse("[KEYBOARD]\nA = SHIFT, NUMPAD 4, F9");
        assert!(page(&shifted, 1.0).iter().any(|l| l.starts_with("A ") && l.contains("SHIFT NUM4 F9")));
    }

    #[test]
    fn a_missing_file_gets_the_defaults_with_the_speed() {
        let dir = std::env::temp_dir().join(format!("victory-controls-test-{}", std::process::id()));
        let path = dir.join(NAME);
        let _ = std::fs::remove_dir_all(&dir);
        let b = load(&path, 3.0);
        assert_eq!(b.speed, Some(3.0));
        let text = std::fs::read_to_string(&path).expect("written");
        assert!(text.contains("SPEED = 3\r\n") && !text.contains("{speed}"));
        assert_eq!(load(&path, 7.0).speed, Some(3.0), "the file wins once it is there");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
