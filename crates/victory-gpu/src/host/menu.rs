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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_DOWN, VK_ESCAPE, VK_F1, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::font::{ASCII_WIDTH, TextBox, colours, glyph, supports, width as text_width};
use super::language::{Labels, Language};
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
    ["MAKES EVERY SHADER AHEAD, THEN STARTS THE GAME AGAIN.", "提前编译全部着色器，然后重新启动游戏。", "預先建立著色器和管線，再啟動遊戲。", "COMPILA TODOS LOS SHADERS Y REINICIA EL JUEGO."],
    ["TAKES A WHILE: MAYBE A LONG TIME, DEPENDING ON HARDWARE.", "处理时间取决于硬件，可能需要等很久。", "處理時間取決於硬體，可能需要很久。", "PUEDE TARDAR MUCHO, SEGÚN EL EQUIPO."],
    ["A LATER UPDATE OF THIS BUILD MAY NEED A NEW PRELOAD.", "兼容层更新后可能需要重新预加载。", "更新執行版本後，可能需要重新預熱。", "UNA ACTUALIZACIÓN PODRÍA REQUERIR OTRA PRECARGA."],
];
/// The actions that export and import the game's save (`crate::save_share`), and what is said
/// of them while they are picked.
const EXPORT: usize = 4;
const IMPORT: usize = 5;
const ABOUT_EXPORT: [Labels; 3] = [
    ["MAKES A ZIP OF YOUR SAVE IN SAVES, IN THE FOLDER OF", "把存档打包为 ZIP，放入游戏目录的 SAVES 文件夹。", "將存檔打包成 ZIP，放入遊戲目錄的 SAVES。", "EMPAQUETA LA PARTIDA EN UN ZIP EN SAVES, DENTRO DE"],
    ["GAME_SCARLETT_MASTER.EXE. SEND THAT ZIP TO SHARE IT.", "可把 ZIP 发给其他人分享。", "可將 ZIP 傳給其他人分享。", "LA CARPETA DEL JUEGO. PUEDES COMPARTIR ESE ZIP."],
    ["YOUR SAVE STAYS AS IT IS. GAME SETTINGS ARE LEFT OUT.", "原存档保持原样，ZIP 不包含游戏设置。", "原存檔維持原樣，ZIP 不含遊戲設定。", "CONSERVA TU PARTIDA. EL ZIP NO INCLUYE LOS AJUSTES."],
];
const ABOUT_IMPORT: [Labels; 3] = [
    ["PUT A SAVE ZIP INTO SAVES, IN THE FOLDER OF", "把存档 ZIP 放入游戏目录的 SAVES 文件夹。", "將存檔 ZIP 放入遊戲目錄的 SAVES。", "PON UN ZIP DE PARTIDA EN SAVES, EN LA CARPETA DE"],
    ["GAME_SCARLETT_MASTER.EXE, THEN PRESS ENTER HERE AND", "在此按回车，再从列表中选择存档。", "在此按 Enter，再從清單選擇存檔。", "GAME_SCARLETT_MASTER.EXE. PULSA ENTER AQUÍ Y"],
    ["PICK IT FROM THE LIST. YOUR GAME SETTINGS ARE KEPT.", "导入时保留你的游戏设置。", "匯入時保留你的遊戲設定。", "ELÍGELO EN LA LISTA. CONSERVARÁS TUS AJUSTES."],
];
/// The list IMPORT SAVE opens: the saves in `Saves`, then a line back to the settings.
const PICK_TITLE: Labels = ["IMPORT SAVE                       ESC GOES BACK", "导入存档                   ESC 返回", "匯入存檔                   ESC 返回", "IMPORTAR PARTIDA             ESC PARA VOLVER"];
const PICK_HELP: Labels = ["UP/DOWN PICK  ENTER TWICE IMPORTS IT", "上下选择    按两次回车导入", "上下選擇    按兩次 Enter 匯入", "ARRIBA/ABAJO ELIGE  ENTER DOS VECES"];
const PICK_NONE: Labels = ["  NONE YET: PUT A SAVE ZIP INTO SAVES IN THE GAME FOLDER", "  暂无存档。请把存档 ZIP 放入游戏目录的 SAVES", "  尚無存檔。請將存檔 ZIP 放入遊戲目錄的 SAVES", "  SIN PARTIDAS. PON UN ZIP EN SAVES, DENTRO DEL JUEGO"];
const PICK_BACK: Labels = ["BACK TO THE SETTINGS", "返回设置", "返回設定", "VOLVER A LOS AJUSTES"];
const ABOUT_PICK: [Labels; 3] = [
    ["THE SAVE ZIPS IN SAVES, IN THE GAME FOLDER, NEWEST FIRST.", "下列 ZIP 位于游戏目录的 SAVES，最新的排在最前。", "SAVES 內的存檔 ZIP，依新到舊排列。", "ARCHIVOS ZIP EN SAVES, DEL MÁS NUEVO AL MÁS ANTIGUO."],
    ["ENTER TWICE: THE GAME RESTARTS WITH THE SAVE PICKED.", "按两次回车后，游戏将使用所选存档重启。", "按兩次 Enter，用所選存檔重新啟動遊戲。", "ENTER DOS VECES: REINICIA CON LA PARTIDA ELEGIDA."],
    ["YOUR OLD SAVE IS ZIPPED INTO SAVES FIRST, SO IT IS KEPT.", "旧存档会先打包备份到 SAVES。", "原存檔會先打包備份到 SAVES。", "ANTES SE GUARDA UNA COPIA DE TU PARTIDA EN SAVES."],
];
/// The note after the first Enter on a save in the list: the second one imports it.
const CONFIRM_IMPORT: Labels = ["ENTER AGAIN: IMPORT IT AND RESTART THE GAME", "再次按回车：导入存档并重启游戏", "再按 Enter：匯入存檔並重新啟動遊戲", "PULSA ENTER OTRA VEZ PARA IMPORTAR Y REINICIAR"];
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
    ["A MOD, NOT OURS: DLSS FRAME GEN ON RTX 20 AND 30 CARDS.", "第三方模组，可让 RTX 20 和 30 系列使用 DLSS 帧生成。", "第三方模組，讓 RTX 20 / 30 使用 DLSS 畫格生成。", "MOD DE TERCEROS: GEN. DE FOTOGRAMAS EN RTX 20 Y 30."],
    ["ON: DOWNLOADED FROM ITS GITHUB AT THE NEXT START, THEN RUN.", "开启后会在下次启动时从其 GITHUB 下载并加载。", "開啟後，下次啟動會從 GITHUB 下載並載入。", "AL ACTIVARLO, SE DESCARGA DE GITHUB Y SE CARGA AL INICIO."],
    ["NVIDIA DOES NOT SUPPORT IT. IF THE GAME FAILS, TURN IT OFF.", "NVIDIA 未提供支持。游戏异常时请关闭它。", "NVIDIA 未提供支援。遊戲無法啟動時請關閉。", "NVIDIA NO LO ADMITE. DESACTÍVALO SI EL JUEGO FALLA."],
];
/// The DLSS setting, and what is said while it is picked on in a start without NVIDIA's files.
const DLSS: &str = "VICTORY_MENU_DLSS";
const ABOUT_NO_DLSS: [Labels; 3] = [
    ["THE NVIDIA DLSS FILES ARE NOT ON THIS PC YET: THEY ARE", "本机还没有 NVIDIA DLSS 文件。", "此電腦尚未安裝 NVIDIA DLSS 檔案。", "AÚN FALTAN LOS ARCHIVOS DE NVIDIA DLSS."],
    ["DOWNLOADED FROM NVIDIA ON GITHUB AT THE NEXT START", "下次启动时会从 NVIDIA 的 GITHUB 下载。", "下次啟動時會從 NVIDIA 的 GITHUB 下載。", "SE DESCARGAN DEL GITHUB DE NVIDIA AL INICIAR."],
    ["(ABOUT 70 MB, ONCE). SAVE AND RESTART NOW DOES IT.", "约 70 MB，只下载一次。可选保存并立即重启。", "約 70 MB，只下載一次。可儲存並立即重啟。", "APROX. 70 MB, UNA SOLA VEZ. GUARDA Y REINICIA AHORA."],
];
/// Frame generation that this run did not start with: it is made ready only at a start with it
/// on (a start with it ready but off held some PCs at 14 fps, #515, #574), so turning it on takes
/// a restart; a run started with it on changes it, or turns it off, live.
const ABOUT_FRAMEGEN_RESTART: [Labels; 3] = [
    ["RESTART REQUIRED TO ENABLE FRAME GENERATION: THIS RUN", "本次启动未启用帧生成，开启需重启游戏。", "本次啟動未開啟畫格生成，需要重新啟動遊戲。", "DEBES REINICIAR PARA ACTIVAR LA GEN. DE FOTOGRAMAS."],
    ["STARTED WITHOUT IT. SAVE AND RESTART NOW TURNS IT ON.", "选择保存并立即重启，即可启用帧生成。", "選擇儲存並立即重新啟動，即可開啟畫格生成。", "GUARDA Y REINICIA AHORA PARA ACTIVARLA."],
    ["AFTER THAT IT CHANGES, OR TURNS OFF, LIVE.", "重启后可即时调整或关闭帧生成。", "之後可即時調整或關閉畫格生成。", "LUEGO PUEDES AJUSTARLA O DESACTIVARLA AL INSTANTE."],
];
const RESTART_FRAMEGEN: Labels = ["RESTART REQUIRED TO ENABLE FRAME GENERATION", "重启游戏后才能开启帧生成", "重新啟動遊戲後才能開啟畫格生成", "REINICIA PARA ACTIVAR LA GEN. DE FOTOGRAMAS"];
const FRAMEGEN: &str = "VICTORY_MENU_FRAMEGEN";
/// The console, and the picture row of each: only the one of the console picked is shown.
const CONSOLE: &str = "VICTORY_MENU_CONSOLE";
const X_RES: &str = "VICTORY_MENU_RES";
const S_RES: &str = "VICTORY_MENU_S_RES";
/// The Series S scene sizes and the DLSS mode of each (the picture is 1920x1080). Only DLSS
/// changes it: the title's own FidelityFX path from 540 stopped it at the start (0xc0000094,
/// a division by zero in the title, 2026-11-28), so with DLSS off the scene stays at 720.
const S_MODES: [(&str, &str); 4] = [("1080", "dlaa"), ("720", "quality"), ("626", "balanced"), ("540", "performance")];
const ABOUT_S_RES: [Labels; 3] = [
    ["THE SIZE THE GAME RENDERS AT. THE PICTURE IS 1920X1080.", "游戏内部渲染分辨率。输出画面为 1920X1080。", "遊戲的內部渲染解析度。輸出為 1920X1080。", "RESOLUCIÓN INTERNA DEL JUEGO. LA SALIDA ES 1920X1080."],
    ["OTHER THAN 720 NEEDS DLSS: THIS SETS ITS MODE (1080 IS", "除 720P 外均需 DLSS。分辨率决定 DLSS 模式。", "除 720P 外均需 DLSS。解析度決定 DLSS 模式。", "SALVO 720P, REQUIERE DLSS. ESTO FIJA SU MODO:"],
    ["DLAA, 720 QUALITY, 626 BALANCED, 540 PERFORMANCE).", "1080P 为 DLAA，720P 质量，626P 平衡，540P 性能。", "1080P 為 DLAA，720P 畫質，626P 平衡，540P 效能。", "1080 DLAA, 720 CALIDAD, 626 EQUIL., 540 RENDIMIENTO."],
];
/// The note after the first Enter on it: the second one does it.
const CONFIRM_PRELOAD: Labels = ["ENTER AGAIN: SAVE, CLOSE THE GAME AND PRELOAD", "再次按回车：保存设置、退出游戏并预加载", "再按 Enter：儲存設定、關閉遊戲並預載", "PULSA ENTER OTRA VEZ PARA GUARDAR, CERRAR Y PRECARGAR"];
/// The last line of the box: where this build comes from.
const REPO: &str = "GITHUB.COM/TRUSTNO1-CODER/VICTORY-NVIDIA";
const BOX: TextBox = TextBox { columns: COLUMNS, lines: LINES, rim: 4 };
const FPS_BOX: TextBox = TextBox { columns: 48, lines: 1, rim: 4 };
const UPLOADS: usize = 4;

const LANGUAGE: &str = "VICTORY_MENU_LANGUAGE";
const LANGUAGE_VALUES: &[(&str, Labels)] = &[
    ("en", ["ENGLISH", "英语", "英語", "INGLÉS"]),
    ("zh-CN", ["SIMPLIFIED CHINESE", "简体中文", "簡體中文", "CHINO SIMPLIFICADO"]),
    ("zh-TW", ["TRADITIONAL CHINESE", "繁体中文", "繁體中文", "CHINO TRADICIONAL"]),
    ("es", ["SPANISH", "西班牙语", "西班牙語", "ESPAÑOL"]),
    ("auto", ["AUTO (WINDOWS)", "跟随 WINDOWS", "跟隨 WINDOWS", "AUTO (WINDOWS)"]),
];

const fn same(value: &'static str) -> Labels {
    [value; Language::ALL.len()]
}

fn language_index(value: &str) -> usize {
    if value.eq_ignore_ascii_case("auto") { Language::ALL.len() } else { Language::from_setting(value).index() }
}

fn language_choice() -> &'static AtomicUsize {
    static CHOICE: OnceLock<AtomicUsize> = OnceLock::new();
    CHOICE.get_or_init(|| AtomicUsize::new(language_index(&get(LANGUAGE))))
}

pub(super) fn language() -> Language {
    let choice = language_choice().load(Ordering::Relaxed);
    let requested = Language::ALL.get(choice).copied().unwrap_or_else(|| Language::from_setting("auto"));
    if supports(requested) { requested } else { Language::English }
}

/// A setting: the variable, its name in the menu, the values it takes
/// (`""`: not set, the build's own default), and whether a change applies at once.
struct Item {
    var: &'static str,
    label: Labels,
    values: &'static [(&'static str, Labels)],
    live: bool,
}

const ON: Labels = ["ON", "开启", "開啟", "SÍ"];
const OFF: Labels = ["OFF", "关闭", "關閉", "NO"];
const NONE: Labels = ["NONE", "无限制", "不限", "SIN LÍMITE"];
const ON_UNSET: &[(&str, Labels)] = &[("1", ON), ("", OFF)];
const UNSET_ON: &[(&str, Labels)] = &[("", OFF), ("1", ON)];
const STAGES: &[(&str, Labels)] = &[("2", same("2")), ("1", same("1")), ("0", ["0 OFF", "0 关闭", "0 關閉", "0 NO"])];

/// The first value of each is what the launcher starts with.
#[rustfmt::skip]
const ITEMS: &[Item] = &[
    Item { var: LANGUAGE, label: ["LANGUAGE", "界面语言", "介面語言", "IDIOMA"], values: LANGUAGE_VALUES, live: true },
    // The launcher's own choices.
    Item { var: DLSS, label: same("DLSS"), values: &[("quality", ["QUALITY", "质量", "畫質", "CALIDAD"]), ("balanced", ["BALANCED", "平衡", "平衡", "EQUILIBRADO"]), ("performance", ["PERFORMANCE", "性能", "效能", "RENDIMIENTO"]), ("dlaa", ["DLAA (NATIVE)", "DLAA (原生分辨率)", "DLAA (原生解析度)", "DLAA (NATIVO)"]), ("off", OFF)], live: false },
    Item { var: FRAMEGEN, label: ["FRAME GENERATION", "帧生成", "畫格生成", "GEN. DE FOTOGRAMAS"], values: &[("0", OFF), ("2", same("2X")), ("3", same("3X")), ("4", same("4X")), ("5", same("5X")), ("6", ["6X (MOST)", "6X (最高)", "6X (最高)", "6X (MÁXIMO)"])], live: false },
    // DLSS Frame Generation where it runs (RTX 40 and later), else AMD's FSR 3 (2X only).
    Item { var: "VICTORY_MENU_FG_TYPE", label: ["FRAME GEN TYPE", "帧生成类型", "畫格生成類型", "TIPO DE GEN. DE FOTOGRAMAS"], values: &[("auto", ["AUTO", "自动", "自動", "AUTO"]), ("dlss", same("DLSS")), ("fsr", ["FSR 3 (OLDER CARDS)", "FSR 3 (旧显卡)", "FSR 3 (舊顯示卡)", "FSR 3 (GPU ANTIGUAS)"])], live: false },
    // Someone else's mod (apps\victory\src\play.rs): downloaded at the next start, then loaded.
    Item { var: MFG, label: ["MFG UNLOCKER (MOD, RTX 20/30)", "MFG 解锁模组 (RTX 20/30)", "MFG 解鎖模組 (RTX 20/30)", "DESBLOQUEO MFG (MOD, RTX 20/30)"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_MENU_FPS", label: ["FRAME RATE CAP", "帧率上限", "畫格率上限", "LÍMITE DE FPS"], values: &[("1000", NONE), ("120", same("120")), ("60", same("60")), ("30", ["30 (THE GAME)", "30 (游戏默认)", "30 (遊戲預設)", "30 (DEL JUEGO)"])], live: false },
    Item { var: "VICTORY_FPS_LINE", label: ["FPS AT THE TOP", "顶部帧率", "上方畫格率", "FPS ARRIBA"], values: &[("1", ON), ("0", OFF)], live: true },
    Item { var: "VICTORY_PANEL", label: ["STATS PANEL (F3)", "性能统计 (F3)", "效能統計 (F3)", "PANEL DE ESTADÍSTICAS (F3)"], values: &[("0", OFF), ("1", ON)], live: true },
    Item { var: "VICTORY_FULLSCREEN", label: ["FULLSCREEN (NO BORDER)", "无边框全屏", "無邊框全螢幕", "PANTALLA COMPLETA SIN BORDES"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_MOUSE_CAPTURE", label: ["MOUSE CAPTURE", "锁定鼠标", "鎖定滑鼠游標", "CAPTURA DEL RATÓN"], values: UNSET_ON, live: false },
    Item { var: "VICTORY_IME_ENGLISH", label: ["ENGLISH KEYBOARD", "使用英文键盘", "使用英文鍵盤", "TECLADO INGLÉS"], values: UNSET_ON, live: false },
    Item { var: CONSOLE, label: ["CONSOLE", "主机型号", "主機模式", "CONSOLA"], values: &[("series-x", same("SERIES X")), ("series-s", ["SERIES S (FASTER)", "SERIES S (更快)", "SERIES S (較快)", "SERIES S (MÁS RÁPIDA)"])], live: false },
    Item { var: X_RES, label: ["SERIES X PICTURE", "SERIES X 分辨率", "SERIES X 解析度", "RESOLUCIÓN SERIES X"], values: &[("1440", same("2560X1440")), ("1080to1440", ["1080P TO 1440P", "1080P 升至 1440P", "1080P 升至 1440P", "1080P A 1440P"]), ("4k", same("3840X2160"))], live: false },
    // In place of SERIES X PICTURE when CONSOLE is Series S.
    Item { var: S_RES, label: ["SERIES S PICTURE", "SERIES S 分辨率", "SERIES S 解析度", "RESOLUCIÓN SERIES S"], values: &[("1080", same("1920X1080 (DLAA)")), ("720", ["1280X720 (THE GAME)", "1280X720 (游戏默认)", "1280X720 (遊戲預設)", "1280X720 (DEL JUEGO)"]), ("626", same("1113X626")), ("540", ["960X540 (FASTEST)", "960X540 (最快)", "960X540 (最快)", "960X540 (MÁS RÁPIDA)"])], live: false },
    // The layer's switches.
    Item { var: "VICTORY_DLSS_PRESET", label: ["DLSS MODEL", "DLSS 模型", "DLSS 模型", "MODELO DLSS"], values: &[("", ["DEFAULT", "默认", "預設", "PREDETERMINADO"]), ("k", same("K")), ("j", same("J")), ("m", same("M")), ("l", same("L"))], live: false },
    Item { var: "VICTORY_AMBIENT", label: ["LIGHT ESTIMATE", "环境光估算", "近似環境光", "ESTIMACIÓN DE LUZ"], values: &[("0.8", same("0.8")), ("1", same("1")), ("0.5", same("0.5")), ("", OFF)], live: false },
    Item { var: "VICTORY_AMBIENT_SPECULAR", label: ["REFLECTION ESTIMATE", "反射估算", "近似反射", "ESTIMACIÓN DE REFLEJOS"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_AMBIENT_AIR", label: ["FOG CAP", "雾效强度上限", "霧效上限", "LÍMITE DE NIEBLA"], values: &[("1.5", same("1.5")), ("1", same("1")), ("2", same("2")), ("", NONE)], live: false },
    Item { var: "VICTORY_BVH_BUILDER", label: ["RAY TRACING (SLOW, BROKEN)", "光线追踪 (慢且有故障)", "光線追蹤 (慢且有缺陷)", "TRAZADO DE RAYOS (LENTO, FALLA)"], values: UNSET_ON, live: false },
    // On unless "0": a file saved by an earlier version holds an empty value (its OFF) or "1",
    // and both start it on, so nobody keeps it off from before by accident (2026-11-27).
    Item { var: "VICTORY_WATER_LIGHT", label: ["WATER LIGHT", "水面光照", "水面光照", "LUZ DEL AGUA"], values: &[("", ON), ("0", OFF)], live: false },
    Item { var: "VICTORY_LEAVE_OUT", label: ["HEAT HAZE AND DECAL DEFRAG", "热浪和贴花整理", "熱浪與貼花整理", "CALOR Y CALCOMANÍAS"], values: &[("02384a60fda6f2d0,731a39171b568298", ["LEFT OUT", "跳过", "略過", "OMITIR"]), ("", ["RUN (SLOWER, SMEARS)", "开启 (慢且有拖影)", "執行 (較慢，可能拖影)", "ACTIVAR (LENTO, ESTELAS)"])], live: false },
    Item { var: "VICTORY_EARLY_DEPTH", label: ["EARLY DEPTH TEST", "提前深度测试", "提前深度測試", "PRUEBA TEMPRANA DE PROFUNDIDAD"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_WC_NO_BACK", label: ["SKIP BIG COPY-BACKS", "跳过大块数据回拷", "略過大區塊回寫", "OMITIR COPIAS DE RETORNO GRANDES"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_SOFT_VIEWS", label: ["SOFT VIEWS (NO CUT FREEZES)", "软视图 (减少转场卡顿)", "軟體記憶體映射 (減少切鏡停頓)", "MAPEO DE MEMORIA (EVITA PAUSAS)"], values: &[("copies", ON), ("", OFF)], live: false },
    Item { var: "VICTORY_TEXTURE_NO_WAIT", label: ["TEXTURE NO WAIT", "纹理异步加载", "紋理上傳免等待", "TEXTURAS SIN ESPERA"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_RELAY", label: ["RELAY (SPEED, TRIANGLES)", "GPU 中继 (速度、三角面)", "分批接力 (可能出現三角形)", "RELEVO (RÁPIDO, TRIÁNGULOS)"], values: STAGES, live: false },
    Item { var: "VICTORY_CARD_STEPS", label: ["CARD STEPS", "GPU 处理阶段", "GPU 寫入分步", "PASOS DE GPU"], values: STAGES, live: false },
    Item { var: "VICTORY_INDIRECT_BATCH", label: ["INDIRECT BATCH", "间接绘制批处理", "間接繪製批次", "LOTES DE DIBUJO INDIRECTO"], values: STAGES, live: false },
    Item { var: "VICTORY_CARD_BUFFERS_MIN_KB", label: ["CARD COMPARES FROM (KB)", "GPU 缓冲区比较阈值 (KB)", "GPU 緩衝區比較門檻 (KB)", "COMPARAR BÚFERES DESDE (KB)"], values: &[("64", same("64")), ("1024", same("1024"))], live: false },
    Item { var: "VICTORY_PORT_CONSTANTS", label: ["CONSTANTS HEAP IN PLACE", "原地更新常量缓冲区", "常數堆積就地更新", "ACTUALIZAR CONSTANTES IN SITU"], values: UNSET_ON, live: false },
    // THIN records the title's list calls on our threads. On an RTX 5080 with a 6-core CPU heads did not
    // draw with it and frames were slower (14 a second with it, 21 to 28 without): switches for it and for
    // what goes with it. Frame generation needs the line of submissions: without THIN, ASYNC SUBMIT gives it.
    Item { var: "VICTORY_THIN", label: ["THIN LISTS (HEADS MAY BE MISSING)", "THIN 列表 (人物头部可能不显示)", "精簡清單 (可能缺頭)", "LISTAS THIN (PUEDEN FALTAR CABEZAS)"], values: ON_UNSET, live: false },
    Item { var: "VICTORY_JOIN_PARTS", label: ["JOIN PARTS", "合并提交分段", "合併批次", "UNIR LOTES"], values: &[("8", same("8")), ("", ["1 (OFF)", "1 (关闭)", "1 (關閉)", "1 (NO)"])], live: false },
    Item { var: "VICTORY_ASYNC_SUBMIT", label: ["ASYNC SUBMIT (FRAME GEN WITHOUT THIN)", "异步提交 (关闭 THIN 时用于帧生成)", "非同步提交 (停用 THIN 時用於畫格生成)", "ENVÍO ASÍNCRONO (SIN LISTAS THIN)"], values: UNSET_ON, live: false },
];
/// The lines after the settings.
const ACTIONS: [Labels; 7] = [
    ["SAVE AND RESTART NOW", "保存并立即重启", "儲存並立即重新啟動", "GUARDAR Y REINICIAR AHORA"],
    ["SAVE FOR NEXT START", "保存设置", "儲存設定", "GUARDAR PARA EL PRÓXIMO INICIO"],
    ["PRELOAD SHADERS (RESTARTS THE GAME)", "预加载着色器 (重启游戏)", "預載著色器 (重新啟動遊戲)", "PRECARGAR SHADERS (REINICIA EL JUEGO)"],
    ["RESET ALL TO PLAY DEFAULTS", "恢复启动默认设置", "還原啟動器預設設定", "RESTABLECER AJUSTES PREDETERMINADOS"],
    ["EXPORT SAVE (TO SAVES IN THE GAME FOLDER)", "导出存档 (到游戏目录的 SAVES)", "匯出存檔 (到遊戲目錄的 SAVES)", "EXPORTAR PARTIDA (A LA CARPETA SAVES)"],
    ["IMPORT SAVE (PICK FROM SAVES)", "导入存档 (从 SAVES 选择)", "匯入存檔 (從 SAVES 選擇)", "IMPORTAR PARTIDA (ELEGIR EN SAVES)"],
    ["CLOSE", "关闭", "關閉", "CERRAR"],
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
pub(super) fn warmup_notice(seconds: u64, language: Language) -> Vec<String> {
    let tr = |text| language.text(text).to_owned();
    vec![
        tr(["FIRST START OF THIS VERSION: A ONE-TIME WARM-UP", "首次启动此版本，游戏将进行一次预热", "首次啟動此版本，遊戲將進行一次預熱", "PRIMER INICIO DE ESTA VERSIÓN: PREPARACIÓN ÚNICA"]),
        match language {
            Language::English => format!("RESTARTS AFTER {seconds} SECONDS IN-GAME"),
            Language::SimplifiedChinese => format!("进入游戏 {seconds} 秒后自动重启"),
            Language::TraditionalChinese => format!("進入遊戲 {seconds} 秒後自動重啟"),
            Language::Spanish => format!("REINICIA TRAS {seconds} S DE JUEGO"),
        },
        tr(["PRESS A AT THE TITLE SCREEN TO ENTER THE GAME", "请在标题画面按 A 进入游戏", "請在標題畫面按 A 進入遊戲", "PULSA A EN LA PANTALLA DE TÍTULO PARA JUGAR"]),
        tr(["THIS HAPPENS ONCE PER NEW VERSION, AND IS NEEDED:", "每个新版本只需预热一次", "每個新版本只需預熱一次", "SOLO OCURRE UNA VEZ POR CADA VERSIÓN NUEVA:"]),
        tr(["WITHOUT IT THE FIRST RUN STAYS AT ABOUT 14 FPS", "预热前首次运行约为 14 帧", "預熱前首次執行約為 14 FPS", "SIN ESTO, EL PRIMER INICIO QUEDA EN UNOS 14 FPS"]),
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
    Some(format_fps(fps, crate::streamline::shown_per_frame(), language))
}

fn format_fps(fps: f64, per: u32, language: Language) -> String {
    let rate = language.text(["FPS", "帧率", "畫格率", "FPS"]);
    if per == 1 {
        format!("{rate}: {fps:.1}")
    } else {
        let displayed = language.text(["FG", "显示帧率", "顯示畫格率", "MOSTRADOS"]);
        format!("{rate}: {fps:.1}  {displayed}: {:.1}", fps * f64::from(per))
    }
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
        None if value.is_empty() => language.text(["NOT SET", "未设置", "未設定", "SIN AJUSTAR"]).to_owned(),
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
    fn new(host: &Host, shape: TextBox, format: DXGI_FORMAT, language: Language, scale: u32) -> Option<Buffers> {
        let (width, height) = shape.size(language, scale);
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

    fn write(&mut self, lines: &[String], language: Language) {
        let Some(colours) = colours(self.format) else { return };
        let picture = self.shape.paint(lines, language, self.scale, colours, self.pitch as usize);
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
    painted_language: Language,
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
                LANGUAGE_VALUES[language_choice().load(Ordering::Relaxed)].0.to_owned()
            } else {
                get(i.var)
            }
        }).collect();
        Menu { started: values.clone(), values, at: 0, note: None, changed: true, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false }
    }

    fn say(&mut self, text: Labels) {
        self.note = Some(Note::Text(text));
    }

    fn note_text(&self, language: Language) -> Option<String> {
        Some(match self.note.as_ref()? {
            Note::Text(text) => language.text(*text).to_owned(),
            Note::Exported(name) => format!("{}: {name}.ZIP", language.text(["EXPORTED TO SAVES", "已导出到 SAVES", "已匯出到 SAVES", "EXPORTADA A SAVES"])),
        })
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
    fn rows(&self) -> Vec<usize> {
        (0..ITEMS.len() + ACTIONS.len()).filter(|r| !self.hidden(*r)).collect()
    }

    /// The row shown `step` (+1 / -1) from the one picked, round the ends.
    fn step(&self, step: isize) -> usize {
        let rows = self.rows();
        let n = rows.len() as isize;
        match rows.iter().position(|r| *r == self.at) {
            Some(p) => rows[(p as isize + step).rem_euclid(n) as usize],
            None => rows.iter().copied().find(|r| *r > self.at).unwrap_or(rows[0]),
        }
    }

    fn lines(&self, language: Language) -> Vec<String> {
        if self.picking {
            return self.pick_lines(language);
        }
        let tr = |labels| language.text(labels);
        let rows = self.rows();
        let total = rows.len();
        let at = rows.iter().position(|r| *r == self.at).unwrap_or(0);
        let first = at.saturating_sub(ROWS / 2).min(total.saturating_sub(ROWS));
        let mut out = vec![
            tr(["SETTINGS                       F1 OR ESC CLOSES", "设置                     F1 / ESC 关闭", "設定                     F1 / ESC 關閉", "AJUSTES                     F1 O ESC PARA CERRAR"]).to_owned(),
            tr(["UP/DOWN PICK  LEFT/RIGHT CHANGE  ENTER DOES IT", "上下选择    左右修改    回车确认", "上下選擇    左右調整    Enter 確認", "ARRIBA/ABAJO ELIGE  IZQ./DER. CAMBIA  ENTER CONFIRMA"]).to_owned(),
            String::new(),
        ];
        for &row in &rows[first..(first + ROWS).min(total)] {
            let cursor = if row == self.at { '>' } else { ' ' };
            let line = if let Some(item) = ITEMS.get(row) {
                let value = &self.values[row];
                let shown = shown_value(item, value, language);
                let mark = match (*value != self.started[row], live(item)) {
                    (true, true) => tr([" *LIVE", " *即时", " *即時", " *INMED."]),
                    (true, false) => " *",
                    (false, true) => tr([" LIVE", " 即时", " 即時", " INMED."]),
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
            [same(""); 3]
        };
        out.extend(about.map(|line| tr(line).to_owned()));
        out.push(self.note_text(language).unwrap_or_else(|| {
            if language == Language::English && Language::from_setting(self.value(LANGUAGE)) != Language::English {
                "WINDOWS FONT UNAVAILABLE FOR THE SELECTED LANGUAGE".to_owned()
            } else {
                tr(["* = CHANGED: APPLIES AT THE NEXT START UNLESS LIVE", "* 已修改：即时项立即生效，其余下次启动生效", "* 已變更：即時項目立即生效，其餘下次啟動生效", "* CAMBIO: SOLO LOS AJUSTES INMED. SE APLICAN YA"]).to_owned()
            }
        }));
        out.push(REPO.to_owned());
        out
    }

    /// The list IMPORT SAVE opens, in the box's lines.
    fn pick_lines(&self, language: Language) -> Vec<String> {
        let tr = |text| language.text(text).to_owned();
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
        out.push(self.note_text(language).unwrap_or_else(|| {
            let n = self.imports.len();
            match language {
                Language::English => format!("{n} IN SAVES"),
                Language::SimplifiedChinese => format!("SAVES 中有 {n} 个存档"),
                Language::TraditionalChinese => format!("SAVES 內有 {n} 個存檔"),
                Language::Spanish => format!("{n} PARTIDAS EN SAVES"),
            }
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
        if var == LANGUAGE {
            language_choice().store(language_index(value), Ordering::Relaxed);
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
            self.say(["NO FOLDER FOR THE SETTINGS FILE", "找不到保存设置的目录", "找不到設定檔資料夾", "NO SE ENCUENTRA LA CARPETA DE AJUSTES"]);
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
                    ["SAVED. APPLIES AT THE NEXT START", "已保存。其余设置在下次启动时生效", "已儲存，下次啟動生效", "GUARDADO. SE APLICA AL REINICIAR"]
                } else {
                    // Started some other way (a script, another launcher): the file victory play
                    // reads at every start, which this start did not.
                    ["SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME", "已保存。通过 VICTORY.EXE 启动游戏时生效", "已儲存，透過 VICTORY.EXE 啟動時生效", "GUARDADO. SE APLICA AL ABRIR DESDE VICTORY.EXE"]
                });
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "settings menu: not saved");
                self.say(["COULD NOT SAVE THE SETTINGS FILE", "无法写入设置文件", "無法儲存設定檔", "NO SE PUDO GUARDAR EL ARCHIVO DE AJUSTES"]);
                false
            }
        }
    }

    /// Starts the launcher again in a new console, a few seconds on, and ends this run.
    /// `preload`: the launcher makes every shader first (`VICTORY_PRELOAD=1`, for that start alone).
    fn restart(&mut self, preload: bool) {
        let Some(play) = std::env::var_os("VICTORY_MENU_PLAY") else {
            self.say(["SAVED. START THE LAUNCHER AGAIN YOURSELF", "已保存。请手动重新运行启动器", "已儲存，請手動重新啟動", "GUARDADO. VUELVE A ABRIR EL LANZADOR MANUALMENTE"]);
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
                self.say(["SAVED, BUT THE RESTART FAILED", "已保存，重启失败", "已儲存，重新啟動失敗", "GUARDADO, PERO NO SE PUDO REINICIAR"]);
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
            self.say(["WARMED UP, BUT THE MARK WOULD NOT WRITE: THIS RUN GOES ON", "已预热，但无法写入标记。游戏将继续运行", "預熱完成，但無法寫入標記。遊戲將繼續執行。", "PREPARADO. NO SE PUDO GUARDAR LA MARCA. EL JUEGO SIGUE"]);
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
            self.say(["SAVE FOLDERS NOT KNOWN IN THIS START", "本次启动找不到存档文件夹", "本次啟動找不到存檔資料夾", "NO SE ENCONTRARON LAS CARPETAS DE PARTIDAS"]);
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
                    ["NOTHING TO EXPORT: THE GAME HAS NOT SAVED YET", "无法导出：游戏尚未保存存档", "無法匯出：遊戲尚未儲存存檔", "NADA QUE EXPORTAR: AÚN NO HAY UNA PARTIDA GUARDADA"]
                } else {
                    ["EXPORT FAILED: THE GAME FOLDER CANNOT BE WRITTEN", "导出失败：无法写入游戏目录", "匯出失敗：無法寫入遊戲目錄", "ERROR AL EXPORTAR: NO SE PUEDE ESCRIBIR EN LA CARPETA"]
                });
            }
        }
    }

    /// Stages the save picked to be put in place at the next start, and starts the game again.
    fn import_save(&mut self) {
        let (Some(data), Some(share)) = (crate::save_share::data(), crate::save_share::share()) else {
            self.say(["SAVE FOLDERS NOT KNOWN IN THIS START", "本次启动找不到存档文件夹", "本次啟動找不到存檔資料夾", "NO SE ENCONTRARON LAS CARPETAS DE PARTIDAS"]);
            return;
        };
        let Some(name) = self.imports.get(self.import_at).cloned() else {
            self.say(["NO SAVE TO IMPORT: PUT ONE IN SAVES FIRST", "无可用存档，请先把存档 ZIP 放入 SAVES", "無可用存檔，請先將存檔 ZIP 放入 SAVES", "NO HAY PARTIDAS PARA IMPORTAR. PON UN ZIP EN SAVES"]);
            return;
        };
        if let Err(e) = crate::save_share::stage(&data, &share, &name) {
            tracing::warn!(error = %e, name, "settings menu: save import not staged");
            self.say(if e.kind() == std::io::ErrorKind::NotFound {
                ["IMPORT FAILED: NO SAVE IN THAT ZIP", "导入失败：压缩包内没有存档", "匯入失敗：ZIP 裡沒有存檔", "ERROR AL IMPORTAR: EL ZIP NO CONTIENE UNA PARTIDA"]
            } else {
                ["IMPORT FAILED: THAT ZIP IS DAMAGED OR NOT A SAVE", "导入失败：压缩包已损坏或不含有效存档", "匯入失敗：ZIP 已損毀或不含有效存檔", "ERROR AL IMPORTAR: ZIP DAÑADO O SIN PARTIDA VÁLIDA"]
            });
            return;
        }
        tracing::info!(name, "settings menu: save import staged; put in place at the next start");
        if std::env::var_os("VICTORY_MENU_PLAY").is_some() {
            self.restart(false);
        } else {
            self.say(["IMPORTED: CLOSE THE GAME AND START IT AGAIN", "已导入。请退出游戏并重新启动", "已匯入。請關閉遊戲並重新啟動", "IMPORTADA. CIERRA Y VUELVE A ABRIR EL JUEGO"]);
        }
    }

    /// Removes the settings file: the launcher's own settings at the next start.
    fn reset(&mut self) {
        if let Some(path) = settings_file() {
            let _ = std::fs::remove_file(path);
        }
        self.say(["RESET. PLAY DEFAULTS AT THE NEXT START", "已重置。下次启动将使用默认设置", "已還原預設，下次啟動生效", "RESTABLECIDO. SE APLICA AL REINICIAR"]);
    }

    /// Changes a setting and saves it at once: many players changed settings and closed the
    /// menu without the save lines, and lost them. The save lines stay (and restart, preload).
    fn change_and_save(&mut self, i: usize, step: isize) {
        self.change(i, step);
        if self.save() && live(&ITEMS[i]) {
            self.say(["SAVED. ON NOW", "已保存，立即生效", "設定已儲存並生效", "GUARDADO Y APLICADO"]);
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
                VK_UP => self.at = self.step(-1),
                VK_DOWN => self.at = self.step(1),
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
        let language = language();
        let line = if open { None } else { fps_line(language) };
        if !open && line.is_none() {
            return;
        }
        // Our own copies are not part of a recorded frame (the recorder is in diagnostics builds).
        #[cfg(feature = "diagnostics")]
        let _ours = crate::replay::record::unrecorded();
        let scale = (height / 540).max(1);
        let shape = if open { BOX } else { FPS_BOX };
        let mode = usize::from(language.unicode());
        let storage = if open { &mut self.buffers } else { &mut self.fps_buffers };
        let buffers = &mut storage[mode];
        if buffers.as_ref().is_none_or(|b| (b.format, b.scale) != (format, scale)) {
            std::mem::forget(buffers.take());
            *buffers = Buffers::new(host, shape, format, language, scale);
            self.failed = buffers.is_none();
            self.changed = true;
        }
        if self.painted != line || self.painted_language != language {
            self.painted = line.clone();
            self.painted_language = language;
            self.changed = true;
        }
        let lines = if self.changed { Some(line.as_ref().map_or_else(|| self.lines(language), |l| vec![l.clone()])) } else { None };
        let storage = if open { &mut self.buffers } else { &mut self.fps_buffers };
        let Some(buffers) = &mut storage[mode] else { return };
        if let Some(lines) = lines {
            self.changed = false;
            buffers.write(&lines, language);
        }
        // The whole box in the middle, or of it the first line's part in the corner.
        let (box_width, box_height) = shape.size(language, scale);
        let (w, h) = line.as_ref().map_or((box_width, box_height), |l| {
            shape.area(text_width(l).min(shape.columns * ASCII_WIDTH), 1, language, scale)
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
        let home = tempfile::tempdir().unwrap();
        // SAFETY: no other test in this crate reads these two.
        unsafe {
            std::env::remove_var("VICTORY_MENU_FILE");
            std::env::set_var("VICTORY_HOME", home.path());
        }
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        let dlss = ITEMS.iter().position(|i| i.var == "VICTORY_MENU_DLSS").expect("in the menu");
        menu.change_and_save(dlss, 1);
        let path = home.path().join("victory-settings.txt");
        let text = std::fs::read_to_string(&path).expect("saved without a save line");
        assert!(text.contains("VICTORY_MENU_DLSS=quality"), "{text}");
        assert_eq!(menu.note_text(Language::English).as_deref(), Some("SAVED. APPLIES WHEN VICTORY.EXE STARTS THE GAME"));
        // Every note fits the box and has its letters.
        for note in [menu.note_text(Language::English).as_deref().expect("saved"), "SAVED. ON NOW", "SAVED. APPLIES AT THE NEXT START", "NO FOLDER FOR THE SETTINGS FILE"] {
            assert!(note.len() <= COLUMNS as usize, "{note}");
            assert!(note.bytes().all(|c| glyph(c) != glyph(b'#')), "{note}");
        }
        let language = ITEMS.iter().position(|item| item.var == LANGUAGE).expect("in the menu");
        assert_eq!(language_index(""), Language::English.index());
        assert_eq!(language_index("unrecognized"), Language::English.index());
        for (index, (choice, _)) in LANGUAGE_VALUES.iter().enumerate() {
            assert_eq!(language_index(choice), index);
            (*choice).clone_into(&mut menu.values[language]);
            assert!(menu.save());
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.lines().any(|line| line == format!("{LANGUAGE}={choice}")), "{text}");
        }
        // SAFETY: this test is the only one using these variables.
        unsafe { std::env::remove_var("VICTORY_HOME") };
    }

    fn assert_line_fits(line: &str, columns: u32) {
        assert!(text_width(line) <= columns * ASCII_WIDTH, "{line}");
        for c in line.bytes().filter(u8::is_ascii) {
            assert_ne!(glyph(c.to_ascii_uppercase()), glyph(b'#'), "{line}: {}", c as char);
        }
    }

    #[test]
    fn every_line_fits_and_every_character_has_a_picture() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        let console = ITEMS.iter().position(|item| item.var == CONSOLE).expect("in the menu");
        for language in Language::ALL {
            for (at, item) in ITEMS.iter().enumerate() {
                menu.at = at;
                match item.var {
                    X_RES => "series-x".clone_into(&mut menu.values[console]),
                    S_RES => "series-s".clone_into(&mut menu.values[console]),
                    _ => {}
                }
                for (value, _) in item.values {
                    (*value).clone_into(&mut menu.values[at]);
                    for line in menu.lines(language) {
                        assert_line_fits(&line, COLUMNS);
                    }
                }
            }
            for at in ITEMS.len()..ITEMS.len() + ACTIONS.len() {
                menu.at = at;
                for line in menu.lines(language) {
                    assert_line_fits(&line, COLUMNS);
                }
            }
            for per in [1, 6] {
                let fps = f64::from(u32::MAX) / 10.0;
                assert_line_fits(&format_fps(fps, per, language), FPS_BOX.columns);
            }
            for line in warmup_notice(u64::MAX, language) {
                assert_line_fits(&line, super::super::panel::NOTICE_COLUMNS);
            }
        }
    }

    #[test]
    fn the_link_is_always_right_under_the_note() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        for note in [None, Some(same("SAVED"))] {
            menu.note = note.map(Note::Text);
            for at in 0..ITEMS.len() + ACTIONS.len() {
                menu.at = at;
                let lines = menu.lines(Language::English);
                assert_eq!(lines.len(), LINES as usize);
                let n = lines.len();
                assert!(lines[n - 2].starts_with("* = CHANGED") || note.is_some_and(|note| lines[n - 2] == Language::English.text(note)), "{}", lines[n - 2]);
                assert_eq!(lines[n - 1], REPO);
                // What a preload costs, and what MFG Unlocker is, is said while it is picked, and only then.
                let about = &lines[n - 5..n - 2];
                if at == ITEMS.len() + PRELOAD {
                    assert_eq!(about, ABOUT_PRELOAD.map(|line| Language::English.text(line)));
                } else if at == ITEMS.len() + EXPORT {
                    assert_eq!(about, ABOUT_EXPORT.map(|line| Language::English.text(line)));
                } else if at == ITEMS.len() + IMPORT {
                    assert_eq!(about, ABOUT_IMPORT.map(|line| Language::English.text(line)));
                } else if at < ITEMS.len() && ITEMS[at].var == MFG {
                    assert_eq!(about, ABOUT_MFG.map(|line| Language::English.text(line)));
                } else if at < ITEMS.len() && ITEMS[at].var == S_RES {
                    assert_eq!(about, ABOUT_S_RES.map(|line| Language::English.text(line)));
                } else {
                    assert!(about.iter().all(String::is_empty), "{about:?}");
                }
            }
        }
        assert_eq!(Language::English.text(ACTIONS[PRELOAD]), "PRELOAD SHADERS (RESTARTS THE GAME)");
        assert_eq!(Language::English.text(ACTIONS[EXPORT]), "EXPORT SAVE (TO SAVES IN THE GAME FOLDER)");
        assert_eq!(Language::English.text(ACTIONS[IMPORT]), "IMPORT SAVE (PICK FROM SAVES)");
        let pick = [PICK_TITLE, PICK_HELP, PICK_NONE, PICK_BACK, CONFIRM_IMPORT, CONFIRM_PRELOAD,
            ["IMPORT FAILED: THAT ZIP IS DAMAGED OR NOT A SAVE", "导入失败：压缩包已损坏或不含有效存档", "匯入失敗：ZIP 已損毀或不含有效存檔", "ERROR AL IMPORTAR: ZIP DAÑADO O SIN PARTIDA VÁLIDA"]];
        for lines in [ABOUT_EXPORT, ABOUT_IMPORT, ABOUT_PICK, ABOUT_MFG, ABOUT_NO_DLSS, ABOUT_S_RES].iter().flatten().chain(&pick) {
            for line in *lines {
                assert_line_fits(line, COLUMNS);
            }
        }
        // DLSS picked on where NVIDIA's files are missing (no VICTORY_STREAMLINE_DIR in tests): said so.
        let dlss = ITEMS.iter().position(|i| i.var == DLSS).expect("in the menu");
        menu.at = dlss;
        menu.values[dlss] = "quality".to_owned();
        for language in Language::ALL {
            let lines = menu.lines(language);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_NO_DLSS.map(|line| language.text(line)));
        }
        menu.values[dlss] = "off".to_owned();
        let lines = menu.lines(Language::English);
        assert!(lines[lines.len() - 5..lines.len() - 2].iter().all(String::is_empty));
        assert_eq!(ITEMS.iter().filter(|i| i.var == MFG).count(), 1);
        // Frame generation turned on after a start without it requires a restart.
        let fg = ITEMS.iter().position(|i| i.var == FRAMEGEN).expect("in the menu");
        menu.at = fg;
        menu.values[fg] = "0".to_owned();
        menu.change(fg, 1);
        assert_eq!(menu.values[fg], "2");
        for language in Language::ALL {
            assert_eq!(menu.note_text(language).as_deref(), Some(language.text(RESTART_FRAMEGEN)));
            let lines = menu.lines(language);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_FRAMEGEN_RESTART.map(|line| language.text(line)));
        }
        menu.change(fg, -1);
        assert!(menu.note.is_none());
        let lines = menu.lines(Language::English);
        assert!(lines[lines.len() - 5..lines.len() - 2].iter().all(String::is_empty));
    }

    #[test]
    fn the_picture_line_is_the_consoles() {
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
        menu.started = menu.values.clone();
        let at = |var: &str| ITEMS.iter().position(|i| i.var == var).expect("in the menu");
        let (console, x, s, dlss) = (at(CONSOLE), at(X_RES), at(S_RES), at(DLSS));
        let shown = |menu: &Menu, label: &str| menu.lines(Language::English).iter().any(|l| l.contains(label));
        "series-x".clone_into(&mut menu.values[console]);
        assert!(shown(&menu, "SERIES X PICTURE") && !shown(&menu, "SERIES S PICTURE"));
        // down from the console: the Series X line, never the hidden one
        menu.at = console;
        assert_eq!(menu.step(1), x);
        assert!(!menu.rows().contains(&s));
        "series-s".clone_into(&mut menu.values[console]);
        assert!(shown(&menu, "SERIES S PICTURE") && !shown(&menu, "SERIES X PICTURE"));
        assert_eq!(menu.step(1), s);
        assert_eq!(menu.step(-1), console - 1);
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
        for language in Language::ALL {
            let lines = menu.lines(language);
            assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_S_RES.map(|line| language.text(line)));
        }
    }

    #[test]
    fn values_cycle_both_ways() {
        let mut menu = Menu { values: ITEMS.iter().map(|i| i.values[0].0.to_owned()).collect(), started: Vec::new(), at: 0, note: None, changed: false, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: Vec::new(), import_at: 0, picking: false };
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
        let mut menu = Menu { values: ITEMS.iter().map(|_| String::new()).collect(), started: Vec::new(), at: ITEMS.len() + IMPORT, note: None, changed: false, painted: None, painted_language: Language::English, buffers: [None, None], fps_buffers: [None, None], failed: false, imports: names, import_at: 0, picking: true };
        menu.started = menu.values.clone();
        let check = |menu: &Menu| {
            for language in Language::ALL {
                let lines = menu.lines(language);
                assert_eq!(lines.len(), LINES as usize);
                assert_eq!(lines[lines.len() - 1], REPO);
                assert_eq!(&lines[lines.len() - 5..lines.len() - 2], ABOUT_PICK.map(|line| language.text(line)));
                for line in &lines {
                    assert_line_fits(line, COLUMNS);
                }
            }
            menu.lines(Language::English)
        };
        let lines = check(&menu);
        assert!(lines.iter().any(|l| l == "> SAVE 2026-11-27 23-45-10.ZIP"), "{lines:?}");
        assert!(lines.iter().any(|l| l == &format!("  {}", Language::English.text(PICK_BACK))), "{lines:?}");
        menu.import_at = 1;
        check(&menu);
        menu.import_at = 2;
        assert!(check(&menu).iter().any(|l| l == &format!("> {}", Language::English.text(PICK_BACK))));
        // Many saves: the list scrolls and the box keeps its size.
        menu.imports = (0..50).map(|i| format!("save {i}.zip")).collect();
        for at in [0, 25, 49, 50] {
            menu.import_at = at;
            let lines = check(&menu);
            let picked = if at == 50 { format!("> {}", Language::English.text(PICK_BACK)) } else { format!("> SAVE {at}.ZIP") };
            assert!(lines.contains(&picked), "{at}: {lines:?}");
        }
        menu.imports.clear();
        menu.import_at = 0;
        let lines = check(&menu);
        assert!(lines.iter().any(|l| l == Language::English.text(PICK_NONE)));
        assert!(lines.iter().any(|l| l == &format!("> {}", Language::English.text(PICK_BACK))));
    }
}
