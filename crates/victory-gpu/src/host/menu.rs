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

use super::font::{CELL, Script, colours, painted, text_width};
use super::i18n::{self, Language, Text, plain};
use super::{Host, HostList, transition};

const COLUMNS: u32 = 62;
const LABEL_CELLS: u32 = 27;
/// Lines of the box: a title, a help line, a blank, the settings shown, a
/// blank, what the line picked does (`ABOUT`, else blank), a note and the
/// repository's link.
const ROWS: usize = 20;
const FIXED_LINES: u32 = 9;
const LINES: u32 = ROWS as u32 + FIXED_LINES;
/// The action that preloads shaders, and what is said of it while it is picked.
const PRELOAD: usize = 2;
const ABOUT_PRELOAD: [Text; 3] = [
    ["MAKES EVERY SHADER AHEAD, THEN STARTS THE GAME AGAIN.", "提前生成着色器和管线，再启动游戏。", "預先建立著色器和管線，再啟動遊戲。"],
    ["TAKES A WHILE: MAYBE A LONG TIME, DEPENDING ON HARDWARE.", "处理时间取决于硬件，可能需要很久。", "處理時間取決於硬體，可能需要很久。"],
    ["A LATER UPDATE OF THIS BUILD MAY NEED A NEW PRELOAD.", "更新运行版本后，可能需要重新预热。", "更新執行版本後，可能需要重新預熱。"],
];
/// The setting that turns MFG Unlocker on, and what is said of it while it is picked.
const MFG: &str = "VICTORY_MFG_UNLOCKER";
const ABOUT_MFG: [Text; 3] = [
    ["A MOD, NOT OURS: DLSS FRAME GEN ON RTX 20 AND 30 CARDS.", "第三方模组，为 RTX 20 / 30 开启 DLSS 插帧。", "第三方模組，為 RTX 20 / 30 開啟 DLSS 補幀。"],
    ["ON: DOWNLOADED FROM ITS GITHUB AT THE NEXT START, THEN RUN.", "开启后，下次启动会从模组的 GitHub 下载并加载。", "開啟後，下次啟動會從模組的 GitHub 下載並載入。"],
    ["NVIDIA DOES NOT SUPPORT IT. IF THE GAME FAILS, TURN IT OFF.", "NVIDIA 未提供支持。游戏无法启动时请关闭。", "NVIDIA 未提供支援。遊戲無法啟動時請關閉。"],
];
/// The note after the first Enter on it: the second one does it.
const CONFIRM_PRELOAD: Text = ["ENTER AGAIN: SAVE, CLOSE THE GAME AND PRELOAD", "再次按回车，保存设置、关闭游戏并预热", "再次按 Enter，儲存設定、關閉遊戲並預熱"];
/// The last line of the box: where this build comes from.
const REPO: &str = "GITHUB.COM/TRUSTNO1-CODER/VICTORY-NVIDIA";
const RIM: u32 = 4;
const UPLOADS: usize = 4;

/// A setting: the variable, its name in the menu, the values it takes
/// (`""`: not set, the build's own default), and whether a change applies at once.
struct Item {
    var: &'static str,
    label: Text,
    values: &'static [(&'static str, Text)],
    live: bool,
}

const ON_UNSET: &[(&str, Text)] = &[("1", ["ON", "开", "開"]), ("", ["OFF", "关", "關"])];
const UNSET_ON: &[(&str, Text)] = &[("", ["OFF", "关", "關"]), ("1", ["ON", "开", "開"])];
const STAGES: &[(&str, Text)] = &[("2", plain("2")), ("1", plain("1")), ("0", ["0 OFF", "0 关", "0 關"])];

/// The first value of each is what the launcher starts with.
#[rustfmt::skip]
const ITEMS: &[Item] = &[
    Item { var: i18n::SETTING, label: plain("LANGUAGE"), values: &[
        ("auto", ["AUTO (WINDOWS)", "自动 (WINDOWS)", "自動 (WINDOWS)"]),
        ("en", plain("ENGLISH")), ("zh-Hans", plain("简体中文")), ("zh-Hant", plain("繁體中文")),
    ], live: true },
    // The launcher's own choices.
    Item { var: "VICTORY_MENU_DLSS", label: plain("DLSS"), values: &[("quality", ["QUALITY", "质量", "畫質"]), ("balanced", ["BALANCED", "平衡", "平衡"]), ("performance", ["PERFORMANCE", "性能", "效能"]), ("dlaa", ["DLAA (NATIVE)", "DLAA (原生抗锯齿)", "DLAA (原生反鋸齒)"]), ("off", ["OFF", "关", "關"])], live: false },
    Item { var: "VICTORY_MENU_FRAMEGEN", label: ["FRAME GENERATION", "插帧倍率", "補幀倍率"], values: &[("0", ["OFF", "关", "關"]), ("2", plain("2X")), ("3", plain("3X")), ("4", plain("4X")), ("5", plain("5X")), ("6", ["6X (MOST)", "6 倍 (上限)", "6 倍 (上限)"])], live: false },
    // DLSS Frame Generation where it runs (RTX 40 and later), else AMD's FSR 3 (2X only).
    Item { var: "VICTORY_MENU_FG_TYPE", label: ["FRAME GEN TYPE", "插帧技术", "補幀技術"], values: &[("auto", ["AUTO", "自动", "自動"]), ("dlss", plain("DLSS")), ("fsr", ["FSR 3 (OLDER CARDS)", "FSR 3 (兼容旧显卡)", "FSR 3 (相容舊顯卡)"])], live: false },
    // Someone else's mod (apps\victory\src\play.rs): downloaded at the next start, then loaded.
    Item { var: MFG, label: ["MFG UNLOCKER (MOD, RTX 20/30)", "MFG 解锁模组 (RTX 20/30)", "MFG 解鎖模組 (RTX 20/30)"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_FPS", label: ["FRAME RATE CAP", "帧率上限", "幀率上限"], values: &[("1000", ["NONE", "不限", "不限"]), ("120", plain("120")), ("60", plain("60")), ("30", ["30 (THE GAME)", "30 (游戏原始上限)", "30 (遊戲原始上限)"])], live: false },
    Item { var: "VICTORY_FPS_LINE", label: ["FPS AT THE TOP", "左上角帧率", "左上角幀率"], values: &[("1", ["ON", "开", "開"]), ("0", ["OFF", "关", "關"])], live: true },
    Item { var: "VICTORY_PANEL", label: ["STATS PANEL (F3)", "性能统计面板 (F3)", "效能統計面板 (F3)"], values: &[("0", ["OFF", "关", "關"]), ("1", ["ON", "开", "開"])], live: true },
    Item { var: "VICTORY_FULLSCREEN", label: ["FULLSCREEN (NO BORDER)", "无边框全屏", "無邊框全螢幕"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_MOUSE_CAPTURE", label: ["MOUSE CAPTURE", "锁定鼠标指针", "鎖定滑鼠游標"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_IME_ENGLISH", label: ["ENGLISH KEYBOARD", "锁定英文输入", "鎖定英文輸入"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_CONSOLE", label: ["CONSOLE", "主机模式", "主機模式"], values: &[("series-x", plain("SERIES X")), ("series-s", ["SERIES S (FASTER)", "SERIES S (较快)", "SERIES S (較快)"])], live: false },
    Item { var: "VICTORY_MENU_RES", label: ["SERIES X PICTURE", "SERIES X 分辨率", "SERIES X 解析度"], values: &[("1440", plain("2560X1440")), ("1080to1440", ["1080P TO 1440P", "1080P 升至 1440P", "1080P 升至 1440P"]), ("4k", plain("3840X2160"))], live: false },
    // The layer's switches.
    Item { var: "VICTORY_DLSS_PRESET", label: ["DLSS MODEL", "DLSS 模型", "DLSS 模型"], values: &[("", ["DEFAULT", "默认", "預設"]), ("k", plain("K")), ("j", plain("J")), ("m", plain("M")), ("l", plain("L"))], live: false },
    Item { var: "VICTORY_AMBIENT", label: ["LIGHT ESTIMATE", "近似环境光", "近似環境光"], values: &[("0.8", plain("0.8")), ("1", plain("1")), ("0.5", plain("0.5")), ("", ["OFF", "关", "關"])], live: false },
    Item { var: "VICTORY_AMBIENT_SPECULAR", label: ["REFLECTION ESTIMATE", "近似反射", "近似反射"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_AMBIENT_AIR", label: ["FOG CAP", "雾效上限", "霧效上限"], values: &[("1.5", plain("1.5")), ("1", plain("1")), ("2", plain("2")), ("", ["NONE", "不限", "不限"])], live: false },
    Item { var: "VICTORY_BVH_BUILDER", label: ["RAY TRACING (SLOW, BROKEN)", "光追 (慢且有缺陷)", "光線追蹤 (慢且有缺陷)"], values: UNSET_ON, live: false },
    // On unless "0": a file saved by an earlier version holds an empty value (its OFF) or "1",
    // and both start it on, so nobody keeps it off from before by accident (2026-11-27).
    Item { var: "VICTORY_WATER_LIGHT", label: ["WATER LIGHT", "水面光照", "水面光照"], values: &[("", ["ON", "开", "開"]), ("0", ["OFF", "关", "關"])], live: false },
    Item { var: "VICTORY_LEAVE_OUT", label: ["HEAT HAZE AND DECAL DEFRAG", "热浪与贴花整理", "熱浪與貼花整理"], values: &[("02384a60fda6f2d0,731a39171b568298", ["LEFT OUT", "跳过", "略過"]), ("", ["RUN (SLOWER, SMEARS)", "执行 (较慢，可能拖影)", "執行 (較慢，可能拖影)"])], live: false },
    Item { var: "VICTORY_EARLY_DEPTH", label: ["EARLY DEPTH TEST", "提前深度测试", "提前深度測試"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_WC_NO_BACK", label: ["SKIP BIG COPY-BACKS", "跳过大块回写", "略過大區塊回寫"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_SOFT_VIEWS", label: ["SOFT VIEWS (NO CUT FREEZES)", "软件内存映射 (减少切镜卡顿)", "軟體記憶體映射 (減少切鏡停頓)"], values: &[("copies", ["ON", "开", "開"]), ("", ["OFF", "关", "關"])], live: false },
    Item { var: "VICTORY_TEXTURE_NO_WAIT", label: ["TEXTURE NO WAIT", "贴图上传免等待", "紋理上傳免等待"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_RELAY", label: ["RELAY (SPEED, TRIANGLES)", "分批接力 (可能出现三角形)", "分批接力 (可能出現三角形)"], values: STAGES, live: false },
    Item { var: "VICTORY_CARD_STEPS", label: ["CARD STEPS", "GPU 写入分步", "GPU 寫入分步"], values: STAGES, live: false },
    Item { var: "VICTORY_INDIRECT_BATCH", label: ["INDIRECT BATCH", "间接绘制批处理", "間接繪製批次"], values: STAGES, live: false },
    Item { var: "VICTORY_CARD_BUFFERS_MIN_KB", label: ["CARD COMPARES FROM (KB)", "GPU 缓冲比较阈值 (KB)", "GPU 緩衝區比較門檻 (KB)"], values: &[("64", plain("64")), ("1024", plain("1024"))], live: false },
    Item { var: "VICTORY_PORT_CONSTANTS", label: ["CONSTANTS HEAP IN PLACE", "常量堆原位更新", "常數堆積就地更新"], values: UNSET_ON, live: false },
];
/// The lines after the settings.
const ACTIONS: [Text; 5] =
    [["SAVE AND RESTART NOW", "保存并立即重启", "儲存並立即重新啟動"], ["SAVE FOR NEXT START", "保存，供下次启动使用", "儲存，供下次啟動使用"], ["PRELOAD SHADERS (RESTARTS THE GAME)", "预热着色器和管线 (会重启游戏)", "預熱著色器和管線 (會重新啟動遊戲)"], ["RESET ALL TO PLAY DEFAULTS", "恢复启动器默认设置", "還原啟動器預設設定"], ["CLOSE", "关闭菜单", "關閉選單"]];

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
fn fps_line(language: Language) -> Option<String> {
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
    let label = language.text(["FPS:", "帧率", "幀率"]);
    let rendered = format!("{label} {fps:.1}");
    let per = crate::streamline::shown_per_frame();
    Some(if per <= 1 {
        rendered
    } else {
        let label = language.text(["FG:", "插帧后", "補幀後"]);
        format!("{rendered}  {label} {:.1}", fps * f64::from(per))
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
fn shown_value(item: &Item, value: &str, language: Language) -> String {
    match index_of(item, value) {
        Some(i) => language.text(item.values[i].1).to_owned(),
        None if value.is_empty() => language.text(["NOT SET", "未设置", "未設定"]).to_owned(),
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

/// The box's size in pixels.
const fn size(scale: u32) -> (u32, u32) {
    ((COLUMNS * CELL.0 + 2 * RIM) * scale, box_height(LINES, scale))
}

const fn box_height(lines: u32, scale: u32) -> u32 {
    (lines * CELL.1 + 2 * RIM) * scale
}

fn visible_rows(height: u32, scale: u32) -> usize {
    let lines = (height / scale).saturating_sub(2 * RIM) / CELL.1;
    lines.saturating_sub(FIXED_LINES).clamp(1, ROWS as u32) as usize
}

struct Buffers {
    format: DXGI_FORMAT,
    scale: u32,
    pitch: u32,
    uploads: Vec<(ID3D12Resource, *mut u8)>,
    card: ID3D12Resource,
    turn: usize,
    fresh: u64,
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
        Some(Buffers { format, scale, pitch, uploads, card: card?, turn: 0, fresh: 0, used: false })
    }

    fn write(&mut self, lines: &[String], script: Script) {
        let Some(colours) = colours(self.format) else { return };
        let (width, _) = size(self.scale);
        let height = box_height(lines.len() as u32, self.scale);
        let picture = painted(lines, (width, height), self.scale, RIM, script, colours, self.pitch as usize);
        self.turn = (self.turn + 1) % UPLOADS;
        // SAFETY: a mapped upload buffer of the picture's size, which the card read frames ago.
        unsafe { std::ptr::copy_nonoverlapping(picture.as_ptr(), self.uploads[self.turn].1, picture.len()) };
        self.fresh = picture.len() as u64;
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
    note: Text,
    rows: usize,
    changed: bool,
    /// What the buffer holds: the frame-rate line, or (none) the menu's lines.
    painted: Option<String>,
    buffers: Option<Buffers>,
    failed: bool,
}

impl Menu {
    pub(super) fn new() -> Menu {
        // Start the key thread now: F1 must be seen while the menu is closed.
        let _ = keys();
        // `VICTORY_MENU_OPEN=1`: shown from the start (to look at it in a run nobody presses keys in).
        if std::env::var_os("VICTORY_MENU_OPEN").is_some_and(|v| v == "1") {
            crate::SETTINGS_MENU_OPEN.store(true, Ordering::Relaxed);
        }
        let values = ITEMS.iter().map(|item| {
            let value = get(item.var);
            if item.var == i18n::SETTING { i18n::choice(&value).to_owned() } else { value }
        }).collect::<Vec<_>>();
        Menu { started: values.clone(), values, at: 0, note: plain(""), rows: ROWS, changed: true, painted: None, buffers: None, failed: false }
    }

    fn open() -> bool {
        crate::SETTINGS_MENU_OPEN.load(Ordering::Relaxed)
    }

    fn lines(&self, language: Language) -> Vec<String> {
        let total = ITEMS.len() + ACTIONS.len();
        let first = self.at.saturating_sub(self.rows / 2).min(total.saturating_sub(self.rows));
        let mut out = vec![
            language.text(["SETTINGS                       F1 OR ESC CLOSES", "VICTORY 设置  /  F1 或 ESC 关闭", "VICTORY 設定  /  F1 或 ESC 關閉"]).to_owned(),
            language.text(["UP/DOWN PICK  LEFT/RIGHT CHANGE  ENTER DOES IT", "上下选择  左右调整  回车切换或执行", "上下選擇  左右調整  ENTER 切換或執行"]).to_owned(),
            String::new(),
        ];
        for row in first..(first + self.rows).min(total) {
            let cursor = if row == self.at { '>' } else { ' ' };
            let line = if let Some(item) = ITEMS.get(row) {
                let value = &self.values[row];
                let shown = shown_value(item, value, language);
                let mark = match (*value != self.started[row], live(item)) {
                    (true, true) => language.text([" *LIVE", " * 即时", " * 即時"]),
                    (false, true) => language.text([" LIVE", " 即时", " 即時"]),
                    (true, false) => " *",
                    (false, false) => "",
                };
                let label = language.text(item.label);
                let pad = LABEL_CELLS.saturating_sub(text_width(label)) as usize;
                format!("{cursor} {label}{:pad$} < {shown} >{mark}", "")
            } else {
                format!("{cursor} {}", language.text(ACTIONS[row - ITEMS.len()]))
            };
            out.push(line);
        }
        while out.len() < 3 + self.rows {
            out.push(String::new());
        }
        out.push(String::new());
        let about = if self.at == ITEMS.len() + PRELOAD {
            ABOUT_PRELOAD
        } else if ITEMS.get(self.at).is_some_and(|i| i.var == MFG) {
            ABOUT_MFG
        } else {
            [plain(""); 3]
        };
        out.extend(about.into_iter().map(|l| language.text(l).to_owned()));
        let note = if self.note == plain("") { ["* = CHANGED: APPLIES AT THE NEXT START UNLESS LIVE", "* 已调整。即时项立即生效，其余在重启后生效。", "* 已調整。即時項目立即生效，其餘在重啟後生效。"] } else { self.note };
        out.push(language.text(note).to_owned());
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
        self.note = plain("");
    }

    /// A setting the build reads at each use: changed in this run's environment at once.
    fn apply_live(&self, i: usize) {
        let (var, value) = (ITEMS[i].var, &self.values[i]);
        if var == i18n::SETTING {
            i18n::select(value);
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
            self.note = ["NO FOLDER FOR THE SETTINGS FILE", "找不到设置文件目录", "找不到設定檔資料夾"];
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
                    ["SAVED. APPLIES AT THE NEXT START", "已保存，下次启动生效", "已儲存，下次啟動生效"]
                } else {
                    // Started some other way (a script, another launcher): the file `victory play`
                    // reads at every start, which this start did not.
                    ["SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME", "已保存，经 VICTORY.EXE 启动时生效", "已儲存，透過 VICTORY.EXE 啟動時生效"]
                };
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "settings menu: not saved");
                self.note = ["COULD NOT SAVE THE SETTINGS FILE", "无法保存设置文件", "無法儲存設定檔"];
                false
            }
        }
    }

    /// Starts the launcher again in a new console, a few seconds on, and ends this run.
    /// `preload`: the launcher makes every shader first (`VICTORY_PRELOAD=1`, for that start alone).
    fn restart(&mut self, preload: bool) {
        let Some(play) = std::env::var_os("VICTORY_MENU_PLAY") else {
            self.note = ["SAVED. START THE LAUNCHER AGAIN YOURSELF", "已保存，请手动重新启动", "已儲存，請手動重新啟動"];
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
                self.note = ["SAVED, BUT THE RESTART FAILED", "已保存，重启失败", "已儲存，重新啟動失敗"];
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
        if !mark.as_os_str().is_empty() {
            if let Some(dir) = mark.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&mark, "this victory.exe was warmed up (its first start, then a restart)\r\n");
        }
        tracing::info!(seconds, mark = %mark.display(), "first start of this victory.exe: warmed up; the game starts again");
        self.restart(false);
    }

    /// Removes the settings file: the launcher's own settings at the next start.
    fn reset(&mut self) {
        if let Some(path) = settings_file() {
            let _ = std::fs::remove_file(path);
        }
        self.note = ["RESET. PLAY DEFAULTS AT THE NEXT START", "已恢复默认，下次启动生效", "已還原預設，下次啟動生效"];
    }

    /// Changes a setting and saves it at once: many players changed settings and closed the
    /// menu without the save lines, and lost them. The save lines stay (and restart, preload).
    fn change_and_save(&mut self, i: usize, step: isize) {
        self.change(i, step);
        if self.save() && live(&ITEMS[i]) {
            self.note = ["SAVED. ON NOW", "设置已保存并生效", "設定已儲存並生效"];
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
            if vk == VK_F1 || (open && vk == VK_ESCAPE) {
                crate::SETTINGS_MENU_OPEN.store(!open && vk == VK_F1, Ordering::Relaxed);
                self.note = plain("");
                self.changed = true;
                continue;
            }
            if !open {
                continue;
            }
            // Only an Enter right after the first one starts a preload.
            let confirmed = self.note == CONFIRM_PRELOAD;
            if confirmed {
                self.note = plain("");
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
                    PRELOAD if !confirmed => self.note = CONFIRM_PRELOAD,
                    PRELOAD => {
                        if self.save() {
                            self.restart(true);
                        }
                    }
                    3 => self.reset(),
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
        let language = i18n::current();
        let line = if open { None } else { fps_line(language) };
        if !open && line.is_none() {
            return;
        }
        // Our own copies are not part of a recorded frame (the recorder is in diagnostics builds).
        #[cfg(feature = "diagnostics")]
        let _ours = crate::replay::record::unrecorded();
        let scale = (height / 540).max(1);
        let rows = visible_rows(height, scale);
        if rows != self.rows {
            self.rows = rows;
            self.changed = true;
        }
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
        let lines = if self.changed { Some(line.as_ref().map_or_else(|| self.lines(language), |l| vec![l.clone()])) } else { None };
        let Some(buffers) = &mut self.buffers else { return };
        if let Some(lines) = lines {
            self.changed = false;
            buffers.write(&lines, language.script());
        }
        // The whole box in the middle, or of it the first line's part in the corner.
        let (w, h) = match &line {
            Some(l) => ((text_width(l) * CELL.0 + 2 * RIM) * scale, box_height(1, scale)),
            None => (size(scale).0, box_height(self.rows as u32 + FIXED_LINES, scale)),
        };
        let (w, h) = (w.min(size(scale).0).min(width), h.min(height));
        let (x, y) = if line.is_some() { (0, 0) } else { ((width - w) / 2, (height - h) / 2) };
        let list = &list.list;
        // SAFETY: an open list and live resources; the box lies within both.
        unsafe {
            let fresh = std::mem::take(&mut buffers.fresh);
            if fresh != 0 {
                if std::mem::replace(&mut buffers.used, true) {
                    list.ResourceBarrier(&[transition(&buffers.card, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]);
                }
                list.CopyBufferRegion(&buffers.card, 0, &buffers.uploads[buffers.turn].0, 0, fresh);
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

    const LANGUAGES: [Language; 3] = [Language::English, Language::Simplified, Language::Traditional];

    fn menu() -> Menu {
        let values = ITEMS.iter().map(|i| i.values[0].0.to_owned()).collect::<Vec<_>>();
        Menu {
            started: values.clone(), values, at: 0, note: plain(""), rows: ROWS, changed: false,
            painted: None, buffers: None, failed: false,
        }
    }

    #[test]
    fn small_back_buffer_keeps_the_selection_and_footer_visible() {
        let mut menu = menu();
        for height in [360, 480, 540, 720, 1080, 2160] {
            let scale = (height / 540).max(1);
            menu.rows = visible_rows(height, scale);
            assert!(box_height(menu.rows as u32 + FIXED_LINES, scale) <= height);
            for at in 0..ITEMS.len() + ACTIONS.len() {
                menu.at = at;
                let lines = menu.lines(Language::Traditional);
                assert_eq!(lines.len(), menu.rows + FIXED_LINES as usize);
                assert_eq!(lines.iter().filter(|line| line.starts_with('>')).count(), 1);
                assert_eq!(lines.last().unwrap(), REPO);
            }
        }
    }

    #[test]
    fn language_switch_is_live_and_saved_at_once() {
        let home = tempfile::tempdir().unwrap();
        let previous_file = std::env::var_os("VICTORY_MENU_FILE");
        let previous_home = std::env::var_os("VICTORY_HOME");
        // SAFETY: this is the only test changing the menu settings environment.
        unsafe {
            std::env::remove_var("VICTORY_MENU_FILE");
            std::env::set_var("VICTORY_HOME", home.path());
        }
        let mut menu = menu();
        let dlss = ITEMS.iter().position(|i| i.var == "VICTORY_MENU_DLSS").unwrap();
        menu.change_and_save(dlss, 1);
        let file = home.path().join("victory-settings.txt");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("VICTORY_MENU_DLSS=balanced"), "{text}");
        assert_eq!(Language::English.text(menu.note), "SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME");
        let language = ITEMS.iter().position(|i| i.var == i18n::SETTING).unwrap();
        for (choice, expected) in [("en", Language::English), ("zh-Hans", Language::Simplified), ("zh-Hant", Language::Traditional)] {
            menu.change_and_save(language, 1);
            assert_eq!(i18n::current(), expected);
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(text.contains(&format!("VICTORY_UI_LANGUAGE={choice}")), "{text}");
        }
        menu.change_and_save(language, 1);
        assert!(std::fs::read_to_string(&file).unwrap().contains("VICTORY_UI_LANGUAGE=auto"));
        // SAFETY: restore the original values after the isolated test.
        unsafe {
            for (key, previous) in [("VICTORY_MENU_FILE", previous_file), ("VICTORY_HOME", previous_home)] {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn every_known_line_fits_in_each_language() {
        let mut menu = menu();
        for language in LANGUAGES {
            for at in 0..ITEMS.len() + ACTIONS.len() {
                menu.at = at;
                let values = ITEMS.get(at).map_or(&[][..], |item| item.values);
                for value in std::iter::once("").chain(values.iter().map(|(value, _)| *value)) {
                    if let Some(selected) = menu.values.get_mut(at) {
                        *selected = value.to_owned();
                    }
                    for line in menu.lines(language) {
                        assert!(text_width(&line) <= COLUMNS, "{language:?}: {line}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_link_is_always_right_under_the_note() {
        let mut menu = menu();
        for language in LANGUAGES {
            for note in [plain(""), CONFIRM_PRELOAD] {
                menu.note = note;
                for at in 0..ITEMS.len() + ACTIONS.len() {
                    menu.at = at;
                    let lines = menu.lines(language);
                    assert_eq!(lines.len(), LINES as usize);
                    let n = lines.len();
                    assert!(lines[n - 2].starts_with('*') || lines[n - 2] == language.text(note));
                    assert_eq!(lines[n - 1], REPO);
                    let about = &lines[n - 5..n - 2];
                    if at == ITEMS.len() + PRELOAD {
                        assert_eq!(about, ABOUT_PRELOAD.map(|line| language.text(line)));
                    } else if at < ITEMS.len() && ITEMS[at].var == MFG {
                        assert_eq!(about, ABOUT_MFG.map(|line| language.text(line)));
                    } else {
                        assert!(about.iter().all(String::is_empty), "{about:?}");
                    }
                }
            }
        }
        assert_eq!(Language::English.text(ACTIONS[PRELOAD]), "PRELOAD SHADERS (RESTARTS THE GAME)");
        assert_eq!(ITEMS.iter().filter(|i| i.var == MFG).count(), 1);
    }

    #[test]
    fn values_cycle_both_ways() {
        let mut menu = menu();
        let i = ITEMS.iter().position(|i| i.var == "VICTORY_MENU_DLSS").unwrap();
        let n = ITEMS[i].values.len();
        for _ in 0..n {
            menu.change(i, 1);
        }
        assert_eq!(menu.values[i], ITEMS[i].values[0].0);
        menu.change(i, -1);
        assert_eq!(menu.values[i], ITEMS[i].values[n - 1].0);
    }
}
