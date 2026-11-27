//! The F1 settings menu. F1 shows and hides it while our window has the
//! focus; Up/Down pick a setting, Left/Right change it, Enter runs the line
//! picked (save, save and restart, reset, close), Esc closes. While it is
//! shown the title sees no keyboard (`crate::SETTINGS_MENU_OPEN`, read by the
//! input module); a pad still works.
//!
//! The settings are read once at start, so a change is saved to the file
//! `VICTORY_MENU_FILE` names (`scripts\play-foxhound.ps1` sets it to
//! `victory-settings.txt` in the repository's folder and applies it at every
//! start) and takes effect at the next start; "SAVE AND RESTART" starts the
//! launcher again (`VICTORY_MENU_PLAY`, with `VICTORY_MENU_ARGS`) and ends
//! this run. The `VICTORY_MENU_...` settings are the launcher's own choices
//! (DLSS mode, frame generation, frame rate, console, picture size), which it
//! turns into the layer's switches; the others are the switches themselves.
//!
//! After the settings come the keys of the keyboard's stand-in pad
//! (`crate::keybinds`): Enter on one waits for the next key pressed and
//! makes it that control's key at once (Esc gives up), Left/Right put the
//! default back. Those are saved as `VICTORY_KEY_...` with the rest.
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
    GetAsyncKeyState, VK_DOWN, VK_ESCAPE, VK_F1, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::font::{colours, glyph};
use crate::keybinds::{self, BINDS};
use super::{Host, HostList, transition};

const GLYPH: (u32, u32) = (5, 7);
const CELL: (u32, u32) = (6, 9);
const COLUMNS: u32 = 60;
/// Lines of the box: a title, a help line, a blank, the settings shown, a
/// blank and a note.
const ROWS: usize = 20;
const LINES: u32 = ROWS as u32 + 5;
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
    Item { var: "VICTORY_MENU_FPS", label: "FRAME RATE CAP", values: &[("1000", "NONE"), ("120", "120"), ("60", "60"), ("30", "30 (THE GAME)")], live: false },
    Item { var: "VICTORY_FPS_LINE", label: "FPS AT THE TOP", values: &[("1", "ON"), ("0", "OFF")], live: true },
    Item { var: "VICTORY_PANEL", label: "STATS PANEL (F3)", values: &[("0", "OFF"), ("1", "ON")], live: true },
    Item { var: "VICTORY_FULLSCREEN", label: "FULLSCREEN (NO BORDER)", values: UNSET_ON, live: false },
    Item { var: "VICTORY_MOUSE_CAPTURE", label: "MOUSE CAPTURE", values: UNSET_ON, live: false },
    Item { var: "VICTORY_IME_ENGLISH", label: "ENGLISH KEYBOARD", values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_CONSOLE", label: "CONSOLE", values: &[("series-x", "SERIES X"), ("series-s", "SERIES S (FASTER)")], live: false },
    Item { var: "VICTORY_MENU_RES", label: "SERIES X PICTURE", values: &[("1440", "2560X1440"), ("1080to1440", "1080P TO 1440P"), ("4k", "3840X2160"), ("wide", "5120X1440 (32:9)"), ("ultrawide", "3440X1440 (21:9)")], live: false },
    // The layer's switches.
    // Both HDR items apply at the next start: the title is asked about HDR once at start and the swap chain's
    // format is fixed then. The paper white is the level the interface, intro videos and F1/F3 text sit at.
    Item { var: "VICTORY_HDR", label: "HDR10 (WINDOWS HDR ON)", values: UNSET_ON, live: false },
    Item { var: "VICTORY_HDR_WHITE", label: "HDR PAPER WHITE (NITS)", values: &[("", "203"), ("120", "120"), ("160", "160"), ("250", "250"), ("300", "300")], live: false },
    // the wide picture's runner defaults this to 16x9, and an empty saved value means unset, so the launcher drops the
    // key and the runner's default stands (16x9 is what the runner exports, so it shows the same); 0 is read by compose::ui_aspect as no letterbox.
    Item { var: "VICTORY_UI_LETTERBOX", label: "INTERFACE ON A WIDE PICTURE", values: &[("", "16:9 IN THE MIDDLE"), ("0", "STRETCHED"), ("16x9", "16:9 IN THE MIDDLE")], live: false },
    Item { var: "VICTORY_DLSS_PRESET", label: "DLSS MODEL", values: &[("", "DEFAULT"), ("k", "K"), ("j", "J"), ("m", "M"), ("l", "L")], live: false },
    // DLSS Ray Reconstruction (`streamline`, `VICTORY_DLSS_RR`): its plugin loads and reports beside DLSS; it does not draw yet
    // (the title's guide buffers are not identified), so Super Resolution still does the upscaling. Needs DLSS on.
    Item { var: "VICTORY_DLSS_RR", label: "RAY RECONSTRUCTION (LOADS ONLY)", values: UNSET_ON, live: false },
    Item { var: "VICTORY_AMBIENT", label: "LIGHT ESTIMATE", values: &[("0.8", "0.8"), ("1", "1"), ("0.5", "0.5"), ("", "OFF")], live: false },
    Item { var: "VICTORY_AMBIENT_SPECULAR", label: "REFLECTION ESTIMATE", values: ON_UNSET, live: false },
    Item { var: "VICTORY_AMBIENT_AIR", label: "FOG CAP", values: &[("1.5", "1.5"), ("1", "1"), ("2", "2"), ("", "NONE")], live: false },
    Item { var: "VICTORY_BVH_BUILDER", label: "RAY TRACING (SLOW, BROKEN)", values: UNSET_ON, live: false },
    // The same builder, with the title's calls of it through `victory_hle::modules::bvh_builder`.
    Item { var: "VICTORY_RT_NATIVE", label: "RAY TRACING NATIVE (UNTESTED)", values: UNSET_ON, live: false },
    // Reflex by itself (`streamline::reflex`): its sleep and the frame's own markers. Needs DLSS or frame
    // generation on, which load Streamline.
    Item { var: "VICTORY_REFLEX", label: "REFLEX (UNTESTED)", values: &[("", "OFF"), ("on", "ON"), ("boost", "ON + BOOST")], live: false },
    Item { var: "VICTORY_WATER_LIGHT", label: "WATER LIGHT", values: &[("", "OFF (NO DARK BLOCKS)"), ("1", "ON (GREEN, DARK BLOCKS)")], live: false },
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
];
/// The lines after the settings.
const ACTIONS: [&str; 4] = ["SAVE AND RESTART NOW", "SAVE FOR NEXT START", "RESET ALL TO PLAY DEFAULTS", "CLOSE"];

fn get(var: &str) -> String {
    std::env::var(var).unwrap_or_default()
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
/// It also carries the loading lines (`crate::loading`).
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
    let mut line = match crate::streamline::shown_per_frame() {
        1 => format!("FPS: {fps:.1}"),
        per => format!("FPS: {fps:.1}  FG: {:.1}", fps * f64::from(per)),
    };
    // a second line under it while host pipelines are still being made:
    // draws that need one are dropped until it is, so a dark or patchy
    // picture with this count up is the cache warming, not a fault (run 10:
    // 600 queued at world entry after a translator change, a black screen
    // for about a minute).
    let making = crate::pipeline::making();
    if making > 0 {
        line.push_str(&format!("\nSHADERS: {making}"));
    }
    // then what the title is loading or streaming, one line each and only
    // while its number is not zero (taken once a second by the meter, so this
    // is four atomic loads): directstorage rate and queue, textures waiting
    // for upload, uploads in flight. they are part of this line, so
    // VICTORY_FPS_LINE=0 and the stats panel hide them with it.
    line.push_str(&crate::loading::lines());
    Some(line)
}

/// The index of `value` among the item's values (an unknown value: shown as
/// itself, at the end).
fn index_of(item: &Item, value: &str) -> Option<usize> {
    item.values.iter().position(|(v, _)| *v == value)
}

/// The menu waits for a key to give a control of the stand-in pad: the key
/// thread then also reports every key pressed, as `CAPTURED | vk`.
static CAPTURE: AtomicBool = AtomicBool::new(false);
/// Marks a key in the pressed list as one seen while [`CAPTURE`] was set.
const CAPTURED: u16 = 0x100;

/// Keys pressed, gathered on a thread of their own every 10 ms: a frame
/// lasts 50 ms and more, and a short press would fall between two frames.
fn keys() -> &'static Mutex<Vec<u16>> {
    static KEYS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let _ = std::thread::Builder::new().name("settings menu keys".into()).spawn(|| {
            const WATCHED: [u16; 7] = [VK_F1, VK_UP, VK_DOWN, VK_LEFT, VK_RIGHT, VK_RETURN, VK_ESCAPE];
            let mut was = [false; WATCHED.len()];
            // while capturing, every key the input module reports; the
            // first scan only takes the keys' state, so that the Enter that
            // started the capture is not taken as the new key
            let mut all_was = [false; 256];
            let mut scanning = false;
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
                // about 240 queries every 10 ms, only while the menu waits for a key
                if ours && CAPTURE.load(Ordering::Relaxed) {
                    for vk in (0x08..=0xFE_u8).filter(|&vk| keybinds::usable(vk) || vk == 0x1B) {
                        // SAFETY: a plain query.
                        let down = unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
                        if scanning && down && !all_was[usize::from(vk)] {
                            keys().lock().unwrap_or_else(|e| e.into_inner()).push(CAPTURED | u16::from(vk));
                        }
                        all_was[usize::from(vk)] = down;
                    }
                    scanning = true;
                } else {
                    scanning = false;
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

fn painted(lines: &[String], scale: u32, colours: [&[u8]; 2], pitch: usize) -> Vec<u8> {
    let (width, height) = size(scale);
    let bytes = colours[0].len();
    let mut out = vec![0; pitch * height as usize];
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

struct Buffers {
    format: DXGI_FORMAT,
    scale: u32,
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
    fn new(host: &Host, format: DXGI_FORMAT, scale: u32) -> Option<Buffers> {
        let (width, height) = size(scale);
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
        Some(Buffers { format, scale, pitch, uploads, card: card?, turn: 0, fresh: false, used: false })
    }

    fn write(&mut self, lines: &[String]) {
        let Some(colours) = colours(self.format) else { return };
        let picture = painted(lines, self.scale, colours, self.pitch as usize);
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
    /// The control (a place in `BINDS`) waiting for its new key.
    capturing: Option<usize>,
}

/// Lines of the menu: the settings, the keys, then the actions.
const TOTAL: usize = ITEMS.len() + BINDS.len() + ACTIONS.len();

/// The control of a line, if it is one of the keys' lines.
fn bind_at(row: usize) -> Option<usize> {
    row.checked_sub(ITEMS.len()).filter(|&j| j < BINDS.len())
}

/// The key a saved value stands for: the default when it is empty or not a usable key.
fn bound_key(j: usize, value: &str) -> u8 {
    keybinds::parse(value).unwrap_or(BINDS[j].default)
}

impl Menu {
    pub(super) fn new() -> Menu {
        // Start the key thread now: F1 must be seen while the menu is closed.
        let _ = keys();
        // `VICTORY_MENU_OPEN=1`: shown from the start (to look at it in a run nobody presses keys in).
        if std::env::var_os("VICTORY_MENU_OPEN").is_some_and(|v| v == "1") {
            crate::SETTINGS_MENU_OPEN.store(true, Ordering::Relaxed);
        }
        let values: Vec<String> = ITEMS.iter().map(|i| i.var).chain(BINDS.iter().map(|b| b.var)).map(get).collect();
        Menu { started: values.clone(), values, at: 0, note: String::new(), changed: true, painted: None, buffers: None, failed: false, capturing: None }
    }

    fn open() -> bool {
        crate::SETTINGS_MENU_OPEN.load(Ordering::Relaxed)
    }

    fn lines(&self) -> Vec<String> {
        let total = TOTAL;
        let first = self.at.saturating_sub(ROWS / 2).min(total.saturating_sub(ROWS));
        let mut out = vec![
            "SETTINGS                       F1 OR ESC CLOSES".to_owned(),
            "UP/DOWN PICK  LEFT/RIGHT CHANGE  ENTER DOES IT".to_owned(),
            String::new(),
        ];
        for row in first..(first + ROWS).min(total) {
            let cursor = if row == self.at { '>' } else { ' ' };
            let line = if let Some(item) = ITEMS.get(row) {
                let value = &self.values[row];
                let shown = index_of(item, value).map_or(value.to_ascii_uppercase(), |i| item.values[i].1.to_owned());
                let mark = if *value != self.started[row] { if live(item) { " *LIVE" } else { " *" } } else if live(item) { " LIVE" } else { "" };
                format!("{cursor} {:<27} < {shown} >{mark}", item.label)
            } else if let Some(j) = bind_at(row) {
                // the keys apply at once, so they are always live
                let shown = if self.capturing == Some(j) { "PRESS A KEY".to_owned() } else { keybinds::name(bound_key(j, &self.values[row])) };
                let mark = if self.values[row] != self.started[row] { " *LIVE" } else { " LIVE" };
                format!("{cursor} {:<27} < {shown} >{mark}", BINDS[j].label)
            } else {
                format!("{cursor} {}", ACTIONS[row - ITEMS.len() - BINDS.len()])
            };
            out.push(line.chars().take(COLUMNS as usize).collect());
        }
        while out.len() < 3 + ROWS {
            out.push(String::new());
        }
        out.push(String::new());
        out.push(if self.note.is_empty() { "* = CHANGED: APPLIES AT THE NEXT START UNLESS LIVE".to_owned() } else { self.note.chars().take(COLUMNS as usize).collect() });
        out
    }

    /// Changes item `i` by `step` (+1 / -1) through its values.
    fn change(&mut self, i: usize, step: isize) {
        let item = &ITEMS[i];
        let n = item.values.len() as isize;
        let now = index_of(item, &self.values[i]).map_or(-1, |x| x as isize);
        let next = if now < 0 { if step > 0 { 0 } else { n - 1 } } else { (now + step).rem_euclid(n) };
        self.values[i] = item.values[next as usize].0.to_owned();
        if live(item) {
            self.apply_live(i);
        }
        self.note.clear();
    }

    /// Gives control `j` the key `vk` (or its default, for `None`), at once,
    /// and says which other control has the same key if one does.
    fn bind(&mut self, j: usize, vk: Option<u8>) {
        let row = ITEMS.len() + j;
        self.values[row] = vk.filter(|&vk| vk != BINDS[j].default).map(keybinds::value).unwrap_or_default();
        let vk = bound_key(j, &self.values[row]);
        keybinds::set(j, vk);
        // the same key on two controls presses both; allowed, but said
        let other = (0..BINDS.len()).find(|&k| k != j && keybinds::key(k) == vk);
        self.note = match other {
            Some(k) => format!("{} IS ALSO {}", keybinds::name(vk), BINDS[k].label.trim_start_matches("KEY: ")),
            None => format!("{} IS NOW {}", BINDS[j].label.trim_start_matches("KEY: "), keybinds::name(vk)),
        };
    }

    /// A key pressed while the menu waited for one for control `j`.
    fn captured(&mut self, j: usize, vk: u8) {
        self.stop_capture();
        if vk == 0x1B {
            self.note = "KEY NOT CHANGED".to_owned();
        } else if keybinds::usable(vk) {
            self.bind(j, Some(vk));
        } else {
            self.note = "THAT KEY CANNOT BE USED".to_owned();
        }
    }

    /// Stops waiting for a key.
    fn stop_capture(&mut self) {
        self.capturing = None;
        CAPTURE.store(false, Ordering::Relaxed);
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
        let Some(path) = std::env::var_os("VICTORY_MENU_FILE") else {
            self.note = "NO SETTINGS FILE: START WITH VICTORY PLAY".to_owned();
            return false;
        };
        let mut kept: Vec<(String, String)> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())))
            .filter(|(k, _)| k.starts_with("VICTORY_"))
            .collect();
        // the settings, then the keys, by the same places as `values`
        let vars = ITEMS.iter().map(|i| i.var).chain(BINDS.iter().map(|b| b.var));
        for (i, var) in vars.enumerate() {
            if self.values[i] == self.started[i] {
                continue;
            }
            kept.retain(|(k, _)| k != var);
            kept.push((var.to_owned(), self.values[i].clone()));
        }
        let mut text = String::from("# Saved by the F1 menu; applied over the play settings at every start (an empty value: unset).\r\n");
        for (k, v) in &kept {
            text.push_str(&format!("{k}={v}\r\n"));
        }
        match std::fs::write(&path, text) {
            Ok(()) => {
                tracing::info!(path = %std::path::Path::new(&path).display(), "settings menu: saved");
                self.started = self.values.clone();
                self.note = "SAVED. APPLIES AT THE NEXT START".to_owned();
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
    fn restart(&mut self) {
        use std::os::windows::process::CommandExt;
        let Some(play) = std::env::var_os("VICTORY_MENU_PLAY") else {
            self.note = "SAVED. START THE LAUNCHER AGAIN YOURSELF".to_owned();
            return;
        };
        let play = play.to_string_lossy().replace('\'', "''");
        let args = get("VICTORY_MENU_ARGS");
        let command = format!("Start-Sleep 3; & '{play}' {args}");
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        match std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &command])
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
        {
            Ok(_) => {
                tracing::info!(command, "settings menu: restarting");
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

    /// Removes the settings file: the launcher's own settings at the next start.
    fn reset(&mut self) {
        if let Some(path) = std::env::var_os("VICTORY_MENU_FILE") {
            let _ = std::fs::remove_file(path);
        }
        self.note = "RESET. PLAY DEFAULTS AT THE NEXT START".to_owned();
    }

    /// A frame was presented: the keys pressed since the last one.
    pub(super) fn frame(&mut self) {
        let pressed = std::mem::take(&mut *keys().lock().unwrap_or_else(|e| e.into_inner()));
        if pressed.is_empty() {
            return;
        }
        let total = TOTAL;
        for vk in pressed {
            let open = Self::open();
            // waiting for a key: only the keys the capture saw count, and F1
            if let Some(j) = self.capturing {
                if vk & CAPTURED != 0 {
                    self.captured(j, (vk & 0xFF) as u8);
                    self.changed = true;
                    continue;
                }
                if vk != VK_F1 {
                    continue;
                }
                self.stop_capture();
            }
            // (one seen by a capture that has just ended)
            if vk & CAPTURED != 0 {
                continue;
            }
            if vk == VK_F1 || (open && vk == VK_ESCAPE) {
                crate::SETTINGS_MENU_OPEN.store(!open && vk == VK_F1, Ordering::Relaxed);
                self.note.clear();
                self.changed = true;
                continue;
            }
            if !open {
                continue;
            }
            match vk {
                VK_UP => self.at = (self.at + total - 1) % total,
                VK_DOWN => self.at = (self.at + 1) % total,
                VK_LEFT if self.at < ITEMS.len() => self.change(self.at, -1),
                VK_RIGHT if self.at < ITEMS.len() => self.change(self.at, 1),
                VK_RETURN if self.at < ITEMS.len() => self.change(self.at, 1),
                VK_LEFT | VK_RIGHT if bind_at(self.at).is_some() => self.bind(self.at - ITEMS.len(), None),
                VK_RETURN if bind_at(self.at).is_some() => {
                    self.capturing = bind_at(self.at);
                    CAPTURE.store(true, Ordering::Relaxed);
                    self.note = "PRESS THE NEW KEY. ESC KEEPS THE OLD ONE".to_owned();
                }
                VK_RETURN => match self.at - ITEMS.len() - BINDS.len() {
                    0 => {
                        if self.save() {
                            self.restart();
                        }
                    }
                    1 => {
                        self.save();
                    }
                    2 => self.reset(),
                    _ => crate::SETTINGS_MENU_OPEN.store(false, Ordering::Relaxed),
                },
                _ => {}
            }
            self.changed = true;
        }
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
        if self.buffers.as_ref().is_none_or(|b| (b.format, b.scale) != (format, scale)) {
            std::mem::forget(self.buffers.take());
            self.buffers = Buffers::new(host, format, scale);
            self.failed = self.buffers.is_none();
            self.changed = true;
        }
        if self.painted != line {
            self.painted = line.clone();
            self.changed = true;
        }
        // (the corner line may hold a second line, split at its newline)
        let lines = if self.changed {
            Some(line.as_ref().map_or_else(|| self.lines(), |l| l.split('\n').map(String::from).collect()))
        } else {
            None
        };
        let Some(buffers) = &mut self.buffers else { return };
        if let Some(lines) = lines {
            self.changed = false;
            buffers.write(&lines);
        }
        // The whole box in the middle, or of it the first line's part in the corner.
        let (w, h) = match &line {
            Some(l) => {
                // as wide as its longest line, as tall as its lines
                let widest = l.split('\n').map(str::len).max().unwrap_or(0) as u32;
                let rows = l.split('\n').count() as u32;
                ((widest * CELL.0 + 2 * RIM) * scale, (rows * CELL.1 + 2 * RIM) * scale)
            }
            None => size(scale),
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
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: size(scale).0, Height: size(scale).1, Depth: 1, RowPitch: buffers.pitch },
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
    use crate::keybinds::control as bind;

    #[test]
    fn every_line_fits_and_every_character_has_a_picture() {
        let mut menu = Menu { values: vec![String::new(); ITEMS.len() + BINDS.len()], started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, capturing: None };
        menu.started = menu.values.clone();
        for at in 0..TOTAL {
            menu.at = at;
            // a key's line both as it is and while it waits for a key
            menu.capturing = bind_at(at);
            for line in menu.lines() {
                assert!(line.len() <= COLUMNS as usize, "{line}");
                for c in line.bytes() {
                    assert_ne!(glyph(c.to_ascii_uppercase()), glyph(b'#'), "{line}: {}", c as char);
                }
            }
        }
    }

    #[test]
    fn loading_lines_have_a_picture_for_every_character() {
        // constructed numbers, all four lines shown
        let text = crate::loading::render([1234, 5, 6, 7]);
        for c in text.bytes().filter(|&c| c != b'\n') {
            assert_ne!(glyph(c.to_ascii_uppercase()), glyph(b'#'), "{}", c as char);
        }
        let longest = text.split('\n').map(|l| l.chars().count()).max().unwrap_or(0);
        assert!(longest <= COLUMNS as usize, "{text}");
    }

    #[test]
    fn values_cycle_both_ways() {
        let mut menu = Menu { values: ITEMS.iter().map(|i| i.values[0].0.to_owned()).chain(BINDS.iter().map(|_| String::new())).collect(), started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, capturing: None };
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
    fn every_key_name_and_note_has_a_picture() {
        let mut menu = Menu { values: vec![String::new(); ITEMS.len() + BINDS.len()], started: Vec::new(), at: 0, note: String::new(), changed: false, painted: None, buffers: None, failed: false, capturing: None };
        menu.started = menu.values.clone();
        // each usable key on the longest label, and the note it leaves
        for vk in (0..=0xFF_u8).filter(|&vk| keybinds::usable(vk)) {
            menu.values[ITEMS.len() + bind::RIGHT_STICK_RIGHT] = keybinds::value(vk);
            menu.at = ITEMS.len() + bind::RIGHT_STICK_RIGHT;
            menu.note = format!("{} IS ALSO {}", keybinds::name(vk), "RIGHT STICK RIGHT");
            for line in menu.lines() {
                assert!(line.len() <= COLUMNS as usize, "{line}");
                for c in line.bytes() {
                    assert_ne!(glyph(c.to_ascii_uppercase()), glyph(b'#'), "{line}: {}", c as char);
                }
            }
        }
    }

    #[test]
    fn a_saved_key_reads_back_and_the_default_saves_as_unset() {
        assert_eq!(bound_key(bind::A, ""), BINDS[bind::A].default);
        assert_eq!(bound_key(bind::A, &keybinds::value(b'P')), b'P');
        assert_eq!(bound_key(bind::A, "nonsense"), BINDS[bind::A].default);
    }
}
