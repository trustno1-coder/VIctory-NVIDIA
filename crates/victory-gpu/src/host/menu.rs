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
    GetAsyncKeyState, VK_DOWN, VK_ESCAPE, VK_F1, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::font::{ASCII_WIDTH, TextBox, colours, glyph, supports_chinese, width as text_width};
use super::{Host, HostList, transition};

const COLUMNS: u32 = 60;
/// Lines of the box: a title, a help line, a blank, the settings shown, a
/// blank, what the line picked does (`ABOUT`, else blank), a note and the
/// repository's link.
const ROWS: usize = 20;
const LINES: u32 = ROWS as u32 + 9;
/// The action that preloads shaders, and what is said of it while it is picked.
const PRELOAD: usize = 2;
const ABOUT_PRELOAD: [Labels; 3] = [
    ("MAKES EVERY SHADER AHEAD, THEN STARTS THE GAME AGAIN.", "提前编译全部着色器，然后重新启动游戏。"),
    ("TAKES A WHILE: MAYBE A LONG TIME, DEPENDING ON HARDWARE.", "处理时间取决于硬件，可能需要等很久。"),
    ("A LATER UPDATE OF THIS BUILD MAY NEED A NEW PRELOAD.", "兼容层更新后可能需要重新预加载。"),
];
/// The actions that export and import the game's save (`crate::save_share`), and what is said
/// of them while they are picked.
const EXPORT: usize = 4;
const IMPORT: usize = 5;
const ABOUT_EXPORT: [Labels; 3] = [
    ("MAKES A ZIP OF YOUR SAVE IN SAVES, IN THE FOLDER OF", "把存档打包为 ZIP，放入游戏目录的 SAVES 文件夹。"),
    ("GAME_SCARLETT_MASTER.EXE. SEND THAT ZIP TO SHARE IT.", "可把 ZIP 发给其他人分享。"),
    ("YOUR SAVE STAYS AS IT IS. GAME SETTINGS ARE LEFT OUT.", "原存档保持原样，ZIP 不包含游戏设置。"),
];
const ABOUT_IMPORT: [Labels; 3] = [
    ("PUT A SAVE ZIP INTO SAVES, IN THE FOLDER OF", "把存档 ZIP 放入游戏目录的 SAVES 文件夹。"),
    ("GAME_SCARLETT_MASTER.EXE, THEN PRESS ENTER HERE AND", "在此按回车，再从列表中选择存档。"),
    ("PICK IT FROM THE LIST. YOUR GAME SETTINGS ARE KEPT.", "导入时保留你的游戏设置。"),
];
/// The list IMPORT SAVE opens: the saves in `Saves`, then a line back to the settings.
const PICK_TITLE: Labels = ("IMPORT SAVE                       ESC GOES BACK", "导入存档                   ESC 返回");
const PICK_HELP: Labels = ("UP/DOWN PICK  ENTER TWICE IMPORTS IT", "上下选择    按两次回车导入");
const PICK_NONE: Labels = ("  NONE YET: PUT A SAVE ZIP INTO SAVES IN THE GAME FOLDER", "  暂无存档。请把存档 ZIP 放入游戏目录的 SAVES");
const PICK_BACK: Labels = ("BACK TO THE SETTINGS", "返回设置");
const ABOUT_PICK: [Labels; 3] = [
    ("THE SAVE ZIPS IN SAVES, IN THE GAME FOLDER, NEWEST FIRST.", "下列 ZIP 位于游戏目录的 SAVES，最新的排在最前。"),
    ("ENTER TWICE: THE GAME RESTARTS WITH THE SAVE PICKED.", "按两次回车后，游戏将使用所选存档重启。"),
    ("YOUR OLD SAVE IS ZIPPED INTO SAVES FIRST, SO IT IS KEPT.", "旧存档会先打包备份到 SAVES。"),
];
/// The note after the first Enter on a save in the list: the second one imports it.
const CONFIRM_IMPORT: Labels = ("ENTER AGAIN: IMPORT IT AND RESTART THE GAME", "再次按回车：导入存档并重启游戏");
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
const ABOUT_MFG: [Labels; 3] = [
    ("A MOD, NOT OURS: DLSS FRAME GEN ON RTX 20 AND 30 CARDS.", "第三方模组，可让 RTX 20 和 30 系列使用 DLSS 帧生成。"),
    ("ON: DOWNLOADED FROM ITS GITHUB AT THE NEXT START, THEN RUN.", "开启后会在下次启动时从其 GITHUB 下载并加载。"),
    ("NVIDIA DOES NOT SUPPORT IT. IF THE GAME FAILS, TURN IT OFF.", "NVIDIA 未提供支持。游戏异常时请关闭它。"),
];
/// The DLSS setting, and what is said while it is picked on in a start without NVIDIA's files.
const DLSS: &str = "VICTORY_MENU_DLSS";
const ABOUT_NO_DLSS: [Labels; 3] = [
    ("THE NVIDIA DLSS FILES ARE NOT ON THIS PC YET: THEY ARE", "本机还没有 NVIDIA DLSS 文件。"),
    ("DOWNLOADED FROM NVIDIA ON GITHUB AT THE NEXT START", "下次启动时会从 NVIDIA 的 GITHUB 下载。"),
    ("(ABOUT 70 MB, ONCE). SAVE AND RESTART NOW DOES IT.", "约 70 MB，只下载一次。可选保存并立即重启。"),
];
/// Frame generation that this run did not start with: it is made ready only at a start with it
/// on (a start with it ready but off held some PCs at 14 fps, #515, #574), so turning it on takes
/// a restart; a run started with it on changes it, or turns it off, live.
const ABOUT_FRAMEGEN_RESTART: [Labels; 3] = [
    ("RESTART REQUIRED TO ENABLE FRAME GENERATION: THIS RUN", "本次启动未启用帧生成，开启需重启游戏。"),
    ("STARTED WITHOUT IT. SAVE AND RESTART NOW TURNS IT ON.", "选择保存并立即重启，即可启用帧生成。"),
    ("AFTER THAT IT CHANGES, OR TURNS OFF, LIVE.", "重启后可即时调整或关闭帧生成。"),
];
const RESTART_FRAMEGEN: Labels = ("RESTART REQUIRED TO ENABLE FRAME GENERATION", "重启游戏后才能开启帧生成");
const FRAMEGEN: &str = "VICTORY_MENU_FRAMEGEN";
/// The note after the first Enter on it: the second one does it.
const CONFIRM_PRELOAD: Labels = ("ENTER AGAIN: SAVE, CLOSE THE GAME AND PRELOAD", "再次按回车：保存设置、退出游戏并预加载");
/// The last line of the box: where this build comes from.
const REPO: &str = "GITHUB.COM/TRUSTNO1-CODER/VICTORY-NVIDIA";
const BOX: TextBox = TextBox { columns: COLUMNS, lines: LINES, rim: 4 };
const FPS_BOX: TextBox = TextBox { columns: 38, lines: 1, rim: 4 };
const UPLOADS: usize = 4;

const LANGUAGE: &str = "VICTORY_MENU_LANGUAGE";
type Labels = (&'static str, &'static str);

fn label((english, chinese): Labels, use_chinese: bool) -> &'static str {
    if use_chinese { chinese } else { english }
}

const fn same(value: &'static str) -> Labels {
    (value, value)
}

fn chinese_requested() -> &'static AtomicBool {
    static CHINESE: OnceLock<AtomicBool> = OnceLock::new();
    CHINESE.get_or_init(|| AtomicBool::new(get(LANGUAGE).eq_ignore_ascii_case("zh-CN")))
}

pub(super) fn is_chinese() -> bool {
    chinese_requested().load(Ordering::Relaxed) && supports_chinese()
}

/// A setting: the variable, its name in the menu, the values it takes
/// (`""`: not set, the build's own default), and whether a change applies at once.
struct Item {
    var: &'static str,
    label: Labels,
    values: &'static [(&'static str, Labels)],
    live: bool,
}

const ON: Labels = ("ON", "开启");
const OFF: Labels = ("OFF", "关闭");
const NONE: Labels = ("NONE", "无限制");
const ON_UNSET: &[(&str, Labels)] = &[("1", ON), ("", OFF)];
const UNSET_ON: &[(&str, Labels)] = &[("", OFF), ("1", ON)];
const STAGES: &[(&str, Labels)] = &[("2", same("2")), ("1", same("1")), ("0", ("0 OFF", "0 关闭"))];

/// The first value of each is what the launcher starts with.
#[rustfmt::skip]
const ITEMS: &[Item] = &[
    Item { var: LANGUAGE, label: ("LANGUAGE", "界面语言"), values: &[("en", same("ENGLISH")), ("zh-CN", ("SIMPLIFIED CHINESE", "简体中文"))], live: true },
    // The launcher's own choices.
    Item { var: DLSS, label: same("DLSS"), values: &[("quality", ("QUALITY", "质量")), ("balanced", ("BALANCED", "平衡")), ("performance", ("PERFORMANCE", "性能")), ("dlaa", ("DLAA (NATIVE)", "DLAA (原生分辨率)")), ("off", OFF)], live: false },
    Item { var: FRAMEGEN, label: ("FRAME GENERATION", "帧生成"), values: &[("0", OFF), ("2", same("2X")), ("3", same("3X")), ("4", same("4X")), ("5", same("5X")), ("6", ("6X (MOST)", "6X (最高)"))], live: false },
    // DLSS Frame Generation where it runs (RTX 40 and later), else AMD's FSR 3 (2X only).
    Item { var: "VICTORY_MENU_FG_TYPE", label: ("FRAME GEN TYPE", "帧生成类型"), values: &[("auto", ("AUTO", "自动")), ("dlss", same("DLSS")), ("fsr", ("FSR 3 (OLDER CARDS)", "FSR 3 (旧显卡)"))], live: false },
    // Someone else's mod (apps\victory\src\play.rs): downloaded at the next start, then loaded.
    Item { var: MFG, label: ("MFG UNLOCKER (MOD, RTX 20/30)", "MFG 解锁模组 (RTX 20/30)"), values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_FPS", label: ("FRAME RATE CAP", "帧率上限"), values: &[("1000", NONE), ("120", same("120")), ("60", same("60")), ("30", ("30 (THE GAME)", "30 (游戏默认)"))], live: false },
    Item { var: "VICTORY_FPS_LINE", label: ("FPS AT THE TOP", "顶部帧率"), values: &[("1", ON), ("0", OFF)], live: true },
    Item { var: "VICTORY_PANEL", label: ("STATS PANEL (F3)", "性能统计 (F3)"), values: &[("0", OFF), ("1", ON)], live: true },
    Item { var: "VICTORY_FULLSCREEN", label: ("FULLSCREEN (NO BORDER)", "无边框全屏"), values: UNSET_ON, live: false },
    Item { var: "VICTORY_MOUSE_CAPTURE", label: ("MOUSE CAPTURE", "锁定鼠标"), values: UNSET_ON, live: false },
    Item { var: "VICTORY_IME_ENGLISH", label: ("ENGLISH KEYBOARD", "使用英文键盘"), values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_CONSOLE", label: ("CONSOLE", "主机型号"), values: &[("series-x", same("SERIES X")), ("series-s", ("SERIES S (FASTER)", "SERIES S (更快)"))], live: false },
    Item { var: "VICTORY_MENU_RES", label: ("SERIES X PICTURE", "SERIES X 分辨率"), values: &[("1440", same("2560X1440")), ("1080to1440", ("1080P TO 1440P", "1080P 升至 1440P")), ("4k", same("3840X2160"))], live: false },
    // The layer's switches.
    Item { var: "VICTORY_DLSS_PRESET", label: ("DLSS MODEL", "DLSS 模型"), values: &[("", ("DEFAULT", "默认")), ("k", same("K")), ("j", same("J")), ("m", same("M")), ("l", same("L"))], live: false },
    Item { var: "VICTORY_AMBIENT", label: ("LIGHT ESTIMATE", "环境光估算"), values: &[("0.8", same("0.8")), ("1", same("1")), ("0.5", same("0.5")), ("", OFF)], live: false },
    Item { var: "VICTORY_AMBIENT_SPECULAR", label: ("REFLECTION ESTIMATE", "反射估算"), values: ON_UNSET, live: false },
    Item { var: "VICTORY_AMBIENT_AIR", label: ("FOG CAP", "雾效强度上限"), values: &[("1.5", same("1.5")), ("1", same("1")), ("2", same("2")), ("", NONE)], live: false },
    Item { var: "VICTORY_BVH_BUILDER", label: ("RAY TRACING (SLOW, BROKEN)", "光线追踪 (慢且有故障)"), values: UNSET_ON, live: false },
    // On unless "0": a file saved by an earlier version holds an empty value (its OFF) or "1",
    // and both start it on, so nobody keeps it off from before by accident (2026-11-27).
    Item { var: "VICTORY_WATER_LIGHT", label: ("WATER LIGHT", "水面光照"), values: &[("", ON), ("0", OFF)], live: false },
    Item { var: "VICTORY_LEAVE_OUT", label: ("HEAT HAZE AND DECAL DEFRAG", "热浪和贴花整理"), values: &[("02384a60fda6f2d0,731a39171b568298", ("LEFT OUT", "跳过")), ("", ("RUN (SLOWER, SMEARS)", "开启 (慢且有拖影)"))], live: false },
    Item { var: "VICTORY_EARLY_DEPTH", label: ("EARLY DEPTH TEST", "提前深度测试"), values: ON_UNSET, live: false },
    Item { var: "VICTORY_WC_NO_BACK", label: ("SKIP BIG COPY-BACKS", "跳过大块数据回拷"), values: ON_UNSET, live: false },
    Item { var: "VICTORY_SOFT_VIEWS", label: ("SOFT VIEWS (NO CUT FREEZES)", "软视图 (减少转场卡顿)"), values: &[("copies", ON), ("", OFF)], live: false },
    Item { var: "VICTORY_TEXTURE_NO_WAIT", label: ("TEXTURE NO WAIT", "纹理异步加载"), values: ON_UNSET, live: false },
    Item { var: "VICTORY_RELAY", label: ("RELAY (SPEED, TRIANGLES)", "GPU 中继 (速度、三角面)"), values: STAGES, live: false },
    Item { var: "VICTORY_CARD_STEPS", label: ("CARD STEPS", "GPU 处理阶段"), values: STAGES, live: false },
    Item { var: "VICTORY_INDIRECT_BATCH", label: ("INDIRECT BATCH", "间接绘制批处理"), values: STAGES, live: false },
    Item { var: "VICTORY_CARD_BUFFERS_MIN_KB", label: ("CARD COMPARES FROM (KB)", "GPU 缓冲区比较阈值 (KB)"), values: &[("64", same("64")), ("1024", same("1024"))], live: false },
    Item { var: "VICTORY_PORT_CONSTANTS", label: ("CONSTANTS HEAP IN PLACE", "原地更新常量缓冲区"), values: UNSET_ON, live: false },
    // THIN records the title's list calls on our threads. On an RTX 5080 with a 6-core CPU heads did not
    // draw with it and frames were slower (14 a second with it, 21 to 28 without): switches for it and for
    // what goes with it. Frame generation needs the line of submissions: without THIN, ASYNC SUBMIT gives it.
    Item { var: "VICTORY_THIN", label: ("THIN LISTS (HEADS MAY BE MISSING)", "THIN 列表 (人物头部可能不显示)"), values: ON_UNSET, live: false },
    Item { var: "VICTORY_JOIN_PARTS", label: ("JOIN PARTS", "合并提交分段"), values: &[("8", same("8")), ("", ("1 (OFF)", "1 (关闭)"))], live: false },
    Item { var: "VICTORY_ASYNC_SUBMIT", label: ("ASYNC SUBMIT (FRAME GEN WITHOUT THIN)", "异步提交 (关闭 THIN 时用于帧生成)"), values: UNSET_ON, live: false },
];
/// The lines after the settings.
const ACTIONS: [Labels; 7] = [
    ("SAVE AND RESTART NOW", "保存并立即重启"),
    ("SAVE FOR NEXT START", "保存设置"),
    ("PRELOAD SHADERS (RESTARTS THE GAME)", "预加载着色器 (重启游戏)"),
    ("RESET ALL TO PLAY DEFAULTS", "恢复启动默认设置"),
    ("EXPORT SAVE (TO SAVES IN THE GAME FOLDER)", "导出存档 (到游戏目录的 SAVES)"),
    ("IMPORT SAVE (PICK FROM SAVES)", "导入存档 (从 SAVES 选择)"),
    ("CLOSE", "关闭"),
];

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
pub(super) fn warmup_notice(seconds: u64, chinese: bool) -> Vec<String> {
    let tr = |text| label(text, chinese).to_owned();
    vec![
        tr(("FIRST START OF THIS VERSION: A ONE-TIME WARM-UP", "首次启动此版本，游戏将进行一次预热")),
        if chinese { format!("进入游戏 {seconds} 秒后，游戏将自动重启") } else { format!("THE GAME RESTARTS BY ITSELF {seconds} SECONDS AFTER IT") },
        tr(("LOADS INTO THE GAME (PRESS A AT THE TITLE SCREEN)", "请在标题画面按 A 进入游戏")),
        tr(("THIS HAPPENS ONCE PER NEW VERSION, AND IS NEEDED:", "每个新版本只需预热一次")),
        tr(("WITHOUT IT THE FIRST RUN STAYS AT ABOUT 14 FPS", "预热前首次运行约为 14 帧")),
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
fn fps_line(chinese: bool) -> Option<String> {
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
    let rate = label(("FPS", "帧率"), chinese);
    let per = crate::streamline::shown_per_frame();
    Some(if per == 1 {
        format!("{rate}: {fps:.1}")
    } else {
        let displayed = label(("FG", "显示帧率"), chinese);
        format!("{rate}: {fps:.1}  {displayed}: {:.1}", fps * f64::from(per))
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
fn shown_value(item: &Item, value: &str, chinese: bool) -> String {
    match index_of(item, value) {
        Some(i) => label(item.values[i].1, chinese).to_owned(),
        None if value.is_empty() => label(("NOT SET", "未设置"), chinese).to_owned(),
        None => value.to_ascii_uppercase(),
    }
}

/// Keys pressed, gathered on a thread of their own every 10 ms: a frame
/// lasts 50 ms and more, and a short press would fall between two frames.
fn keys() -> &'static Mutex<Vec<u16>> {
    static KEYS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let _ = std::thread::Builder::new().name("settings menu keys".into()).spawn(|| {
            const WATCHED: [u16; 7] = [VK_F1, VK_UP, VK_DOWN, VK_LEFT, VK_RIGHT, VK_RETURN, VK_ESCAPE];
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

struct Buffers {
    shape: TextBox,
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
    fn new(host: &Host, shape: TextBox, format: DXGI_FORMAT, chinese: bool, scale: u32) -> Option<Buffers> {
        let (width, height) = shape.size(chinese, scale);
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
        Some(Buffers { shape, format, scale, pitch, uploads, card: card?, turn: 0, fresh: false, used: false })
    }

    fn write(&mut self, lines: &[String], chinese: bool) {
        let Some(colours) = colours(self.format) else { return };
        let picture = self.shape.paint(lines, chinese, self.scale, colours, self.pitch as usize);
        self.turn = (self.turn + 1) % UPLOADS;
        // SAFETY: a mapped upload buffer of the picture's size, which the card read frames ago.
        unsafe { std::ptr::copy_nonoverlapping(picture.as_ptr(), self.uploads[self.turn].1, picture.len()) };
        self.fresh = true;
    }
}

enum Note {
    Text(Labels),
    Exported(String),
}

/// The menu of a presenter.
pub(super) struct Menu {
    /// The values chosen, by item (as at start until changed).
    values: Vec<String>,
    /// The values at start.
    started: Vec<String>,
    /// The line picked: an item, or an action after them.
    at: usize,
    note: Option<Note>,
    changed: bool,
    /// What the buffer holds: the frame-rate line, or (none) the menu's lines.
    painted: Option<String>,
    buffers: [Option<Buffers>; 2],
    fps_buffers: [Option<Buffers>; 2],
    failed: bool,
    /// The saves in `Saves` that can be imported (read when the menu opens), and the line
    /// picked in their list (`imports.len()`: the line back), shown while `picking`.
    imports: Vec<String>,
    import_at: usize,
    picking: bool,
}

impl Menu {
    pub(super) fn new() -> Menu {
        // Start the key thread now: F1 must be seen while the menu is closed.
        let _ = keys();
        // `VICTORY_MENU_OPEN=1`: shown from the start (to look at it in a run nobody presses keys in).
        if std::env::var_os("VICTORY_MENU_OPEN").is_some_and(|v| v == "1") {
            crate::SETTINGS_MENU_OPEN.store(true, Ordering::Relaxed);
        }
        let values: Vec<String> = ITEMS.iter().map(|i| {
            if i.var == LANGUAGE {
                label(("en", "zh-CN"), chinese_requested().load(Ordering::Relaxed)).to_owned()
            } else {
                get(i.var)
            }
        }).collect();
        Menu { started: values.clone(), values, at: 0, note: None, changed: true, painted: None, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false }
    }

    fn say(&mut self, text: Labels) {
        self.note = Some(Note::Text(text));
    }

    fn note_text(&self, chinese: bool) -> Option<String> {
        Some(match self.note.as_ref()? {
            Note::Text(text) => label(*text, chinese).to_owned(),
            Note::Exported(name) => format!("{}: {name}.ZIP", label(("EXPORTED TO SAVES", "已导出到 SAVES"), chinese)),
        })
    }

    fn open() -> bool {
        crate::SETTINGS_MENU_OPEN.load(Ordering::Relaxed)
    }

    fn lines(&self, chinese: bool) -> Vec<String> {
        if self.picking {
            return self.pick_lines(chinese);
        }
        let tr = |labels| label(labels, chinese);
        let total = ITEMS.len() + ACTIONS.len();
        let first = self.at.saturating_sub(ROWS / 2).min(total.saturating_sub(ROWS));
        let mut out = vec![
            tr(("SETTINGS                       F1 OR ESC CLOSES", "设置                     F1 / ESC 关闭")).to_owned(),
            tr(("UP/DOWN PICK  LEFT/RIGHT CHANGE  ENTER DOES IT", "上下选择    左右修改    回车确认")).to_owned(),
            String::new(),
        ];
        for row in first..(first + ROWS).min(total) {
            let cursor = if row == self.at { '>' } else { ' ' };
            let line = if let Some(item) = ITEMS.get(row) {
                let value = &self.values[row];
                let shown = shown_value(item, value, chinese);
                let mark = match (*value != self.started[row], live(item)) {
                    (true, true) => tr((" *LIVE", " *即时")),
                    (true, false) => " *",
                    (false, true) => tr((" LIVE", " 即时")),
                    (false, false) => "",
                };
                let name = tr(item.label);
                let padding = (27 * ASCII_WIDTH).saturating_sub(text_width(name)) / ASCII_WIDTH;
                format!("{cursor} {name}{:width$} < {shown} >{mark}", "", width = padding as usize)
            } else {
                format!("{cursor} {}", tr(ACTIONS[row - ITEMS.len()]))
            };
            out.push(line);
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
            [same(""); 3]
        };
        out.extend(about.map(|line| tr(line).to_owned()));
        out.push(self.note_text(chinese).unwrap_or_else(|| {
            if !chinese && chinese_requested().load(Ordering::Relaxed) {
                "CHINESE FONTS NOT FOUND IN WINDOWS".to_owned()
            } else {
                tr(("* = CHANGED: APPLIES AT THE NEXT START UNLESS LIVE",
                    "* 已修改：即时项立即生效，其余下次启动生效")).to_owned()
            }
        }));
        out.push(REPO.to_owned());
        out
    }

    /// The list IMPORT SAVE opens, in the box's lines.
    fn pick_lines(&self, chinese: bool) -> Vec<String> {
        let tr = |text| label(text, chinese).to_owned();
        let mut out = vec![tr(PICK_TITLE), tr(PICK_HELP), String::new()];
        if self.imports.is_empty() {
            out.push(tr(PICK_NONE));
        }
        let rows = ROWS - (out.len() - 3);
        let total = self.imports.len() + 1;
        let first = self.import_at.saturating_sub(rows / 2).min(total.saturating_sub(rows));
        for row in first..(first + rows).min(total) {
            let cursor = if row == self.import_at { '>' } else { ' ' };
            let name = self.imports.get(row).map_or_else(|| tr(PICK_BACK), |n| shown_name(n));
            out.push(format!("{cursor} {name}").chars().take(COLUMNS as usize).collect());
        }
        while out.len() < 3 + ROWS {
            out.push(String::new());
        }
        out.push(String::new());
        out.extend(ABOUT_PICK.map(tr));
        out.push(self.note_text(chinese).unwrap_or_else(|| {
            let n = self.imports.len();
            if chinese { format!("SAVES 中有 {n} 个存档") } else { format!("{n} IN SAVES") }
        }));
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
        if live(item) {
            self.apply_live(i);
        }
        self.note = None;
        if item.var == FRAMEGEN && !live(item) && !matches!(self.values[i].as_str(), "" | "0") {
            self.say(RESTART_FRAMEGEN);
        }
    }

    /// A setting the build reads at each use: changed in this run's environment at once.
    fn apply_live(&self, i: usize) {
        let (var, value) = (ITEMS[i].var, &self.values[i]);
        if var == LANGUAGE {
            chinese_requested().store(value == "zh-CN", Ordering::Relaxed);
            return;
        }
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
            self.say(("NO FOLDER FOR THE SETTINGS FILE", "找不到保存设置的目录"));
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
            // The live language can outlast a presenter recreated before Save.
            if self.values[i] == self.started[i] && item.var != LANGUAGE {
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
                self.say(if std::env::var_os("VICTORY_MENU_FILE").is_some() {
                    ("SAVED. APPLIES AT THE NEXT START", "已保存。其余设置在下次启动时生效")
                } else {
                    // Started some other way (a script, another launcher): the file victory play
                    // reads at every start, which this start did not.
                    ("SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME", "已保存。通过 VICTORY.EXE 启动游戏时生效")
                });
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "settings menu: not saved");
                self.say(("COULD NOT SAVE THE SETTINGS FILE", "无法写入设置文件"));
                false
            }
        }
    }

    /// Starts the launcher again in a new console, a few seconds on, and ends this run.
    /// `preload`: the launcher makes every shader first (`VICTORY_PRELOAD=1`, for that start alone).
    fn restart(&mut self, preload: bool) {
        let Some(play) = std::env::var_os("VICTORY_MENU_PLAY") else {
            self.say(("SAVED. START THE LAUNCHER AGAIN YOURSELF", "已保存。请手动重新运行启动器"));
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
                self.say(("SAVED, BUT THE RESTART FAILED", "已保存，重启失败"));
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
            self.say(("WARMED UP, BUT THE MARK WOULD NOT WRITE: THIS RUN GOES ON", "已预热，但无法写入标记。游戏将继续运行"));
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
            self.say(("SAVE FOLDERS NOT KNOWN IN THIS START", "本次启动找不到存档文件夹"));
            return;
        };
        let name = format!("save {}", crate::save_share::stamp());
        match crate::save_share::export(&data, &share, &name) {
            Ok(to) => {
                tracing::info!(to = %to.display(), "settings menu: save exported");
                self.note = Some(Note::Exported(shown_name(&name)));
                self.read_imports();
            }
            Err(e) => {
                tracing::warn!(error = %e, share = %share.display(), "settings menu: save not exported");
                self.say(if e.kind() == std::io::ErrorKind::NotFound {
                    ("NOTHING TO EXPORT: THE GAME HAS NOT SAVED YET", "无法导出：游戏尚未保存存档")
                } else {
                    ("EXPORT FAILED: THE GAME FOLDER CANNOT BE WRITTEN", "导出失败：无法写入游戏目录")
                });
            }
        }
    }

    /// Stages the save picked to be put in place at the next start, and starts the game again.
    fn import_save(&mut self) {
        let (Some(data), Some(share)) = (crate::save_share::data(), crate::save_share::share()) else {
            self.say(("SAVE FOLDERS NOT KNOWN IN THIS START", "本次启动找不到存档文件夹"));
            return;
        };
        let Some(name) = self.imports.get(self.import_at).cloned() else {
            self.say(("NO SAVE TO IMPORT: PUT ONE IN SAVES FIRST", "无可用存档，请先把存档 ZIP 放入 SAVES"));
            return;
        };
        if let Err(e) = crate::save_share::stage(&data, &share, &name) {
            tracing::warn!(error = %e, name, "settings menu: save import not staged");
            self.say(if e.kind() == std::io::ErrorKind::NotFound {
                ("IMPORT FAILED: NO SAVE IN THAT ZIP", "导入失败：压缩包内没有存档")
            } else {
                ("IMPORT FAILED: THAT ZIP IS DAMAGED OR NOT A SAVE", "导入失败：压缩包已损坏或不含有效存档")
            });
            return;
        }
        tracing::info!(name, "settings menu: save import staged; put in place at the next start");
        if std::env::var_os("VICTORY_MENU_PLAY").is_some() {
            self.restart(false);
        } else {
            self.say(("IMPORTED: CLOSE THE GAME AND START IT AGAIN", "已导入。请退出游戏并重新启动"));
        }
    }

    /// Removes the settings file: the launcher's own settings at the next start.
    fn reset(&mut self) {
        if let Some(path) = settings_file() {
            let _ = std::fs::remove_file(path);
        }
        self.say(("RESET. PLAY DEFAULTS AT THE NEXT START", "已重置。下次启动将使用默认设置"));
    }

    /// Changes a setting and saves it at once: many players changed settings and closed the
    /// menu without the save lines, and lost them. The save lines stay (and restart, preload).
    fn change_and_save(&mut self, i: usize, step: isize) {
        self.change(i, step);
        if self.save() && live(&ITEMS[i]) {
            self.say(("SAVED. ON NOW", "已保存，立即生效"));
        }
    }

    /// A frame was presented: the keys pressed since the last one.
    pub(super) fn frame(&mut self) {
        self.warmup();
        let pressed = std::mem::take(&mut *keys().lock().unwrap_or_else(|e| e.into_inner()));
        if pressed.is_empty() {
            return;
        }
        let total = ITEMS.len() + ACTIONS.len();
        for vk in pressed {
            let open = Self::open();
            // Esc in the list of saves goes back to the settings.
            if open && self.picking && vk == VK_ESCAPE {
                self.picking = false;
                self.note = None;
                self.changed = true;
                continue;
            }
            if vk == VK_F1 || (open && vk == VK_ESCAPE) {
                crate::SETTINGS_MENU_OPEN.store(!open && vk == VK_F1, Ordering::Relaxed);
                self.note = None;
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
            // Only an Enter right after the first one starts a preload or an import.
            let confirmed = matches!(&self.note, Some(Note::Text(CONFIRM_PRELOAD | CONFIRM_IMPORT)));
            if confirmed {
                self.note = None;
            }
            if self.picking {
                let (n, back) = (self.imports.len() + 1, self.import_at >= self.imports.len());
                match vk {
                    VK_UP => self.import_at = (self.import_at + n - 1) % n,
                    VK_DOWN => self.import_at = (self.import_at + 1) % n,
                    VK_RETURN if back => self.picking = false,
                    VK_RETURN if !confirmed => self.say(CONFIRM_IMPORT),
                    VK_RETURN => self.import_save(),
                    _ => {}
                }
                self.changed = true;
                continue;
            }
            match vk {
                VK_UP => self.at = (self.at + total - 1) % total,
                VK_DOWN => self.at = (self.at + 1) % total,
                VK_LEFT if self.at < ITEMS.len() => self.change_and_save(self.at, -1),
                VK_RIGHT if self.at < ITEMS.len() => self.change_and_save(self.at, 1),
                VK_RETURN if self.at < ITEMS.len() => self.change_and_save(self.at, 1),
                VK_RETURN => match self.at - ITEMS.len() {
                    0 => {
                        if self.save() {
                            self.restart(false);
                        }
                    }
                    1 => {
                        self.save();
                    }
                    PRELOAD if !confirmed => self.say(CONFIRM_PRELOAD),
                    PRELOAD => {
                        if self.save() {
                            self.restart(true);
                        }
                    }
                    3 => self.reset(),
                    EXPORT => self.export_save(),
                    IMPORT => {
                        self.read_imports();
                        self.import_at = 0;
                        self.picking = true;
                    }
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
        let chinese = is_chinese();
        let line = if open { None } else { fps_line(chinese) };
        if !open && line.is_none() {
            return;
        }
        // Our own copies are not part of a recorded frame (the recorder is in diagnostics builds).
        #[cfg(feature = "diagnostics")]
        let _ours = crate::replay::record::unrecorded();
        let scale = (height / 540).max(1);
        let shape = if open { BOX } else { FPS_BOX };
        let mode = usize::from(chinese);
        let storage = if open { &mut self.buffers } else { &mut self.fps_buffers };
        let buffers = &mut storage[mode];
        if buffers.as_ref().is_none_or(|b| (b.format, b.scale) != (format, scale)) {
            std::mem::forget(buffers.take());
            *buffers = Buffers::new(host, shape, format, chinese, scale);
            self.failed = buffers.is_none();
            self.changed = true;
        }
        if self.painted != line {
            self.painted = line.clone();
            self.changed = true;
        }
        let lines = if self.changed { Some(line.as_ref().map_or_else(|| self.lines(chinese), |l| vec![l.clone()])) } else { None };
        let storage = if open { &mut self.buffers } else { &mut self.fps_buffers };
        let Some(buffers) = &mut storage[mode] else { return };
        if let Some(lines) = lines {
            self.changed = false;
            buffers.write(&lines, chinese);
        }
        // The whole box in the middle, or of it the first line's part in the corner.
        let (box_width, box_height) = shape.size(chinese, scale);
        let (w, h) = line.as_ref().map_or((box_width, box_height), |l| {
            shape.area(text_width(l).min(shape.columns * ASCII_WIDTH), 1, chinese, scale)
        });
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
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: box_width, Height: box_height, Depth: 1, RowPitch: buffers.pitch },
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
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        let dlss = ITEMS.iter().position(|i| i.var == "VICTORY_MENU_DLSS").expect("in the menu");
        menu.change_and_save(dlss, 1);
        let text = std::fs::read_to_string(home.join("victory-settings.txt")).expect("saved without a save line");
        assert!(text.contains("VICTORY_MENU_DLSS=quality"), "{text}");
        assert_eq!(menu.note_text(false).as_deref(), Some("SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME"));
        // Every note fits the box and has its letters.
        for note in [menu.note_text(false).as_deref().expect("saved"), "SAVED. ON NOW", "SAVED. APPLIES AT THE NEXT START", "NO FOLDER FOR THE SETTINGS FILE"] {
            assert!(note.len() <= COLUMNS as usize, "{note}");
            assert!(note.bytes().all(|c| glyph(c) != glyph(b'#')), "{note}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_line_fits_and_every_character_has_a_picture() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        for chinese in [false, true] {
            for (at, item) in ITEMS.iter().enumerate() {
                menu.at = at;
                for (value, _) in item.values {
                    menu.values[at] = (*value).to_owned();
                    for line in menu.lines(chinese) {
                        assert!(text_width(&line) <= COLUMNS * ASCII_WIDTH, "{line}");
                        for c in line.bytes().filter(u8::is_ascii) {
                            assert_ne!(glyph(c.to_ascii_uppercase()), glyph(b'#'), "{line}: {}", c as char);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_link_is_always_right_under_the_note() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        for note in [None, Some(same("SAVED"))] {
            menu.note = note.map(Note::Text);
            for at in 0..ITEMS.len() + ACTIONS.len() {
                menu.at = at;
                let lines = menu.lines(false);
                assert_eq!(lines.len(), LINES as usize);
                let n = lines.len();
                assert!(lines[n - 2].starts_with("* = CHANGED") || note.is_some_and(|note| lines[n - 2] == note.0), "{}", lines[n - 2]);
                assert_eq!(lines[n - 1], REPO);
                // What a preload costs, and what MFG Unlocker is, is said while it is picked, and only then.
                let about = &lines[n - 5..n - 2];
                if at == ITEMS.len() + PRELOAD {
                    assert_eq!(about, ABOUT_PRELOAD.map(|line| line.0));
                } else if at == ITEMS.len() + EXPORT {
                    assert_eq!(about, ABOUT_EXPORT.map(|line| line.0));
                } else if at == ITEMS.len() + IMPORT {
                    assert_eq!(about, ABOUT_IMPORT.map(|line| line.0));
                } else if at < ITEMS.len() && ITEMS[at].var == MFG {
                    assert_eq!(about, ABOUT_MFG.map(|line| line.0));
                } else {
                    assert!(about.iter().all(String::is_empty), "{about:?}");
                }
            }
        }
        assert_eq!(ACTIONS[PRELOAD].0, "PRELOAD SHADERS (RESTARTS THE GAME)");
        assert_eq!(ACTIONS[EXPORT].0, "EXPORT SAVE (TO SAVES IN THE GAME FOLDER)");
        assert_eq!(ACTIONS[IMPORT].0, "IMPORT SAVE (PICK FROM SAVES)");
        let pick = [PICK_TITLE, PICK_HELP, PICK_NONE, PICK_BACK, CONFIRM_IMPORT, CONFIRM_PRELOAD,
            ("IMPORT FAILED: THAT ZIP IS DAMAGED OR NOT A SAVE", "导入失败：压缩包已损坏或不含有效存档")];
        for lines in [ABOUT_EXPORT, ABOUT_IMPORT, ABOUT_PICK, ABOUT_MFG, ABOUT_NO_DLSS].iter().flatten().chain(&pick) {
            for line in [lines.0, lines.1] {
                assert!(text_width(line) <= COLUMNS * ASCII_WIDTH, "{line}");
                assert!(line.bytes().filter(u8::is_ascii).all(|c| glyph(c) != glyph(b'#')), "{line}");
            }
        }
        // DLSS picked on where NVIDIA's files are missing (no VICTORY_STREAMLINE_DIR in tests): said so.
        let dlss = ITEMS.iter().position(|i| i.var == DLSS).expect("in the menu");
        menu.at = dlss;
        menu.values[dlss] = "quality".to_owned();
        for chinese in [false, true] {
            let lines = menu.lines(chinese);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_NO_DLSS.map(|line| label(line, chinese)));
        }
        menu.values[dlss] = "off".to_owned();
        let lines = menu.lines(false);
        assert!(lines[lines.len() - 5..lines.len() - 2].iter().all(String::is_empty));
        assert_eq!(ITEMS.iter().filter(|i| i.var == MFG).count(), 1);
        // Frame generation turned on after a start without it requires a restart.
        let fg = ITEMS.iter().position(|i| i.var == FRAMEGEN).expect("in the menu");
        menu.at = fg;
        menu.values[fg] = "0".to_owned();
        menu.change(fg, 1);
        assert_eq!(menu.values[fg], "2");
        for chinese in [false, true] {
            assert_eq!(menu.note_text(chinese).as_deref(), Some(label(RESTART_FRAMEGEN, chinese)));
            let lines = menu.lines(chinese);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_FRAMEGEN_RESTART.map(|line| label(line, chinese)));
        }
        menu.change(fg, -1);
        assert!(menu.note.is_none());
        let lines = menu.lines(false);
        assert!(lines[lines.len() - 5..lines.len() - 2].iter().all(String::is_empty));
    }

    #[test]
    fn values_cycle_both_ways() {
        let mut menu = Menu { values: ITEMS.iter().map(|i| i.values[0].0.to_owned()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        let i = 1; // DLSS, a setting with more than two options.
        let n = ITEMS[i].values.len();
        for _ in 0..n {
            menu.change(i, 1);
        }
        assert_eq!(menu.values[i], ITEMS[i].values[0].0);
        menu.change(i, -1);
        assert_eq!(menu.values[i], ITEMS[i].values[n - 1].0);
    }

    #[test]
    fn a_shared_save_name_is_shown_in_letters_the_menu_has() {
        let names = vec!["save 2026-11-27 23-45-10.zip".to_owned(), "Jason's boat ~ 100% [\u{e9}] and a very long name that goes on and on.zip".to_owned()];
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: ITEMS.len() + IMPORT, note: None, changed: false, painted: None, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: names, import_at: 0, picking: true };
        menu.started = menu.values.clone();
        let check = |menu: &Menu| {
            let lines = menu.lines(false);
            assert_eq!(lines.len(), LINES as usize);
            assert_eq!(lines[lines.len() - 1], REPO);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_PICK.map(|line| line.0));
            for line in &lines {
                assert!(line.len() <= COLUMNS as usize, "{line}");
                assert!(line.bytes().all(|c| glyph(c.to_ascii_uppercase()) != glyph(b'#')), "{line}");
            }
            lines
        };
        let lines = check(&menu);
        assert!(lines.iter().any(|l| l == "> SAVE 2026-11-27 23-45-10.ZIP"), "{lines:?}");
        assert!(lines.iter().any(|l| l == &format!("  {}", PICK_BACK.0)), "{lines:?}");
        menu.import_at = 1;
        check(&menu);
        menu.import_at = 2;
        assert!(check(&menu).iter().any(|l| l == &format!("> {}", PICK_BACK.0)));
        // Many saves: the list scrolls and the box keeps its size.
        menu.imports = (0..50).map(|i| format!("save {i}.zip")).collect();
        for at in [0, 25, 49, 50] {
            menu.import_at = at;
            let lines = check(&menu);
            let picked = if at == 50 { format!("> {}", PICK_BACK.0) } else { format!("> SAVE {at}.ZIP") };
            assert!(lines.contains(&picked), "{at}: {lines:?}");
        }
        menu.imports.clear();
        menu.import_at = 0;
        let lines = check(&menu);
        assert!(lines.iter().any(|l| l == PICK_NONE.0));
        assert!(lines.iter().any(|l| l == &format!("> {}", PICK_BACK.0)));
    }
}
