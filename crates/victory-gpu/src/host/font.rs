//! Unicode text for the settings and statistics overlays.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::{Dxgi::Common::*, Gdi::*};
use windows::core::{PCWSTR, w};

pub(super) const CELL: (u32, u32) = (7, 17);

#[derive(Clone, Copy)]
pub(super) enum Script {
    Simplified,
    Traditional,
}

fn cells(c: char) -> u32 {
    if c.is_ascii() { 1 } else { 2 }
}

pub(super) fn text_width(text: &str) -> u32 {
    text.chars().map(cells).sum()
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    fonts: [HFONT; 2],
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the DC and these GDI objects are owned here. Deleting the DC unselects them.
        unsafe {
            let _ = DeleteDC(self.dc);
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            }
            for font in self.fonts {
                if !font.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(font.0));
                }
            }
        }
    }
}

fn paint_text(
    lines: &[String],
    (width, height): (u32, u32),
    scale: u32,
    rim: u32,
    script: Script,
    colours: [&[u8]; 2],
    (out, pitch): (&mut [u8], usize),
) -> Option<()> {
    // SAFETY: a private DC, a top-down 32-bit DIB of the requested size, and
    // counted UTF-16 strings with one advance per code unit. The bits are read
    // only after GDI finishes and while the bitmap is alive.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let mut surface = Surface { dc, bitmap: HBITMAP::default(), fonts: [HFONT::default(); 2] };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        surface.bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let count = width as usize * height as usize;
        if bits.is_null() || SelectObject(dc, HGDIOBJ(surface.bitmap.0)).is_invalid() {
            return None;
        }
        let pixels = bits.cast::<u32>();
        std::ptr::write_bytes(pixels, 0, count);
        let font = |face, width| CreateFontW(
            -((14 * scale) as i32), width, 0, 0, FW_NORMAL.0 as i32,
            0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
            NONANTIALIASED_QUALITY, u32::from(FF_MODERN.0), face,
        );
        surface.fonts[0] = font(w!("Consolas"), (CELL.0 * scale) as i32);
        if lines.iter().any(|line| !line.is_ascii()) {
            let face = match script {
                Script::Simplified => w!("Microsoft YaHei UI"),
                Script::Traditional => w!("Microsoft JhengHei UI"),
            };
            surface.fonts[1] = font(face, 0);
        }
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0xFFFFFF));
        SetTextAlign(dc, TA_BASELINE);
        let inset = (rim * scale) as i32;
        let clip = RECT { left: inset, top: inset, right: width as i32 - inset, bottom: height as i32 - inset };
        let mut wide = Vec::new();
        let mut advances = Vec::new();
        for (row, line) in lines.iter().enumerate().filter(|(_, line)| !line.is_empty()) {
            let mut x = inset;
            let y = inset + ((row as u32 * CELL.1 + 13) * scale) as i32;
            let mut start = 0;
            let mut ascii = line.as_bytes()[0].is_ascii();
            // Consolas keeps the ASCII columns aligned. Consecutive CJK characters
            // use the appropriate Windows UI face, with twice the ASCII advance.
            let boundaries = line.char_indices().skip(1).map(|(i, c)| (i, c.is_ascii()))
                .chain(std::iter::once((line.len(), false)));
            for (end, next_ascii) in boundaries {
                if next_ascii == ascii && end != line.len() {
                    continue;
                }
                let font = surface.fonts[usize::from(!ascii)];
                if font.is_invalid() || SelectObject(dc, HGDIOBJ(font.0)).is_invalid() {
                    return None;
                }
                wide.clear();
                advances.clear();
                for c in line[start..end].chars() {
                    wide.extend_from_slice(c.encode_utf16(&mut [0; 2]));
                    advances.extend(std::iter::repeat_n(0, c.len_utf16() - 1));
                    advances.push((cells(c) * CELL.0 * scale) as i32);
                }
                if !ExtTextOutW(dc, x, y, ETO_CLIPPED, Some(&clip), PCWSTR(wide.as_ptr()), wide.len() as u32, Some(advances.as_ptr())).as_bool() {
                    return None;
                }
                x += advances.iter().sum::<i32>();
                (start, ascii) = (end, next_ascii);
            }
        }
        if !GdiFlush().as_bool() {
            return None;
        }
        let bytes = colours[0].len();
        let src = std::slice::from_raw_parts(pixels, count);
        for (row, dest) in src.chunks_exact(width as usize).zip(out.chunks_exact_mut(pitch)) {
            for (&pixel, target) in row.iter().zip(dest.chunks_exact_mut(bytes)) {
                target.copy_from_slice(colours[usize::from(pixel & 0x00FFFFFF != 0)]);
            }
        }
    }
    Some(())
}

pub(super) fn painted(
    lines: &[String],
    size: (u32, u32),
    scale: u32,
    rim: u32,
    script: Script,
    colours: [&[u8]; 2],
    pitch: usize,
) -> Vec<u8> {
    let mut out = vec![0; pitch * size.1 as usize];
    if paint_text(lines, size, scale, rim, script, colours, (&mut out, pitch)).is_none() {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| tracing::warn!("could not draw overlay text with Windows GDI"));
        for pixel in out.chunks_exact_mut(colours[0].len()) {
            pixel.copy_from_slice(colours[0]);
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_glyphs_are_distinct_and_leave_the_rim_clear() {
        let scale = 2;
        let rim = 4;
        let dimensions = (80, 44);
        let pitch = 512;
        let colours = colours(DXGI_FORMAT_R10G10B10A2_UNORM).unwrap();
        let draw = |s: &str| painted(&[s.to_owned()], dimensions, scale, rim, Script::Simplified, colours, pitch);
        let settings = draw("设");
        assert_eq!(text_width("F1 设置"), 7);
        assert_ne!(settings, draw("置"));
        assert_ne!(settings, draw(""));
        assert_eq!(&settings[..4], colours[0]);
        assert_ne!(settings, painted(&["設".to_owned()], dimensions, scale, rim, Script::Traditional, colours, pitch));
    }
}
