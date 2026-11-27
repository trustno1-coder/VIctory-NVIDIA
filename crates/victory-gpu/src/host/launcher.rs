//! The launcher: a window shown before the game (`victory launcher`, started by
//! `scripts\play-foxhound.ps1`). It shows what the next start will use, and
//! has Play, the F1 menu's settings (the same page, drawn in this window, with
//! the mouse too), a check for a newer version on GitHub, and whether to show
//! this window at the next start.
//!
//! "Build shaders" runs `victory preload` (every shader translated, the
//! pipelines made ahead, the loading ones stored) as a process of its own,
//! with a progress bar read from its lines; its output goes to a log under
//! `%LOCALAPPDATA%\victory\logs`.
//!
//! Settings are read from the environment the script set, as the F1 menu reads
//! them, and saved to the same `victory-settings.txt`; the script then starts
//! again with them. The exit code says what was picked ([`PLAY`], [`CLOSED`],
//! [`PRELOAD`], [`AGAIN`], [`BUILD`]).
//!
//! The picture is drawn as the F1 menu's is (`menu_look.rs`: GDI text, shapes
//! with smoothed edges); "PC" and "PLAY" are outlines (`launcher_shapes.rs`).
//! The banner at the top is a gradient until a picture is picked for it (a click
//! on it): that is copied to `%LOCALAPPDATA%\victory\launcher-banner.*`, outside
//! the repository, and decoded by Windows' own WIC. "Edit image" moves it (drag)
//! and zooms it (wheel); the framing is kept beside it.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use super::look::{self, Canvas, Font, INK, WHITE, mix, pack};
use super::{ITEMS, Leave, Menu, REPO, pretty, shown_value, tab_items};

/// Exit codes: start the game; the window was closed; preload the shaders, then start the game;
/// show the launcher again (its settings were reset).
pub const PLAY: i32 = 0;
pub const CLOSED: i32 = 1;
pub const PRELOAD: i32 = 3;
pub const AGAIN: i32 = 4;
/// Settings were saved here, which the script turns into this start's switches: start again with them and build
/// the shaders at once (`VICTORY_LAUNCHER_BUILD=1`).
pub const BUILD: i32 = 5;

/// The settings file's line that hides the launcher ("0") at the next start.
const SHOW_VAR: &str = "VICTORY_MENU_LAUNCHER";

/// Where "PC" starts (units): the version line is put left of it.
const PC_LEFT: f32 = 1502.0;
/// The banner's own picture, if one was picked: this name with its type's extension.
const BANNER_NAME: &str = "launcher-banner";
/// The empty banner's gradient, top left to bottom right.
const EMPTY_FROM: [f32; 3] = [0.17, 0.07, 0.27];
const EMPTY_TO: [f32; 3] = [0.56, 0.25, 0.50];
/// "Click to add an image" (or, with one, "Change image"): where a click picks the banner's picture.
const IMAGE_BOX: (f32, f32, f32, f32) = (430.0, 190.0, 1010.0, 270.0);
const CHANGE_BOX: (f32, f32, f32, f32) = (24.0, 20.0, 300.0, 84.0);
/// The edit mode's buttons, in their order, and the zoom's range.
const EDIT_BUTTONS: [&str; 5] = ["Change Image", "Remove", "Reset", "Cancel", "Done"];
const ZOOM_MOST: f32 = 6.0;
/// Posted to the window to show the file picker outside the click's handling.
const PICK_IMAGE: u32 = WM_APP + 1;
/// The text drawn here: Windows' heaviest face, as the design's letters are bold.
const FACE: &str = "Segoe UI Black";

/// The home page's units: its design is 2150 by 1272.
const DESIGN: (f32, f32) = (2150.0, 1272.0);
const BANNER_H: f32 = 466.0;
/// The background: pink at the bottom to purple at the top (the banner's tint too).
const PINK: [f32; 3] = [0.941, 0.588, 0.557];
const PURPLE: [f32; 3] = [0.459, 0.341, 0.635];
/// How strongly the banner is tinted.
const TINT: f32 = 0.55;
const PLAY_BOX: (f32, f32, f32, f32) = (794.0, 801.0, 1358.0, 947.0);
const BOTTOM_Y: f32 = 1196.0;
/// GitHub and Build shaders, one above the other.
const GITHUB_Y: f32 = BOTTOM_Y - 30.0;
const BUILD_Y: f32 = BOTTOM_Y + 34.0;
/// Home units a settings-page unit (the dialogs are laid out in the latter).
const K_HOME: f32 = DESIGN.0 / 1180.0;
const GEAR: (f32, f32, f32) = (2023.0, 1190.0, 52.0);
const CLOSE_BOX: (f32, f32, f32, f32) = (2060.0, 18.0, 2132.0, 90.0);
/// The settings page's way back: a big arrow in its top right corner (the page's units, `look::size`).
const BACK_BOX: (f32, f32, f32, f32) = (1074.0, 14.0, 1144.0, 84.0);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Spot {
    Play,
    Settings,
    Updates,
    GitHub,
    Build,
    Show,
    Close,
    Image,
}

/// How the banner's picture sits in it: its zoom over just covering the banner, and the picture's point
/// (0 to 1 across and down) at the banner's middle.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Framing {
    zoom: f32,
    cx: f32,
    cy: f32,
}

impl Default for Framing {
    fn default() -> Framing {
        Framing { zoom: 1.0, cx: 0.5, cy: 0.5 }
    }
}

/// The banner's picture, decoded (0x00RRGGBB).
struct Picture {
    w: u32,
    h: u32,
    pixels: Vec<u32>,
}

/// Editing the banner's picture: the framing before (for Cancel), and a drag under way (where it began, and
/// the framing then).
struct Edit {
    before: Framing,
    drag: Option<(i32, i32, Framing)>,
}

/// A box over the home page.
#[derive(Clone, Copy, PartialEq)]
enum Dialog {
    /// The shaders were built for this victory.exe and these settings: build again? (the button picked)
    Again(usize),
    /// Building, or done.
    Progress,
}

/// What `victory preload` has said so far.
#[derive(Default)]
struct Building {
    /// 0 translating shaders, 1 making pipelines, 2 storing the loading ones, 3 done.
    stage: usize,
    count: usize,
    total: usize,
    /// How it ended: none while it runs.
    ended: Option<Result<(), String>>,
    pid: u32,
    began: Option<Instant>,
    /// Its whole output.
    log: PathBuf,
}

impl Building {
    fn running(&self) -> bool {
        self.began.is_some() && self.ended.is_none()
    }

    /// The share done, 0 to 1 (the pipelines are most of the time).
    fn fraction(&self) -> f32 {
        let part = if self.total > 0 { (self.count as f32 / self.total as f32).min(1.0) } else { 0.0 };
        match self.stage {
            0 => 0.08 * (1.0 - (-(self.count as f32) / 12_000.0).exp()),
            1 => 0.08 + 0.82 * part,
            2 => 0.9 + 0.1 * part,
            _ => 1.0,
        }
    }

    /// One line of the preload's output.
    fn read(&mut self, line: &str) {
        let numbers: Vec<usize> = line.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect();
        let first = |n: usize| numbers.get(n).copied().unwrap_or(0);
        if line.contains("shaders in the cache") {
            self.count = first(0);
            if line.contains("do not translate") {
                (self.stage, self.count, self.total) = (1, 0, 0);
            }
        } else if line.contains("pipelines known") && line.contains("to make") {
            let todo = line.split("; ").nth(1).and_then(|t| t.split(' ').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
            (self.stage, self.count, self.total) = (1, 0, todo);
        } else if line.starts_with("into the pipeline library:") {
            (self.stage, self.count, self.total) = (2, 0, first(0));
        } else if line.contains("in the pipeline library in") {
            self.stage = 3;
        } else if line.contains(" in the library,") || (self.stage == 1 && line.contains(" of ") && line.ends_with(" s")) {
            (self.count, self.total) = (first(0), first(1));
        }
    }
}

struct State {
    menu: Menu,
    settings: bool,
    hover: Option<Spot>,
    /// The mouse is on the settings page's back arrow.
    hover_back: bool,
    /// Where the settings page's parts are, as last drawn.
    hits: look::Hits,
    show: bool,
    /// The version line and the update check's answer, written by threads.
    version: Arc<Mutex<String>>,
    updates: Arc<Mutex<String>>,
    /// The banner as drawn last, and for what size and framing.
    banner: Option<(u32, u32, Vec<u32>)>,
    banner_for: Option<(u32, u32, Framing)>,
    /// The banner's picture (looked for once: `picture_read`), and how it sits.
    picture: Option<Picture>,
    picture_read: bool,
    framing: Framing,
    edit: Option<Edit>,
    edit_hits: Vec<[i32; 4]>,
    /// Where the mouse was last (pixels).
    mouse: (i32, i32),
    size: (u32, u32),
    /// The settings page's scale (`look::size`).
    s: f32,
    exit: i32,
    /// Leaving the settings page with changes not saved: the question shown, and its button picked
    /// (0 save, 1 don't save).
    confirm: Option<usize>,
    /// Where the question's buttons are (pixels), as last drawn.
    confirm_hits: Vec<[i32; 4]>,
    /// The settings as this start has them (saved ones are only this start's switches at the next).
    initial: Vec<String>,
    dialog: Option<Dialog>,
    dialog_hits: Vec<[i32; 4]>,
    build: Arc<Mutex<Building>>,
}

/// The question's buttons.
const CONFIRM: [&str; 2] = ["Save", "Don't Save"];

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

impl Menu {
    /// The value of setting `var` as chosen (empty when there is no such setting).
    fn get(&self, var: &str) -> String {
        ITEMS.iter().position(|item| item.var == var).map(|i| self.values[i].clone()).unwrap_or_default()
    }

    #[cfg(test)]
    fn set(&mut self, var: &str, value: &str) {
        if let Some(i) = ITEMS.iter().position(|item| item.var == var) {
            self.values[i] = value.to_owned();
        }
    }

    /// The settings as this start has them, for the launcher's window.
    fn for_launcher() -> Menu {
        let values: Vec<String> = ITEMS.iter().map(|i| super::get(i.var)).collect();
        Menu {
            started: values.clone(),
            values,
            at: tab_items(0)[0],
            note: String::new(),
            changed: true,
            painted: None,
            buffers: None,
            failed: false,
            imports: Vec::new(),
            import_at: 0,
            picking: false,
            tab: 0,
            launcher: true,
        }
    }
}

/// The folder the settings file is in (the repository's), else this one.
fn repo() -> PathBuf {
    std::env::var_os("VICTORY_MENU_FILE")
        .and_then(|f| PathBuf::from(f).parent().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Whether the settings file hides the launcher.
fn shown_at_start() -> bool {
    let Some(path) = super::settings_file() else { return true };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    !text.lines().any(|l| l.split_once('=').is_some_and(|(k, v)| k.trim() == SHOW_VAR && v.trim() == "0"))
}

/// Writes whether to show the launcher at the next start into the settings file.
fn set_shown_at_start(show: bool) {
    let Some(path) = super::settings_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: Vec<String> = text.lines().filter(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != SHOW_VAR)).map(str::to_owned).collect();
    if out.is_empty() {
        out.push("# Saved by the F1 menu; applied over the play settings at every start (an empty value: unset).".to_owned());
    }
    if !show {
        out.push(format!("{SHOW_VAR}=0"));
    }
    let _ = std::fs::write(&path, out.join("\r\n") + "\r\n");
}

/// `git` in the repository's folder, without a window; its output if it worked.
fn git(args: &[&str]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("git").arg("-C").arg(repo()).args(args).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Asks GitHub for the newest commit of the main branch and says whether this checkout has it.
/// Only asks (`git ls-remote`): nothing is fetched or changed.
fn check_updates() -> String {
    let url = format!("https://{}.git", REPO.to_ascii_lowercase());
    let Some(listed) = git(&["ls-remote", &url, "refs/heads/main"]) else {
        return "Could not ask GitHub (is git installed?)".to_owned();
    };
    let Some(newest) = listed.split_whitespace().next().filter(|h| h.len() >= 7) else {
        return "GitHub gave no answer".to_owned();
    };
    if git(&["merge-base", "--is-ancestor", newest, "HEAD"]).is_some() {
        "Up to date".to_owned()
    } else {
        format!("New version on GitHub ({})", &newest[..7])
    }
}

/// One line of what the next start uses.
fn summary(menu: &Menu) -> String {
    let get = |var: &str| menu.get(var);
    let label = |var: &str| {
        ITEMS.iter().position(|i| i.var == var).map(|i| pretty(&shown_value(&ITEMS[i], &menu.values[i]))).unwrap_or_default()
    };
    let mut parts = Vec::new();
    if get("VICTORY_MENU_ULTRAWIDE") == "1" || get("VICTORY_MENU_RES") == "3440" {
        parts.push("3440x1440".to_owned());
        parts.push("Ultrawide".to_owned());
    } else if get("VICTORY_MENU_CONSOLE") == "series-s" {
        parts.push("Series S".to_owned());
    } else {
        parts.push(label("VICTORY_MENU_RES"));
    }
    match get("VICTORY_MENU_DLSS").as_str() {
        "off" | "" => parts.push("No DLSS".to_owned()),
        "dlaa" => parts.push("DLAA".to_owned()),
        _ => parts.push(format!("DLSS {}", label("VICTORY_MENU_DLSS"))),
    }
    match get("VICTORY_MENU_FRAMEGEN").as_str() {
        "" | "0" => {}
        n if get("VICTORY_MENU_FG_TYPE") == "fsr" => parts.push(format!("FSR FG x{n}")),
        n => parts.push(format!("FG x{n}")),
    }
    if let Ok(fps) = get("VICTORY_MENU_FPS").parse::<u32>()
        && fps < 1000
    {
        parts.push(format!("{fps} FPS cap"));
    }
    parts.push(if get("VICTORY_FULLSCREEN") == "1" { "Fullscreen" } else { "Window" }.to_owned());
    parts.join("  |  ")
}

/// The folder the banner's picture is kept in.
fn banner_folder() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("victory"))
}

/// The banner's picture, if one was picked.
fn banner_file() -> Option<PathBuf> {
    std::fs::read_dir(banner_folder()?)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.file_stem().is_some_and(|n| n == BANNER_NAME) && p.is_file())
}

/// The banner's picture, read for a banner `w` by `h` pixels: at up to three times the size that covers it.
fn banner_picture(w: u32, h: u32) -> Option<Picture> {
    let bytes = std::fs::read(banner_file()?).ok()?;
    decode(&bytes, |iw, ih| {
        let cover = (w as f32 / iw as f32).max(h as f32 / ih as f32);
        let k = (cover * 3.0).min(1.0);
        (((iw as f32 * k).round() as u32).max(1), ((ih as f32 * k).round() as u32).max(1))
    })
    .map(|(w, h, pixels)| Picture { w, h, pixels })
}

/// A picture (JPEG, PNG, BMP, GIF) decoded at the size `size` gives for its own (0x00RRGGBB).
fn decode(picture: &[u8], size: impl FnOnce(u32, u32) -> (u32, u32)) -> Option<(u32, u32, Vec<u32>)> {
    // SAFETY: COM on this thread, then WIC's own calls on a picture in memory that outlives them.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = factory.CreateStream().ok()?;
        stream.InitializeFromMemory(picture).ok()?;
        let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut iw, mut ih) = (0, 0);
        frame.GetSize(&mut iw, &mut ih).ok()?;
        if iw == 0 || ih == 0 {
            return None;
        }
        let (w, h) = size(iw, ih);
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler.Initialize(&frame, w, h, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let converter = factory.CreateFormatConverter().ok()?;
        converter.Initialize(&scaler, &GUID_WICPixelFormat32bppBGR, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut bytes).ok()?;
        Some((w, h, bytes.chunks_exact(4).map(|p| u32::from_le_bytes([p[0], p[1], p[2], 0])).collect()))
    }
}

/// The picture's pixels per banner pixel at zoom 1: just covering a banner `w` by `h`.
fn cover_scale(picture: &Picture, w: u32, h: u32) -> f32 {
    (w as f32 / picture.w as f32).max(h as f32 / picture.h as f32)
}

/// `framing` with its zoom in range and the picture still covering the banner.
fn clamped(framing: Framing, picture: &Picture, w: u32, h: u32) -> Framing {
    let zoom = framing.zoom.clamp(1.0, ZOOM_MOST);
    let k = cover_scale(picture, w, h) * zoom;
    // Half the banner, as a share of the picture.
    let (hx, hy) = (w as f32 / 2.0 / k / picture.w as f32, h as f32 / 2.0 / k / picture.h as f32);
    let keep = |c: f32, half: f32| if half >= 0.5 { 0.5 } else { c.clamp(half, 1.0 - half) };
    Framing { zoom, cx: keep(framing.cx, hx), cy: keep(framing.cy, hy) }
}

/// The banner `w` by `h` pixels: the picture as `framing` places it, sampled between its pixels.
fn framed(picture: &Picture, framing: Framing, w: u32, h: u32) -> Vec<u32> {
    let k = cover_scale(picture, w, h) * framing.zoom;
    let (cx, cy) = (framing.cx * picture.w as f32, framing.cy * picture.h as f32);
    let at = |x: i32, y: i32| picture.pixels[(y.clamp(0, picture.h as i32 - 1) as u32 * picture.w + x.clamp(0, picture.w as i32 - 1) as u32) as usize];
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let sy = cy + (y as f32 + 0.5 - h as f32 / 2.0) / k - 0.5;
        let (y0, fy) = (sy.floor() as i32, sy - sy.floor());
        for x in 0..w {
            let sx = cx + (x as f32 + 0.5 - w as f32 / 2.0) / k - 0.5;
            let (x0, fx) = (sx.floor() as i32, sx - sx.floor());
            let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
            let ch = |shift: u32| {
                let v = |p: u32| ((p >> shift) & 0xFF) as f32;
                let top = v(a) + (v(b) - v(a)) * fx;
                let bottom = v(c) + (v(d) - v(c)) * fx;
                ((top + (bottom - top) * fy).round() as u32).min(255)
            };
            out.push(ch(16) << 16 | ch(8) << 8 | ch(0));
        }
    }
    out
}

/// Where the framing is kept: beside the picture.
fn framing_file() -> Option<PathBuf> {
    Some(banner_folder()?.join("launcher-banner-framing.txt"))
}

fn read_framing() -> Framing {
    let text = framing_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
    let n: Vec<f32> = text.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    match n[..] {
        [zoom, cx, cy] => Framing { zoom, cx, cy },
        _ => Framing::default(),
    }
}

fn keep_framing(framing: Framing) {
    if let Some(file) = framing_file() {
        let _ = std::fs::write(file, format!("{} {} {}\r\n", framing.zoom, framing.cx, framing.cy));
    }
}

/// Removes the banner's picture and its framing.
fn remove_banner() {
    while let Some(old) = banner_file() {
        if std::fs::remove_file(old).is_err() {
            break;
        }
    }
    if let Some(file) = framing_file() {
        let _ = std::fs::remove_file(file);
    }
}

/// Asks for a picture (Windows' own file picker) and keeps a copy of it as the banner's; whether one was picked.
fn pick_banner(hwnd: HWND) -> bool {
    use windows::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW};
    let mut file = vec![0u16; 2048];
    let filter: Vec<u16> = "Pictures (*.jpg, *.png, *.bmp, *.gif)\0*.jpg;*.jpeg;*.png;*.bmp;*.gif\0\0".encode_utf16().collect();
    let title: Vec<u16> = "Picture for the launcher's banner\0".encode_utf16().collect();
    let mut ask = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: windows::core::PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrTitle: windows::core::PCWSTR(title.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: the structure and its buffers live through the call.
    if !unsafe { GetOpenFileNameW(&mut ask) }.as_bool() {
        return false;
    }
    let picked = PathBuf::from(String::from_utf16_lossy(&file[..file.iter().position(|&c| c == 0).unwrap_or(0)]));
    let Ok(bytes) = std::fs::read(&picked) else { return false };
    // Only a picture WIC reads is kept.
    if decode(&bytes, |_, _| (8, 8)).is_none() {
        return false;
    }
    let Some(folder) = banner_folder() else { return false };
    let _ = std::fs::create_dir_all(&folder);
    remove_banner();
    let ext = picked.extension().map_or("jpg".to_owned(), |e| e.to_string_lossy().to_ascii_lowercase());
    std::fs::write(folder.join(format!("{BANNER_NAME}.{ext}")), bytes).is_ok()
}

/// The background's colour at height `y` (units) of the home page.
fn back_at(y: f32) -> [f32; 3] {
    mix(PINK, PURPLE, 1.0 - y / DESIGN.1)
}

/// A gear, white at `alpha`, centred on (`cx`, `cy`) with radius `r` (units).
fn gear(c: &mut Canvas, (cx, cy, r): (f32, f32, f32), alpha: f32) {
    look::flush();
    let s = c.s;
    let (cx, cy, r) = (cx * s, cy * s, r * s);
    for py in (cy - r - 2.0) as i32..=(cy + r + 2.0) as i32 {
        for px in (cx - r - 2.0) as i32..=(cx + r + 2.0) as i32 {
            let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
            let d = (dx * dx + dy * dy).sqrt();
            let a = dy.atan2(dx);
            // Eight teeth: the outline is out at the teeth, in between them.
            let tooth = ((8.0 * a).cos() - 0.1) / 0.5;
            let outer = r * (0.74 + 0.26 * tooth.clamp(0.0, 1.0));
            let cover = (outer - d + 0.5).clamp(0.0, 1.0) * (d - r * 0.34 + 0.5).clamp(0.0, 1.0);
            c.blend(px, py, WHITE, cover * alpha);
        }
    }
}

/// Outlines (`launcher_shapes.rs`, units) filled (non-zero winding) with smoothed edges, a gradient from `top` (at
/// `y0`) to `bottom` (at `y1`, units).
fn fill_shape(c: &mut Canvas, contours: &[&[[f32; 2]]], (y0, y1): (f32, f32), top: [f32; 3], bottom: [f32; 3]) {
    const SUB: usize = 4;
    let s = c.s;
    let points = contours.iter().flat_map(|c| c.iter());
    let (mut x_lo, mut x_hi, mut y_lo, mut y_hi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in points {
        (x_lo, x_hi, y_lo, y_hi) = (x_lo.min(p[0]), x_hi.max(p[0]), y_lo.min(p[1]), y_hi.max(p[1]));
    }
    let (px0, px1) = ((x_lo * s).floor() as i32, (x_hi * s).ceil() as i32 + 1);
    look::flush();
    let mut cover = vec![0f32; (px1 - px0).max(0) as usize];
    let mut xs = Vec::new();
    for py in (y_lo * s).floor() as i32..=(y_hi * s).ceil() as i32 {
        cover.iter_mut().for_each(|v| *v = 0.0);
        for sub in 0..SUB {
            let y = (py as f32 + (sub as f32 + 0.5) / SUB as f32) / s;
            xs.clear();
            for contour in contours {
                for i in 0..contour.len() {
                    let (a, b) = (contour[i], contour[(i + 1) % contour.len()]);
                    if (a[1] <= y) != (b[1] <= y) {
                        let winding = if b[1] > a[1] { 1 } else { -1 };
                        xs.push(((a[0] + (y - a[1]) / (b[1] - a[1]) * (b[0] - a[0])) * s, winding));
                    }
                }
            }
            xs.sort_by(|a, b| a.0.total_cmp(&b.0));
            // Inside where the outlines wind around (overlapping outlines stay filled).
            let mut spans = Vec::new();
            let mut winding = 0;
            for &(x, w) in &xs {
                if winding == 0 {
                    spans.push([x, x]);
                }
                winding += w;
                if winding == 0
                    && let Some(span) = spans.last_mut()
                {
                    span[1] = x;
                }
            }
            for span in &spans {
                let (l, r) = (span[0], span[1]);
                for px in l.floor() as i32..r.ceil() as i32 {
                    let part = (r.min(px as f32 + 1.0) - l.max(px as f32)).clamp(0.0, 1.0);
                    if let Some(v) = cover.get_mut((px - px0) as usize) {
                        *v += part / SUB as f32;
                    }
                }
            }
        }
        let rgb = mix(top, bottom, ((py as f32 / s - y0) / (y1 - y0)).clamp(0.0, 1.0));
        for (i, &a) in cover.iter().enumerate() {
            if a > 0.0 {
                c.blend(px0 + i as i32, py, rgb, a.min(1.0));
            }
        }
    }
}

/// A box in the middle of a page `page` units in size, `k` page units a settings-page unit: a title, lines,
/// a progress bar (0 to 1) and buttons (`picked` filled); where its buttons are (pixels).
#[allow(clippy::too_many_arguments)]
fn dialog(c: &mut Canvas, page: (f32, f32), k: f32, title: &str, lines: &[String], bar: Option<f32>, buttons: &[&str], picked: usize) -> Vec<[i32; 4]> {
    let s = c.s;
    c.bar((0.0, 0.0, page.0, page.1), [0.05, 0.02, 0.08], 0.6, 0.6);
    let height = 112.0 + 34.0 * lines.len().saturating_sub(1) as f32 + if bar.is_some() { 56.0 } else { 0.0 } + 104.0;
    let (x0, x1) = (page.0 / 2.0 - 320.0 * k, page.0 / 2.0 + 320.0 * k);
    let (y0, y1) = (page.1 / 2.0 - height / 2.0 * k, page.1 / 2.0 + height / 2.0 * k);
    c.round_rect((x0, y0, x1, y1), 18.0 * k, [0.30, 0.13, 0.38], 1.0, None);
    c.round_rect((x0, y0, x1, y1), 18.0 * k, WHITE, 0.85, Some(2.0 * k));
    let (title_font, body, button) = (Font::new(30.0 * k, s, 600), Font::new(21.0 * k, s, 400), Font::new(19.0 * k, s, 600));
    c.text(&title_font, title, x0 + 36.0 * k, y0 + 48.0 * k, WHITE, false);
    c.rule(x0 + 36.0 * k, x1 - 36.0 * k, y0 + 76.0 * k, 0.6);
    let mut y = y0 + 112.0 * k;
    for line in lines {
        c.text(&body, line, x0 + 36.0 * k, y, WHITE, false);
        y += 34.0 * k;
    }
    if let Some(done) = bar {
        let (b0, b1) = (x0 + 36.0 * k, x1 - 36.0 * k);
        let (t0, t1) = (y - 4.0 * k, y + 14.0 * k);
        c.round_rect((b0, t0, b1, t1), 9.0 * k, WHITE, 0.2, None);
        if done > 0.0 {
            c.round_rect((b0, t0, b0 + (b1 - b0) * done.clamp(0.02, 1.0), t1), 9.0 * k, WHITE, 1.0, None);
        }
    }
    // The buttons, right-aligned, in their order.
    let widths: Vec<f32> = buttons
        .iter()
        .map(|b| Canvas::new(1, 1, s).map_or(60.0 * k, |mut p| p.text(&button, b, -10_000.0, -10_000.0, WHITE, false)) + 40.0 * k)
        .collect();
    let mut x = x1 - 36.0 * k - widths.iter().sum::<f32>() - 14.0 * k * widths.len().saturating_sub(1) as f32;
    let by = y1 - 50.0 * k;
    let mut hits = Vec::new();
    for (i, (name, width)) in buttons.iter().zip(&widths).enumerate() {
        let (bx0, bx1) = (x, x + width);
        let (r0, r1) = (by - 21.0 * k, by + 21.0 * k);
        if picked == i {
            c.round_rect((bx0, r0, bx1, r1), 21.0 * k, WHITE, 1.0, None);
            c.text(&button, name, bx0 + 20.0 * k, by, INK, false);
        } else {
            c.round_rect((bx0, r0, bx1, r1), 21.0 * k, WHITE, 0.8, Some(1.5 * k));
            c.text(&button, name, bx0 + 20.0 * k, by, WHITE, false);
        }
        hits.push([(bx0 * s) as i32, (r0 * s) as i32, (bx1 * s).ceil() as i32, (r1 * s).ceil() as i32]);
        x = bx1 + 14.0 * k;
    }
    hits
}

/// The unsaved-changes question over the settings page (its units); where its buttons are.
fn question(c: &mut Canvas, picked: usize) -> Vec<[i32; 4]> {
    let (w, h) = look::size(1.0);
    dialog(c, (w as f32, h as f32), 1.0, "Unsaved changes", &["Save them before going back?".to_owned()], None, &CONFIRM, picked)
}

/// What marks the shaders as built: this victory.exe (its size and time) and the settings their caches depend on.
fn built_key() -> Option<String> {
    let exe = std::env::current_exe().ok()?.metadata().ok()?;
    let time = exe.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{} {time} {} {}", exe.len(), super::get("VICTORY_MENU_CONSOLE"), super::get("VICTORY_ULTRAWIDE")))
}

fn built_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("victory").join("launcher-shaders-built.txt"))
}

/// Whether a build finished for this victory.exe and these settings.
fn built() -> bool {
    let (Some(key), Some(file)) = (built_key(), built_file()) else { return false };
    std::fs::read_to_string(file).unwrap_or_default().lines().any(|l| l == key)
}

fn mark_built() {
    let (Some(key), Some(file)) = (built_key(), built_file()) else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(f, "{key}");
    }
}

/// Starts `victory preload` on the game's folder (`VICTORY_MENU_EXE`'s), its lines read into `build`.
fn start_build(build: &Arc<Mutex<Building>>, window: isize) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut state = build.lock().unwrap_or_else(|e| e.into_inner());
    *state = Building { began: Some(Instant::now()), ..Building::default() };
    let post = move || {
        // SAFETY: a message to our window; a closed window just drops it.
        unsafe {
            let _ = PostMessageW(Some(HWND(window as *mut _)), WM_APP, WPARAM(0), LPARAM(0));
        }
    };
    let Some(game) = std::env::var_os("VICTORY_MENU_EXE").and_then(|e| PathBuf::from(e).parent().map(PathBuf::from)) else {
        state.ended = Some(Err("No game folder: start this from play-foxhound.ps1".to_owned()));
        return;
    };
    let logs = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("victory").join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    state.log = logs.join(format!("victory-preload-{stamp}.log"));
    let (Ok(exe), Ok(log)) = (std::env::current_exe(), std::fs::File::create(&state.log)) else {
        state.ended = Some(Err("Could not start it".to_owned()));
        return;
    };
    let child = std::process::Command::new(exe)
        .arg("preload")
        .arg(&game)
        .stdout(std::process::Stdio::piped())
        .stderr(log.try_clone().map_or_else(|_| std::process::Stdio::null(), std::process::Stdio::from))
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            state.ended = Some(Err(format!("Could not start it: {e}")));
            return;
        }
    };
    state.pid = child.id();
    drop(state);
    let (build, mut log) = (build.clone(), log);
    std::thread::spawn(move || {
        if let Some(out) = child.stdout.take() {
            let mut out = std::io::BufReader::new(out);
            let mut bytes = Vec::new();
            while out.read_until(b'\n', &mut bytes).is_ok_and(|n| n > 0) {
                let _ = log.write_all(&bytes);
                let line = String::from_utf8_lossy(&bytes).trim().to_owned();
                bytes.clear();
                let mut state = build.lock().unwrap_or_else(|e| e.into_inner());
                let before = (state.stage, state.count, state.total);
                state.read(&line);
                if (state.stage, state.count, state.total) != before {
                    drop(state);
                    post();
                }
            }
        }
        let ended = match child.wait() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("Stopped (code {})", status.code().unwrap_or(-1))),
            Err(e) => Err(format!("Stopped ({e})")),
        };
        if ended.is_ok() {
            mark_built();
        }
        build.lock().unwrap_or_else(|e| e.into_inner()).ended = Some(ended);
        post();
    });
}

/// Ends the preload and the processes it started; what they made is kept.
fn stop_build(build: &Arc<Mutex<Building>>) {
    use std::os::windows::process::CommandExt;
    let pid = build.lock().unwrap_or_else(|e| e.into_inner()).pid;
    if pid != 0 {
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(0x0800_0000).status();
    }
}

/// The progress box's lines, bar, buttons and title.
fn progress_box(b: &Building) -> (String, Vec<String>, f32, &'static [&'static str]) {
    let minutes = b.began.map_or(0.0, |t| t.elapsed().as_secs_f32() / 60.0);
    match &b.ended {
        None => {
            let what = ["Translating the shaders", "Building the pipelines", "Storing the loading pipelines", "Finishing"][b.stage.min(3)];
            let count = match (b.stage, b.total) {
                (0, _) => format!("{} shaders", b.count),
                (_, 0) => String::new(),
                _ => format!("{} of {}", b.count, b.total),
            };
            let lines = vec![format!("{what}  {count}"), format!("{:.0}%   {minutes:.1} min. Stop keeps what is done.", b.fraction() * 100.0)];
            ("Building shaders".to_owned(), lines, b.fraction(), &["Stop"])
        }
        Some(Ok(())) => (
            "Shaders built".to_owned(),
            vec![format!("Done in {minutes:.1} min."), "The game will not stop to make them while you play.".to_owned()],
            1.0,
            &["OK"],
        ),
        Some(Err(why)) => (
            "Build stopped".to_owned(),
            vec![why.clone(), "What was made is kept: the next build goes on from there.".to_owned()],
            b.fraction(),
            &["OK"],
        ),
    }
}

impl State {
    /// The home page's scale: pixels a design unit.
    fn home_scale(&self) -> f32 {
        self.size.0 as f32 / DESIGN.0
    }

    fn spot_box(spot: Spot) -> (f32, f32, f32, f32) {
        match spot {
            Spot::Play => PLAY_BOX,
            Spot::Settings => (GEAR.0 - GEAR.2 - 14.0, GEAR.1 - GEAR.2 - 14.0, GEAR.0 + GEAR.2 + 14.0, GEAR.1 + GEAR.2 + 14.0),
            Spot::Updates => (24.0, BOTTOM_Y - 34.0, 860.0, BOTTOM_Y + 34.0),
            Spot::GitHub => (960.0, GITHUB_Y - 30.0, 1190.0, GITHUB_Y + 30.0),
            Spot::Build => (960.0, BUILD_Y - 30.0, 1400.0, BUILD_Y + 30.0),
            Spot::Show => (1660.0, BOTTOM_Y - 34.0, 1900.0, BOTTOM_Y + 34.0),
            Spot::Close => CLOSE_BOX,
            Spot::Image => IMAGE_BOX,
        }
    }

    /// The home page's spot under (`x`, `y`) pixels.
    fn spot_at(&self, x: i32, y: i32) -> Option<Spot> {
        let s = self.home_scale();
        let (x, y) = (x as f32 / s, y as f32 / s);
        if self.edit.is_some() {
            return None;
        }
        let image = if self.picture.is_some() { CHANGE_BOX } else { IMAGE_BOX };
        [Spot::Play, Spot::Settings, Spot::Updates, Spot::GitHub, Spot::Build, Spot::Show, Spot::Close, Spot::Image].into_iter().find(|&spot| {
            let (x0, y0, x1, y1) = if spot == Spot::Image { image } else { Self::spot_box(spot) };
            x >= x0 && x < x1 && y >= y0 && y < y1
        })
    }

    fn home(&mut self) -> Option<Vec<u32>> {
        let (w, h) = self.size;
        let s = self.home_scale();
        let mut c = Canvas::new(w, h, s)?;
        look::flush();
        // Background, then the banner: its picture tinted with the same gradient, or a gradient of its own.
        let banner_px = (BANNER_H * s).round() as u32;
        if !self.picture_read {
            self.picture = banner_picture(w, banner_px);
            self.framing = read_framing();
            self.picture_read = true;
        }
        if let Some(picture) = &self.picture {
            self.framing = clamped(self.framing, picture, w, banner_px);
        }
        if self.banner_for != Some((w, banner_px, self.framing)) {
            self.banner = self.picture.as_ref().map(|p| (w, banner_px, framed(p, self.framing, w, banner_px)));
            self.banner_for = Some((w, banner_px, self.framing));
        }
        let pixels = c.pixels();
        for y in 0..h {
            let units = y as f32 / s;
            let back = pack(back_at(units));
            let row = &mut pixels[(y * w) as usize..((y + 1) * w) as usize];
            match &self.banner {
                Some((_, bh, picture)) if y < *bh => {
                    // The tint runs from purple above the banner to pink below it.
                    let tint = mix(PINK, PURPLE, (834.0 - units) / 1200.0);
                    for (x, p) in row.iter_mut().enumerate() {
                        let q = picture[(y * w) as usize + x];
                        let ch = |shift: u32, t: f32| {
                            let v = ((q >> shift) & 0xFF) as f32 / 255.0;
                            ((v + (t - v) * TINT).clamp(0.0, 1.0) * 255.0).round() as u32
                        };
                        *p = ch(16, tint[0]) << 16 | ch(8, tint[1]) << 8 | ch(0, tint[2]);
                    }
                }
                None if y < banner_px => {
                    for (x, p) in row.iter_mut().enumerate() {
                        let t = (x as f32 / w as f32 * 0.7 + y as f32 / banner_px as f32 * 0.3).clamp(0.0, 1.0);
                        *p = pack(mix(EMPTY_FROM, EMPTY_TO, t));
                    }
                }
                _ => row.fill(back),
            }
        }
        // The banner's foot: a shade, then a white line.
        for i in 0..40 {
            let y = BANNER_H - 160.0 + i as f32 * 4.0;
            c.bar((0.0, y, DESIGN.0, y + 4.0), [0.12, 0.05, 0.18], i as f32 / 40.0 * 0.45, i as f32 / 40.0 * 0.45);
        }
        c.bar((0.0, BANNER_H, DESIGN.0, BANNER_H + 8.0), WHITE, 1.0, 1.0);

        let version_font = Font::face(FACE, 40.0, s, 900);
        let line_font = Font::face(FACE, 38.0, s, 900);
        let bottom_font = Font::face(FACE, 36.0, s, 900);
        let small = Font::face(FACE, 28.0, s, 900);

        // Where a click picks the banner's picture: in its middle while it has none, discreet in its corner after.
        let image_hover = self.hover == Some(Spot::Image);
        if self.edit.is_some() {
            // Editing: an outline round the banner, its buttons and how to move it.
            c.round_rect((6.0, 6.0, DESIGN.0 - 6.0, BANNER_H - 2.0), 6.0, WHITE, 0.7, Some(3.0));
            let button = Font::face(FACE, 26.0, s, 900);
            let mut x = 28.0;
            self.edit_hits.clear();
            for (i, name) in EDIT_BUTTONS.iter().enumerate() {
                let probe = Canvas::new(1, 1, s).map_or(80.0, |mut p| p.text(&button, name, -10_000.0, -10_000.0, WHITE, false));
                let (x0, x1, y0, y1) = (x, x + probe + 44.0, 26.0, 78.0);
                let done = i == EDIT_BUTTONS.len() - 1;
                c.round_rect((x0, y0, x1, y1), 26.0, [0.10, 0.04, 0.16], 0.55, None);
                if done {
                    c.round_rect((x0, y0, x1, y1), 26.0, WHITE, 1.0, None);
                    c.text(&button, name, x0 + 22.0, (y0 + y1) / 2.0, INK, false);
                } else {
                    c.round_rect((x0, y0, x1, y1), 26.0, WHITE, 0.85, Some(2.5));
                    c.text(&button, name, x0 + 22.0, (y0 + y1) / 2.0, WHITE, false);
                }
                self.edit_hits.push([(x0 * s) as i32, (y0 * s) as i32, (x1 * s).ceil() as i32, (y1 * s).ceil() as i32]);
                x = x1 + 16.0;
            }
            c.text(&small, "Drag to move it, the mouse wheel zooms. Enter: done, Esc: cancel.", 40.0, 116.0, WHITE, false);
        } else if self.picture.is_none() {
            let (x0, y0, x1, y1) = IMAGE_BOX;
            let ink = if image_hover { WHITE } else { mix(EMPTY_FROM, WHITE, 0.6) };
            if image_hover {
                c.round_rect(IMAGE_BOX, 40.0, WHITE, 0.12, None);
            }
            c.round_rect(IMAGE_BOX, 40.0, ink, 0.6, Some(3.0));
            c.text(&small, "+   Click to add an image", (x0 + x1) / 2.0 - 190.0, (y0 + y1) / 2.0, ink, false);
        } else {
            let (x0, _, _, y1) = CHANGE_BOX;
            let ink = if image_hover { WHITE } else { mix(PURPLE, WHITE, 0.55) };
            c.text(&small, "Edit image", x0 + 18.0, y1 - 32.0, ink, false);
        }

        // "PC" and the version, on the banner's line.
        fill_shape(&mut c, super::launcher_shapes::PC_SHAPE, (251.0, 470.0), WHITE, WHITE);
        let version = self.version.lock().map(|v| v.clone()).unwrap_or_default();
        c.text(&version_font, &version, PC_LEFT - 26.0, BANNER_H - 38.0, WHITE, true);
        // Close.
        let close_alpha = if self.hover == Some(Spot::Close) { 1.0 } else { 0.7 };
        let (x0, y0, x1, y1) = CLOSE_BOX;
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        for i in 0..24 {
            let t = i as f32 / 23.0 * 36.0 - 18.0;
            c.round_rect((cx + t - 2.5, cy + t - 2.5, cx + t + 2.5, cy + t + 2.5), 2.5, WHITE, close_alpha, None);
            c.round_rect((cx + t - 2.5, cy - t - 2.5, cx + t + 2.5, cy - t + 2.5), 2.5, WHITE, close_alpha, None);
        }

        // What the next start uses.
        let line_y = 534.0;
        let soft = mix(back_at(line_y), WHITE, 0.6);
        c.text(&line_font, "Starts with", 148.0, line_y, soft, false);
        c.text(&line_font, &summary(&self.menu), 2002.0, line_y, WHITE, true);
        c.bar((0.0, 574.0, DESIGN.0, 582.0), WHITE, 0.14, 0.14);

        // Play.
        let hover = self.hover == Some(Spot::Play);
        let (x0, y0, x1, y1) = PLAY_BOX;
        if hover {
            c.round_rect((x0 - 10.0, y0 - 10.0, x1 + 10.0, y1 + 10.0), 44.0, WHITE, 0.25, None);
        }
        c.round_rect(PLAY_BOX, 34.0, WHITE, 1.0, None);
        fill_shape(&mut c, super::launcher_shapes::PLAY_SHAPE, (840.5, 913.5), PURPLE, mix(PINK, INK, 0.25));

        // The hint for the box, while the mouse is on it.
        if self.hover == Some(Spot::Show) {
            let text = "Unticked, the game starts at once. -Launcher (or holding Shift at the start) shows this again.";
            c.text(&small, text, 2002.0, BOTTOM_Y - 86.0, mix(back_at(1110.0), WHITE, 0.85), true);
        }

        // The bottom row.
        let lit = |spot: Spot, hover: Option<Spot>| if hover == Some(spot) { WHITE } else { mix(back_at(BOTTOM_Y), WHITE, 0.82) };
        let updates = self.updates.lock().map(|u| u.clone()).unwrap_or_default();
        let w_up = c.text(&bottom_font, "Check updates", 40.0, BOTTOM_Y, lit(Spot::Updates, self.hover), false);
        if !updates.is_empty() {
            c.text(&small, &updates, 40.0 + w_up + 28.0, BOTTOM_Y, mix(back_at(BOTTOM_Y), WHITE, 0.75), false);
        }
        c.text(&bottom_font, "GitHub", 1007.0, GITHUB_Y, lit(Spot::GitHub, self.hover), false);
        c.text(&bottom_font, "Build shaders", 1007.0, BUILD_Y, lit(Spot::Build, self.hover), false);
        let ink = lit(Spot::Show, self.hover);
        let (b0, b1) = ((1680.0, BOTTOM_Y - 16.0), (1712.0, BOTTOM_Y + 16.0));
        if self.show {
            c.round_rect((b0.0, b0.1, b1.0, b1.1), 5.0, ink, 1.0, None);
            // The tick: two strokes of dots.
            for i in 0..12 {
                let t = i as f32 / 11.0;
                let (x, y) = (1686.0 + 7.0 * t, BOTTOM_Y + 1.0 + 7.0 * t);
                c.circle(x, y, 2.2, INK);
                let (x, y) = (1693.0 + 13.0 * t, BOTTOM_Y + 8.0 - 18.0 * t);
                c.circle(x, y, 2.2, INK);
            }
        } else {
            c.round_rect((b0.0, b0.1, b1.0, b1.1), 5.0, ink, 1.0, Some(3.0));
        }
        c.text(&bottom_font, "Launcher", 1730.0, BOTTOM_Y, ink, false);
        gear(&mut c, GEAR, if self.hover == Some(Spot::Settings) { 1.0 } else { 0.7 });

        match self.dialog {
            Some(Dialog::Again(picked)) => {
                let lines = vec![
                    "You have built them for this version and these settings.".to_owned(),
                    "Build again? Only what is missing is made.".to_owned(),
                ];
                self.dialog_hits = dialog(&mut c, DESIGN, K_HOME, "Shaders already built", &lines, None, &["Build Again", "Cancel"], picked);
            }
            Some(Dialog::Progress) => {
                let (title, lines, done, buttons) = progress_box(&self.build.lock().unwrap_or_else(|e| e.into_inner()));
                self.dialog_hits = dialog(&mut c, DESIGN, K_HOME, &title, &lines, Some(done), buttons, 0);
            }
            None => {}
        }

        look::flush();
        Some(c.pixels().iter().map(|p| p & 0x00FF_FFFF).collect())
    }

    /// Whether (`x`, `y`) pixels is on the settings page's back arrow.
    fn on_back(&self, x: i32, y: i32) -> bool {
        let (x0, y0, x1, y1) = BACK_BOX;
        let (x, y) = (x as f32 / self.s, y as f32 / self.s);
        self.settings && x >= x0 && x < x1 && y >= y0 && y < y1
    }

    fn picture(&mut self) -> Option<Vec<u32>> {
        if self.settings {
            let (pixels, hits) = self.menu.picture_with_hits(self.s)?;
            self.hits = hits;
            let (w, h) = self.size;
            let mut c = Canvas::new(w, h, self.s)?;
            look::flush();
            c.pixels().copy_from_slice(&pixels);
            // The back arrow: a ring with an arrow pointing left, brighter under the mouse.
            let alpha = if self.hover_back { 1.0 } else { 0.8 };
            let (x0, y0, x1, y1) = BACK_BOX;
            let (cx, cy, r) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0) / 2.0);
            if self.hover_back {
                c.round_rect(BACK_BOX, r, WHITE, 0.18, None);
            }
            c.round_rect(BACK_BOX, r, WHITE, alpha, Some(2.5));
            let dot = 2.6;
            for i in 0..=30 {
                let t = i as f32 / 30.0;
                // The shaft, then the two strokes of the head.
                let (x, y) = (cx - 15.0 + 30.0 * t, cy);
                c.round_rect((x - dot, y - dot, x + dot, y + dot), dot, WHITE, alpha, None);
                let (x, y) = (cx - 15.0 + 12.0 * t, cy - 12.0 * t);
                c.round_rect((x - dot, y - dot, x + dot, y + dot), dot, WHITE, alpha, None);
                let (x, y) = (cx - 15.0 + 12.0 * t, cy + 12.0 * t);
                c.round_rect((x - dot, y - dot, x + dot, y + dot), dot, WHITE, alpha, None);
            }
            if let Some(picked) = self.confirm {
                self.confirm_hits = question(&mut c, picked);
            }
            look::flush();
            Some(c.pixels().iter().map(|p| p & 0x00FF_FFFF).collect())
        } else {
            self.home()
        }
    }

    /// A click at (`x`, `y`) pixels; whether to close the window.
    fn click(&mut self, hwnd: HWND, x: i32, y: i32) -> bool {
        if self.confirm.is_some() {
            if let Some(b) = self.confirm_hits.iter().position(|&r| look::Hits::inside(r, x, y)) {
                self.confirm = Some(b);
                self.answer();
            }
            return false;
        }
        if self.on_back(x, y) {
            self.hover_back = false;
            if self.menu.picking {
                // Back from the list of saves to the settings, as Esc.
                return self.key(hwnd, VK_ESCAPE_CODE);
            }
            self.leave_settings();
            return false;
        }
        if self.settings {
            let inside = look::Hits::inside;
            if inside(self.hits.prev, x, y) {
                return self.key(hwnd, VK_Q_CODE);
            }
            if inside(self.hits.next, x, y) {
                return self.key(hwnd, VK_E_CODE);
            }
            if let Some(tab) = self.hits.tabs.iter().position(|&r| inside(r, x, y)) {
                self.menu.open_tab(tab);
                return false;
            }
            if self.menu.picking {
                // A save (or the way back) in the list: a click picks it, a second one on it is Enter.
                if let Some(&(row, ..)) = self.hits.rows.iter().find(|r| inside(r.1, x, y)) {
                    if self.menu.import_at == row {
                        return self.key(hwnd, VK_RETURN_CODE);
                    }
                    self.menu.import_at = row;
                    self.menu.note.clear();
                }
                return false;
            }
            let rows = self.menu.shown_items(self.menu.tab);
            if let Some(&(row, _, less, more)) = self.hits.rows.iter().find(|r| inside(r.1, x, y)) {
                let Some(&item) = rows.get(row) else { return false };
                self.menu.at = item;
                if inside(more, x, y) {
                    self.menu.change(item, 1);
                } else if inside(less, x, y) {
                    self.menu.change(item, -1);
                }
                return false;
            }
            if let Some(a) = self.hits.actions.iter().position(|&r| inside(r, x, y)) {
                self.menu.at = ITEMS.len() + a;
                return self.key(hwnd, VK_RETURN_CODE);
            }
            return false;
        }
        if self.edit.is_some() {
            if let Some(b) = self.edit_hits.iter().position(|&r| look::Hits::inside(r, x, y)) {
                self.edit_button(hwnd, b);
            } else if (y as f32) < BANNER_H * self.home_scale() {
                // A drag of the picture begins.
                let framing = self.framing;
                if let Some(edit) = &mut self.edit {
                    edit.drag = Some((x, y, framing));
                }
                // SAFETY: our window keeps the mouse until the button is let go.
                unsafe {
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd.0);
                }
            }
            return false;
        }
        if self.dialog.is_some() {
            if let Some(b) = self.dialog_hits.iter().position(|&r| look::Hits::inside(r, x, y)) {
                if let Some(Dialog::Again(_)) = self.dialog {
                    self.dialog = Some(Dialog::Again(b));
                }
                self.dialog_answer(hwnd);
            }
            return false;
        }
        match self.spot_at(x, y) {
            Some(Spot::Play) => return self.play(),
            Some(Spot::Build) => {
                if self.menu.started != self.initial {
                    // Saved here: this start's switches are the old ones; the script starts again with the new.
                    self.exit = BUILD;
                    return true;
                }
                if built() {
                    self.dialog = Some(Dialog::Again(0));
                } else {
                    self.build_now(hwnd);
                }
            }
            Some(Spot::Settings) => {
                self.settings = true;
                self.menu.note.clear();
            }
            Some(Spot::Updates) => {
                *self.updates.lock().unwrap_or_else(|e| e.into_inner()) = "Asking GitHub ...".to_owned();
                let (answer, window) = (self.updates.clone(), hwnd.0 as isize);
                std::thread::spawn(move || {
                    let said = check_updates();
                    *answer.lock().unwrap_or_else(|e| e.into_inner()) = said;
                    // SAFETY: a message to our window; a closed window just drops it.
                    unsafe {
                        let _ = PostMessageW(Some(HWND(window as *mut _)), WM_APP, WPARAM(0), LPARAM(0));
                    }
                });
            }
            Some(Spot::GitHub) => {
                let _ = std::process::Command::new("explorer.exe").arg(format!("https://{}", REPO.to_ascii_lowercase())).spawn();
            }
            Some(Spot::Show) => {
                self.show = !self.show;
                set_shown_at_start(self.show);
            }
            Some(Spot::Close) => {
                self.exit = CLOSED;
                return true;
            }
            Some(Spot::Image) if self.picture.is_some() => self.edit = Some(Edit { before: self.framing, drag: None }),
            Some(Spot::Image) => {
                // SAFETY: a message to our own window.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), PICK_IMAGE, WPARAM(0), LPARAM(0));
                }
            }
            None => {}
        }
        false
    }

    /// Saves what changed and starts the game.
    fn play(&mut self) -> bool {
        if self.menu.values != self.menu.started && !self.menu.save() {
            self.settings = true;
            return false;
        }
        self.exit = PLAY;
        true
    }

    /// A key; whether to close the window.
    fn build_now(&mut self, hwnd: HWND) {
        start_build(&self.build, hwnd.0 as isize);
        self.dialog = Some(Dialog::Progress);
        // SAFETY: our window; a timer for the minutes shown.
        unsafe {
            SetTimer(Some(hwnd), 1, 1000, None);
        }
    }

    /// The home page's box: its picked button pressed.
    fn dialog_answer(&mut self, hwnd: HWND) {
        match self.dialog {
            Some(Dialog::Again(0)) => self.build_now(hwnd),
            Some(Dialog::Again(_)) => self.dialog = None,
            Some(Dialog::Progress) => {
                if self.build.lock().unwrap_or_else(|e| e.into_inner()).running() {
                    stop_build(&self.build);
                } else {
                    self.dialog = None;
                    // SAFETY: our window's timer.
                    unsafe {
                        let _ = KillTimer(Some(hwnd), 1);
                    }
                }
            }
            None => {}
        }
    }

    /// Back to the home page, asking first if there are changes not saved.
    fn leave_settings(&mut self) {
        if self.menu.values != self.menu.started {
            self.confirm = Some(0);
        } else {
            self.settings = false;
        }
    }

    /// The question's button picked: save (or not) and go back. A failed save stays on the page.
    fn answer(&mut self) {
        match self.confirm.take() {
            Some(0) => {
                if self.menu.save() {
                    self.settings = false;
                }
            }
            Some(_) => {
                self.menu.values = self.menu.started.clone();
                self.menu.note.clear();
                self.settings = false;
            }
            None => {}
        }
    }

    /// An edit mode's button: change, remove, reset, cancel, done.
    fn edit_button(&mut self, hwnd: HWND, button: usize) {
        match button {
            0 => {
                // SAFETY: a message to our own window.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), PICK_IMAGE, WPARAM(0), LPARAM(0));
                }
            }
            1 => {
                remove_banner();
                (self.picture, self.edit, self.framing) = (None, None, Framing::default());
                self.banner_for = None;
            }
            2 => self.framing = Framing::default(),
            3 => {
                if let Some(edit) = self.edit.take() {
                    self.framing = edit.before;
                }
            }
            _ => {
                keep_framing(self.framing);
                self.edit = None;
            }
        }
    }

    /// The mouse moved to (`x`, `y`) with a drag of the picture under way: it follows.
    fn drag_to(&mut self, x: i32, y: i32) -> bool {
        let (Some(Edit { drag: Some((x0, y0, start)), .. }), Some(picture)) = (&self.edit, &self.picture) else { return false };
        let (w, h) = (self.size.0, (BANNER_H * self.home_scale()).round() as u32);
        let k = cover_scale(picture, w, h) * start.zoom;
        let moved = Framing {
            cx: start.cx - (x - x0) as f32 / k / picture.w as f32,
            cy: start.cy - (y - y0) as f32 / k / picture.h as f32,
            ..*start
        };
        self.framing = clamped(moved, picture, w, h);
        true
    }

    /// The mouse wheel at (`x`, `y`) while editing: zoom, keeping the picture's point under the mouse.
    fn zoom_at(&mut self, x: i32, y: i32, notches: f32) {
        let Some(picture) = &self.picture else { return };
        let (w, h) = (self.size.0, (BANNER_H * self.home_scale()).round() as u32);
        let before = self.framing;
        let k0 = cover_scale(picture, w, h) * before.zoom;
        let zoom = (before.zoom * 1.15f32.powf(notches)).clamp(1.0, ZOOM_MOST);
        let k1 = cover_scale(picture, w, h) * zoom;
        let (dx, dy) = (x as f32 - w as f32 / 2.0, y as f32 - h as f32 / 2.0);
        let under = (before.cx * picture.w as f32 + dx / k0, before.cy * picture.h as f32 + dy / k0);
        let after = Framing { zoom, cx: (under.0 - dx / k1) / picture.w as f32, cy: (under.1 - dy / k1) / picture.h as f32 };
        self.framing = clamped(after, picture, w, h);
    }

    fn key(&mut self, hwnd: HWND, vk: u16) -> bool {
        if self.edit.is_some() {
            match vk {
                VK_RETURN_CODE => self.edit_button(hwnd, 4),
                VK_ESCAPE_CODE => self.edit_button(hwnd, 3),
                _ => {}
            }
            return false;
        }
        if let Some(dialog) = self.dialog {
            match (vk, dialog) {
                (0x25 | 0x27 | 0x09, Dialog::Again(b)) => self.dialog = Some(Dialog::Again(1 - b)),
                (VK_RETURN_CODE, _) => self.dialog_answer(hwnd),
                (VK_ESCAPE_CODE, Dialog::Again(_)) => self.dialog = None,
                _ => {}
            }
            return false;
        }
        if let Some(b) = self.confirm {
            match vk {
                0x25 | 0x27 | 0x09 => self.confirm = Some(1 - b),
                VK_RETURN_CODE => self.answer(),
                VK_ESCAPE_CODE => self.confirm = None,
                _ => {}
            }
            return false;
        }
        if !self.settings {
            match vk {
                VK_RETURN_CODE => return self.play(),
                VK_ESCAPE_CODE => {
                    self.exit = CLOSED;
                    return true;
                }
                VK_F1_CODE => self.settings = true,
                _ => {}
            }
            return false;
        }
        match self.menu.key(vk) {
            Some(Leave::Play) => {
                self.exit = PLAY;
                true
            }
            Some(Leave::Preload) => {
                self.exit = PRELOAD;
                true
            }
            Some(Leave::Reset) => {
                self.exit = AGAIN;
                true
            }
            Some(Leave::Close) => {
                self.leave_settings();
                false
            }
            None => false,
        }
    }
}

const VK_RETURN_CODE: u16 = 0x0D;
const VK_ESCAPE_CODE: u16 = 0x1B;
const VK_F1_CODE: u16 = 0x70;
const VK_Q_CODE: u16 = 0x51;
const VK_E_CODE: u16 = 0x45;

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
}

fn point(lparam: LPARAM) -> (i32, i32) {
    ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32)
}

extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let redraw = || {
        // SAFETY: our window.
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    };
    let close = || {
        // SAFETY: our window.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    };
    match message {
        WM_PAINT => {
            let picture = with_state(|st| (st.size, st.picture()));
            // SAFETY: painting our window with pixels of its size.
            unsafe {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                if let Some(((w, h), Some(pixels))) = picture {
                    let info = BITMAPINFO {
                        bmiHeader: BITMAPINFOHEADER {
                            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                            biWidth: w as i32,
                            biHeight: -(h as i32),
                            biPlanes: 1,
                            biBitCount: 32,
                            biCompression: BI_RGB.0,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    SetDIBitsToDevice(dc, 0, 0, w, h, 0, 0, 0, h, pixels.as_ptr().cast(), &info, DIB_RGB_COLORS);
                }
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        // Dragged by its banner (or the settings page's title).
        WM_NCHITTEST => {
            let (x, y) = point(lparam);
            let mut p = POINT { x, y };
            // SAFETY: our window, a point of ours.
            unsafe {
                let _ = ScreenToClient(hwnd, &mut p);
            }
            let caption = with_state(|st| {
                if st.settings {
                    p.y < (64.0 * st.s) as i32 && !st.on_back(p.x, p.y)
                } else {
                    st.edit.is_none() && p.y < (BANNER_H * st.home_scale()) as i32 && st.spot_at(p.x, p.y).is_none()
                }
            });
            if caption == Some(true) { LRESULT(HTCAPTION as isize) } else { LRESULT(HTCLIENT as isize) }
        }
        WM_MOUSEMOVE => {
            let (x, y) = point(lparam);
            if with_state(|st| {
                st.mouse = (x, y);
                if st.drag_to(x, y) {
                    return true;
                }
                let spot = if st.settings { None } else { st.spot_at(x, y) };
                let back = st.on_back(x, y);
                (std::mem::replace(&mut st.hover, spot) != spot) | (std::mem::replace(&mut st.hover_back, back) != back)
            }) == Some(true)
            {
                redraw();
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let (hand, moves) = with_state(|st| {
                let on_buttons = st.edit_hits.iter().any(|&r| look::Hits::inside(r, st.mouse.0, st.mouse.1));
                let moves = st.edit.is_some() && !on_buttons && (st.mouse.1 as f32) < BANNER_H * st.home_scale();
                (st.hover.is_some() || st.hover_back || (st.edit.is_some() && on_buttons), moves)
            })
            .unwrap_or((false, false));
            // SAFETY: the system's cursors.
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, if moves { IDC_SIZEALL } else if hand { IDC_HAND } else { IDC_ARROW }) {
                    SetCursor(Some(cursor));
                }
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = point(lparam);
            if with_state(|st| st.click(hwnd, x, y)) == Some(true) {
                close();
            } else {
                redraw();
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if with_state(|st| st.edit.as_mut().and_then(|e| e.drag.take()).is_some()) == Some(true) {
                // SAFETY: the mouse given back.
                unsafe {
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            let (x, y) = point(lparam);
            let mut p = POINT { x, y };
            // SAFETY: our window, a point of ours.
            unsafe {
                let _ = ScreenToClient(hwnd, &mut p);
            }
            with_state(|st| {
                if st.settings && st.menu.picking {
                    let _ = st.menu.key(if delta > 0 { 0x26 } else { 0x28 });
                } else if st.settings {
                    st.menu.step(if delta > 0 { -1 } else { 1 });
                } else if st.edit.is_some() {
                    st.zoom_at(p.x, p.y, delta as f32 / 120.0);
                }
            });
            redraw();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if with_state(|st| st.key(hwnd, wparam.0 as u16)) == Some(true) {
                close();
            } else {
                redraw();
            }
            LRESULT(0)
        }
        PICK_IMAGE => {
            if pick_banner(hwnd) {
                // Read again, framed anew, and edited at once.
                with_state(|st| {
                    (st.picture_read, st.banner_for, st.framing) = (false, None, Framing::default());
                    st.edit = Some(Edit { before: Framing::default(), drag: None });
                });
            }
            redraw();
            LRESULT(0)
        }
        WM_APP | WM_TIMER => {
            redraw();
            LRESULT(0)
        }
        WM_DESTROY => {
            // A build still running ends with the window (what it made is kept).
            with_state(|st| {
                if st.build.lock().unwrap_or_else(|e| e.into_inner()).running() {
                    stop_build(&st.build);
                }
            });
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the default handling of our window's messages.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Shows the launcher until something is picked; the exit code for it.
pub fn run() -> i32 {
    // SAFETY: plain window calls on this thread, which owns the window and its loop.
    unsafe {
        let _ = SetProcessDPIAware();
        let screen = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        // About three fifths of the screen's height.
        let s = (screen.1 as f32 * 0.6 / 700.0).clamp(0.6, 3.0);
        let size = look::size(s);
        let state = State {
            menu: Menu::for_launcher(),
            settings: false,
            hover: None,
            hover_back: false,
            hits: look::Hits::default(),
            show: shown_at_start(),
            version: Arc::new(Mutex::new(String::new())),
            updates: Arc::new(Mutex::new(String::new())),
            banner: None,
            banner_for: None,
            picture: None,
            picture_read: false,
            framing: Framing::default(),
            edit: None,
            edit_hits: Vec::new(),
            mouse: (0, 0),
            size,
            s,
            exit: CLOSED,
            confirm: None,
            confirm_hits: Vec::new(),
            initial: Vec::new(),
            dialog: None,
            dialog_hits: Vec::new(),
            build: Arc::new(Mutex::new(Building::default())),
        };
        let mut state = state;
        state.initial = state.menu.started.clone();
        let version = state.version.clone();
        std::thread::spawn(move || {
            let said = git(&["log", "-1", "--format=%cs  %h"]).map_or_else(|| format!("Ver. {}", env!("CARGO_PKG_VERSION")), |v| format!("Ver. {v}"));
            *version.lock().unwrap_or_else(|e| e.into_inner()) = said;
        });
        STATE.with(|s| *s.borrow_mut() = Some(state));

        let Ok(module) = GetModuleHandleW(None) else { return PLAY };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            lpszClassName: w!("VictoryLauncher"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let (w, h) = (size.0 as i32, size.1 as i32);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("VictoryLauncher"),
            w!("VIctory"),
            WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX,
            (screen.0 - w) / 2,
            (screen.1 - h) / 2,
            w,
            h,
            None,
            None,
            Some(module.into()),
            None,
        ) else {
            // No window: the game starts as it did before the launcher.
            return PLAY;
        };
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        // Started again to build with settings saved in the last window.
        if super::get("VICTORY_LAUNCHER_BUILD") == "1" {
            with_state(|st| st.build_now(hwnd));
        }
        // Redrawn once the version line is in.
        let window = hwnd.0 as isize;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let _ = PostMessageW(Some(HWND(window as *mut _)), WM_APP, WPARAM(0), LPARAM(0));
        });
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    STATE.with(|s| s.borrow().as_ref().map_or(CLOSED, |st| st.exit))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preload's own lines move the bar through its three steps.
    #[test]
    fn reads_preload_lines() {
        let mut b = Building::default();
        b.read("12000 shaders in the cache, 52 s");
        assert_eq!((b.stage, b.count), (0, 12000));
        b.read("25760 shaders in the cache, 225 do not translate, 56 s");
        assert_eq!(b.stage, 1);
        b.read("88568 pipelines known, 35062 made before (abc), 3 bad; 53506 to make, 10 processes");
        assert_eq!((b.stage, b.total), (1, 53506));
        b.read("pipeline 77 abc: refused");
        b.read("23000 of 53506, 401 s");
        assert_eq!(b.count, 23000);
        let middle = b.fraction();
        assert!(middle > 0.4 && middle < 0.5, "{middle}");
        b.read("into the pipeline library: 5484 pipelines (loading and slow ones), 10 threads");
        b.read("2500 of 5484 in the library, 300 s");
        assert_eq!((b.stage, b.count, b.total), (2, 2500, 5484));
        b.read("5481 of 5484 in the pipeline library in 673 s");
        assert_eq!(b.stage, 3);
        assert!((b.fraction() - 1.0).abs() < f32::EPSILON);
    }

    /// With `VICTORY_LAUNCHER_PREVIEW=<path>`: the home page as `<path>.bmp`, to look at without a window.
    #[test]
    fn preview() {
        let Some(path) = std::env::var_os("VICTORY_LAUNCHER_PREVIEW") else { return };
        let mut menu = Menu::for_launcher();
        menu.set("VICTORY_MENU_ULTRAWIDE", "1");
        menu.set("VICTORY_MENU_RES", "3440");
        menu.set("VICTORY_MENU_DLSS", "performance");
        menu.set("VICTORY_MENU_FRAMEGEN", "4");
        let s = 1.27;
        let mut st = State {
            menu,
            settings: false,
            hover: Some(Spot::Play),
            hover_back: false,
            hits: look::Hits::default(),
            show: true,
            version: Arc::new(Mutex::new("Ver. 2026-11-27  7b367fb".to_owned())),
            updates: Arc::new(Mutex::new("Up to date".to_owned())),
            banner: None,
            banner_for: None,
            picture: None,
            picture_read: false,
            framing: Framing::default(),
            edit: None,
            edit_hits: Vec::new(),
            mouse: (0, 0),
            size: look::size(s),
            s,
            exit: CLOSED,
            confirm: None,
            confirm_hits: Vec::new(),
            initial: Vec::new(),
            dialog: None,
            dialog_hits: Vec::new(),
            build: Arc::new(Mutex::new(Building::default())),
        };
        let (w, h) = st.size;
        let write = |name: &str, pixels: &[u32]| {
            let mut bmp = Vec::new();
            bmp.extend(b"BM");
            for v in [54 + pixels.len() as u32 * 4, 0, 54, 40, w, (h as i32).wrapping_neg() as u32] {
                bmp.extend(v.to_le_bytes());
            }
            bmp.extend(1u16.to_le_bytes());
            bmp.extend(32u16.to_le_bytes());
            bmp.extend([0u8; 24]);
            for p in pixels {
                bmp.extend(p.to_le_bytes());
            }
            std::fs::write(format!("{}{name}.bmp", path.to_string_lossy()), bmp).unwrap();
        };
        write("", &st.home().expect("drawn"));
        // Editing a picture (stripes of our own).
        st.picture = Some(Picture { w: 800, h: 400, pixels: (0..800 * 400).map(|i| if (i % 800 / 40 + i / 800 / 40) % 2 == 0 { 0x00C0_6090 } else { 0x0030_4080 }).collect() });
        st.picture_read = true;
        st.framing = Framing { zoom: 2.0, cx: 0.3, cy: 0.5 };
        st.edit = Some(Edit { before: Framing::default(), drag: None });
        write("-edit", &st.home().expect("drawn"));
        st.edit = None;
        // And with a build under way.
        st.dialog = Some(Dialog::Progress);
        {
            let mut b = st.build.lock().unwrap();
            b.began = Some(Instant::now());
            b.read("53506 pipelines known, 0 made before (x), 0 bad; 53506 to make, 10 processes");
            b.read("23000 of 53506, 401 s");
        }
        write("-build", &st.home().expect("drawn"));
    }
}
