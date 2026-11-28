//! A DualSense picture for the title's Settings controls screen, which
//! shows an Xbox controller from a 1024x512 BC7 texture (one zlib stream in
//! the user's copy; tiled 64 KiB `D`). The picture is the user's own, any
//! size, RGBA8 with transparency: what is opaque in it is fitted, keeping
//! its shape, into the box the title's own picture fills, and it is drawn
//! wider than it shows because the menu draws the texture at about 0.75 of
//! its width (0.748, measured in the PR author's runs). The stream keeps
//! the title's length, so the file is served in place
//! (`VICTORY_FILE_PATCH`).
//!
//! Ported from Marvisak/VIctory PR #3399 by anoyarou (GPL-3.0-or-later).
//! There the picture was Xelu's "Free Controller & Key Prompts" PS5 line
//! art (thoseawesomeguys.com/prompts, CC0), downloaded by the user; a
//! picture of the user's takes its place.

use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
use victory_formats::{bc7, zlib_exact};

pub const WIDTH: usize = 1024;
pub const HEIGHT: usize = 512;
/// Bytes of the texture: BC7, 16 bytes per 4x4 block, no mips.
pub const TEXTURE_BYTES: usize = WIDTH * HEIGHT;

/// The share of the texture's width the menu shows it at.
const SQUEEZE: f64 = 0.748;
/// The box the title's own picture fills in the texture (`x0, x1, y0, y1`,
/// texels): x 26..998 of 1024, the full height. A new picture's opaque
/// bounds are fitted into it as it shows (x by `SQUEEZE`).
const FIT_BOX: [f64; 4] = [26.0, 998.0, 0.0, 512.0];

/// The Settings texture ([`TEXTURE_BYTES`] bytes, tiled) with the picture
/// `rgba` in place of the title's own: `width` by `height` texels, RGBA8
/// with straight alpha, of any size. What is opaque in it (alpha 8 or more)
/// is fitted into `FIT_BOX` as it shows, keeping its shape and centred,
/// with an area filter over premultiplied colour, so transparent texels
/// add no colour of their own; a texel that ends up fully transparent is
/// `(0, 0, 0, 0)`. Panics if `rgba` is not that many texels.
pub fn texture_from_picture(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert_eq!(
        rgba.len(),
        width * height * 4,
        "the picture is {width} by {height} texels of RGBA8"
    );
    let mut pic = vec![0u8; WIDTH * HEIGHT * 4];
    if let Some([x0, x1, y0, y1]) = bounds(rgba, width, 8) {
        // Shown texels per source texel that fill the box along each axis
        // of the opaque bounds; the smaller fills the box, both ways.
        let shown = [
            (FIT_BOX[1] - FIT_BOX[0]) * SQUEEZE / (x1 - x0) as f64,
            (FIT_BOX[3] - FIT_BOX[2]) / (y1 - y0) as f64,
        ];
        // Texture texels per source texel: x is stretched back by `SQUEEZE`.
        let scale = shown[0].min(shown[1]);
        let step = [scale / SQUEEZE, scale];
        // The source texel the centre of the box takes its colour from.
        let centre = [(x0 + x1) as f64 / 2.0, (y0 + y1) as f64 / 2.0];
        resample(rgba, width, height, centre, step, &mut pic);
    }
    tile(&pic)
}

/// `[x0, x1, y0, y1]` of the texels of `rgba` (rows of `width` texels)
/// whose alpha is `least` or more; `None` if there are none.
fn bounds(rgba: &[u8], width: usize, least: u8) -> Option<[usize; 4]> {
    let mut bounds: Option<[usize; 4]> = None;
    for (i, px) in rgba.chunks_exact(4).enumerate() {
        if px[3] < least {
            continue;
        }
        let (x, y) = (i % width, i / width);
        let b = bounds.get_or_insert([x, x + 1, y, y + 1]);
        *b = [b[0].min(x), b[1].max(x + 1), b[2].min(y), b[3].max(y + 1)];
    }
    bounds
}

/// For each texture texel along one axis (`to` of them; `mid` is the box's
/// centre among them): the source lines it covers, each with the share of
/// the line. Texel `d` covers the source lines from `centre + (d - mid) *
/// extent` over `extent` lines; lines outside the `from` the picture has
/// count for nothing.
fn spans(from: usize, to: usize, mid: f64, centre: f64, extent: f64) -> Vec<Vec<(usize, f64)>> {
    (0..to)
        .map(|d| {
            let lo = centre + (d as f64 - mid) * extent;
            let hi = lo + extent;
            (lo.floor().max(0.0) as usize..hi.ceil().min(from as f64) as usize)
                .map(|i| (i, (i as f64 + 1.0).min(hi) - (i as f64).max(lo)))
                .filter(|&(_, share)| share > 0.0)
                .collect()
        })
        .collect()
}

/// Puts `rgba` (`width` by `height` texels, straight alpha) into the
/// texture's `pic`: a texture texel takes the average over the source lines
/// it covers (see `spans`; `step` texture texels per source texel along x
/// and y, `centre` as in `texture_from_picture`), colour counted
/// premultiplied so that transparent lines add no colour. A texel over
/// nothing stays clear.
fn resample(
    rgba: &[u8],
    width: usize,
    height: usize,
    centre: [f64; 2],
    step: [f64; 2],
    pic: &mut [u8],
) {
    let cols = spans(
        width,
        WIDTH,
        (FIT_BOX[0] + FIT_BOX[1]) / 2.0,
        centre[0],
        1.0 / step[0],
    );
    let rows = spans(
        height,
        HEIGHT,
        (FIT_BOX[2] + FIT_BOX[3]) / 2.0,
        centre[1],
        1.0 / step[1],
    );
    // A texel's area, in source lines.
    let area = 1.0 / (step[0] * step[1]);
    for (y, row) in rows.iter().enumerate() {
        for (x, col) in cols.iter().enumerate() {
            if row.is_empty() || col.is_empty() {
                continue;
            }
            let mut acc = [0f64; 4];
            for &(j, wy) in row {
                for &(i, wx) in col {
                    let px = &rgba[(j * width + i) * 4..][..4];
                    let a = f64::from(px[3]);
                    acc[3] += a * wx * wy;
                    for c in 0..3 {
                        acc[c] += f64::from(px[c]) * a * wx * wy;
                    }
                }
            }
            let alpha = (acc[3] / area).round();
            let out = &mut pic[(y * WIDTH + x) * 4..][..4];
            if alpha > 0.0 {
                for c in 0..3 {
                    out[c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u8;
                }
                out[3] = alpha.clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// The texture's layout: 1024x512 BC7, one mip, tiled 64 KiB `D`.
fn layout() -> Layout {
    let desc = Desc {
        dimension: Dimension::Tex2d,
        width: (WIDTH / 4) as u32,
        height: (HEIGHT / 4) as u32,
        depth: 1,
        mips: 1,
        element_bytes: 16,
        block: 1,
        samples: 1,
        swizzle: Swizzle::SW_64KB_D,
    };
    Layout::new(&desc).expect("a 1024x512 BC7 texture has a layout")
}

/// The picture (1024x512 RGBA8) as the title stores it: BC7 blocks, tiled
/// 64 KiB `D`.
fn tile(pic: &[u8]) -> Vec<u8> {
    let blocks = bc7::encode(pic, WIDTH, HEIGHT);
    let mut tiled = vec![0u8; TEXTURE_BYTES];
    let pitch = WIDTH / 4 * 16;
    layout()
        .tile(&mut tiled, 0, 0, &blocks, pitch, blocks.len())
        .expect("the blocks fit the texture");
    tiled
}

/// Where the texture's zlib stream starts in the title's file, and its
/// length: the first stream (at a 16-byte step) that inflates to exactly
/// the texture's size.
pub fn find_stream(file: &[u8]) -> Option<(usize, usize)> {
    (0..file.len().saturating_sub(2))
        .step_by(16)
        .find_map(|at| {
            let s = &file[at..];
            if s[0] != 0x78 {
                return None;
            }
            zlib_exact::inflate(s, TEXTURE_BYTES)
                .filter(|i| i.data.len() == TEXTURE_BYTES)
                .map(|i| (at, i.stream_len))
        })
}

/// The zlib stream for the new texture, `stream_len` bytes long.
pub fn pack(tiled: &[u8], stream_len: usize) -> Result<Vec<u8>, String> {
    zlib_exact::exact(tiled, stream_len, zlib_exact::fast)
        .ok_or_else(|| format!("the picture does not compress to {stream_len} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture of our own: a white ellipse on transparency, inscribed in
    /// a `width` by `height` image (both odd), so it is opaque exactly in
    /// x 1..width - 1 and y 1..height - 1.
    fn ellipse(width: usize, height: usize) -> Vec<u8> {
        let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
        let (rx, ry) = (cx - 1.5, cy - 1.5);
        let mut px = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = ((x as f64 + 0.5 - cx) / rx, (y as f64 + 0.5 - cy) / ry);
                if dx * dx + dy * dy <= 1.0 {
                    px[(y * width + x) * 4..][..4].copy_from_slice(&[244, 244, 247, 255]);
                }
            }
        }
        px
    }

    /// The texture's texels: detiled, then every block decoded.
    fn texels(tiled: &[u8]) -> Vec<u8> {
        let mut blocks = vec![0u8; TEXTURE_BYTES];
        let pitch = WIDTH / 4 * 16;
        let len = blocks.len();
        layout().detile(tiled, 0, 0, &mut blocks, pitch, len).unwrap();
        let mut px = vec![0u8; WIDTH * HEIGHT * 4];
        for (b, block) in blocks.as_chunks::<16>().0.iter().enumerate() {
            for (i, texel) in bc7::decode_mode6(block).unwrap().into_iter().enumerate() {
                let x = b % (WIDTH / 4) * 4 + i % 4;
                let y = b / (WIDTH / 4) * 4 + i / 4;
                px[(y * WIDTH + x) * 4..][..4].copy_from_slice(&texel);
            }
        }
        px
    }

    /// Makes the texture of an ellipse picture and returns it with the
    /// bounds of what shows in it, after checking that the ellipse shows
    /// with the source's shape, where the title's own picture is.
    fn shown_ellipse(width: usize, height: usize) -> (Vec<u8>, [usize; 4]) {
        let source = ellipse(width, height);
        let [x0, x1, y0, y1] = bounds(&source, width, 8).unwrap();
        assert_eq!([x0, x1, y0, y1], [1, width - 1, 1, height - 1]);
        let aspect = (x1 - x0) as f64 / (y1 - y0) as f64;
        let tiled = texture_from_picture(&source, width, height);
        assert_eq!(tiled.len(), TEXTURE_BYTES);
        let visible @ [tx0, tx1, ty0, ty1] = bounds(&texels(&tiled), WIDTH, 128).unwrap();
        // As it shows (x by `SQUEEZE`), the ellipse is as wide as its
        // height and the source's shape make it...
        let shown_width = (tx1 - tx0) as f64 * SQUEEZE;
        let shape_width = (ty1 - ty0) as f64 * aspect;
        assert!(
            (shown_width - shape_width).abs() <= 2.0,
            "{shown_width} shown wide, {shape_width} by its shape ({visible:?})"
        );
        // ...and it fills the box along the axis that limits it.
        let box_aspect = (FIT_BOX[1] - FIT_BOX[0]) * SQUEEZE / (FIT_BOX[3] - FIT_BOX[2]);
        if aspect > box_aspect {
            assert_eq!(tx1 - tx0, (FIT_BOX[1] - FIT_BOX[0]) as usize);
        } else {
            assert_eq!(ty1 - ty0, HEIGHT);
        }
        (tiled, visible)
    }

    /// A picture of any size takes the title's picture's place: it shows
    /// with its shape, clear around it, and its stream is the title's
    /// length.
    #[test]
    fn a_picture_takes_the_place_of_the_titles_own() {
        // Wider than the box: fitted to its width, clear above and below.
        let (tiled, [_, _, ty0, ty1]) = shown_ellipse(1301, 809);
        assert!(ty0 > 0 && ty1 < HEIGHT, "{ty0}..{ty1}");
        let px = texels(&tiled);
        let at = |x: usize, y: usize| &px[(y * WIDTH + x) * 4..][..4];
        // The body has the picture's colour.
        let body = at(512, 256);
        assert!(
            body.iter()
                .zip([244u8, 244, 247, 255])
                .all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= 2),
            "{body:?}"
        );
        // Outside it, the corners and the margin around it are fully
        // transparent.
        for (x, y) in [
            (0, 0),
            (WIDTH - 1, 0),
            (0, HEIGHT - 1),
            (WIDTH - 1, HEIGHT - 1),
            (FIT_BOX[0] as usize - 5, 256),
            (512, ty0 - 5),
        ] {
            assert_eq!(at(x, y), &[0, 0, 0, 0], "{x}, {y}");
        }
        // Taller than the box: fitted to its height.
        shown_ellipse(503, 1009);

        // The stream is as long as the title's, and is found again.
        let mut file = vec![0u8; 0x1100];
        let target = 200_000;
        file.extend(pack(&tiled, target).unwrap());
        assert_eq!(find_stream(&file), Some((0x1100, target)));
    }
}
