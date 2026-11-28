//! The F1 menu's picture: tabs, rows with toggles and value pickers, a
//! description beside them and the actions below, on a purple gradient, in
//! the system's Bahnschrift (Segoe UI where it is missing) drawn by GDI with
//! smoothed edges. Nothing is shipped with the layer: the fonts are Windows'
//! own. Shapes (bars, lines, toggles) are drawn here with smoothed edges;
//! GDI draws only the text.

use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::core::PCWSTR;

/// The picture's size at `s` (its scale: 1.0 at 1080 lines).
pub fn size(s: f32) -> (u32, u32) {
    ((1180.0 * s).round() as u32, (700.0 * s).round() as u32)
}

/// One row of the list.
pub struct Row {
    pub label: String,
    pub value: Value,
    pub picked: bool,
    /// Changed since the start ("*"), and whether it applies at once.
    pub changed: bool,
}

pub enum Value {
    Toggle(bool),
    /// The value shown, its place among the values (1-based) and how many there are.
    Pick(String, usize, usize),
    /// Only the label (a line of a list, such as the saves to import).
    Plain,
}

/// What the picture shows.
pub struct Page<'a> {
    pub tabs: &'a [&'a str],
    pub tab: usize,
    pub rows: &'a [Row],
    /// The first row shown (the list scrolls).
    pub first: usize,
    pub actions: &'a [&'a str],
    /// The action picked, if the cursor is on the actions.
    pub action: Option<usize>,
    /// The description beside the list: a heading and its lines.
    pub heading: String,
    pub about: Vec<String>,
    pub note: String,
    pub link: &'a str,
}

/// Colours, sRGB 0 to 1.
const BACK_LEFT: [f32; 3] = [0.25, 0.10, 0.33];
const BACK_RIGHT: [f32; 3] = [0.62, 0.30, 0.58];
const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
const INK: [f32; 3] = [0.20, 0.07, 0.26];
const SOFT: [f32; 3] = [0.88, 0.80, 0.90];

/// The rows that fit in the list.
pub const ROWS: usize = 9;
const ROW_H: f32 = 46.0;
const LIST_TOP: f32 = 138.0;
const LIST_LEFT: f32 = 24.0;
const LIST_RIGHT: f32 = 760.0;

struct Canvas {
    w: i32,
    h: i32,
    s: f32,
    /// BGRX pixels, top-down, as GDI writes them.
    bits: *mut u32,
    dc: HDC,
    bitmap: HBITMAP,
}

impl Canvas {
    fn new(w: u32, h: u32, s: f32) -> Option<Canvas> {
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
        let mut bits = std::ptr::null_mut();
        // SAFETY: a memory DC and a DIB section of our own, released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            SelectObject(dc, bitmap.into());
            SetBkMode(dc, TRANSPARENT);
            Some(Canvas { w: w as i32, h: h as i32, s, bits: bits.cast(), dc, bitmap })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the DIB section holds w * h pixels while the canvas lives.
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }

    /// `rgb` over the pixel at `at` by `alpha`.
    fn blend(&mut self, x: i32, y: i32, rgb: [f32; 3], alpha: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || alpha <= 0.0 {
            return;
        }
        let w = self.w;
        let p = &mut self.pixels()[(y * w + x) as usize];
        let old = [(*p >> 16 & 0xFF) as f32 / 255.0, (*p >> 8 & 0xFF) as f32 / 255.0, (*p & 0xFF) as f32 / 255.0];
        let a = alpha.min(1.0);
        let c = |i: usize| ((old[i] + (rgb[i] - old[i]) * a) * 255.0).round() as u32;
        *p = c(0) << 16 | c(1) << 8 | c(2);
    }

    /// The background: a gradient from left to right.
    fn background(&mut self) {
        flush();
        let (w, h) = (self.w, self.h);
        for x in 0..w {
            let rgb = mix(BACK_LEFT, BACK_RIGHT, x as f32 / (w - 1).max(1) as f32);
            let word = pack(rgb);
            for y in 0..h {
                self.pixels()[(y * w + x) as usize] = word;
            }
        }
    }

    /// A rectangle (in units) whose colour fades from `left` at `alpha_left`
    /// to `alpha_right` across it.
    fn bar(&mut self, (x0, y0, x1, y1): (f32, f32, f32, f32), rgb: [f32; 3], alpha_left: f32, alpha_right: f32) {
        flush();
        let s = self.s;
        let (px0, px1) = ((x0 * s).round() as i32, (x1 * s).round() as i32);
        let (py0, py1) = ((y0 * s).round() as i32, (y1 * s).round() as i32);
        for x in px0..px1 {
            let t = (x - px0) as f32 / (px1 - px0 - 1).max(1) as f32;
            let a = alpha_left + (alpha_right - alpha_left) * t;
            for y in py0..py1 {
                self.blend(x, y, rgb, a);
            }
        }
    }

    /// A line one pixel high (at least), in units.
    fn rule(&mut self, x0: f32, x1: f32, y: f32, alpha: f32) {
        let thick = (self.s).max(1.0);
        self.bar((x0, y, x1, y + thick / self.s), WHITE, alpha, alpha);
    }

    /// A rounded rectangle with smoothed edges, filled (or only its outline,
    /// `line` wide, in units).
    fn round_rect(&mut self, (x0, y0, x1, y1): (f32, f32, f32, f32), radius: f32, rgb: [f32; 3], alpha: f32, line: Option<f32>) {
        flush();
        let s = self.s;
        let (x0, y0, x1, y1, r) = (x0 * s, y0 * s, x1 * s, y1 * s, radius * s);
        let line = line.map(|l| (l * s).max(1.0));
        for py in (y0.floor() as i32 - 1)..=(y1.ceil() as i32 + 1) {
            for px in (x0.floor() as i32 - 1)..=(x1.ceil() as i32 + 1) {
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                // Signed distance to the rounded rectangle.
                let qx = (cx - (x0 + x1) / 2.0).abs() - ((x1 - x0) / 2.0 - r);
                let qy = (cy - (y0 + y1) / 2.0).abs() - ((y1 - y0) / 2.0 - r);
                let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                let cover = match line {
                    None => (0.5 - outside).clamp(0.0, 1.0),
                    Some(l) => (0.5 - outside).clamp(0.0, 1.0) * (0.5 + outside + l).clamp(0.0, 1.0),
                };
                self.blend(px, py, rgb, cover * alpha);
            }
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, radius: f32, rgb: [f32; 3]) {
        self.round_rect((cx - radius, cy - radius, cx + radius, cy + radius), radius, rgb, 1.0, None);
    }

    /// `text` in `font`, its left (or, with `right`, its right) edge at `x` and its middle at `y` (units).
    fn text(&mut self, font: &Font, text: &str, x: f32, y: f32, rgb: [f32; 3], right: bool) -> f32 {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        // SAFETY: our DC and font; plain GDI text calls.
        unsafe {
            let old = SelectObject(self.dc, font.0.into());
            let _ = GetTextExtentPoint32W(self.dc, &wide, &mut size);
            SetTextColor(self.dc, colorref(rgb));
            let left = if right { (x * self.s).round() as i32 - size.cx } else { (x * self.s).round() as i32 };
            let top = (y * self.s).round() as i32 - size.cy / 2;
            let _ = TextOutW(self.dc, left, top, &wide);
            SelectObject(self.dc, old);
        }
        size.cx as f32 / self.s
    }

    /// `text` wrapped into the box (units), from its top.
    fn paragraph(&mut self, font: &Font, text: &str, (x0, y0, x1, y1): (f32, f32, f32, f32), rgb: [f32; 3]) -> f32 {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let s = self.s;
        let mut rect = RECT { left: (x0 * s) as i32, top: (y0 * s) as i32, right: (x1 * s) as i32, bottom: (y1 * s) as i32 };
        // SAFETY: our DC and font.
        unsafe {
            let old = SelectObject(self.dc, font.0.into());
            SetTextColor(self.dc, colorref(rgb));
            let height = DrawTextW(self.dc, &mut wide, &mut rect, DT_WORDBREAK | DT_NOPREFIX | DT_LEFT | DT_TOP);
            SelectObject(self.dc, old);
            height as f32 / s
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: ours.
        unsafe {
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Font(HFONT);

impl Font {
    /// Bahnschrift at `px` units high and `weight` (400 regular, 600 semibold), smoothed.
    fn new(px: f32, s: f32, weight: i32) -> Font {
        let face: Vec<u16> = "Bahnschrift".encode_utf16().chain(Some(0)).collect();
        // SAFETY: plain font creation; the face name is terminated.
        let font = unsafe {
            CreateFontW(
                -((px * s).round() as i32),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                PCWSTR(face.as_ptr()),
            )
        };
        Font(font)
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        // SAFETY: ours.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

/// GDI may hold text back; the pixels are written directly after it.
fn flush() {
    // SAFETY: a plain call.
    unsafe {
        let _ = GdiFlush();
    }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn pack(rgb: [f32; 3]) -> u32 {
    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    c(rgb[0]) << 16 | c(rgb[1]) << 8 | c(rgb[2])
}

fn colorref(rgb: [f32; 3]) -> COLORREF {
    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF(c(rgb[0]) | c(rgb[1]) << 8 | c(rgb[2]) << 16)
}

/// Draws `page` at scale `s`: its pixels as 0x00RRGGBB, top-down, `size(s)`.
pub fn draw(page: &Page, s: f32) -> Option<Vec<u32>> {
    let (w, h) = size(s);
    let mut c = Canvas::new(w, h, s)?;
    let (title, tab_font, row_font, small, heading) =
        (Font::new(40.0, s, 600), Font::new(19.0, s, 600), Font::new(22.0, s, 400), Font::new(16.0, s, 400), Font::new(26.0, s, 600));
    let width = w as f32 / s;
    c.background();

    // Title and tabs.
    c.text(&title, "SETTINGS", 36.0, 44.0, WHITE, false);
    let mut x = 36.0;
    let (tab_y0, tab_y1) = (78.0, 112.0);
    c.round_rect((x - 6.0, tab_y0 + 6.0, x + 24.0, tab_y1 - 6.0), 4.0, WHITE, 0.9, None);
    let q = c.text(&small, "Q", -10_000.0, -10_000.0, INK, false);
    c.text(&small, "Q", x + 9.0 - q / 2.0, (tab_y0 + tab_y1) / 2.0, INK, false);
    x += 38.0;
    for (i, name) in page.tabs.iter().enumerate() {
        let probe = c.text(&tab_font, name, -10_000.0, -10_000.0, WHITE, false);
        let (x0, x1) = (x, x + probe + 28.0);
        if i == page.tab {
            c.bar((x0, tab_y0, x1, tab_y1), WHITE, 1.0, 1.0);
            c.text(&tab_font, name, x0 + 14.0, (tab_y0 + tab_y1) / 2.0, INK, false);
        } else {
            c.text(&tab_font, name, x0 + 14.0, (tab_y0 + tab_y1) / 2.0, WHITE, false);
            if i + 1 != page.tab && i + 1 < page.tabs.len() {
                c.bar((x1 - 1.0, tab_y0 + 8.0, x1, tab_y1 - 8.0), WHITE, 0.45, 0.45);
            }
        }
        x = x1;
    }
    c.round_rect((x + 8.0, tab_y0 + 6.0, x + 38.0, tab_y1 - 6.0), 4.0, WHITE, 0.9, None);
    let e = c.text(&small, "E", -10_000.0, -10_000.0, INK, false);
    c.text(&small, "E", x + 23.0 - e / 2.0, (tab_y0 + tab_y1) / 2.0, INK, false);
    c.rule(LIST_LEFT, width - 24.0, 124.0, 0.55);

    // The list.
    let shown = page.rows.iter().enumerate().skip(page.first).take(ROWS);
    for (n, (_, row)) in shown.enumerate() {
        let y0 = LIST_TOP + n as f32 * ROW_H;
        let mid = y0 + ROW_H / 2.0;
        if row.picked {
            // A white bar fading out to the right, as the title's own menus have it.
            c.bar((LIST_LEFT, y0 + 1.0, LIST_RIGHT, y0 + ROW_H - 1.0), WHITE, 1.0, 0.25);
        }
        let ink = if row.picked { INK } else { WHITE };
        let label = if row.changed { format!("{} *", row.label) } else { row.label.clone() };
        c.text(&row_font, &label, LIST_LEFT + 16.0, mid, ink, false);
        match &row.value {
            Value::Toggle(on) => {
                let (x0, x1) = (LIST_RIGHT - 64.0, LIST_RIGHT - 16.0);
                let (t0, t1) = (mid - 12.0, mid + 12.0);
                c.text(&row_font, if *on { "On" } else { "Off" }, x0 - 14.0, mid, ink, true);
                if *on {
                    c.round_rect((x0, t0, x1, t1), 12.0, ink, 1.0, None);
                    c.circle(x1 - 12.0, mid, 8.5, if row.picked { WHITE } else { BACK_LEFT });
                } else {
                    c.round_rect((x0, t0, x1, t1), 12.0, ink, 0.18, None);
                    c.round_rect((x0, t0, x1, t1), 12.0, ink, 0.9, Some(2.0));
                    c.circle(x0 + 12.0, mid, 8.0, ink);
                }
            }
            Value::Pick(value, at, of) => {
                c.text(&row_font, ">", LIST_RIGHT - 18.0, mid, ink, true);
                c.text(&row_font, "<", LIST_RIGHT - 44.0, mid, ink, true);
                let count = format!("{at} / {of}");
                let wc = c.text(&small, &count, LIST_RIGHT - 66.0, mid, if row.picked { INK } else { SOFT }, true);
                c.text(&row_font, value, LIST_RIGHT - 84.0 - wc, mid, ink, true);
            }
            Value::Plain => {}
        }
        if !row.picked {
            c.rule(LIST_LEFT, LIST_RIGHT, y0 + ROW_H - 1.0, 0.28);
        }
    }
    // A scroll bar when not every row fits.
    if page.rows.len() > ROWS {
        let (top, height) = (LIST_TOP, ROWS as f32 * ROW_H);
        c.round_rect((LIST_RIGHT + 6.0, top, LIST_RIGHT + 9.0, top + height), 1.5, WHITE, 0.2, None);
        let part = height * ROWS as f32 / page.rows.len() as f32;
        let at = top + height * page.first as f32 / page.rows.len() as f32;
        c.round_rect((LIST_RIGHT + 5.0, at, LIST_RIGHT + 10.0, at + part), 2.5, WHITE, 0.85, None);
    }

    // The description.
    let (dx0, dx1) = (800.0, width - 36.0);
    c.text(&heading, &page.heading, dx0, LIST_TOP + 18.0, WHITE, false);
    c.rule(dx0, dx1, LIST_TOP + 40.0, 0.7);
    let mut y = LIST_TOP + 56.0;
    for line in &page.about {
        y += c.paragraph(&row_font, line, (dx0, y, dx1, y + 200.0), WHITE) + 10.0;
    }

    // The actions.
    let action_y = LIST_TOP + ROWS as f32 * ROW_H + 30.0;
    let mut x = LIST_LEFT + 12.0;
    for (i, name) in page.actions.iter().enumerate() {
        let probe = c.text(&tab_font, name, -10_000.0, -10_000.0, WHITE, false);
        let (x0, x1) = (x, x + probe + 32.0);
        let (y0, y1) = (action_y - 19.0, action_y + 19.0);
        if page.action == Some(i) {
            c.round_rect((x0, y0, x1, y1), 19.0, WHITE, 1.0, None);
            c.text(&tab_font, name, x0 + 16.0, action_y, INK, false);
        } else {
            c.round_rect((x0, y0, x1, y1), 19.0, WHITE, 0.75, Some(1.5));
            c.text(&tab_font, name, x0 + 16.0, action_y, WHITE, false);
        }
        x = x1 + 12.0;
    }

    // The note and where this build comes from.
    let foot = h as f32 / s - 28.0;
    c.text(&small, &page.note, LIST_LEFT + 12.0, foot, WHITE, false);
    c.text(&small, page.link, width - 36.0, foot, SOFT, true);

    // SAFETY: GDI is done with the bits before they are read.
    unsafe {
        let _ = GdiFlush();
    }
    Some(c.pixels().iter().map(|p| p & 0x00FF_FFFF).collect())
}
