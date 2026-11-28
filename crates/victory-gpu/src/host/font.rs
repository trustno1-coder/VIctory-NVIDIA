//! The dot letters the stats panel (`panel.rs`, diagnostics builds) and the settings menu
//! (`menu.rs`, every build) are written with, and the pixels they are painted in.

use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Foundation::{COLORREF, SIZE};
use windows::Win32::Graphics::Gdi::*;

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

struct TextSurface {
    dc: HDC,
    font: HFONT,
    bitmap: HBITMAP,
}

impl Drop for TextSurface {
    fn drop(&mut self) {
        // SAFETY: these handles belong to this surface. Deleting the DC
        // releases its selections before the bitmap and font are deleted.
        unsafe {
            let _ = DeleteDC(self.dc);
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            }
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

pub(super) struct TextMask {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Rasterizes the guide into a mask using a Windows system font.
pub(super) fn text_mask(
    lines: &[&str],
    (max_width, max_height): (u32, u32),
    pixels: u32,
    rim: u32,
) -> Option<TextMask> {
    if lines.is_empty() || max_width == 0 || max_height == 0 {
        return None;
    }
    let lines: Vec<Vec<u16>> = lines.iter().map(|line| line.encode_utf16().collect()).collect();
    // SAFETY: the memory DC, selected GDI objects, and DIB pixels all live
    // for the duration of this call. BI_RGB with 32 bits has no row padding.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let mut surface = TextSurface { dc, font: HFONT::default(), bitmap: HBITMAP::default() };
        surface.font = CreateFontW(-(pixels as i32), 0, 0, 0, FW_NORMAL.0 as i32,
            0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY, 0, windows::core::w!("Microsoft YaHei UI"));
        if surface.font.is_invalid() || SelectObject(dc, HGDIOBJ(surface.font.0)).is_invalid() {
            return None;
        }
        let mut row_height = pixels;
        let mut width = 0;
        for line in &lines {
            let mut extent = SIZE::default();
            if !GetTextExtentPoint32W(dc, line, &mut extent).as_bool() {
                return None;
            }
            row_height = row_height.max(extent.cy as u32);
            width = width.max(extent.cx as u32);
        }
        row_height += 3;
        let size = ((width + 2 * rim).min(max_width),
            (row_height * lines.len() as u32 + 2 * rim).min(max_height));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.0 as i32,
                biHeight: -(size.1 as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        surface.bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() || SelectObject(dc, HGDIOBJ(surface.bitmap.0)).is_invalid() {
            return None;
        }
        let len = size.0 as usize * size.1 as usize;
        std::ptr::write_bytes(bits.cast::<u32>(), 0, len);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00ff_ffff));
        for (row, line) in lines.iter().enumerate() {
            if !TextOutW(dc, rim as i32, (rim + row as u32 * row_height) as i32, line).as_bool() {
                return None;
            }
        }
        if !GdiFlush().as_bool() {
            return None;
        }
        let mask = std::slice::from_raw_parts(bits.cast::<u32>(), len)
            .iter().map(|pixel| u8::from(pixel & 0xff >= 0x80)).collect();
        Some(TextMask { width: size.0, height: size.1, pixels: mask })
    }
}
