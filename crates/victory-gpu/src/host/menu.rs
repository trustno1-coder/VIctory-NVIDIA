//! The F1 settings menu. F1 shows and hides it while our window has the
//! focus; Up/Down pick a setting, Left/Right change it, Enter runs the line
//! picked (save, save and restart, preload shaders, reset, close), Esc
//! closes. Preloading saves, ends this run and has the launcher make every
//! shader before the title starts again (`VICTORY_PRELOAD=1`, `victory
//! preload`); it asks for a second Enter first. While it is
//! shown the title sees no keyboard (`crate::SETTINGS_MENU_OPEN`, read by the
//! input module); a pad still works.
//!
//! The settings are read once at start, so a change is saved at once (as well as by the save
//! lines) to the file
//! `VICTORY_MENU_FILE` names (`scripts\play-foxhound.ps1` sets it to
//! `victory-settings.txt` in the repository's folder and applies it at every
//! start) and takes effect at the next start; "SAVE AND RESTART" starts the
//! launcher again (`VICTORY_MENU_PLAY`, with `VICTORY_MENU_ARGS`) and ends
//! this run. The `VICTORY_MENU_...` settings are the launcher's own choices
//! (DLSS mode, frame generation, frame rate, console, picture size), which it
//! turns into the layer's switches; the others are the switches themselves.
//!
//! In every build, as the stats panel is. Drawn as
//! that panel is: white text on a black box, copied onto the swap chain's
//! buffer, in the middle of the picture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_DOWN, VK_ESCAPE, VK_F1, VK_LEFT, VK_RETURN, VK_RIGHT, VK_TAB, VK_UP,
};
/// Q and E: the previous and next tab.
const VK_Q: u16 = 0x51;
const VK_E: u16 = 0x45;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::font::{colours, glyph};

#[path = "menu_look.rs"]
mod look;
#[path = "launcher.rs"]
pub(super) mod launcher;
#[path = "launcher_shapes.rs"]
mod launcher_shapes;
use super::{Host, HostList, transition};

const GLYPH: (u32, u32) = (5, 7);
const CELL: (u32, u32) = (6, 9);
const COLUMNS: u32 = 60;
/// Lines of the box: a title, a help line, a blank, the settings shown, a
/// blank, what the line picked does (`ABOUT`, else blank), a note and the
/// repository's link.
const ROWS: usize = 20;
const LINES: u32 = ROWS as u32 + 9;
/// The action that preloads shaders, and what is said of it while it is picked.
const PRELOAD: usize = 2;
const ABOUT_PRELOAD: [&str; 3] = [
    "MAKES EVERY SHADER AHEAD, THEN STARTS THE GAME AGAIN.",
    "TAKES A WHILE: MAYBE A LONG TIME, DEPENDING ON HARDWARE.",
    "A LATER UPDATE OF THIS BUILD MAY NEED A NEW PRELOAD.",
];
/// The actions that export and import the game's save (`crate::save_share`), and what is said
/// of them while they are picked.
const EXPORT: usize = 4;
const IMPORT: usize = 5;
const ABOUT_EXPORT: [&str; 3] = [
    "MAKES A ZIP OF YOUR SAVE IN SAVES, IN THE FOLDER OF",
    "GAME_SCARLETT_MASTER.EXE. SEND THAT ZIP TO SHARE IT.",
    "YOUR SAVE STAYS AS IT IS. GAME SETTINGS ARE LEFT OUT.",
];
const ABOUT_IMPORT: [&str; 3] = [
    "PUT A SAVE ZIP INTO SAVES, IN THE FOLDER OF",
    "GAME_SCARLETT_MASTER.EXE, THEN PRESS ENTER HERE AND",
    "PICK IT FROM THE LIST. YOUR GAME SETTINGS ARE KEPT.",
];
/// The list IMPORT SAVE opens: the saves in `Saves`, then a line back to the settings.
const PICK_TITLE: &str = "IMPORT SAVE                       ESC GOES BACK";
const PICK_HELP: &str = "UP/DOWN PICK  ENTER TWICE IMPORTS IT";
const PICK_NONE: &str = "  NONE YET: PUT A SAVE ZIP INTO SAVES IN THE GAME FOLDER";
const PICK_BACK: &str = "BACK TO THE SETTINGS";
const ABOUT_PICK: [&str; 3] = [
    "THE SAVE ZIPS IN SAVES, IN THE GAME FOLDER, NEWEST FIRST.",
    "ENTER TWICE: THE GAME RESTARTS WITH THE SAVE PICKED.",
    "YOUR OLD SAVE IS ZIPPED INTO SAVES FIRST, SO IT IS KEPT.",
];
/// The note after the first Enter on a save in the list: the second one imports it.
const CONFIRM_IMPORT: &str = "ENTER AGAIN: IMPORT IT AND RESTART THE GAME";
/// The longest save name shown in the list.
const IMPORT_NAME: usize = 56;

/// A save's file name as the menu's letters can show it.
fn shown_name(name: &str) -> String {
    name.chars()
        .map(|c| c.to_ascii_uppercase())
        .map(|c| if c.is_ascii() && glyph(c as u8) != glyph(b'#') { c } else { '-' })
        .take(IMPORT_NAME)
        .collect()
}

/// The setting that turns MFG Unlocker on, and what is said of it while it is picked.
const MFG: &str = "VICTORY_MFG_UNLOCKER";
const ABOUT_MFG: [&str; 3] = [
    "A MOD, NOT OURS: DLSS FRAME GEN ON RTX 20 AND 30 CARDS.",
    "ON: DOWNLOADED FROM ITS GITHUB AT THE NEXT START, THEN RUN.",
    "NVIDIA DOES NOT SUPPORT IT. IF THE GAME FAILS, TURN IT OFF.",
];
/// The DLSS setting, and what is said while it is picked on in a start without NVIDIA's files.
const DLSS: &str = "VICTORY_MENU_DLSS";
const ABOUT_NO_DLSS: [&str; 3] = [
    "THE NVIDIA DLSS FILES ARE NOT ON THIS PC YET: THEY ARE",
    "DOWNLOADED FROM NVIDIA ON GITHUB AT THE NEXT START",
    "(ABOUT 70 MB, ONCE). SAVE AND RESTART NOW DOES IT.",
];
/// Frame generation that this run did not start with: it is made ready only at a start with it
/// on (a start with it ready but off held some PCs at 14 fps, #515, #574), so turning it on takes
/// a restart; a run started with it on changes it, or turns it off, live.
const ABOUT_FRAMEGEN_RESTART: [&str; 3] = [
    "RESTART REQUIRED TO ENABLE FRAME GENERATION: THIS RUN",
    "STARTED WITHOUT IT. SAVE AND RESTART NOW TURNS IT ON.",
    "AFTER THAT IT CHANGES, OR TURNS OFF, LIVE.",
];
const FRAMEGEN: &str = "VICTORY_MENU_FRAMEGEN";
/// The console, and the picture row of each: only the one of the console picked is shown.
const CONSOLE: &str = "VICTORY_MENU_CONSOLE";
const X_RES: &str = "VICTORY_MENU_RES";
const S_RES: &str = "VICTORY_MENU_S_RES";
/// The Series S scene sizes and the DLSS mode of each (the picture is 1920x1080). Only DLSS
/// changes it: the title's own FidelityFX path from 540 stopped it at the start (0xc0000094,
/// a division by zero in the title, 2026-11-28), so with DLSS off the scene stays at 720.
const S_MODES: [(&str, &str); 4] = [("1080", "dlaa"), ("720", "quality"), ("626", "balanced"), ("540", "performance")];
const ABOUT_S_RES: [&str; 3] = [
    "THE SIZE THE GAME RENDERS AT. THE PICTURE IS 1920X1080.",
    "OTHER THAN 720 NEEDS DLSS: THIS SETS ITS MODE (1080 IS",
    "DLAA, 720 QUALITY, 626 BALANCED, 540 PERFORMANCE).",
];
/// The note after the first Enter on it: the second one does it.
const CONFIRM_PRELOAD: &str = "ENTER AGAIN: SAVE, CLOSE THE GAME AND PRELOAD";
/// The last line of the box: where this build comes from.
const REPO: &str = "GITHUB.COM/TRUSTNO1-CODER/VICTORY-NVIDIA";
const RIM: u32 = 4;
const UPLOADS: usize = 4;

/// A setting: the variable, its name in the menu, the values it takes
/// (`""`: not set, the build's own default), and whether a change applies at once.
struct Item {
    var: &'static str,
    label: &'static str,
    values: &'static [(&'static str, &'static str)],
    live: bool,
}

const ON_UNSET: &[(&str, &str)] = &[("1", "ON"), ("", "OFF")];
const UNSET_ON: &[(&str, &str)] = &[("", "OFF"), ("1", "ON")];
const STAGES: &[(&str, &str)] = &[("2", "2"), ("1", "1"), ("0", "0 OFF")];

/// The first value of each is what the launcher starts with.
#[rustfmt::skip]
const ITEMS: &[Item] = &[
    // The launcher's own choices.
    Item { var: "VICTORY_MENU_DLSS", label: "DLSS", values: &[("quality", "QUALITY"), ("balanced", "BALANCED"), ("performance", "PERFORMANCE"), ("dlaa", "DLAA (NATIVE)"), ("off", "OFF")], live: false },
    Item { var: "VICTORY_MENU_FRAMEGEN", label: "FRAME GENERATION", values: &[("0", "OFF"), ("2", "2X"), ("3", "3X"), ("4", "4X"), ("5", "5X"), ("6", "6X (MOST)")], live: false },
    // DLSS Frame Generation where it runs (RTX 40 and later), else AMD's FSR 3 (2X only).
    Item { var: "VICTORY_MENU_FG_TYPE", label: "FRAME GEN TYPE", values: &[("auto", "AUTO"), ("dlss", "DLSS"), ("fsr", "FSR 3 (OLDER CARDS)")], live: false },
    // Someone else's mod (apps\victory\src\play.rs): downloaded at the next start, then loaded.
    Item { var: MFG, label: "MFG UNLOCKER (MOD, RTX 20/30)", values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_FPS", label: "FRAME RATE CAP", values: &[("1000", "NONE"), ("120", "120"), ("60", "60"), ("30", "30 (THE GAME)")], live: false },
    Item { var: "VICTORY_FPS_LINE", label: "FPS AT THE TOP", values: &[("1", "ON"), ("0", "OFF")], live: true },
    Item { var: "VICTORY_PANEL", label: "STATS PANEL (F3)", values: &[("0", "OFF"), ("1", "ON")], live: true },
    Item { var: "VICTORY_FULLSCREEN", label: "FULLSCREEN (NO BORDER)", values: UNSET_ON, live: false },
    Item { var: "VICTORY_MOUSE_CAPTURE", label: "MOUSE CAPTURE", values: UNSET_ON, live: false },
    Item { var: "VICTORY_IME_ENGLISH", label: "ENGLISH KEYBOARD", values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_CONSOLE", label: "CONSOLE", values: &[("series-x", "SERIES X"), ("series-s", "SERIES S (FASTER)")], live: false },
    Item { var: X_RES, label: "SERIES X PICTURE", values: &[("1440", "2560X1440"), ("1080to1440", "1080P TO 1440P"), ("4k", "3840X2160")], live: false },
    // In place of SERIES X PICTURE when CONSOLE is Series S.
    Item { var: S_RES, label: "SERIES S PICTURE", values: &[("1080", "1920X1080 (DLAA)"), ("720", "1280X720 (THE GAME)"), ("626", "1113X626"), ("540", "960X540 (FASTEST)")], live: false },
    // The layer's switches.
    Item { var: "VICTORY_DLSS_PRESET", label: "DLSS MODEL", values: &[("", "DEFAULT"), ("k", "K"), ("j", "J"), ("m", "M"), ("l", "L")], live: false },
    Item { var: "VICTORY_AMBIENT", label: "LIGHT ESTIMATE", values: &[("0.8", "0.8"), ("1", "1"), ("0.5", "0.5"), ("", "OFF")], live: false },
    Item { var: "VICTORY_AMBIENT_SPECULAR", label: "REFLECTION ESTIMATE", values: ON_UNSET, live: false },
    Item { var: "VICTORY_AMBIENT_AIR", label: "FOG CAP", values: &[("1.5", "1.5"), ("1", "1"), ("2", "2"), ("", "NONE")], live: false },
    Item { var: "VICTORY_BVH_BUILDER", label: "RAY TRACING (SLOW, BROKEN)", values: UNSET_ON, live: false },
    // On unless "0": a file saved by an earlier version holds an empty value (its OFF) or "1",
    // and both start it on, so nobody keeps it off from before by accident (2026-11-27).
    Item { var: "VICTORY_WATER_LIGHT", label: "WATER LIGHT", values: &[("", "ON"), ("0", "OFF")], live: false },
    Item { var: "VICTORY_LEAVE_OUT", label: "HEAT HAZE AND DECAL DEFRAG", values: &[("02384a60fda6f2d0,731a39171b568298", "LEFT OUT"), ("", "RUN (SLOWER, SMEARS)")], live: false },
    Item { var: "VICTORY_EARLY_DEPTH", label: "EARLY DEPTH TEST", values: ON_UNSET, live: false },
    Item { var: "VICTORY_WC_NO_BACK", label: "SKIP BIG COPY-BACKS", values: ON_UNSET, live: false },
    Item { var: "VICTORY_SOFT_VIEWS", label: "SOFT VIEWS (NO CUT FREEZES)", values: &[("copies", "ON"), ("", "OFF")], live: false },
    Item { var: "VICTORY_TEXTURE_NO_WAIT", label: "TEXTURE NO WAIT", values: ON_UNSET, live: false },
    Item { var: "VICTORY_RELAY", label: "RELAY (SPEED, TRIANGLES)", values: STAGES, live: false },
    Item { var: "VICTORY_CARD_STEPS", label: "CARD STEPS", values: STAGES, live: false },
    Item { var: "VICTORY_INDIRECT_BATCH", label: "INDIRECT BATCH", values: STAGES, live: false },
    Item { var: "VICTORY_CARD_BUFFERS_MIN_KB", label: "CARD COMPARES FROM (KB)", values: &[("64", "64"), ("1024", "1024")], live: false },
    Item { var: "VICTORY_PORT_CONSTANTS", label: "CONSTANTS HEAP IN PLACE", values: UNSET_ON, live: false },
    // THIN records the title's list calls on our threads. On an RTX 5080 with a 6-core CPU heads did not
    // draw with it and frames were slower (14 a second with it, 21 to 28 without): switches for it and for
    // what goes with it. Frame generation needs the line of submissions: without THIN, ASYNC SUBMIT gives it.
    Item { var: "VICTORY_THIN", label: "THIN LISTS (HEADS MAY BE MISSING)", values: ON_UNSET, live: false },
    Item { var: "VICTORY_JOIN_PARTS", label: "JOIN PARTS", values: &[("8", "8"), ("", "1 (OFF)")], live: false },
    Item { var: "VICTORY_ASYNC_SUBMIT", label: "ASYNC SUBMIT (FRAME GEN WITHOUT THIN)", values: UNSET_ON, live: false },
];
/// The lines after the settings.
const ACTIONS: [&str; 7] = [
    "SAVE AND RESTART NOW",
    "SAVE FOR NEXT START",
    "PRELOAD SHADERS (RESTARTS THE GAME)",
    "RESET ALL TO PLAY DEFAULTS",
    "EXPORT SAVE (TO SAVES IN THE GAME FOLDER)",
    "IMPORT SAVE (PICK FROM SAVES)",
    "CLOSE",
];

/// The menu's tabs, and the tab of each setting (the rest are under ADVANCED).
const TABS: [&str; 5] = ["DISPLAY", "GRAPHICS", "PERFORMANCE", "CONTROLS", "ADVANCED"];
fn tab_of(var: &str) -> usize {
    match var {
        "VICTORY_FULLSCREEN" | "VICTORY_MENU_CONSOLE" | "VICTORY_MENU_RES" | "VICTORY_MENU_S_RES" | "VICTORY_FPS_LINE" | "VICTORY_PANEL" => 0,
        "VICTORY_MENU_DLSS" | "VICTORY_DLSS_PRESET" | "VICTORY_AMBIENT" | "VICTORY_AMBIENT_SPECULAR" | "VICTORY_AMBIENT_AIR" | "VICTORY_BVH_BUILDER"
        | "VICTORY_WATER_LIGHT" | "VICTORY_LEAVE_OUT" => 1,
        "VICTORY_MENU_FRAMEGEN" | "VICTORY_MENU_FG_TYPE" | MFG | "VICTORY_MENU_FPS" | "VICTORY_EARLY_DEPTH" | "VICTORY_WC_NO_BACK" | "VICTORY_SOFT_VIEWS"
        | "VICTORY_TEXTURE_NO_WAIT" => 2,
        "VICTORY_MOUSE_CAPTURE" | "VICTORY_IME_ENGLISH" => 3,
        _ => 4,
    }
}

/// The settings of `tab`, by their index in `ITEMS`.
fn tab_items(tab: usize) -> Vec<usize> {
    (0..ITEMS.len()).filter(|&i| tab_of(ITEMS[i].var) == tab).collect()
}

/// Upper-case menu words as a title has them ("FRAME RATE CAP" -> "Frame Rate Cap"),
/// with names and sizes left as they are (DLSS, RTX; 3440X1440 -> 3440x1440).
fn pretty(text: &str) -> String {
    const KEEP: [&str; 15] = ["DLSS", "DLAA", "FPS", "FG", "MFG", "RTX", "FSR", "AMD", "KB", "F1", "F3", "X", "S", "UI", "GPU"];
    text.split(' ')
        .map(|word| {
            let core: String = word.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            if KEEP.contains(&core.as_str()) {
                word.to_owned()
            } else if word.chars().any(|c| c.is_ascii_digit()) {
                word.replace('X', "x")
            } else {
                let mut first = true;
                word.chars()
                    .map(|c| {
                        if c.is_ascii_alphabetic() {
                            let out = if first { c } else { c.to_ascii_lowercase() };
                            first = false;
                            out
                        } else {
                            c
                        }
                    })
                    .collect()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a setting does, and what its value picked means, for the description beside the list.
fn explained(var: &str, value: &str) -> &'static [&'static str] {
    const MODEL: &str = "The AI model DLSS rebuilds the picture with.";
    match (var, value) {
        ("VICTORY_DLSS_PRESET", "") => &[MODEL, "Default: the one NVIDIA's files pick for the DLSS mode chosen."],
        ("VICTORY_DLSS_PRESET", "k") => &[MODEL, "K: the transformer model of DLSS 4. Sharp and steady, little ghosting: the safe choice."],
        ("VICTORY_DLSS_PRESET", "j") => &[MODEL, "J: DLSS 4's transformer too, a little sharper than K but with more shimmer and ghosting."],
        ("VICTORY_DLSS_PRESET", "m") => &[
            MODEL,
            "M: the newer transformer of DLSS 4.5, made for Performance mode (as ultrawide uses). More detail there; costs more on RTX 20 and 30.",
        ],
        ("VICTORY_DLSS_PRESET", "l") => &[
            MODEL,
            "L: DLSS 4.5's newer transformer, made for Ultra Performance. The heaviest; little gain at the other modes.",
        ],
        ("VICTORY_MENU_DLSS", _) => &[
            "How much smaller the picture is drawn before DLSS brings it up to the output size: Quality the least, Performance half each way. DLAA draws it at full size and only smooths it.",
        ],
        _ => &[],
    }
}

/// The actions' short names on their buttons.
fn action_label(action: &str, launcher: bool) -> &str {
    match action {
        "SAVE AND RESTART NOW" if launcher => "SAVE AND PLAY",
        "CLOSE" if launcher => "BACK",
        "SAVE AND RESTART NOW" => "SAVE AND RESTART",
        "SAVE FOR NEXT START" => "SAVE",
        "PRELOAD SHADERS (RESTARTS THE GAME)" => "PRELOAD SHADERS",
        "RESET ALL TO PLAY DEFAULTS" => "RESET",
        "EXPORT SAVE (TO SAVES IN THE GAME FOLDER)" => "EXPORT SAVE",
        "IMPORT SAVE (PICK FROM SAVES)" => "IMPORT SAVE",
        other => other,
    }
}

/// An upper-case sentence as a sentence ("TAKES A WHILE." -> "Takes a while."), names kept.
fn pretty_sentence(text: &str) -> String {
    let words = pretty(text);
    let mut out = String::new();
    let mut start = true;
    for word in words.split(' ') {
        let keep = word.chars().filter(|c| c.is_ascii_uppercase()).count() > 1 || word.chars().any(|c| c.is_ascii_digit());
        let w = if start || keep { word.to_owned() } else { word.to_ascii_lowercase() };
        start = word.ends_with('.') || word.ends_with(':');
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w);
    }
    out
}

fn get(var: &str) -> String {
    std::env::var(var).unwrap_or_default()
}

/// Where the menu saves: the file `VICTORY_MENU_FILE` names (`victory play` sets it); else,
/// however the game was started (a script, another launcher), the one `victory play` reads at
/// every start, `victory-settings.txt` in `VICTORY_HOME` or `%LOCALAPPDATA%\victory`.
fn settings_file() -> Option<std::path::PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
    set("VICTORY_MENU_FILE")
        .or_else(|| set("VICTORY_HOME").map(|home| home.join("victory-settings.txt")))
        .or_else(|| set("LOCALAPPDATA").map(|base| base.join("victory").join("victory-settings.txt")))
}

/// The first start of a new victory.exe: the seconds it plays before it starts
/// again by itself (`VICTORY_WARMUP_RESTART`, set by the launcher while the exe
/// has no mark under `%LOCALAPPDATA%\victory\warmed`). On the RTX 5070, every
/// new build's first run stayed at a flat 14 fps (Present ~40 ms) for as long as
/// it ran, and every later run was normal (27-29 fps), even after a first run of
/// 45 seconds; a copy of an exe already run was fast at once (2026-11-27). Likely
/// the driver's cache of the program; not ours to fix, so it is run through once.
/// Draws in a frame from which the warm-up's time counts: the game's scenes
/// (loading into the world: about 300, in the world: thousands), not its
/// title screen and menus (about 10).
const SCENE_DRAWS: u64 = 100;

pub(crate) fn warmup_seconds() -> Option<u64> {
    get("VICTORY_WARMUP_RESTART").parse().ok().filter(|s| *s > 0)
}

/// Marks this victory.exe as warmed up, whether it went down. Only a
/// written mark can end the warm-up: the launcher arms a new one whenever
/// none exists (`apps/victory/src/play.rs`, `warmup_mark`), so the write's
/// result is what decides whether the restart is safe to do.
fn mark_written(mark: &std::path::Path) -> bool {
    if let Some(dir) = mark.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(mark, "this victory.exe was warmed up (its first start, then a restart)\r\n").is_ok()
}

/// The start's notice lines for the warm-up start ([`warmup_seconds`]).
pub(crate) fn warmup_notice(seconds: u64) -> Vec<String> {
    vec![
        "FIRST START OF THIS VERSION: A ONE-TIME WARM-UP".to_owned(),
        format!("THE GAME RESTARTS BY ITSELF {seconds} SECONDS AFTER IT"),
        "LOADS INTO THE GAME (PRESS A AT THE TITLE SCREEN)".to_owned(),
        "THIS HAPPENS ONCE PER NEW VERSION, AND IS NEEDED:".to_owned(),
        "WITHOUT IT THE FIRST RUN STAYS AT ABOUT 14 FPS".to_owned(),
        String::new(),
    ]
}

/// Starts `command_line` in a console window of its own, with that console's input and output.
/// (`std::process::Command` always hands its child this process's standard handles: the restarted
/// launcher then wrote into the old launcher's pipe and log, not its own window, and kept the old
/// launcher waiting on that pipe.)
fn new_console(command_line: &str) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_CONSOLE, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
    let mut line: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
    // SAFETY: plain structs, zero is their empty value.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `line` is a writable, terminated UTF-16 string; no handles inherited; this
    // process's environment and folder.
    let made = unsafe {
        CreateProcessW(
            std::ptr::null(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if made == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the two handles CreateProcessW gave, closed once.
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

/// Whether a change of `item` shows at once. Frame generation does when this run started with
/// it ready (the launcher asks for that whenever Streamline is there).
fn live(item: &Item) -> bool {
    item.live || (item.var == "VICTORY_MENU_FRAMEGEN" && crate::streamline::framegen_live())
}

/// Whether the frame-rate line is shown at the top while the menu is closed
/// (`VICTORY_FPS_LINE=0`: not; the menu changes it).
fn fps_line_on() -> &'static AtomicBool {
    static ON: OnceLock<AtomicBool> = OnceLock::new();
    ON.get_or_init(|| AtomicBool::new(std::env::var_os("VICTORY_FPS_LINE").is_none_or(|v| v != "0")))
}

/// That line: the frames rendered a second and, while frame generation shows more, those shown.
fn fps_line() -> Option<String> {
    if !fps_line_on().load(Ordering::Relaxed) {
        return None;
    }
    // The stats panel has that corner (F3), and says the same.
    if super::panel::shown().load(Ordering::Relaxed) {
        return None;
    }
    let fps = f64::from(super::METER_FPS_TENTHS.load(Ordering::Relaxed)) / 10.0;
    if fps <= 0.0 {
        return None;
    }
    Some(match crate::streamline::shown_per_frame() {
        1 => format!("FPS: {fps:.1}"),
        per => format!("FPS: {fps:.1}  FG: {:.1}", fps * f64::from(per)),
    })
}

/// The index of `value` among the item's values (an unknown value: shown as
/// itself, at the end).
fn index_of(item: &Item, value: &str) -> Option<usize> {
    item.values.iter().position(|(v, _)| *v == value)
}

/// What the menu shows for `value`: its label, an unknown value as itself,
/// and a value the launcher did not set (a start without play.ps1) as
/// "NOT SET" rather than a blank.
fn shown_value(item: &Item, value: &str) -> String {
    match index_of(item, value) {
        Some(i) => item.values[i].1.to_owned(),
        None if value.is_empty() => "NOT SET".to_owned(),
        None => value.to_ascii_uppercase(),
    }
}

/// Keys pressed, gathered on a thread of their own every 10 ms: a frame
/// lasts 50 ms and more, and a short press would fall between two frames.
fn keys() -> &'static Mutex<Vec<u16>> {
    static KEYS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let _ = std::thread::Builder::new().name("settings menu keys".into()).spawn(|| {
            const WATCHED: [u16; 10] = [VK_F1, VK_UP, VK_DOWN, VK_LEFT, VK_RIGHT, VK_RETURN, VK_ESCAPE, VK_Q, VK_E, VK_TAB];
            let mut was = [false; WATCHED.len()];
            loop {
                std::thread::sleep(Duration::from_millis(10));
                let mut process = 0;
                // SAFETY: plain queries; room for the answer.
                unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut process) };
                let ours = process == std::process::id();
                for (i, &vk) in WATCHED.iter().enumerate() {
                    // SAFETY: a plain query.
                    let down = ours && unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
                    if down && !was[i] {
                        keys().lock().unwrap_or_else(|e| e.into_inner()).push(vk);
                    }
                    was[i] = down;
                }
            }
        });
        Mutex::new(Vec::new())
    })
}

/// The box's size in pixels.
const fn size(scale: u32) -> (u32, u32) {
    ((COLUMNS * CELL.0 + 2 * RIM) * scale, (LINES * CELL.1 + 2 * RIM) * scale)
}

fn lit(lines: &[String], x: u32, y: u32) -> bool {
    let (Some(x), Some(y)) = (x.checked_sub(RIM), y.checked_sub(RIM)) else {
        return false;
    };
    if x % CELL.0 >= GLYPH.0 || y % CELL.1 >= GLYPH.1 || x / CELL.0 >= COLUMNS {
        return false;
    }
    let Some(&c) = lines.get((y / CELL.1) as usize).and_then(|l| l.as_bytes().get((x / CELL.0) as usize)) else {
        return false;
    };
    (glyph(c.to_ascii_uppercase())[(y % CELL.1) as usize] >> (GLYPH.0 - 1 - x % CELL.0)) & 1 != 0
}

/// `rgb` (sRGB, 0 to 1) in the swap chain's `format`, into `out` (its bytes).
fn encode(format: DXGI_FORMAT, rgb: [f32; 3], out: &mut [u8]) {
    let unorm = |c: f32, max: f32| (c.clamp(0.0, 1.0) * max).round() as u32;
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => out.copy_from_slice(&[unorm(rgb[0], 255.0) as u8, unorm(rgb[1], 255.0) as u8, unorm(rgb[2], 255.0) as u8, 0xFF]),
        DXGI_FORMAT_B8G8R8A8_UNORM => out.copy_from_slice(&[unorm(rgb[2], 255.0) as u8, unorm(rgb[1], 255.0) as u8, unorm(rgb[0], 255.0) as u8, 0xFF]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let word = unorm(rgb[0], 1023.0) | unorm(rgb[1], 1023.0) << 10 | unorm(rgb[2], 1023.0) << 20 | 3 << 30;
            out.copy_from_slice(&word.to_le_bytes());
        }
        // Linear light (scRGB), as halves.
        DXGI_FORMAT_R16G16B16A16_FLOAT => {
            let linear = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
            for (i, c) in [linear(rgb[0]), linear(rgb[1]), linear(rgb[2]), 1.0].into_iter().enumerate() {
                out[2 * i..2 * i + 2].copy_from_slice(&half(c).to_le_bytes());
            }
        }
        _ => {}
    }
}

/// `value` (0 to 1) as a half float.
fn half(value: f32) -> u16 {
    let bits = value.to_bits();
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    if value <= 0.0 || exponent <= 0 {
        return 0;
    }
    ((exponent as u32) << 10 | (bits >> 13) & 0x3FF) as u16
}

/// The frame-rate line: white text on black, in the corner.
fn painted(lines: &[String], scale: u32, colours: [&[u8]; 2], pitch: usize, rows: usize) -> Vec<u8> {
    let (width, height) = size(scale);
    let bytes = colours[0].len();
    let mut out = vec![0; pitch * rows];
    for y in 0..height / scale {
        let top = pitch * (y * scale) as usize;
        for x in 0..width / scale {
            let colour = colours[usize::from(lit(lines, x, y))];
            for i in 0..scale {
                let at = top + (x * scale + i) as usize * bytes;
                out[at..at + bytes].copy_from_slice(colour);
            }
        }
        for i in 1..scale as usize {
            out.copy_within(top..top + pitch, top + i * pitch);
        }
    }
    out
}

/// The menu's picture (0x00RRGGBB, `width` a row) in the swap chain's format, rows `pitch` apart.
fn converted(pixels: &[u32], width: usize, format: DXGI_FORMAT, bytes: usize, pitch: usize, rows: usize) -> Vec<u8> {
    let mut out = vec![0; pitch * rows];
    let mut cache: Option<(u32, [u8; 8])> = None;
    for (y, row) in pixels.chunks(width).enumerate() {
        for (x, &p) in row.iter().enumerate() {
            let at = y * pitch + x * bytes;
            let encoded = match cache {
                Some((q, e)) if q == p => e,
                _ => {
                    let rgb = [(p >> 16 & 0xFF) as f32 / 255.0, (p >> 8 & 0xFF) as f32 / 255.0, (p & 0xFF) as f32 / 255.0];
                    let mut e = [0u8; 8];
                    encode(format, rgb, &mut e[..bytes]);
                    cache = Some((p, e));
                    e
                }
            };
            out[at..at + bytes].copy_from_slice(&encoded[..bytes]);
        }
    }
    out
}

struct Buffers {
    format: DXGI_FORMAT,
    /// The size held: the menu's picture or the frame-rate line, whichever is larger.
    size: (u32, u32),
    pitch: u32,
    uploads: Vec<(ID3D12Resource, *mut u8)>,
    card: ID3D12Resource,
    turn: usize,
    fresh: bool,
    /// The card's buffer was copied into before (it is in the copy-source state;
    /// at first it is in the common state, promoted by the first copy).
    used: bool,
}

// SAFETY: free-threaded resources, and memory only written under the presenter's lock.
unsafe impl Send for Buffers {}

impl Buffers {
    fn new(host: &Host, format: DXGI_FORMAT, (width, height): (u32, u32)) -> Option<Buffers> {
        let pitch = (width * colours(format)?[0].len() as u32).next_multiple_of(256);
        let bytes = u64::from(pitch) * u64::from(height);
        let uploads = (0..UPLOADS).map(|_| host.upload_buffer(bytes)).collect::<Option<Vec<_>>>()?;
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut card: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a buffer.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut card)
                .ok()?;
        }
        Some(Buffers { format, size: (width, height), pitch, uploads, card: card?, turn: 0, fresh: false, used: false })
    }

    fn write(&mut self, picture: Vec<u8>) {
        self.turn = (self.turn + 1) % UPLOADS;
        // SAFETY: a mapped upload buffer of the picture's size, which the card read frames ago.
        unsafe { std::ptr::copy_nonoverlapping(picture.as_ptr(), self.uploads[self.turn].1, picture.len()) };
        self.fresh = true;
    }
}

/// The menu of a presenter.
pub(super) struct Menu {
    /// The values chosen, by item (as at start until changed).
    values: Vec<String>,
    /// The values at start.
    started: Vec<String>,
    /// The line picked: an item, or an action after them.
    at: usize,
    note: String,
    changed: bool,
    /// What the buffer holds: the frame-rate line, or (none) the menu's lines.
    painted: Option<String>,
    buffers: Option<Buffers>,
    failed: bool,
    /// The saves in `Saves` that can be imported (read when the menu opens), and the line
    /// picked in their list (`imports.len()`: the line back), shown while `picking`.
    imports: Vec<String>,
    import_at: usize,
    picking: bool,
    /// The tab shown.
    tab: usize,
    /// Shown in the launcher's window (`launcher.rs`), before the game: nothing changes right away,
    /// and its actions play, preload and go back.
    launcher: bool,
}

/// What an action asks of whoever shows the menu.
#[derive(Debug, PartialEq)]
enum Leave {
    /// Saved; start the game (again).
    Play,
    /// Saved; make every shader, then start the game.
    Preload,
    /// The settings file was removed.
    Reset,
    Close,
}

impl Menu {
    pub(super) fn new() -> Menu {
        // Start the key thread now: F1 must be seen while the menu is closed.
        let _ = keys();
        // `VICTORY_MENU_OPEN=1`: shown from the start (to look at it in a run nobody presses keys in).
        if std::env::var_os("VICTORY_MENU_OPEN").is_some_and(|v| v == "1") {
            crate::SETTINGS_MENU_OPEN.store(true, Ordering::Relaxed);
        }
        let values: Vec<String> = ITEMS.iter().map(|i| get(i.var)).collect();
        Menu { started: values.clone(), values, at: tab_items(0)[0], note: String::new(), changed: true, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false }
    }

    fn open() -> bool {
        crate::SETTINGS_MENU_OPEN.load(Ordering::Relaxed)
    }

    /// The value of the setting `var` as the menu holds it now.
    fn value(&self, var: &str) -> &str {
        ITEMS.iter().position(|i| i.var == var).map_or("", |i| self.values[i].as_str())
    }

    /// Row `row` is left out: the picture row of the console not picked.
    fn hidden(&self, row: usize) -> bool {
        let series_s = self.value(CONSOLE) == "series-s";
        ITEMS.get(row).is_some_and(|i| if series_s { i.var == X_RES } else { i.var == S_RES })
    }

    /// The rows shown, settings then actions.
    #[cfg(test)]
    fn rows(&self) -> Vec<usize> {
        (0..ITEMS.len() + ACTIONS.len()).filter(|r| !self.hidden(*r)).collect()
    }

    /// The menu as lines of text (its words, checked by the tests; the picture is `picture`).
    #[cfg(test)]
    fn lines(&self) -> Vec<String> {
        if self.picking {
            return self.pick_lines();
        }
        let rows = self.rows();
        let total = rows.len();
        let at = rows.iter().position(|r| *r == self.at).unwrap_or(0);
        let first = at.saturating_sub(ROWS / 2).min(total.saturating_sub(ROWS));
        let mut out = vec![
            "SETTINGS                       F1 OR ESC CLOSES".to_owned(),
            "UP/DOWN PICK  LEFT/RIGHT CHANGE  ENTER DOES IT".to_owned(),
            String::new(),
        ];
        for &row in &rows[first..(first + ROWS).min(total)] {
            let cursor = if row == self.at { '>' } else { ' ' };
            let line = if let Some(item) = ITEMS.get(row) {
                let value = &self.values[row];
                let shown = shown_value(item, value);
                let mark = if *value != self.started[row] { if live(item) { " *LIVE" } else { " *" } } else if live(item) { " LIVE" } else { "" };
                format!("{cursor} {:<27} < {shown} >{mark}", item.label)
            } else {
                format!("{cursor} {}", ACTIONS[row - ITEMS.len()])
            };
            out.push(line.chars().take(COLUMNS as usize).collect());
        }
        while out.len() < 3 + ROWS {
            out.push(String::new());
        }
        out.push(String::new());
        let about = if self.at == ITEMS.len() + PRELOAD {
            ABOUT_PRELOAD
        } else if self.at == ITEMS.len() + EXPORT {
            ABOUT_EXPORT
        } else if self.at == ITEMS.len() + IMPORT {
            ABOUT_IMPORT
        } else if ITEMS.get(self.at).is_some_and(|i| i.var == MFG) {
            ABOUT_MFG
        } else if ITEMS.get(self.at).is_some_and(|i| i.var == S_RES) {
            ABOUT_S_RES
        } else if ITEMS.get(self.at).is_some_and(|i| i.var == DLSS)
            && !matches!(self.values[self.at].as_str(), "" | "off")
            && std::env::var_os("VICTORY_STREAMLINE_DIR").is_none()
        {
            ABOUT_NO_DLSS
        } else if ITEMS.get(self.at).is_some_and(|i| i.var == FRAMEGEN && !live(i))
            && !matches!(self.values[self.at].as_str(), "" | "0")
        {
            ABOUT_FRAMEGEN_RESTART
        } else {
            [""; 3]
        };
        out.extend(about.iter().map(|l| (*l).to_owned()));
        out.push(if self.note.is_empty() { "* = CHANGED: APPLIES AT THE NEXT START UNLESS LIVE".to_owned() } else { self.note.clone() });
        out.push(REPO.to_owned());
        out
    }

    /// The list IMPORT SAVE opens, in the box's lines.
    #[cfg(test)]
    fn pick_lines(&self) -> Vec<String> {
        let mut out = vec![PICK_TITLE.to_owned(), PICK_HELP.to_owned(), String::new()];
        if self.imports.is_empty() {
            out.push(PICK_NONE.to_owned());
        }
        let rows = ROWS - (out.len() - 3);
        let total = self.imports.len() + 1;
        let first = self.import_at.saturating_sub(rows / 2).min(total.saturating_sub(rows));
        for row in first..(first + rows).min(total) {
            let cursor = if row == self.import_at { '>' } else { ' ' };
            let name = self.imports.get(row).map_or_else(|| PICK_BACK.to_owned(), |n| shown_name(n));
            out.push(format!("{cursor} {name}").chars().take(COLUMNS as usize).collect());
        }
        while out.len() < 3 + ROWS {
            out.push(String::new());
        }
        out.push(String::new());
        out.extend(ABOUT_PICK.iter().map(|l| (*l).to_owned()));
        out.push(if self.note.is_empty() { format!("{} IN SAVES", self.imports.len()) } else { self.note.clone() });
        out.push(REPO.to_owned());
        out
    }

    /// Changes item `i` by `step` (+1 / -1) through its values.
    fn change(&mut self, i: usize, step: isize) {
        let item = &ITEMS[i];
        let n = item.values.len() as isize;
        let now = index_of(item, &self.values[i]).map_or(-1, |x| x as isize);
        let next = if now < 0 { if step > 0 { 0 } else { n - 1 } } else { (now + step).rem_euclid(n) };
        self.values[i] = item.values[next as usize].0.to_owned();
        if live(item) && !self.launcher {
            self.apply_live(i);
        }
        self.note.clear();
        if !self.launcher && item.var == FRAMEGEN && !live(item) && !matches!(self.values[i].as_str(), "" | "0") {
            "RESTART REQUIRED TO ENABLE FRAME GENERATION".clone_into(&mut self.note);
        }
        self.match_series_s(item.var);
    }

    /// On Series S the scene's size and DLSS's mode are one choice: a change of either
    /// (`changed`) sets the other. DLSS off is the game's 720, and 720 picked leaves it off.
    fn match_series_s(&mut self, changed: &str) {
        let (Some(dlss), Some(res)) = (ITEMS.iter().position(|i| i.var == DLSS), ITEMS.iter().position(|i| i.var == S_RES)) else {
            return;
        };
        if self.value(CONSOLE) != "series-s" {
            return;
        }
        let off = matches!(self.values[dlss].as_str(), "" | "off");
        if changed == DLSS {
            let size = S_MODES.iter().find(|(_, m)| *m == self.values[dlss]).map_or("720", |(s, _)| *s);
            size.clone_into(&mut self.values[res]);
        } else if changed == S_RES
            && !(off && self.values[res] == "720")
            && let Some((_, mode)) = S_MODES.iter().find(|(s, _)| *s == self.values[res])
        {
            (*mode).clone_into(&mut self.values[dlss]);
        }
    }

    /// A setting the build reads at each use: changed in this run's environment at once.
    fn apply_live(&self, i: usize) {
        let (var, value) = (ITEMS[i].var, &self.values[i]);
        // SAFETY: the variables changed here are read by this build with
        // std::env::var at each use; Windows' environment block is locked by
        // the process heap's routines, and std holds its own lock.
        unsafe {
            if value.is_empty() {
                std::env::remove_var(var);
            } else {
                std::env::set_var(var, value);
            }
        }
        match var {
            // Off (0) or the frames shown for each one rendered, from the next present on.
            "VICTORY_MENU_FRAMEGEN" => crate::streamline::set_framegen(value.parse().unwrap_or(0)),
            "VICTORY_FPS_LINE" => fps_line_on().store(value != "0", Ordering::Relaxed),
            "VICTORY_PANEL" => super::panel::shown().store(value == "1", Ordering::Relaxed),
            _ => {}
        }
    }

    /// Writes the changed values over those in the settings file.
    fn save(&mut self) -> bool {
        let Some(path) = settings_file() else {
            self.note = "NO FOLDER FOR THE SETTINGS FILE".to_owned();
            return false;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut kept: Vec<(String, String)> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())))
            .filter(|(k, _)| k.starts_with("VICTORY_"))
            .collect();
        for (i, item) in ITEMS.iter().enumerate() {
            if self.values[i] == self.started[i] {
                continue;
            }
            kept.retain(|(k, _)| k != item.var);
            kept.push((item.var.to_owned(), self.values[i].clone()));
        }
        let mut text = String::from("# Saved by the F1 menu; applied over the play settings at every start (an empty value: unset).\r\n");
        for (k, v) in &kept {
            text.push_str(&format!("{k}={v}\r\n"));
        }
        match std::fs::write(&path, text) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "settings menu: saved");
                self.started = self.values.clone();
                self.note = if std::env::var_os("VICTORY_MENU_FILE").is_some() {
                    "SAVED. APPLIES AT THE NEXT START"
                } else {
                    // Started some other way (a script, another launcher): the file `victory play`
                    // reads at every start, which this start did not.
                    "SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME"
                }
                .to_owned();
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "settings menu: not saved");
                self.note = "COULD NOT SAVE THE SETTINGS FILE".to_owned();
                false
            }
        }
    }

    /// Starts the launcher again in a new console, a few seconds on, and ends this run.
    /// `preload`: the launcher makes every shader first (`VICTORY_PRELOAD=1`, for that start alone).
    fn restart(&mut self, preload: bool) {
        let Some(play) = std::env::var_os("VICTORY_MENU_PLAY") else {
            self.note = "SAVED. START THE LAUNCHER AGAIN YOURSELF".to_owned();
            return;
        };
        let play = play.to_string_lossy().replace('\'', "''");
        let args = get("VICTORY_MENU_ARGS");
        // The preload is asked by VICTORY_PRELOAD=1, which both launchers read (victory.exe's is
        // `--preload`, the script's `-Preload`: the script's flag given to victory.exe was refused,
        // and the window closed before it could be read, 2026-11-27). A launcher that fails keeps
        // its window until Enter.
        let command = format!(
            "Start-Sleep 3; {}& '{play}' {args}; if ($LASTEXITCODE) {{ Read-Host 'The start failed (see above). Press Enter to close this window' }}",
            if preload { "$env:VICTORY_PRELOAD='1'; " } else { "" }
        );
        match new_console(&format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \"{}\"", command.replace('"', "\\\""))) {
            Ok(()) => {
                tracing::info!(command, preload, "settings menu: restarting");
                // Ended at once: std::process::exit runs the DLLs' unloading, which with
                // Streamline loaded did not come back (a windowless leftover process).
                // SAFETY: this process, with nothing of its own left to save.
                unsafe {
                    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
                    TerminateProcess(GetCurrentProcess(), 0);
                }
                std::process::exit(0);
            }
            Err(e) => {
                tracing::warn!(error = %e, "settings menu: restart failed");
                self.note = "SAVED, BUT THE RESTART FAILED".to_owned();
            }
        }
    }

    /// The warm-up start ([`warmup_seconds`]): once the game has drawn whole
    /// scenes for that long, this exe is marked as warmed up
    /// (`VICTORY_WARMUP_MARK`) and the game starts again. The time counts
    /// from the first frame of [`SCENE_DRAWS`] draws or more, not from the
    /// start: a player who waited on the title screen ("press A", about ten
    /// draws a frame) was restarted after 20 seconds and still had the 14 fps
    /// first run after it (2026-11-27).
    fn warmup(&mut self) {
        static BEGAN: OnceLock<std::time::Instant> = OnceLock::new();
        static DONE: AtomicBool = AtomicBool::new(false);
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let Some(seconds) = warmup_seconds() else { return };
        if DONE.load(Ordering::Relaxed) {
            return;
        }
        let ever = crate::list::draws_ever();
        let this_frame = ever.saturating_sub(LAST.swap(ever, Ordering::Relaxed));
        if BEGAN.get().is_none() && this_frame < SCENE_DRAWS {
            return;
        }
        if BEGAN.get_or_init(std::time::Instant::now).elapsed() < Duration::from_secs(seconds) {
            return;
        }
        DONE.store(true, Ordering::Relaxed);
        let mark = std::path::PathBuf::from(get("VICTORY_WARMUP_MARK"));
        let marked = mark.as_os_str().is_empty() || mark_written(&mark);
        if marked {
            tracing::info!(seconds, mark = %mark.display(), "first start of this victory.exe: warmed up; the game starts again");
            self.restart(false);
        } else {
            // The launcher arms a warm-up whenever no mark exists: one the
            // host could not write would have the game come back here for
            // ever, so this start goes on at its slow pace instead.
            tracing::warn!(mark = %mark.display(), "first start: the warmed mark could not be written; the game goes on, no restart");
            self.note = "WARMED UP, BUT THE MARK WOULD NOT WRITE: THIS RUN GOES ON".to_owned();
        }
    }

    /// Reads which saves `Saves` holds, keeping the one picked if it is still there.
    fn read_imports(&mut self) {
        let picked = self.imports.get(self.import_at).cloned();
        self.imports = crate::save_share::share().map(|s| crate::save_share::exports(&s)).unwrap_or_default();
        self.import_at = picked.and_then(|p| self.imports.iter().position(|n| *n == p)).unwrap_or(0);
    }

    /// Writes the game's save into a new zip in `Saves`.
    fn export_save(&mut self) {
        let (Some(data), Some(share)) = (crate::save_share::data(), crate::save_share::share()) else {
            self.note = "SAVE FOLDERS NOT KNOWN IN THIS START".to_owned();
            return;
        };
        let name = format!("save {}", crate::save_share::stamp());
        match crate::save_share::export(&data, &share, &name) {
            Ok(to) => {
                tracing::info!(to = %to.display(), "settings menu: save exported");
                self.note = format!("EXPORTED TO SAVES: {}.ZIP", shown_name(&name));
                self.read_imports();
            }
            Err(e) => {
                tracing::warn!(error = %e, share = %share.display(), "settings menu: save not exported");
                self.note = if e.kind() == std::io::ErrorKind::NotFound {
                    "NOTHING TO EXPORT: THE GAME HAS NOT SAVED YET"
                } else {
                    "EXPORT FAILED: THE GAME FOLDER CANNOT BE WRITTEN"
                }
                .to_owned();
            }
        }
    }

    /// Stages the save picked to be put in place at the next start, and starts the game again.
    fn import_save(&mut self) {
        let (Some(data), Some(share)) = (crate::save_share::data(), crate::save_share::share()) else {
            self.note = "SAVE FOLDERS NOT KNOWN IN THIS START".to_owned();
            return;
        };
        let Some(name) = self.imports.get(self.import_at).cloned() else {
            self.note = "NO SAVE TO IMPORT: PUT ONE IN SAVES FIRST".to_owned();
            return;
        };
        if let Err(e) = crate::save_share::stage(&data, &share, &name) {
            tracing::warn!(error = %e, name, "settings menu: save import not staged");
            self.note = if e.kind() == std::io::ErrorKind::NotFound {
                "IMPORT FAILED: NO SAVE IN THAT ZIP"
            } else {
                "IMPORT FAILED: THAT ZIP IS DAMAGED OR NOT A SAVE"
            }
            .to_owned();
            return;
        }
        tracing::info!(name, "settings menu: save import staged; put in place at the next start");
        if self.launcher {
            // Put in place when the game starts, as after the restart in the game.
            self.picking = false;
            "IMPORTED: PLAY STARTS THE GAME WITH IT".clone_into(&mut self.note);
        } else if std::env::var_os("VICTORY_MENU_PLAY").is_some() {
            self.restart(false);
        } else {
            self.note = "IMPORTED: CLOSE THE GAME AND START IT AGAIN".to_owned();
        }
    }

    /// The settings of `tab` shown now (the picture row of the console not picked is left out).
    fn shown_items(&self, tab: usize) -> Vec<usize> {
        tab_items(tab).into_iter().filter(|&i| !self.hidden(i)).collect()
    }

    fn open_tab(&mut self, tab: usize) {
        self.tab = tab % TABS.len();
        self.at = self.shown_items(self.tab).first().copied().unwrap_or(ITEMS.len());
    }

    /// Up (-1) or down (+1) through the tab's settings, then its actions (one row).
    fn step(&mut self, by: isize) {
        let mut rows = self.shown_items(self.tab);
        rows.push(ITEMS.len());
        let here = if self.at >= ITEMS.len() { rows.len() - 1 } else { rows.iter().position(|&r| r == self.at).unwrap_or(0) };
        let next = (here as isize + by).rem_euclid(rows.len() as isize) as usize;
        // Onto the actions: the first one (or the one already picked).
        self.at = if rows[next] == ITEMS.len() && self.at >= ITEMS.len() { self.at } else { rows[next] };
    }

    /// What the picture shows now: the rows, the first shown, the action picked, and the description.
    fn page_parts(&self) -> (Vec<look::Row>, usize, Option<usize>, String, Vec<String>) {
        let items = self.shown_items(self.tab);
        let rows: Vec<look::Row> = items
            .iter()
            .map(|&i| {
                let item = &ITEMS[i];
                let value = &self.values[i];
                let toggle = item.values.len() == 2 && item.values.iter().all(|(_, l)| *l == "ON" || *l == "OFF");
                let value = if toggle {
                    look::Value::Toggle(shown_value(item, value) == "ON")
                } else {
                    let at = index_of(item, value).map_or(item.values.len(), |x| x + 1);
                    look::Value::Pick(pretty(&shown_value(item, value)), at, item.values.len())
                };
                look::Row { label: pretty(item.label), value, picked: i == self.at, changed: self.values[i] != self.started[i] }
            })
            .collect();
        let picked = items.iter().position(|&i| i == self.at).unwrap_or(0);
        let first = picked.saturating_sub(look::ROWS - 1).min(rows.len().saturating_sub(look::ROWS));
        let action = self.at.checked_sub(ITEMS.len());
        let sentence = |lines: &[&str]| vec![pretty_sentence(&lines.join(" "))];
        let (heading, about) = match (ITEMS.get(self.at), action) {
            (Some(item), _) => {
                let value = self.values[self.at].as_str();
                let mut about = if item.var == MFG {
                    sentence(&ABOUT_MFG)
                } else if item.var == S_RES {
                    sentence(&ABOUT_S_RES)
                } else if item.var == DLSS && !matches!(value, "" | "off") && std::env::var_os("VICTORY_STREAMLINE_DIR").is_none() {
                    sentence(&ABOUT_NO_DLSS)
                } else if !self.launcher && item.var == FRAMEGEN && !live(item) && !matches!(value, "" | "0") {
                    sentence(&ABOUT_FRAMEGEN_RESTART)
                } else {
                    Vec::new()
                };
                about.extend(explained(item.var, &self.values[self.at]).iter().map(|l| (*l).to_owned()));
                about.push(if self.launcher {
                    "Play saves it and starts with it.".to_owned()
                } else if live(item) {
                    "Changes right away.".to_owned()
                } else {
                    "Applies at the next start: pick Save And Restart.".to_owned()
                });
                about.push(format!("Now: {}", pretty(&shown_value(item, &self.values[self.at]))));
                (pretty(item.label), about)
            }
            (None, Some(PRELOAD)) => (pretty(action_label(ACTIONS[PRELOAD], self.launcher)), sentence(&ABOUT_PRELOAD)),
            (None, Some(EXPORT)) => (pretty(action_label(ACTIONS[EXPORT], self.launcher)), sentence(&ABOUT_EXPORT)),
            (None, Some(IMPORT)) => (pretty(action_label(ACTIONS[IMPORT], self.launcher)), sentence(&ABOUT_IMPORT)),
            (None, Some(a)) => (pretty(action_label(ACTIONS[a], self.launcher)), Vec::new()),
            (None, None) => (String::new(), Vec::new()),
        };
        (rows, first, action, heading, about)
    }

    /// The list IMPORT SAVE opens, as the picture's rows: the saves, then the way back.
    fn pick_parts(&self) -> (Vec<look::Row>, usize, String, Vec<String>) {
        let mut rows: Vec<look::Row> = self
            .imports
            .iter()
            .enumerate()
            .map(|(n, name)| look::Row { label: name.clone(), value: look::Value::Plain, picked: n == self.import_at, changed: false })
            .collect();
        rows.push(look::Row { label: pretty(PICK_BACK), value: look::Value::Plain, picked: self.import_at >= self.imports.len(), changed: false });
        let first = self.import_at.saturating_sub(look::ROWS - 1).min(rows.len().saturating_sub(look::ROWS));
        let mut about = vec![pretty_sentence(&ABOUT_PICK.join(" "))];
        if self.imports.is_empty() {
            about.push(pretty_sentence(PICK_NONE.trim()));
        }
        about.push(pretty_sentence(PICK_HELP));
        (rows, first, pretty(PICK_TITLE.split("  ").next().unwrap_or("IMPORT SAVE")), about)
    }

    fn picture(&self, s: f32) -> Option<Vec<u32>> {
        self.picture_with_hits(s).map(|(pixels, _)| pixels)
    }

    fn picture_with_hits(&self, s: f32) -> Option<(Vec<u32>, look::Hits)> {
        let (rows, first, action, heading, about) = if self.picking {
            let (rows, first, heading, about) = self.pick_parts();
            (rows, first, None, heading, about)
        } else {
            self.page_parts()
        };
        let actions: Vec<String> = ACTIONS.iter().map(|a| pretty(action_label(a, self.launcher))).collect();
        let actions: Vec<&str> = actions.iter().map(String::as_str).collect();
        let note = if !self.note.is_empty() {
            pretty_sentence(&self.note)
        } else if self.launcher {
            "* Changed. Esc goes back".to_owned()
        } else {
            "* Changed: applies at the next start, unless it changes right away".to_owned()
        };
        let page = look::Page { tabs: &TABS, tab: self.tab, rows: &rows, first, actions: &actions, action, heading, about, note, link: &REPO.to_ascii_lowercase() };
        look::draw_with_hits(&page, s)
    }

    /// Removes the settings file: the launcher's own settings at the next start.
    fn reset(&mut self) {
        if let Some(path) = settings_file() {
            let _ = std::fs::remove_file(path);
        }
        self.note = "RESET. PLAY DEFAULTS AT THE NEXT START".to_owned();
    }

    /// Changes a setting and saves it at once: many players changed settings and closed the
    /// menu without the save lines, and lost them. The save lines stay (and restart, preload).
    fn change_and_save(&mut self, i: usize, step: isize) {
        self.change(i, step);
        // The launcher's window saves on Play or when leaving its settings (it asks).
        if self.launcher {
            return;
        }
        if self.save() && live(&ITEMS[i]) {
            self.note = "SAVED. ON NOW".to_owned();
        }
    }

    /// A frame was presented: the keys pressed since the last one.
    pub(super) fn frame(&mut self) {
        self.warmup();
        let pressed = std::mem::take(&mut *keys().lock().unwrap_or_else(|e| e.into_inner()));
        if pressed.is_empty() {
            return;
        }
        for vk in pressed {
            let open = Self::open();
            // Esc in the list of saves goes back to the settings.
            if open && self.picking && vk == VK_ESCAPE {
                self.picking = false;
                self.note.clear();
                self.changed = true;
                continue;
            }
            if vk == VK_F1 || (open && vk == VK_ESCAPE) {
                crate::SETTINGS_MENU_OPEN.store(!open && vk == VK_F1, Ordering::Relaxed);
                self.note.clear();
                self.picking = false;
                if !open {
                    self.read_imports();
                }
                self.changed = true;
                continue;
            }
            if !open {
                continue;
            }
            match self.key(vk) {
                Some(Leave::Play) => self.restart(false),
                Some(Leave::Preload) => self.restart(true),
                Some(Leave::Close) => crate::SETTINGS_MENU_OPEN.store(false, Ordering::Relaxed),
                Some(Leave::Reset) | None => {}
            }
            self.changed = true;
        }
    }

    /// A key pressed while the menu is shown (not F1); what it asks of the caller, if anything.
    fn key(&mut self, vk: u16) -> Option<Leave> {
        // Esc in the list of saves goes back to the settings.
        if self.picking && vk == VK_ESCAPE {
            self.picking = false;
            self.note.clear();
            return None;
        }
        // Only an Enter right after the first one starts a preload or an import.
        let confirmed = self.note == CONFIRM_PRELOAD || self.note == CONFIRM_IMPORT;
        if confirmed {
            self.note.clear();
        }
        if self.picking {
            let (n, back) = (self.imports.len() + 1, self.import_at >= self.imports.len());
            match vk {
                VK_UP => self.import_at = (self.import_at + n - 1) % n,
                VK_DOWN => self.import_at = (self.import_at + 1) % n,
                VK_RETURN if back => self.picking = false,
                VK_RETURN if !confirmed => self.note = CONFIRM_IMPORT.to_owned(),
                VK_RETURN => self.import_save(),
                _ => {}
            }
            return None;
        }
        match vk {
                VK_Q => self.open_tab(self.tab + TABS.len() - 1),
                VK_E | VK_TAB => self.open_tab(self.tab + 1),
                VK_UP => self.step(-1),
                VK_DOWN => self.step(1),
                VK_LEFT if self.at < ITEMS.len() => self.change_and_save(self.at, -1),
                VK_RIGHT if self.at < ITEMS.len() => self.change_and_save(self.at, 1),
                // On the actions, left and right go from one to the next.
                VK_LEFT => self.at = ITEMS.len() + (self.at - ITEMS.len() + ACTIONS.len() - 1) % ACTIONS.len(),
                VK_RIGHT => self.at = ITEMS.len() + (self.at - ITEMS.len() + 1) % ACTIONS.len(),
                VK_RETURN if self.at < ITEMS.len() => self.change_and_save(self.at, 1),
                VK_RETURN => match self.at - ITEMS.len() {
                    0 => return self.save().then_some(Leave::Play),
                    1 => {
                        self.save();
                    }
                    PRELOAD if !confirmed => self.note = CONFIRM_PRELOAD.to_owned(),
                    PRELOAD => return self.save().then_some(Leave::Preload),
                    3 => {
                        self.reset();
                        return Some(Leave::Reset);
                    }
                    EXPORT => self.export_save(),
                    IMPORT => {
                        self.read_imports();
                        self.import_at = 0;
                        self.picking = true;
                    }
                    _ => return Some(Leave::Close),
                },
                VK_ESCAPE => return Some(Leave::Close),
                _ => {}
            }
        None
    }

    /// Records the copy of the box onto `back` (in the copy-destination state).
    pub(super) fn draw(&mut self, host: &Host, list: &HostList, back: &ID3D12Resource, (width, height): (u32, u32), format: DXGI_FORMAT) {
        if self.failed {
            return;
        }
        // Closed: the frame-rate line in the top left corner, if it is wanted and there is a rate.
        let open = Self::open();
        let line = if open { None } else { fps_line() };
        if !open && line.is_none() {
            return;
        }
        // Our own copies are not part of a recorded frame (the recorder is in diagnostics builds).
        #[cfg(feature = "diagnostics")]
        let _ours = crate::replay::record::unrecorded();
        let scale = (height / 540).max(1);
        // The menu's scale: 1 at 1080 lines, smaller where the picture is narrow.
        let s = (height as f32 / 1080.0).min(width as f32 / 1240.0).max(0.5);
        let menu = look::size(s);
        let held = (menu.0.max(size(scale).0), menu.1.max(size(scale).1));
        if self.buffers.as_ref().is_none_or(|b| (b.format, b.size) != (format, held)) {
            std::mem::forget(self.buffers.take());
            self.buffers = Buffers::new(host, format, held);
            self.failed = self.buffers.is_none();
            self.changed = true;
        }
        if self.painted != line {
            self.painted = line.clone();
            self.changed = true;
        }
        let Some(colours) = colours(format) else { return };
        let bytes = colours[0].len();
        let picture = if !self.changed {
            None
        } else if let Some(l) = &line {
            let pitch = self.buffers.as_ref().map_or(0, |b| b.pitch as usize);
            Some(painted(std::slice::from_ref(l), scale, colours, pitch, held.1 as usize))
        } else {
            let pitch = self.buffers.as_ref().map_or(0, |b| b.pitch as usize);
            self.picture(s).map(|pixels| converted(&pixels, menu.0 as usize, format, bytes, pitch, held.1 as usize))
        };
        let Some(buffers) = &mut self.buffers else { return };
        if self.changed {
            self.changed = false;
            if let Some(picture) = picture {
                buffers.write(picture);
            }
        }
        // The whole box in the middle, or of it the first line's part in the corner.
        let (w, h) = match &line {
            Some(l) => ((l.len() as u32 * CELL.0 + 2 * RIM) * scale, (CELL.1 + 2 * RIM) * scale),
            None => menu,
        };
        let (w, h) = (w.min(width), h.min(height));
        let (x, y) = if line.is_some() { (0, 0) } else { ((width - w) / 2, (height - h) / 2) };
        let list = &list.list;
        // SAFETY: an open list and live resources; the box lies within both.
        unsafe {
            if std::mem::take(&mut buffers.fresh) {
                if std::mem::replace(&mut buffers.used, true) {
                    list.ResourceBarrier(&[transition(&buffers.card, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]);
                }
                list.CopyResource(&buffers.card, &buffers.uploads[buffers.turn].0);
                list.ResourceBarrier(&[transition(&buffers.card, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            }
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffers.card),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: buffers.size.0, Height: buffers.size.1, Depth: 1, RowPitch: buffers.pitch },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let part = D3D12_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            list.CopyTextureRegion(&to, x, y, 0, &from, Some(&part));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mark that lands ends the warm-up; one that cannot (here, beside a
    /// file of the same name) does not: the launcher arms a new warm-up
    /// whenever no mark exists, and a restart without one would come back
    /// here for ever.
    #[test]
    fn a_mark_that_cannot_be_written_does_not_end_the_warm_up() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("a file");
        std::fs::write(&blocked, b"not a folder").unwrap();
        assert!(mark_written(&dir.path().join("warmed").join("mark")));
        assert!(!mark_written(&blocked.join("mark")));
    }

    #[test]
    fn a_change_is_saved_at_once_however_the_game_was_started() {
        let home = std::env::temp_dir().join(format!("victory-menu-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // SAFETY: no other test in this crate reads these two.
        unsafe {
            std::env::remove_var("VICTORY_MENU_FILE");
            std::env::set_var("VICTORY_HOME", &home);
        }
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        let dlss = ITEMS.iter().position(|i| i.var == "VICTORY_MENU_DLSS").expect("in the menu");
        menu.change_and_save(dlss, 1);
        let text = std::fs::read_to_string(home.join("victory-settings.txt")).expect("saved without a save line");
        assert!(text.contains("VICTORY_MENU_DLSS=quality"), "{text}");
        assert_eq!(menu.note, "SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME");
        // Every note fits the box and has its letters.
        for note in [menu.note.as_str(), "SAVED. ON NOW", "SAVED. APPLIES AT THE NEXT START", "NO FOLDER FOR THE SETTINGS FILE"] {
            assert!(note.len() <= COLUMNS as usize, "{note}");
            assert!(note.bytes().all(|c| glyph(c) != glyph(b'#')), "{note}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_line_fits_and_every_character_has_a_picture() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        for at in 0..ITEMS.len() + ACTIONS.len() {
            menu.at = at;
            for line in menu.lines() {
                assert!(line.len() <= COLUMNS as usize, "{line}");
                for c in line.bytes() {
                    assert_ne!(glyph(c.to_ascii_uppercase()), glyph(b'#'), "{line}: {}", c as char);
                }
            }
        }
    }

    #[test]
    fn the_link_is_always_right_under_the_note() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        for note in ["", "SAVED"] {
            menu.note = note.to_owned();
            for at in 0..ITEMS.len() + ACTIONS.len() {
                menu.at = at;
                let lines = menu.lines();
                assert_eq!(lines.len(), LINES as usize);
                let n = lines.len();
                assert!(lines[n - 2].starts_with("* = CHANGED") || lines[n - 2] == note, "{}", lines[n - 2]);
                assert_eq!(lines[n - 1], REPO);
                // What a preload costs, and what MFG Unlocker is, is said while it is picked, and only then.
                let about = &lines[n - 5..n - 2];
                if at == ITEMS.len() + PRELOAD {
                    assert_eq!(about, ABOUT_PRELOAD);
                } else if at == ITEMS.len() + EXPORT {
                    assert_eq!(about, ABOUT_EXPORT);
                } else if at == ITEMS.len() + IMPORT {
                    assert_eq!(about, ABOUT_IMPORT);
                } else if at < ITEMS.len() && ITEMS[at].var == MFG {
                    assert_eq!(about, ABOUT_MFG);
                } else if at < ITEMS.len() && ITEMS[at].var == S_RES {
                    assert_eq!(about, ABOUT_S_RES);
                } else {
                    assert!(about.iter().all(String::is_empty), "{about:?}");
                }
            }
        }
        assert_eq!(ACTIONS[PRELOAD], "PRELOAD SHADERS (RESTARTS THE GAME)");
        assert!(CONFIRM_PRELOAD.len() <= COLUMNS as usize);
        assert_eq!(ACTIONS[EXPORT], "EXPORT SAVE (TO SAVES IN THE GAME FOLDER)");
        assert_eq!(ACTIONS[IMPORT], "IMPORT SAVE (PICK FROM SAVES)");
        let pick = [PICK_TITLE, PICK_HELP, PICK_NONE, PICK_BACK, CONFIRM_IMPORT, "IMPORT FAILED: THAT ZIP IS DAMAGED OR NOT A SAVE"];
        for line in ABOUT_EXPORT.iter().chain(&ABOUT_IMPORT).chain(&ABOUT_PICK).chain(&pick) {
            assert!(line.len() <= COLUMNS as usize, "{line}");
            assert!(line.bytes().all(|c| glyph(c) != glyph(b'#')), "{line}");
        }
        assert!(ABOUT_MFG.iter().all(|l| l.len() <= COLUMNS as usize));
        for line in ABOUT_NO_DLSS {
            assert!(line.len() <= COLUMNS as usize && line.bytes().all(|c| glyph(c) != glyph(b'#')), "{line}");
        }
        // DLSS picked on where NVIDIA's files are missing (no VICTORY_STREAMLINE_DIR in tests): said so.
        let dlss = ITEMS.iter().position(|i| i.var == DLSS).expect("in the menu");
        menu.at = dlss;
        menu.values[dlss] = "quality".to_owned();
        let lines = menu.lines();
        assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_NO_DLSS);
        menu.values[dlss] = "off".to_owned();
        let lines = menu.lines();
        assert!(lines[lines.len() - 5..lines.len() - 2].iter().all(String::is_empty));
        assert_eq!(ITEMS.iter().filter(|i| i.var == MFG).count(), 1);
        // Frame generation turned on in a run started without it (no Streamline in tests): a
        // restart is required, and the menu says so; OFF needs no word.
        for line in ABOUT_FRAMEGEN_RESTART {
            assert!(line.len() <= COLUMNS as usize && line.bytes().all(|c| glyph(c) != glyph(b'#')), "{line}");
        }
        let fg = ITEMS.iter().position(|i| i.var == FRAMEGEN).expect("in the menu");
        menu.at = fg;
        menu.values[fg] = "0".to_owned();
        menu.change(fg, 1);
        assert_eq!(menu.values[fg], "2");
        assert_eq!(menu.note, "RESTART REQUIRED TO ENABLE FRAME GENERATION");
        let lines = menu.lines();
        assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_FRAMEGEN_RESTART);
        menu.change(fg, -1);
        assert!(menu.note.is_empty());
        let lines = menu.lines();
        assert!(lines[lines.len() - 5..lines.len() - 2].iter().all(String::is_empty));
    }

    #[test]
    fn the_picture_line_is_the_consoles() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        let at = |var: &str| ITEMS.iter().position(|i| i.var == var).expect("in the menu");
        let (console, x, s, dlss) = (at(CONSOLE), at(X_RES), at(S_RES), at(DLSS));
        let shown = |menu: &Menu, label: &str| menu.lines().iter().any(|l| l.contains(label));
        "series-x".clone_into(&mut menu.values[console]);
        assert!(shown(&menu, "SERIES X PICTURE") && !shown(&menu, "SERIES S PICTURE"));
        // down from the console: the Series X line, never the hidden one
        menu.at = console;
        menu.step(1);
        assert_eq!(menu.at, x);
        assert!(!menu.rows().contains(&s) && !menu.shown_items(0).contains(&s));
        "series-s".clone_into(&mut menu.values[console]);
        assert!(shown(&menu, "SERIES S PICTURE") && !shown(&menu, "SERIES X PICTURE"));
        assert!(!menu.shown_items(0).contains(&x));
        menu.at = console;
        menu.step(1);
        assert_eq!(menu.at, s);
        menu.step(-1);
        assert_eq!(menu.at, console);
        // the size and DLSS's mode follow each other on Series S
        "quality".clone_into(&mut menu.values[dlss]);
        "720".clone_into(&mut menu.values[s]);
        menu.change(s, 1);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("626", "balanced"));
        menu.change(s, 1);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("540", "performance"));
        menu.change(dlss, -2);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("720", "quality"));
        // DLSS off is 720, and 720 leaves it off; another size turns DLSS on at its mode
        menu.change(dlss, -1);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("720", "off"));
        menu.change(s, -1);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("1080", "dlaa"));
        "off".clone_into(&mut menu.values[dlss]);
        "626".clone_into(&mut menu.values[s]);
        menu.change(s, -1);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("720", "off"));
        menu.change(s, 2);
        assert_eq!((menu.values[s].as_str(), menu.values[dlss].as_str()), ("540", "performance"));
        menu.at = s;
        assert_eq!(&menu.lines()[menu.lines().len() - 5..menu.lines().len() - 2], ABOUT_S_RES);
        for line in ABOUT_S_RES {
            assert!(line.len() <= COLUMNS as usize && line.bytes().all(|b| glyph(b) != glyph(b'#') || b == b'#'), "{line}");
        }
    }

    #[test]
    fn the_picture_has_tabs_rows_and_a_fading_bar() {
        let mut menu = Menu { values: ITEMS.iter().map(|i| i.values[0].0.to_owned()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        menu.open_tab(1);
        let (w, h) = look::size(1.0);
        let pixels = menu.picture(1.0).expect("GDI draws");
        assert_eq!(pixels.len(), (w * h) as usize);
        let at = |x: u32, y: u32| pixels[(y * w + x) as usize];
        // The picked row's bar: white at its left, fading to the right.
        let row_y = 138 + 23;
        let left = at(30, row_y);
        let right = at(750, row_y);
        assert!(left & 0xFF > 0xF0, "{left:x}");
        assert!(right & 0xFF < left & 0xFF, "{right:x}");
        // Every tab has rows, and stepping goes through them and onto the actions.
        for tab in 0..TABS.len() {
            menu.open_tab(tab);
            assert!(menu.at < ITEMS.len());
            for _ in 0..menu.shown_items(tab).len() {
                menu.step(1);
            }
            assert_eq!(menu.at, ITEMS.len());
        }
        assert_eq!(pretty("FRAME RATE CAP"), "Frame Rate Cap");
        assert_eq!(pretty("3440X1440 (21:9)"), "3440x1440 (21:9)");
        assert_eq!(pretty("DLSS MODEL"), "DLSS Model");
        assert_eq!(half(1.0), 0x3C00);
    }

    /// `VICTORY_MENU_PREVIEW=<file.bmp>`: the picture of each tab written out, to look at without the game.
    #[test]
    fn preview() {
        let Some(path) = std::env::var_os("VICTORY_MENU_PREVIEW") else { return };
        let mut menu = Menu { values: ITEMS.iter().map(|i| i.values[0].0.to_owned()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        menu.values[tab_items(1)[1]] = ITEMS[tab_items(1)[1]].values[1].0.to_owned();
        for tab in 0..TABS.len() {
            menu.open_tab(tab);
            menu.step(1);
            let (w, h) = look::size(1.0);
            let pixels = menu.picture(1.0).unwrap();
            let mut bmp = Vec::new();
            let size = 54 + pixels.len() as u32 * 4;
            bmp.extend(b"BM");
            for v in [size, 0, 54, 40, w, (h as i32).wrapping_neg() as u32] {
                bmp.extend(v.to_le_bytes());
            }
            bmp.extend(1u16.to_le_bytes());
            bmp.extend(32u16.to_le_bytes());
            bmp.extend([0u8; 24]);
            for p in &pixels {
                bmp.extend(p.to_le_bytes());
            }
            let mut out = std::path::PathBuf::from(&path);
            out.set_extension(format!("{tab}.bmp"));
            std::fs::write(out, bmp).unwrap();
        }
    }

    #[test]
    fn values_cycle_both_ways() {
        let mut menu = Menu { values: ITEMS.iter().map(|i| i.values[0].0.to_owned()).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: Vec::new(), import_at: 0, picking: false, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        let n = ITEMS[0].values.len();
        for _ in 0..n {
            menu.change(0, 1);
        }
        assert_eq!(menu.values[0], ITEMS[0].values[0].0);
        menu.change(0, -1);
        assert_eq!(menu.values[0], ITEMS[0].values[n - 1].0);
    }

    #[test]
    fn a_shared_save_name_is_shown_in_letters_the_menu_has() {
        let names = vec!["save 2026-11-27 23-45-10.zip".to_owned(), "Jason's boat ~ 100% [\u{e9}] and a very long name that goes on and on.zip".to_owned()];
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: ITEMS.len() + IMPORT, note: String::new(), changed: false, painted: None, buffers: None, failed: false, imports: names, import_at: 0, picking: true, tab: 0, launcher: false };
        menu.started = menu.values.clone();
        let check = |menu: &Menu| {
            let lines = menu.lines();
            assert_eq!(lines.len(), LINES as usize);
            assert_eq!(lines[lines.len() - 1], REPO);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_PICK);
            for line in &lines {
                assert!(line.len() <= COLUMNS as usize, "{line}");
                assert!(line.bytes().all(|c| glyph(c.to_ascii_uppercase()) != glyph(b'#')), "{line}");
            }
            lines
        };
        let lines = check(&menu);
        assert!(lines.iter().any(|l| l == "> SAVE 2026-11-27 23-45-10.ZIP"), "{lines:?}");
        assert!(lines.iter().any(|l| l == &format!("  {PICK_BACK}")), "{lines:?}");
        menu.import_at = 1;
        check(&menu);
        menu.import_at = 2;
        assert!(check(&menu).iter().any(|l| l == &format!("> {PICK_BACK}")));
        // Many saves: the list scrolls and the box keeps its size.
        menu.imports = (0..50).map(|i| format!("save {i}.zip")).collect();
        for at in [0, 25, 49, 50] {
            menu.import_at = at;
            let lines = check(&menu);
            let picked = if at == 50 { format!("> {PICK_BACK}") } else { format!("> SAVE {at}.ZIP") };
            assert!(lines.contains(&picked), "{at}: {lines:?}");
        }
        menu.imports.clear();
        menu.import_at = 0;
        let lines = check(&menu);
        assert!(lines.iter().any(|l| l == PICK_NONE));
        assert!(lines.iter().any(|l| l == &format!("> {PICK_BACK}")));
    }
}
