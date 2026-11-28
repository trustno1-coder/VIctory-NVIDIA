//! The run's numbers on the picture, in every build (`VICTORY_PANEL=1`; off
//! by default; F3 or the settings menu shows and hides it while our window
//! has the focus; the time spent recording is counted in diagnostics builds
//! only): white
//! text on a black box in the upper left corner of what is shown, so that a
//! screenshot carries them, with what it costs itself on the submitting
//! thread. Made again twice a second from the counters of
//! the frame-rate line, memory and textures; a frame itself only copies the
//! box out of a buffer in the card's memory. Copied onto the swap chain's
//! buffer, behind the title's picture: saved frames and the frame recorder
//! see the title's own.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F3};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::font::{ASCII_WIDTH, TextBox, colours, width as text_width};
use super::{Host, HostList, transition};

/// How often the numbers change.
const EVERY: Duration = Duration::from_millis(500);

const PANEL: TextBox = TextBox { columns: 40, lines: 10, rim: 3 };

/// The texts are put into one of these in turn: the card reads the others,
/// at most half a second old.
const UPLOADS: usize = 4;

fn asked() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_PANEL"))
}

/// Whether it is shown now.
pub(super) fn shown() -> &'static AtomicBool {
    static SHOWN: OnceLock<AtomicBool> = OnceLock::new();
    SHOWN.get_or_init(|| AtomicBool::new(asked()))
}

/// F3 was pressed since the last call, with one of our windows in front.
fn pressed() -> bool {
    static DOWN: AtomicBool = AtomicBool::new(false);
    // SAFETY: a plain query.
    let down = unsafe { GetAsyncKeyState(i32::from(VK_F3)) } < 0;
    if DOWN.swap(down, Ordering::Relaxed) || !down {
        return false;
    }
    let mut process = 0;
    // SAFETY: plain queries; room for the answer.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut process) };
    process == std::process::id()
}

/// Pixels to a dot for a picture that high: a 720-line screenshot of it
/// shows one and a third.
fn scale(height: u32) -> u32 {
    (height / 540).max(1)
}

/// The counters read for it. They grow, and go back to 0 with the frame-rate
/// line (right behind a look of ours).
#[derive(Clone, Copy, Default)]
struct Counters([u64; COUNTERS]);

const COUNTERS: usize = 11;

impl Counters {
    fn now() -> Counters {
        use super::{CARD_CLOCK_US, COPY_BACK, PREPARING};
        #[cfg(feature = "diagnostics")]
        let recording = crate::copystat::peek(crate::copystat::C::RecordNs);
        #[cfg(not(feature = "diagnostics"))]
        let recording = 0;
        let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        Counters([
            super::worked(),
            get(&CARD_CLOCK_US[0]),
            get(&CARD_CLOCK_US[1]),
            get(&CARD_CLOCK_US[2]),
            get(&PREPARING[3]),
            get(&COPY_BACK[0]),
            get(&COPY_BACK[1]),
            get(&crate::shadow::CARD_STATS[0]),
            crate::draw::dropped_now(),
            recording,
            crate::draw::drawn(),
        ])
    }

    /// What came since `before`, per frame.
    fn since(self, before: Counters, frames: f64) -> [f64; COUNTERS] {
        let mut out = [0.0; COUNTERS];
        for (i, out) in out.iter_mut().enumerate() {
            let (was, is) = (before.0[i], self.0[i]);
            *out = if is >= was { is - was } else { is } as f64 / frames;
        }
        out
    }
}

/// The process's memory in bytes: in physical memory, and committed
/// (private bytes).
fn process_memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.WorkingSetSize as u64, c.PagefileUsage as u64)
}

/// The frame generation line: `per` frames shown for each one rendered.
fn generated(fps: f64, per: u32, chinese: bool) -> String {
    match (per, chinese) {
        (0 | 1, false) => "FRAME GEN OFF".to_owned(),
        (0 | 1, true) => "帧生成关闭".to_owned(),
        (per, false) => format!("FRAME GEN {per}X  SHOWN {:.1} FPS", fps * f64::from(per)),
        (per, true) => format!("帧生成 {per}X  显示帧率 {:.1}", fps * f64::from(per)),
    }
}

/// The text. `c`: [`Counters`] per frame, `ms` a frame's time, `worst` the
/// longest frame's, `cost` what this takes of a frame, in microseconds.
fn text(c: &[f64; COUNTERS], ms: f64, worst: f64, cost: f64, host: &Host, chinese: bool) -> Vec<String> {
    let gb = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    let (working, private) = process_memory();
    let [budget, used, _, shared] = host.video_memory().unwrap_or_default();
    let idle = (c[1] / 1e3).min(ms);
    let fps = 1e3 / ms.max(0.001);
    let generated = generated(fps, crate::streamline::shown_per_frame(), chinese);
    if chinese {
        return vec![
            format!("帧率 {fps:.1}  帧时 {ms:.1} MS  最长 {worst:.0}"),
            generated,
            format!("绘制 {:.0}  跳过 {:.0}", c[10], c[8]),
            format!("提交 {:.1}  等待 GPU {:.1}", c[0] / 1e3, c[3] / 1e3),
            format!("GPU 忙碌 {:.1}  后续空闲 {:.1}", ms - idle, c[2] / 1e3),
            if cfg!(feature = "diagnostics") {
                format!("录制 {:.0} MS  回拷 {:.1}", c[9] / 1e6, c[5] / 1e3)
            } else {
                format!("回拷 {:.1} MS", c[5] / 1e3)
            },
            format!("上传 {:.0}  纹理 {:.0}  回拷 {:.0} MB", c[7] / 1e6, c[4] / 1e6, c[6] / 1e6),
            format!("内存 {:.1} GB  常驻 {:.1}", gb(private), gb(working)),
            format!("显存 {:.1}/{:.1} GB  共享 {:.1}", gb(used), gb(budget), gb(shared)),
            format!("面板开销 {:.0} 微秒/帧", cost),
        ];
    }
    vec![
        format!("FPS {fps:.1}  FRAME {ms:.1} MS  LONGEST {worst:.0}"),
        generated,
        format!("DRAWS {:.0}  DROPPED {:.0}", c[10], c[8]),
        format!("SUBMIT BUSY {:.1}  WAITS FOR GPU {:.1}", c[0] / 1e3, c[3] / 1e3),
        format!("GPU BUSY {:.1}  IDLE BEHIND US {:.1}", ms - idle, c[2] / 1e3),
        if cfg!(feature = "diagnostics") {
            format!("RECORDING {:.0} MS  COPY BACK {:.1}", c[9] / 1e6, c[5] / 1e3)
        } else {
            format!("COPY BACK {:.1} MS", c[5] / 1e3)
        },
        format!("MB UP {:.0}  TEXTURES {:.0}  BACK {:.0}", c[7] / 1e6, c[4] / 1e6, c[6] / 1e6),
        format!("RAM {:.1} GB  IN MEMORY {:.1}", gb(private), gb(working)),
        format!("VRAM {:.1}/{:.1} GB  SHARED {:.1}", gb(used), gb(budget), gb(shared)),
        format!("THIS PANEL {:.0} US A FRAME", cost),
    ]
}

/// The buffers of a box that size and format.
struct Buffers {
    shape: TextBox,
    chinese: bool,
    format: DXGI_FORMAT,
    scale: u32,
    pitch: u32,
    uploads: Vec<(ID3D12Resource, *mut u8)>,
    card: ID3D12Resource,
    /// The upload written last.
    turn: usize,
    /// It is not in the card's buffer yet.
    fresh: bool,
}

// SAFETY: free-threaded resources, and memory only written under the presenter's lock.
unsafe impl Send for Buffers {}

impl Buffers {
    fn new(host: &Host, shape: TextBox, format: DXGI_FORMAT, chinese: bool, scale: u32) -> Option<Buffers> {
        let (width, height) = shape.size(chinese, scale);
        let pitch = (width * colours(format)?[0].len() as u32).next_multiple_of(256);
        let bytes = u64::from(pitch) * u64::from(height);
        let uploads = (0..UPLOADS).map(|_| host.upload_buffer(bytes)).collect::<Option<Vec<_>>>()?;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
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
        Some(Buffers { shape, chinese, format, scale, pitch, uploads, card: card?, turn: 0, fresh: false })
    }

    fn write(&mut self, lines: &[String]) {
        let Some(colours) = colours(self.format) else { return };
        let picture = self.shape.paint(lines, self.chinese, self.scale, colours, self.pitch as usize);
        self.turn = (self.turn + 1) % UPLOADS;
        // SAFETY: a mapped upload buffer of the picture's size, which the card read a second or more ago.
        unsafe { std::ptr::copy_nonoverlapping(picture.as_ptr(), self.uploads[self.turn].1, picture.len()) };
        self.fresh = true;
    }

    /// Records the copy of the box onto `back` (of `width` x `height`, in the
    /// state to be copied into), its upper left corner at `at`.
    fn copy(&mut self, list: &ID3D12GraphicsCommandList, back: &ID3D12Resource, at: (u32, u32), (width, height): (u32, u32)) {
        let (w, h) = self.shape.size(self.chinese, self.scale);
        if at.0 >= width || at.1 >= height {
            return;
        }
        // SAFETY: an open list and live resources; the box lies within both.
        unsafe {
            if std::mem::take(&mut self.fresh) {
                list.CopyResource(&self.card, &self.uploads[self.turn].0);
                list.ResourceBarrier(&[transition(&self.card, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            }
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.card),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: self.format,
                            Width: w,
                            Height: h,
                            Depth: 1,
                            RowPitch: self.pitch,
                        },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let part = D3D12_BOX { left: 0, top: 0, front: 0, right: w.min(width - at.0), bottom: h.min(height - at.1), back: 1 };
            list.CopyTextureRegion(&to, at.0, at.1, 0, &from, Some(&part));
        }
    }
}

/// The panel of a presenter.
pub(super) struct Panel {
    since: Instant,
    frames: u32,
    last: Instant,
    worst: f64,
    /// What this took since the last text.
    cost: Duration,
    before: Counters,
    lines: Vec<String>,
    chinese: bool,
    changed: bool,
    buffers: [Option<Buffers>; 2],
    failed: bool,
}

impl Panel {
    pub(super) fn new() -> Panel {
        let now = Instant::now();
        Panel {
            since: now,
            frames: 0,
            last: now,
            worst: 0.0,
            cost: Duration::ZERO,
            before: Counters::default(),
            lines: Vec::new(),
            chinese: false,
            changed: false,
            buffers: [None, None],
            failed: false,
        }
    }

    /// A frame was presented; `line`: the counters go back to 0 right behind
    /// this call. (Not shown it asks for the key's state and no more.)
    pub(super) fn frame(&mut self, line: bool) {
        if pressed() {
            shown().fetch_xor(true, Ordering::Relaxed);
        }
        if !shown().load(Ordering::Relaxed) {
            self.frames = 0;
            return;
        }
        let now = Instant::now();
        if self.frames == 0 && now - self.last > 2 * EVERY {
            // (Shown from here on.)
            (self.since, self.last, self.worst, self.before) = (now, now, 0.0, Counters::now());
            self.frames = 0;
        }
        self.frames += 1;
        self.worst = self.worst.max((now - self.last).as_secs_f64() * 1e3);
        self.last = now;
        let elapsed = now - self.since;
        if elapsed < EVERY && !(line && elapsed * 2 >= EVERY) {
            return;
        }
        let Some(host) = super::host() else { return };
        let frames = f64::from(self.frames);
        let counters = Counters::now();
        let c = counters.since(self.before, frames);
        self.before = counters;
        let cost = self.cost.as_secs_f64() * 1e6 / frames;
        self.chinese = super::menu::is_chinese();
        self.lines = text(&c, elapsed.as_secs_f64() * 1e3 / frames, self.worst, cost, host, self.chinese);
        self.changed = true;
        (self.since, self.frames, self.worst) = (now, 0, 0.0);
        self.cost = now.elapsed();
    }

    /// Records the copy of the box onto `back`, a buffer of the swap chain of
    /// that size and format in the state to be copied into.
    pub(super) fn draw(
        &mut self,
        host: &Host,
        list: &HostList,
        back: &ID3D12Resource,
        (width, height): (u32, u32),
        format: DXGI_FORMAT,
    ) {
        if !shown().load(Ordering::Relaxed) || self.lines.is_empty() || self.failed {
            return;
        }
        let began = Instant::now();
        // Not the title's: the frame recorder leaves it out.
        #[cfg(feature = "diagnostics")]
        let _ours = crate::replay::record::unrecorded();
        let scale = scale(height);
        let buffers = &mut self.buffers[usize::from(self.chinese)];
        if buffers.as_ref().is_none_or(|b| (b.format, b.scale) != (format, scale)) {
            // (What the card may still read stays: a picture's size changes once or twice in a run.)
            std::mem::forget(buffers.take());
            *buffers = Buffers::new(host, PANEL, format, self.chinese, scale);
            self.failed = buffers.is_none();
            self.changed = true;
        }
        let Some(buffers) = buffers else { return };
        if std::mem::take(&mut self.changed) {
            buffers.write(&self.lines);
        }
        buffers.copy(&list.list, back, (0, 0), (width, height));
        self.cost += began.elapsed();
    }
}

/// How long the start's notice stays.
const NOTICE_FOR: Duration = Duration::from_secs(30);
/// The longest notice line, in characters.
const NOTICE_COLUMNS: u32 = 52;

/// What this card does for frame generation, said at the top of the picture
/// for the first [`NOTICE_FOR`] of a run (`crate::streamline::notice`); in
/// every build, nothing without a notice.
pub(super) struct Notice {
    lines: [Vec<String>; 2],
    since: Option<Instant>,
    buffers: [Option<Buffers>; 2],
    done: bool,
}

impl Notice {
    pub(super) fn new() -> Notice {
        // The first start of a new victory.exe says first that it restarts by itself.
        let warmup = super::menu::warmup_seconds();
        let lines = [false, true].map(|chinese| {
            warmup.into_iter().flat_map(|seconds| super::menu::warmup_notice(seconds, chinese))
                .chain(crate::streamline::notice_in(chinese)).collect::<Vec<_>>()
        });
        Notice { done: lines[0].is_empty(), lines, since: None, buffers: [None, None] }
    }

    /// Records the copy of the notice onto `back` (as [`Panel::draw`]), at
    /// the top in the middle, while it is due.
    pub(super) fn draw(&mut self, host: &Host, list: &HostList, back: &ID3D12Resource, (width, height): (u32, u32), format: DXGI_FORMAT) {
        if self.done {
            return;
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() > NOTICE_FOR {
            // (Its buffers stay: the card may still read them.)
            for buffers in &mut self.buffers {
                std::mem::forget(buffers.take());
            }
            self.done = true;
            return;
        }
        #[cfg(feature = "diagnostics")]
        let _ours = crate::replay::record::unrecorded();
        let scale = scale(height);
        let chinese = super::menu::is_chinese();
        let lines = &self.lines[usize::from(chinese)];
        let buffers = &mut self.buffers[usize::from(chinese)];
        if buffers.as_ref().is_none_or(|b| (b.format, b.scale) != (format, scale)) {
            std::mem::forget(buffers.take());
            let columns = lines.iter().map(|l| text_width(l).div_ceil(ASCII_WIDTH)).max().unwrap_or(0).min(NOTICE_COLUMNS);
            let shape = TextBox { columns, lines: lines.len() as u32, rim: 3 };
            let Some(mut made) = Buffers::new(host, shape, format, chinese, scale) else {
                self.done = true;
                return;
            };
            made.write(lines);
            *buffers = Some(made);
        }
        let Some(buffers) = buffers else { return };
        let (w, _) = buffers.shape.size(chinese, scale);
        let at = (width.saturating_sub(w) / 2, 24 * scale);
        buffers.copy(&list.list, back, at, (width, height));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::font::glyph;

    /// Every character the lines use has its own picture, and no two look
    /// alike.
    #[test]
    fn characters_differ() {
        let used: Vec<u8> = (b'0'..=b'9').chain(b'A'..=b'Z').chain(*b"./- ").collect();
        for (i, a) in used.iter().enumerate() {
            assert_ne!(glyph(*a), glyph(b'#'), "{} has a picture", *a as char);
            assert!(glyph(*a).iter().all(|row| row >> 5 == 0));
            for b in &used[i + 1..] {
                assert_ne!(glyph(*a), glyph(*b), "{} {}", *a as char, *b as char);
            }
        }
    }

    /// The frame generation line fits the box and says what is shown.
    #[test]
    fn generated_frames_are_told() {
        assert_eq!(generated(27.5, 1, false), "FRAME GEN OFF");
        assert_eq!(generated(27.5, 6, false), "FRAME GEN 6X  SHOWN 165.0 FPS");
        assert_eq!(generated(27.5, 6, true), "帧生成 6X  显示帧率 165.0");
        for chinese in [false, true] {
            assert!(text_width(&generated(999.9, 6, chinese)) <= PANEL.columns * ASCII_WIDTH);
        }
    }

    #[test]
    fn an_english_box_is_painted() {
        let lines = ["I".to_owned(), " i".to_owned()];
        let rim = PANEL.rim;
        for scale in [1, 2, 4] {
            let (width, height) = PANEL.area(PANEL.columns * ASCII_WIDTH, lines.len() as u32, false, scale);
            let pitch = (width * 4).next_multiple_of(256) as usize;
            let colours = colours(DXGI_FORMAT_R10G10B10A2_UNORM).unwrap();
            let picture = PANEL.paint(&lines, false, scale, colours, pitch);
            assert_eq!(picture.len(), pitch * height as usize);
            let at = |x: u32, y: u32| &picture[y as usize * pitch + x as usize * 4..][..4];
            // The rim is black, the first letter's top row lit in its middle.
            assert_eq!(at(0, 0), colours[0]);
            assert_eq!(at((rim + 2) * scale, rim * scale), colours[1]);
            assert_eq!(at((rim + 2) * scale + scale - 1, rim * scale + scale - 1), colours[1]);
            assert_eq!(at(rim * scale, rim * scale), colours[0]);
            // Small letters are shown as capitals, a line and a cell on.
            assert_eq!(at((rim + ASCII_WIDTH + 2) * scale, (rim + 9) * scale), colours[1]);
            // Nothing past a line's end.
            assert_eq!(at((rim + ASCII_WIDTH * 2 + 2) * scale, (rim + 9) * scale), colours[0]);
        }
    }

    #[test]
    fn counters_that_went_back_to_nothing_count_from_there() {
        let (mut a, mut b) = (Counters::default(), Counters::default());
        (a.0[0], b.0[0]) = (10, 30);
        (a.0[1], b.0[1]) = (50, 20);
        let c = b.since(a, 10.0);
        assert_eq!((c[0], c[1]), (2.0, 2.0));
    }
}
