//! Keyboard and mouse on the stand-in pad: which keys press which pad
//! control, in two profiles, on foot and in a vehicle. The title reads pads
//! and keyboards only (GameInput kinds 0x40000 and 0x10, never the mouse), and
//! its controls change with what the player does: on foot the right trigger
//! shoots, in a vehicle it accelerates and the right bumper shoots. A PC player
//! drives with W and shoots with the mouse in both, as on the PC version of the
//! series, so the keys a control takes differ by profile.
//!
//! The defaults follow that PC layout (F enters and leaves vehicles, Space
//! jumps, Shift sprints, Tab holds the weapon wheel, Q covers, W/S accelerate
//! and brake in a vehicle). The profile switches when the interact key (the
//! pad's Y: enter or leave a vehicle) is pressed, unless `VICTORY_BIND_AUTO=0`,
//! and with the profile key (Caps Lock by default) at any time. A key that
//! does nothing in the profile in use but something in the other (Shift
//! sprints on foot only, H is headlights in a vehicle only) switches to that
//! one too, so a profile left wrong puts itself right. A key may press any
//! number of controls: it is bound to each of them.
//!
//! Each control takes up to two keys; a key is a Windows virtual key, the
//! mouse buttons included (1 left, 2 right, 4 middle, 5 and 6 the side ones).
//! The F1 menu changes them at once and saves them as `VICTORY_BIND_FOOT`,
//! `VICTORY_BIND_VEHICLE` (`control=key+key,...`, keys in hex),
//! `VICTORY_BIND_SWITCH`, `VICTORY_BIND_AUTO`, `VICTORY_MOUSE_LOOK` and
//! `VICTORY_MOUSE_INVERT`, read again at the next start.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

/// A pad control a key can press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    A,
    B,
    X,
    Y,
    LeftShoulder,
    RightShoulder,
    LeftTrigger,
    RightTrigger,
    LeftStick,
    RightStick,
    View,
    Menu,
    Up,
    Down,
    Left,
    Right,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    LookUp,
    LookDown,
    LookLeft,
    LookRight,
}

/// Every control: its short name in the settings, and what it does on foot and
/// in a vehicle (the title's own control screen, Standard scheme).
pub const CONTROLS: [(Control, &str, &str, &str); 24] = [
    (Control::A, "A", "JUMP / CLIMB", "HANDBRAKE"),
    (Control::B, "B", "CROUCH", "HEADLIGHTS"),
    (Control::X, "X", "MELEE", "MELEE"),
    (Control::Y, "Y", "INTERACT / ENTER", "LEAVE VEHICLE"),
    (Control::LeftShoulder, "LB", "HOLSTER / WEAPONS", "HOLSTER / WEAPONS"),
    (Control::RightShoulder, "RB", "COVER", "SHOOT"),
    (Control::LeftTrigger, "LT", "AIM", "BRAKE / REVERSE"),
    (Control::RightTrigger, "RT", "SHOOT", "ACCELERATE"),
    (Control::LeftStick, "L3", "SPRINT", "HORN / NITRO"),
    (Control::RightStick, "R3", "LOOK BEHIND", "LOOK BEHIND"),
    (Control::View, "VIEW", "MAP", "MAP"),
    (Control::Menu, "MENU", "PAUSE", "PAUSE"),
    (Control::Up, "UP", "PHONE", "PHONE"),
    (Control::Down, "DOWN", "OBJECTIVE / MINIMAP", "OBJECTIVE / MINIMAP"),
    (Control::Left, "LEFT", "RADIO", "INDICATOR LEFT"),
    (Control::Right, "RIGHT", "DETONATE", "INDICATOR RIGHT"),
    (Control::MoveUp, "MOVE_UP", "MOVE FORWARD", "LEAN FORWARD"),
    (Control::MoveDown, "MOVE_DOWN", "MOVE BACK", "LEAN BACK"),
    (Control::MoveLeft, "MOVE_LEFT", "MOVE LEFT", "STEER LEFT"),
    (Control::MoveRight, "MOVE_RIGHT", "MOVE RIGHT", "STEER RIGHT"),
    (Control::LookUp, "LOOK_UP", "LOOK UP", "LOOK UP"),
    (Control::LookDown, "LOOK_DOWN", "LOOK DOWN", "LOOK DOWN"),
    (Control::LookLeft, "LOOK_LEFT", "LOOK LEFT", "LOOK LEFT"),
    (Control::LookRight, "LOOK_RIGHT", "LOOK RIGHT", "LOOK RIGHT"),
];

/// The settings page of each profile: groups of actions, each the pad control
/// it presses and its name there. Every control is on each page once.
pub const PAGES: [&[(&str, &[(Control, &str)])]; 2] = [
    &[
        ("MOVEMENT", &[
            (Control::MoveUp, "MOVE FORWARD"),
            (Control::MoveDown, "MOVE BACK"),
            (Control::MoveLeft, "MOVE LEFT"),
            (Control::MoveRight, "MOVE RIGHT"),
            (Control::LeftStick, "SPRINT"),
            (Control::A, "JUMP / CLIMB"),
            (Control::B, "CROUCH"),
            (Control::RightShoulder, "TAKE COVER"),
        ]),
        ("COMBAT", &[
            (Control::LeftTrigger, "AIM"),
            (Control::RightTrigger, "SHOOT"),
            (Control::X, "MELEE"),
            (Control::LeftShoulder, "WEAPON WHEEL / HOLSTER"),
            (Control::Right, "DETONATE"),
        ]),
        ("ACTIONS", &[
            (Control::Y, "INTERACT / ENTER VEHICLE"),
            (Control::Up, "PHONE"),
            (Control::Left, "RADIO"),
            (Control::Down, "OBJECTIVE / MINIMAP"),
            (Control::View, "MAP"),
            (Control::Menu, "PAUSE MENU"),
        ]),
        ("CAMERA (THE MOUSE LOOKS TOO)", &[
            (Control::RightStick, "LOOK BEHIND"),
            (Control::LookUp, "LOOK UP"),
            (Control::LookDown, "LOOK DOWN"),
            (Control::LookLeft, "LOOK LEFT"),
            (Control::LookRight, "LOOK RIGHT"),
        ]),
    ],
    &[
        ("DRIVING", &[
            (Control::RightTrigger, "ACCELERATE"),
            (Control::LeftTrigger, "BRAKE / REVERSE"),
            (Control::MoveLeft, "STEER LEFT"),
            (Control::MoveRight, "STEER RIGHT"),
            (Control::A, "HANDBRAKE"),
            (Control::LeftStick, "HORN / NITRO"),
            (Control::MoveUp, "LEAN FORWARD / NOSE DOWN"),
            (Control::MoveDown, "LEAN BACK / NOSE UP"),
        ]),
        ("VEHICLE", &[
            (Control::Y, "LEAVE VEHICLE"),
            (Control::B, "HEADLIGHTS"),
            (Control::Left, "INDICATOR LEFT"),
            (Control::Right, "INDICATOR RIGHT"),
        ]),
        ("COMBAT", &[
            (Control::RightShoulder, "SHOOT"),
            (Control::X, "MELEE"),
            (Control::LeftShoulder, "WEAPON WHEEL / HOLSTER"),
        ]),
        ("OTHER", &[
            (Control::Up, "PHONE"),
            (Control::Down, "OBJECTIVE / MINIMAP"),
            (Control::View, "MAP"),
            (Control::Menu, "PAUSE MENU"),
        ]),
        ("CAMERA (THE MOUSE LOOKS TOO)", &[
            (Control::RightStick, "LOOK BEHIND"),
            (Control::LookUp, "LOOK UP"),
            (Control::LookDown, "LOOK DOWN"),
            (Control::LookLeft, "LOOK LEFT"),
            (Control::LookRight, "LOOK RIGHT"),
        ]),
    ],
];

/// The other actions of `profile` that key `vk` presses, but control `except`.
pub fn also_on(profile: usize, vk: u8, except: usize) -> Vec<&'static str> {
    let map = map().lock().unwrap_or_else(|e| e.into_inner());
    PAGES[profile]
        .iter()
        .flat_map(|(_, actions)| actions.iter())
        .filter(|(c, _)| *c as usize != except && map.profiles[profile][*c as usize].contains(&vk))
        .map(|(_, name)| *name)
        .collect()
}

/// One profile back to the PC layout.
pub fn reset_profile(profile: usize) {
    map().lock().unwrap_or_else(|e| e.into_inner()).profiles[profile] = [FOOT, VEHICLE][profile];
}

/// The mouse and switching options back to their defaults.
pub fn reset_options() {
    init_extras();
    MOUSE_SCALE.store(DEFAULT_SCALE.to_bits(), Ordering::Relaxed);
    INVERT.store(false, Ordering::Relaxed);
    SWITCH.store(CAPS_LOCK, Ordering::Relaxed);
    AUTO.store(true, Ordering::Relaxed);
}

/// Keys of one control: up to two, 0 for none.
pub type Keys = [u8; 2];
type Profile = [Keys; CONTROLS.len()];

const VK_SHIFT: u8 = 0xA0;
const VK_CONTROL: u8 = 0xA2;

/// The PC layout, on foot.
const FOOT: Profile = [
    [0x20, 0x0D],      // A: Space, Enter (menus)
    [VK_CONTROL, 0x08], // B: Ctrl, Backspace (menus)
    [b'R', 0],         // X
    [b'F', 0],         // Y
    [0x09, 0],         // LB: Tab
    [b'Q', 0],         // RB
    [0x02, 0],         // LT: right mouse button
    [0x01, 0],         // RT: left mouse button
    [VK_SHIFT, 0],     // L3
    [b'C', 0],         // R3
    [b'M', 0],         // View
    [0x1B, b'P'],      // Menu: Esc, P
    [0x26, 0],         // arrows
    [0x28, 0],
    [0x25, 0],
    [0x27, 0],
    [b'W', 0],
    [b'S', 0],
    [b'A', 0],
    [b'D', 0],
    [0, 0], // the mouse looks
    [0, 0],
    [0, 0],
    [0, 0],
];

/// The PC layout, in a vehicle.
const VEHICLE: Profile = [
    [0x20, 0x0D],  // A: handbrake on Space
    [b'H', 0x08],  // B: headlights
    [b'R', 0],     // X
    [b'F', 0],     // Y: leave
    [0x09, 0],     // LB
    [0x01, 0],     // RB: shoot with the left mouse button
    [b'S', 0],     // LT: brake, reverse
    [b'W', 0],     // RT: accelerate
    [b'E', 0],     // L3: horn
    [b'C', 0],     // R3
    [b'M', 0],     // View
    [0x1B, b'P'],  // Menu
    [0x26, 0],
    [0x28, 0],
    [0x25, 0],
    [0x27, 0],
    [0x68, 0], // lean or pitch: numpad 8 and 5
    [0x65, 0],
    [b'A', 0], // steer
    [b'D', 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
];

const CAPS_LOCK: u8 = 0x14;

struct Map {
    profiles: [Profile; 2],
}

fn map() -> &'static Mutex<Map> {
    static MAP: OnceLock<Mutex<Map>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut profiles = [FOOT, VEHICLE];
        for (p, var) in ["VICTORY_BIND_FOOT", "VICTORY_BIND_VEHICLE"].into_iter().enumerate() {
            if let Ok(text) = std::env::var(var) {
                parse_into(&text, &mut profiles[p]);
            }
        }
        Mutex::new(Map { profiles })
    })
}

/// Reads `control=key+key,...` (keys in hex) over `profile`; what it does not
/// name keeps its key, a control named with nothing loses them.
fn parse_into(text: &str, profile: &mut Profile) {
    for part in text.split(',') {
        let Some((name, keys)) = part.split_once('=') else { continue };
        let Some(i) = CONTROLS.iter().position(|c| c.1.eq_ignore_ascii_case(name.trim())) else { continue };
        let mut got = [0u8; 2];
        for (slot, key) in keys.split('+').filter(|k| !k.trim().is_empty()).take(2).enumerate() {
            got[slot] = u8::from_str_radix(key.trim(), 16).unwrap_or(0);
        }
        profile[i] = got;
    }
}

fn write(profile: &Profile) -> String {
    CONTROLS
        .iter()
        .zip(profile)
        .map(|(c, keys)| {
            let keys: Vec<String> = keys.iter().filter(|&&k| k != 0).map(|k| format!("{k:02X}")).collect();
            format!("{}={}", c.1, keys.join("+"))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Which profile is in use: 0 on foot, 1 in a vehicle.
static PROFILE: AtomicU8 = AtomicU8::new(0);

/// The profile in use (0 on foot, 1 in a vehicle).
pub fn profile() -> usize {
    usize::from(PROFILE.load(Ordering::Relaxed))
}

/// Switches to the other profile.
pub fn toggle_profile() {
    PROFILE.fetch_xor(1, Ordering::Relaxed);
    tracing::info!(profile = if profile() == 0 { "on foot" } else { "vehicle" }, "keyboard profile");
}

/// The keys of control `i` in `profile`.
pub fn keys(profile: usize, i: usize) -> Keys {
    map().lock().unwrap_or_else(|e| e.into_inner()).profiles[profile][i]
}

/// Sets key `slot` (0 or 1) of control `i` in `profile`; 0 clears it.
pub fn set_key(profile: usize, i: usize, slot: usize, vk: u8) {
    map().lock().unwrap_or_else(|e| e.into_inner()).profiles[profile][i][slot] = vk;
}

/// Back to the PC layout, both profiles.
pub fn reset() {
    map().lock().unwrap_or_else(|e| e.into_inner()).profiles = [FOOT, VEHICLE];
    MOUSE_SCALE.store(DEFAULT_SCALE.to_bits(), Ordering::Relaxed);
    INVERT.store(false, Ordering::Relaxed);
    SWITCH.store(CAPS_LOCK, Ordering::Relaxed);
    AUTO.store(true, Ordering::Relaxed);
}

/// Both profiles as the settings file keeps them.
pub fn saved() -> [(&'static str, String); 6] {
    let map = map().lock().unwrap_or_else(|e| e.into_inner());
    [
        ("VICTORY_BIND_FOOT", write(&map.profiles[0])),
        ("VICTORY_BIND_VEHICLE", write(&map.profiles[1])),
        ("VICTORY_BIND_SWITCH", format!("{:02X}", switch_key())),
        ("VICTORY_BIND_AUTO", if auto_switch() { "1" } else { "0" }.to_owned()),
        ("VICTORY_MOUSE_LOOK", format!("{}", mouse_scale())),
        ("VICTORY_MOUSE_INVERT", if mouse_inverted() { "1" } else { "0" }.to_owned()),
    ]
}

const DEFAULT_SCALE: f32 = 0.08;
static MOUSE_SCALE: AtomicU32 = AtomicU32::new(u32::MAX);
static INVERT: AtomicBool = AtomicBool::new(false);
static SWITCH: AtomicU8 = AtomicU8::new(0);
static AUTO: AtomicBool = AtomicBool::new(true);

fn init_extras() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let scale = std::env::var("VICTORY_MOUSE_LOOK").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_SCALE);
        MOUSE_SCALE.store(f32::to_bits(scale), Ordering::Relaxed);
        INVERT.store(std::env::var_os("VICTORY_MOUSE_INVERT").is_some_and(|v| v == "1"), Ordering::Relaxed);
        let switch = std::env::var("VICTORY_BIND_SWITCH").ok().and_then(|v| u8::from_str_radix(v.trim(), 16).ok());
        SWITCH.store(switch.unwrap_or(CAPS_LOCK), Ordering::Relaxed);
        AUTO.store(std::env::var_os("VICTORY_BIND_AUTO").is_none_or(|v| v != "0"), Ordering::Relaxed);
    });
}

/// Stick deflection per mouse count in one input poll (0: the mouse does not look).
pub fn mouse_scale() -> f32 {
    init_extras();
    f32::from_bits(MOUSE_SCALE.load(Ordering::Relaxed))
}

pub fn set_mouse_scale(scale: f32) {
    init_extras();
    MOUSE_SCALE.store(scale.to_bits(), Ordering::Relaxed);
}

/// Whether moving the mouse up looks down.
pub fn mouse_inverted() -> bool {
    init_extras();
    INVERT.load(Ordering::Relaxed)
}

pub fn set_mouse_inverted(on: bool) {
    init_extras();
    INVERT.store(on, Ordering::Relaxed);
}

/// The key that switches the profile.
pub fn switch_key() -> u8 {
    init_extras();
    SWITCH.load(Ordering::Relaxed)
}

pub fn set_switch_key(vk: u8) {
    init_extras();
    SWITCH.store(vk, Ordering::Relaxed);
}

/// Whether the interact key (the pad's Y) switches the profile too.
pub fn auto_switch() -> bool {
    init_extras();
    AUTO.load(Ordering::Relaxed)
}

pub fn set_auto_switch(on: bool) {
    init_extras();
    AUTO.store(on, Ordering::Relaxed);
}

/// A key's name in the menu.
pub fn name(vk: u8) -> String {
    match vk {
        0 => "-".into(),
        0x01 => "MOUSE LEFT".into(),
        0x02 => "MOUSE RIGHT".into(),
        0x04 => "MOUSE MIDDLE".into(),
        0x05 => "MOUSE 4".into(),
        0x06 => "MOUSE 5".into(),
        0x08 => "BACKSPACE".into(),
        0x09 => "TAB".into(),
        0x0D => "ENTER".into(),
        0x10 | 0xA0 => "SHIFT".into(),
        0xA1 => "RIGHT SHIFT".into(),
        0x11 | 0xA2 => "CTRL".into(),
        0xA3 => "RIGHT CTRL".into(),
        0x12 | 0xA4 => "ALT".into(),
        0x14 => "CAPS LOCK".into(),
        0x1B => "ESC".into(),
        0x20 => "SPACE".into(),
        0x25 => "LEFT ARROW".into(),
        0x26 => "UP ARROW".into(),
        0x27 => "RIGHT ARROW".into(),
        0x28 => "DOWN ARROW".into(),
        0x2E => "DELETE".into(),
        b'0'..=b'9' | b'A'..=b'Z' => char::from(vk).to_string(),
        0x60..=0x69 => format!("NUM {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0xBA => "SEMICOLON".into(),
        0xBB => "PLUS".into(),
        0xBC => "COMMA".into(),
        0xBD => "MINUS".into(),
        0xBE => "PERIOD".into(),
        0xBF => "SLASH".into(),
        0xC0 => "TILDE".into(),
        _ => format!("KEY {vk:02X}"),
    }
}

/// Whether `vk` counts as held, given the keys held and the mouse buttons
/// (bits 0-4: left, right, middle, 4, 5). Shift and Ctrl match either side.
fn held(vk: u8, keys: &[u8], mouse_buttons: u32) -> bool {
    match vk {
        0 => false,
        0x01 => mouse_buttons & 1 != 0,
        0x02 => mouse_buttons & 2 != 0,
        0x04 => mouse_buttons & 4 != 0,
        0x05 => mouse_buttons & 8 != 0,
        0x06 => mouse_buttons & 16 != 0,
        0xA0 | 0x10 => keys.iter().any(|&k| matches!(k, 0x10 | 0xA0 | 0xA1)),
        0xA2 | 0x11 => keys.iter().any(|&k| matches!(k, 0x11 | 0xA2 | 0xA3)),
        _ => keys.contains(&vk),
    }
}

/// What the keys and mouse buttons held press in `profile`: whether each
/// control is pressed (in [`CONTROLS`] order).
pub fn pressed(profile: usize, keys: &[u8], mouse_buttons: u32) -> [bool; CONTROLS.len()] {
    let map = map().lock().unwrap_or_else(|e| e.into_inner());
    std::array::from_fn(|i| map.profiles[profile][i].iter().any(|&vk| held(vk, keys, mouse_buttons)))
}

/// After a switch by the interact key, its presses switch nothing for this
/// long: getting in takes a few seconds, and a second press in the meantime
/// (in game, 0.2-0.3 s after the first) switched straight back.
const AUTO_QUIET_MS: u64 = 2000;

/// What the stand-in pad presses this poll, in the profile in use; switches the
/// profile on a fresh press of the profile key, or of the interact control
/// (auto switch). The press itself still counts in the old profile.
pub fn stand_in(keys: &[u8], mouse_buttons: u32) -> [bool; CONTROLS.len()] {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let ms = START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
    stand_in_at(keys, mouse_buttons, ms)
}

fn stand_in_at(keys: &[u8], mouse_buttons: u32, ms: u64) -> [bool; CONTROLS.len()] {
    static LAST: AtomicU8 = AtomicU8::new(0);
    // When the interact key may switch again.
    static QUIET_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Keys pressed since the last poll: one held from before (Shift, sprinting
    // to a car and pressing F) is no sign of the other profile.
    static HELD: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let fresh: Vec<u8> = {
        let mut before = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = keys.iter().copied().filter(|k| !before.contains(k)).collect();
        *before = keys.to_vec();
        fresh
    };
    let on = pressed(profile(), keys, mouse_buttons);
    let switch = held(switch_key(), keys, mouse_buttons);
    let interact = on[3];
    let last = LAST.swap(u8::from(interact) | u8::from(switch) << 1, Ordering::Relaxed);
    if switch && last & 2 == 0 {
        toggle_profile();
    } else if auto_switch() && interact && last & 1 == 0 {
        if ms >= QUIET_UNTIL.load(Ordering::Relaxed) {
            toggle_profile();
            QUIET_UNTIL.store(ms + AUTO_QUIET_MS, Ordering::Relaxed);
        } else {
            tracing::info!("keyboard profile: interact pressed again within 2 s, not switched");
        }
    } else if auto_switch() && ms >= QUIET_UNTIL.load(Ordering::Relaxed) && other_profile_key(&fresh) {
        // A key that does nothing here and something in the other profile:
        // Shift (sprint) in the vehicle one means on foot, H (headlights) on foot a vehicle.
        toggle_profile();
        // Its own press counts in the profile it belongs to.
        return pressed(profile(), keys, mouse_buttons);
    }
    on
}

/// Whether a key held (not a mouse button, nor the profile key) has no control
/// in the profile in use but one in the other.
fn other_profile_key(keys: &[u8]) -> bool {
    let map = map().lock().unwrap_or_else(|e| e.into_inner());
    let now = profile();
    let bound = |p: usize, vk: u8| map.profiles[p].iter().flatten().any(|&k| k != 0 && held(k, &[vk], 0));
    keys.iter()
        .filter(|&&vk| vk > 0x06 && vk != switch_key())
        .any(|&vk| !bound(now, vk) && bound(now ^ 1, vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_survives_the_settings_file() {
        let mut profile = FOOT;
        profile[3] = [b'G', 0x05];
        let text = write(&profile);
        let mut back = VEHICLE;
        parse_into(&text, &mut back);
        assert_eq!(back, profile);
        // A control named with no key loses its keys; one not named keeps them.
        let mut kept = FOOT;
        parse_into("Y=,lt=01", &mut kept);
        assert_eq!((kept[3], kept[6], kept[0]), ([0, 0], [0x01, 0], FOOT[0]));
    }

    #[test]
    fn the_pc_layout_drives_with_w_and_shoots_with_the_mouse() {
        let foot = |i: usize| FOOT[i];
        let car = |i: usize| VEHICLE[i];
        let at = |c: Control| CONTROLS.iter().position(|x| x.0 == c).unwrap();
        assert_eq!(foot(at(Control::Y))[0], b'F');
        assert_eq!(foot(at(Control::RightTrigger))[0], 0x01);
        assert_eq!(foot(at(Control::MoveUp))[0], b'W');
        assert_eq!(car(at(Control::RightTrigger))[0], b'W');
        assert_eq!(car(at(Control::RightShoulder))[0], 0x01);
        // Either Shift sprints; the left mouse button is button bit 0.
        assert!(held(VK_SHIFT, &[0xA1], 0));
        assert!(held(0x01, &[], 1) && !held(0x02, &[], 1));
    }

    #[test]
    fn every_control_is_on_each_page_once() {
        for (i, c) in CONTROLS.iter().enumerate() {
            assert_eq!(c.0 as usize, i);
        }
        for page in PAGES {
            let mut seen: Vec<usize> = page.iter().flat_map(|(_, a)| a.iter().map(|(c, _)| *c as usize)).collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..CONTROLS.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_second_interact_press_soon_after_does_not_switch_back() {
        let start = profile();
        let tap = |ms: u64| {
            stand_in_at(&[b'F'], 0, ms);
            stand_in_at(&[], 0, ms + 100);
        };
        tap(10_000);
        assert_ne!(profile(), start);
        tap(10_250);
        assert_ne!(profile(), start, "switched back 0.25 s later");
        tap(13_000);
        assert_eq!(profile(), start);
        // (In the same test: the profile is one for the process.) Shift sprints
        // on foot only, H is the vehicle's headlights only: either brings its profile.
        for (vk, wanted) in [(0xA0, 0), (b'H', 1), (b'H', 1), (0xA1, 0)] {
            let on = stand_in_at(&[vk], 0, 20_000);
            assert_eq!(profile(), wanted, "{vk:02X}");
            assert!(on.iter().any(|&c| c), "{vk:02X} pressed nothing");
            stand_in_at(&[], 0, 20_100);
        }
        // W is in both: it switches nothing.
        stand_in_at(&[b'W'], 0, 20_200);
        assert_eq!(profile(), 0);
        // Sprinting to a car (Shift held) and pressing F: in the vehicle it stays.
        stand_in_at(&[0xA0], 0, 30_000);
        stand_in_at(&[0xA0, b'F'], 0, 30_010);
        stand_in_at(&[0xA0, b'F'], 0, 30_020);
        stand_in_at(&[0xA0], 0, 30_100);
        assert_eq!(profile(), 1);
        // A fresh Shift soon after getting in switches nothing; later it does.
        stand_in_at(&[], 0, 30_200);
        stand_in_at(&[0xA0], 0, 30_300);
        assert_eq!(profile(), 1);
        stand_in_at(&[], 0, 33_000);
        stand_in_at(&[0xA0], 0, 33_100);
        assert_eq!(profile(), 0);
        stand_in_at(&[], 0, 33_200);
    }
}
