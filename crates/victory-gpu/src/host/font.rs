//! The dot letters the stats panel (`panel.rs`, diagnostics builds) and the settings menu
//! (`menu.rs`, every build) are written with, and the pixels they are painted in.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::core::{PCWSTR, w};

use super::language::Language;

pub(crate) const ASCII_WIDTH: u32 = 6;
const WIDE_WIDTH: u32 = 14;
const ASCII_HEIGHT: u32 = 9;
const WIDE_HEIGHT: u32 = 16;
type Bitmap = [u16; WIDE_HEIGHT as usize];

const fn advance(c: char) -> u32 {
    if c.is_ascii() { ASCII_WIDTH } else { WIDE_WIDTH }
}

/// How many dots a line uses, before scaling.
pub(crate) fn width(line: &str) -> u32 {
    line.chars().map(advance).sum()
}

/// Seven rows of five dots, the leftmost the highest bit.
#[rustfmt::skip]
pub(super) const fn glyph(c: u8) -> [u8; 7] {
    match c {
        b'0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        b'1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        b'3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        b'4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        b'5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        b'6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        b'7' => [0b11111, 0b00001, 0b00010, 0b00010, 0b00100, 0b00100, 0b00100],
        b'8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        b'9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        b'A' => [0b00100, 0b01010, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001],
        b'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        b'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        b'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        b'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        b'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        b'G' => [0b01111, 0b10000, 0b10000, 0b10011, 0b10001, 0b10001, 0b01111],
        b'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        b'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        b'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        b'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        b'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        b'N' => [0b10001, 0b11001, 0b11001, 0b10101, 0b10011, 0b10011, 0b10001],
        b'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        b'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        b'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        b'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        b'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        b'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        b'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        b'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        b'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        b'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        b'.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        b'/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        b'-' => [0, 0, 0, 0b11111, 0, 0, 0],
        // For the settings menu (menu.rs).
        b'>' => [0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000],
        b'<' => [0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010],
        b'*' => [0, 0b00100, 0b10101, 0b01110, 0b10101, 0b00100, 0],
        b':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        b'_' => [0, 0, 0, 0, 0, 0, 0b11111],
        b'=' => [0, 0, 0b11111, 0, 0b11111, 0, 0],
        b'(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        b')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        b'+' => [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
        b',' => [0, 0, 0, 0, 0b01100, 0b00100, 0b01000],
        b'!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        b'?' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0, 0b00100],
        b' ' => [0; 7],
        _ => [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
    }
}

struct SystemFonts {
    dc: HDC,
    faces: [HFONT; 4],
}

impl SystemFonts {
    fn new(language: Language) -> Option<Self> {
        // SAFETY: these create GDI objects used and freed on this thread.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let yahei = w!("Microsoft YaHei UI");
            let jhenghei = w!("Microsoft JhengHei UI");
            let simsun = w!("SimSun");
            let segoe = w!("Segoe UI");
            let names = match language {
                Language::TraditionalChinese => [jhenghei, yahei, simsun, segoe],
                Language::Spanish => [segoe, yahei, jhenghei, simsun],
                _ => [yahei, simsun, jhenghei, segoe],
            };
            let faces = names.map(|name| {
                CreateFontW(
                    -14,
                    0,
                    0,
                    0,
                    FW_NORMAL.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_TT_ONLY_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    DEFAULT_QUALITY,
                    0,
                    name,
                )
            });
            Some(Self { dc, faces })
        }
    }

    fn bitmap(&self, c: char) -> Option<Bitmap> {
        let code = u16::try_from(u32::from(c)).ok()?;
        let one = FIXED { fract: 0, value: 1 };
        let transform = MAT2 {
            eM11: one,
            eM22: one,
            ..Default::default()
        };
        for face in self.faces {
            if face.is_invalid() {
                continue;
            }
            let mut index = 0;
            let mut metrics = GLYPHMETRICS::default();
            let mut bits = [0u8; 128];
            // SAFETY: this context belongs to this paint; the input and output buffers stay live.
            let read = unsafe {
                let old = SelectObject(self.dc, HGDIOBJ(face.0));
                if old.is_invalid() {
                    continue;
                }
                let count = GetGlyphIndicesW(
                    self.dc,
                    PCWSTR(&code),
                    1,
                    &mut index,
                    GGI_MARK_NONEXISTING_GLYPHS,
                );
                let bytes = if count == 1 && index != u16::MAX {
                    let format = GET_GLYPH_OUTLINE_FORMAT(GGO_BITMAP.0 | GGO_GLYPH_INDEX.0);
                    GetGlyphOutlineW(
                        self.dc,
                        u32::from(index),
                        format,
                        &mut metrics,
                        bits.len() as u32,
                        Some(bits.as_mut_ptr().cast()),
                        &transform,
                    )
                } else {
                    0
                };
                let _ = SelectObject(self.dc, old);
                bytes
            };
            if read == 0 || read as usize > bits.len() {
                continue;
            }
            let pitch = metrics.gmBlackBoxX.div_ceil(32) as usize * 4;
            if pitch == 0 {
                continue;
            }
            let left = WIDE_WIDTH.saturating_sub(metrics.gmBlackBoxX) / 2;
            let top = (13 - metrics.gmptGlyphOrigin.y).max(0) as u32;
            let mut bitmap = [0; WIDE_HEIGHT as usize];
            for (y, row) in bits[..read as usize].chunks_exact(pitch).enumerate() {
                let Some(dest) = bitmap.get_mut(top as usize + y) else {
                    break;
                };
                for x in 0..metrics.gmBlackBoxX.min(WIDE_WIDTH) {
                    if row[(x / 8) as usize] & (0x80 >> (x % 8)) != 0 {
                        *dest |= 0x8000 >> (left + x);
                    }
                }
            }
            return Some(bitmap);
        }
        None
    }
}

impl Drop for SystemFonts {
    fn drop(&mut self) {
        // SAFETY: all fonts have been deselected and both kinds of handles are ours.
        unsafe {
            for face in self.faces.into_iter().filter(|face| !face.is_invalid()) {
                let _ = DeleteObject(HGDIOBJ(face.0));
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

pub(super) fn supports(language: Language) -> bool {
    static PRESENT: [OnceLock<bool>; Language::ALL.len()] = [const { OnceLock::new() }; Language::ALL.len()];
    let probe = match language {
        Language::English => return true,
        Language::SimplifiedChinese => '简',
        Language::TraditionalChinese => '體',
        Language::Spanish => 'Ñ',
    };
    *PRESENT[language.index()].get_or_init(|| SystemFonts::new(language).and_then(|fonts| fonts.bitmap(probe)).is_some())
}

/// Dimensions shared by the menu and stats overlays, in unscaled dots.
#[derive(Clone, Copy)]
pub(super) struct TextBox {
    pub columns: u32,
    pub lines: u32,
    pub rim: u32,
}

impl TextBox {
    pub(super) const fn size(self, language: Language, scale: u32) -> (u32, u32) {
        self.area(self.columns * ASCII_WIDTH, self.lines, language, scale)
    }

    pub(super) const fn area(self, width: u32, lines: u32, language: Language, scale: u32) -> (u32, u32) {
        let row = if language.unicode() { WIDE_HEIGHT } else { ASCII_HEIGHT };
        ((width + 2 * self.rim) * scale, (lines * row + 2 * self.rim) * scale)
    }

    pub(super) fn paint(self, lines: &[String], language: Language, scale: u32, [black, white]: [&[u8]; 2], pitch: usize) -> Vec<u8> {
        static GLYPHS: [OnceLock<Mutex<HashMap<char, Bitmap>>>; Language::ALL.len()] = [const { OnceLock::new() }; Language::ALL.len()];
        let unicode = language.unicode();
        let mut cache = unicode.then(|| GLYPHS[language.index()].get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner()));
        let mut fonts = None;
        let (width, height) = self.area(self.columns * ASCII_WIDTH, lines.len().min(self.lines as usize) as u32, language, scale);
        let bytes = black.len();
        let mut out = vec![0; pitch * height as usize];
        for pixel in out[..width as usize * bytes].chunks_exact_mut(bytes) {
            pixel.copy_from_slice(black);
        }
        for y in 1..height as usize {
            out.copy_within(..pitch, y * pitch);
        }

        let height = if unicode { WIDE_HEIGHT } else { ASCII_HEIGHT };
        for (y, line) in lines.iter().take(self.lines as usize).enumerate() {
            let mut x = 0;
            for c in line.chars() {
                let c = if !unicode && !c.is_ascii() { '?' } else { c };
                let steps = advance(c);
                if x + steps > self.columns * ASCII_WIDTH {
                    break;
                }
                let bitmap = if let Some(cache) = cache.as_mut().filter(|_| !c.is_ascii()) {
                    *cache.entry(c).or_insert_with(|| {
                        fonts.get_or_insert_with(|| SystemFonts::new(language)).as_ref()
                            .and_then(|fonts| fonts.bitmap(c))
                            .unwrap_or_else(|| ascii(b'?', true))
                    })
                } else {
                    ascii(c as u8, unicode)
                };
                for (dy, row) in bitmap[..height as usize].iter().enumerate() {
                    for dx in 0..steps {
                        if row & (0x8000 >> dx) != 0 {
                            let top = (self.rim + y as u32 * height + dy as u32) * scale;
                            let left = (self.rim + x + dx) * scale;
                            for py in top..top + scale {
                                let first = py as usize * pitch + left as usize * bytes;
                                for pixel in out[first..first + scale as usize * bytes].chunks_exact_mut(bytes) {
                                    pixel.copy_from_slice(white);
                                }
                            }
                        }
                    }
                }
                x += steps;
            }
        }
        out
    }
}

fn ascii(c: u8, unicode: bool) -> Bitmap {
    let mut bits = [0; WIDE_HEIGHT as usize];
    let top = if unicode { 5 } else { 0 };
    for (row, dots) in bits[top..top + 7].iter_mut().zip(glyph(c.to_ascii_uppercase())) {
        *row = u16::from(dots) << 11;
    }
    bits
}

/// The bytes of a black and a white pixel of a swap chain's format.
pub(super) fn colours(format: DXGI_FORMAT) -> Option<[&'static [u8]; 2]> {
    Some(match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM => [&[0, 0, 0, 0xFF], &[0xFF; 4]],
        DXGI_FORMAT_R10G10B10A2_UNORM => [&[0, 0, 0, 0xC0], &[0xFF; 4]],
        // Halves of 0 and 1.
        DXGI_FORMAT_R16G16B16A16_FLOAT => [
            &[0, 0, 0, 0, 0, 0, 0, 0x3C],
            &[0, 0x3C, 0, 0x3C, 0, 0x3C, 0, 0x3C],
        ],
        _ => return None,
    })
}
