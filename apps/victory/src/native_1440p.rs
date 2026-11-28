//! A 2560x1440 SDR display for the supported Series X retail image (the game,
//! 1.0.2.1): the title presents at 3840x2160 on Series X, its scene drawn at
//! 2560x1440 and scaled up by its own temporal pass. With 1440p asked for
//! (`VICTORY_NATIVE_1440P=1`, or `VICTORY_RENDER_1080P=1` for a 1920x1080
//! scene scaled to 1440p), the image's SDR display mode, the scanout
//! reservation, the temporal scene scale and the Settings viewport load are
//! patched before it is mapped: the title then makes a 1440p output, and no
//! 4K pass runs.
//!
//! Ported from DexNom's fork (DexNom/VIctory, `apps/victory/src/native_1440p.rs`,
//! GPL-3.0-or-later), its native 1440p part only. Every field is checked
//! against the stock bytes first: another build is refused, not patched.

use anyhow::{Context, Result, ensure};
use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
use victory_hle::streamline::Mode;
use victory_loader::PreparedImage;

/// Whether `bytes` are the stock ones whose BLAKE3 hash (hex) is `hash`.
/// The stock bytes of each place patched are kept as their hash only: no
/// bytes of the title are in this repository.
fn same(bytes: &[u8], hash: &str) -> bool {
    blake3::hash(bytes).to_hex().as_str() == hash
}

pub(super) const RETAIL_BUILD: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";
pub(super) const WIDTH: u32 = 2560;
pub(super) const HEIGHT: u32 = 1440;
const SDR_MODE: u32 = 0x0615_c100;
const STOCK_SDR_MODE: &str = "abfb5438d8153b6a0b8d08f33f4f0066e5383925b55aacc5f669131f3f51d4cf";
const SCANOUT_RESERVATION: u32 = 0x0247_ad4a;
const TEMPORAL_SCALE: u32 = 0x028f_1e7e;
const UI_VIEWPORT_LOAD: u32 = 0x0580_48d4;

/// A display wider than 16:9 asked for with `VICTORY_DISPLAY_WIDE=WxH`
/// (5120x1440 for a 32:9 one): the size the output is patched to in place
/// of 2560x1440, so that the title's own camera, its culling, its motion
/// vectors and its rays are all made for the wide picture, and nothing of
/// ours has to widen a projection on its way to the card (`VICTORY_ASPECT`
/// does that to a copy and leaves the title's own value narrow).
///
/// The height is kept at 1440 (what the other patched fields were worked
/// out for) and the width is a multiple of 8 from 2560 to 7680. `None`
/// without the variable; an error names a value that is not such a size.
pub(super) fn wide_asked() -> Result<Option<(u32, u32)>> {
    let Ok(asked) = std::env::var("VICTORY_DISPLAY_WIDE") else {
        return Ok(None);
    };
    wide_of(&asked).map(Some)
}

/// [`wide_asked`]'s reading of the variable's value.
fn wide_of(asked: &str) -> Result<(u32, u32)> {
    let size = asked.trim().split_once(['x', 'X']).and_then(|(w, h)| {
        Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
    });
    match size {
        Some((w, HEIGHT)) if (WIDTH..=7680).contains(&w) && w % 8 == 0 => Ok((w, HEIGHT)),
        _ => anyhow::bail!(
            "VICTORY_DISPLAY_WIDE={asked}: not a size this output can be patched to (a width of 2560 to 7680, a multiple of 8, by 1440: 5120x1440)"
        ),
    }
}

/// Patches `image` for a 2560x1440 output; the scene is drawn at 2560x1440,
/// or at 1920x1080 with `render_1080p`. Returns the scene's size. (The
/// runner asks through [`prepare_sized`]; this is the tests' short form.)
#[cfg(test)]
pub(super) fn prepare(image: &mut PreparedImage, render_1080p: bool) -> Result<(u32, u32)> {
    prepare_sized(image, (WIDTH, HEIGHT), render_1080p)
}

/// As [`prepare`], for an output of `output` (2560x1440, or a wider one of
/// [`wide_asked`]). The 1080p scene is one of the 16:9 output alone.
pub(super) fn prepare_sized(image: &mut PreparedImage, output: (u32, u32), render_1080p: bool) -> Result<(u32, u32)> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "1440p output is only known for the retail build {RETAIL_BUILD}"
    );
    let (width, height) = output;
    ensure!(
        !render_1080p || output == (WIDTH, HEIGHT),
        "a 1920x1080 scene is only known for the 2560x1440 output"
    );

    let mode = field::<16>(image, SDR_MODE)?;
    ensure!(same(&mode[..], STOCK_SDR_MODE), "unexpected Series X SDR mode");
    mode[..4].copy_from_slice(&width.to_le_bytes());
    mode[4..8].copy_from_slice(&height.to_le_bytes());

    // Two 64 KiB-aligned, tiled RGBA8 scanout buffers. The reservation is
    // rounded to 2 MiB, matching the title's allocation check.
    let texture = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width,
        height,
        depth_or_array: 1,
        mips: 1,
        format: 28,
        samples: 1,
        swizzle: Swizzle::SW_64KB_D_X,
    })
    .map_err(|error| anyhow::anyhow!("1440p scanout layout: {error:?}"))?;
    let pool_size =
        u32::try_from((texture.size.next_multiple_of(64 * 1024) * 2).next_multiple_of(2 << 20))?;
    let reservation = field::<5>(image, SCANOUT_RESERVATION)?;
    ensure!(*reservation == [0xb9, 0, 0, 0, 4], "unexpected Series X scanout reservation");
    reservation[1..].copy_from_slice(&pool_size.to_le_bytes());

    let scale = field::<4>(image, TEMPORAL_SCALE)?;
    ensure!(*scale == (2.0f32 / 3.0).to_le_bytes(), "unexpected Series X temporal scene scale");
    // (One scale for both directions: the scene is the output's shape.)
    let (render_width, render_height) = if render_1080p { (1920, 1080) } else { (width, height) };
    *scale = (render_width as f32 / width as f32).to_le_bytes();

    // Settings lays out its preview panels with a viewport setter of its
    // own that loads the legacy UI size (3840x2160) even when a 1440p UI
    // surface was made: it reads the selected SDR mode instead.
    let viewport = field::<8>(image, UI_VIEWPORT_LOAD)?;
    ensure!(
        *viewport == [0xc5, 0xfb, 0x10, 0x0d, 0x10, 0xf0, 0x13, 0x05],
        "unexpected Settings viewport size load"
    );
    let displacement = (SDR_MODE as i32) - (UI_VIEWPORT_LOAD as i32 + 8);
    viewport[4..].copy_from_slice(&displacement.to_le_bytes());
    Ok((render_width, render_height))
}

/// The title's renderer selection: `mov eax, 6; cmovne eax, ecx` (the
/// title's own temporal pass, or on Series S with the upscaler there the
/// FidelityFX path, 7).
const RENDERER_SELECTION: u32 = 0x007e_9421;
const STOCK_RENDERER_SELECTION: &str = "f0633abf37df17124108b1e37be64645bd4b2412b0524c2f89334f22bd948464";
/// The FidelityFX path's render size: the console check's result (Series S)
/// is noted as the FidelityFX quality mode and picks the scaled branch
/// (`movzx eax, al; mov [r14+0x90], eax; test al, al; jne`), whose scale
/// multiplies the output size.
const UPSCALER_SIZING: u32 = 0x028e_f69a;
const STOCK_UPSCALER_SIZING: &str = "2ae2ac102d12ca003cb218413ef018e4dba68fc2003654a30c1743e13a820ffd";
/// The native branch's load of 1.0, and the scaled branch's of the scale.
const NATIVE_LOAD: u32 = 0x028e_f6a8;
const STOCK_NATIVE_LOAD: &str = "2579c24601d173b086a8136edf54ffd82720e6bf186f9139274bb09a45ddadf5";
const SCALED_LOAD: u32 = 0x028e_f6b2;
const STOCK_SCALED_LOAD: &str = "e218882c95d2e16163a409d55b3451519414eb87c4d6220785dc09852d4e14f4";
/// The scale the scaled branch loads (shared with other code: not written).
const UPSCALER_SCALE: u32 = 0x0615_d95c;

/// Makes the title render through its FidelityFX upscaler path (renderer 7,
/// as Series S does) with the scene at `mode`'s scale of the output, so that
/// DLSS can take its upscale dispatches over (`victory_hle::modules::fidelityfx`).
/// On Series X the stock title uses its own temporal pass instead and never
/// loads its upscaler. The FidelityFX quality mode the title notes is
/// `mode`'s; the temporal scene scale (the title's own pass, and
/// `prepare`'s 1440p patch) is set to the same scale, as the stock Series X
/// image pairs them (2/3 each). Every patched instruction is checked against
/// the stock bytes first; nothing is written if one differs.
///
/// From DexNom's fork (DexNom/VIctory, `apps/victory/src/native_1440p.rs`,
/// `prepare_fsr_4k`, GPL-3.0-or-later), there for a 4K output; kept here
/// with the 1440p one.
pub(super) fn prepare_upscaler(image: &mut PreparedImage, mode: Mode) -> Result<()> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "the upscaler path is only known for the retail build {RETAIL_BUILD}"
    );
    ensure!(same(&field::<8>(image, RENDERER_SELECTION)?[..], STOCK_RENDERER_SELECTION), "unexpected renderer selection");
    ensure!(same(&field::<14>(image, UPSCALER_SIZING)?[..], STOCK_UPSCALER_SIZING), "unexpected upscaler render size selection");
    ensure!(same(&field::<10>(image, NATIVE_LOAD)?[..], STOCK_NATIVE_LOAD), "unexpected upscaler native scale load");
    ensure!(same(&field::<8>(image, SCALED_LOAD)?[..], STOCK_SCALED_LOAD), "unexpected upscaler scaled load");
    ensure!(*field::<4>(image, UPSCALER_SCALE)? == (2.0f32 / 3.0).to_le_bytes(), "unexpected upscaler scale");
    let temporal = f32::from_le_bytes(*field::<4>(image, TEMPORAL_SCALE)?);
    ensure!(
        temporal == 2.0 / 3.0 || temporal == 1.0,
        "unexpected temporal scene scale {temporal} (stock 2/3, or 1 with the 1440p output)"
    );
    // Renderer 7 every time it is asked (the console check has run).
    *field::<8>(image, RENDERER_SELECTION)? = [0xb8, 7, 0, 0, 0, 0x90, 0x90, 0x90];
    // `mov eax, mode; mov [r14+0x90], eax; jmp scaled`.
    *field::<14>(image, UPSCALER_SIZING)? = [0xb8, mode.ffx_mode(), 0, 0, 0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0xeb, 0x0a];
    if mode != Mode::Quality {
        // The jump leaves the native branch unreached: its load's operand
        // becomes a literal of ours, which the scaled branch loads.
        let literal = NATIVE_LOAD + 4;
        *field::<4>(image, literal)? = mode.scale().to_le_bytes();
        let displacement = literal as i32 - (SCALED_LOAD as i32 + 8);
        *field::<4>(image, SCALED_LOAD + 4)? = displacement.to_le_bytes();
    }
    *field::<4>(image, TEMPORAL_SCALE)? = mode.scale().to_le_bytes();
    Ok(())
}

fn field<const N: usize>(image: &mut PreparedImage, rva: u32) -> Result<&mut [u8; N]> {
    let offset = image.info.rva_to_file_offset(rva).context("display profile field missing")? as usize;
    image
        .bytes
        .get_mut(offset..offset + N)
        .context("incomplete display profile field")?
        .try_into()
        .context("invalid display profile field length")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the retail executable named by `VICTORY_DISPLAY_PROFILE_TEST_IMAGE`
    /// (not part of the repository): the four fields hold what a 1440p
    /// output needs, for both scene sizes; without it, nothing is checked.
    #[test]
    fn patches_the_retail_image_for_1440p() {
        let Some(path) = std::env::var_os("VICTORY_DISPLAY_PROFILE_TEST_IMAGE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        for (render_1080p, scene) in [(false, (2560, 1440)), (true, (1920, 1080))] {
            let mut image = victory_loader::prepare(&bytes).unwrap();
            assert_eq!(prepare(&mut image, render_1080p).unwrap(), scene);
            let mode = *field::<16>(&mut image, SDR_MODE).unwrap();
            assert_eq!(mode[..4], WIDTH.to_le_bytes());
            assert_eq!(mode[4..8], HEIGHT.to_le_bytes());
            let scale = f32::from_le_bytes(*field::<4>(&mut image, TEMPORAL_SCALE).unwrap());
            assert_eq!((WIDTH as f32 * scale) as u32, scene.0);
            let load = *field::<8>(&mut image, UI_VIEWPORT_LOAD).unwrap();
            let disp = i32::from_le_bytes(load[4..].try_into().unwrap());
            assert_eq!(UI_VIEWPORT_LOAD.wrapping_add(8).wrapping_add_signed(disp), SDR_MODE);
            // Patched once, the image is no longer the stock one: refused.
            assert!(prepare(&mut image, render_1080p).is_err());
        }
    }

    /// What `VICTORY_DISPLAY_WIDE` takes: a width from 2560 on, a multiple
    /// of 8, by 1440; anything else is named as an error, not passed over.
    #[test]
    fn wide_sizes() {
        assert_eq!(wide_of("5120x1440").unwrap(), (5120, 1440));
        assert_eq!(wide_of(" 3440 X 1440 ").unwrap(), (3440, 1440));
        assert_eq!(wide_of("2560x1440").unwrap(), (2560, 1440));
        for bad in ["", "5120", "5120x1080", "1920x1440", "5121x1440", "7688x1440", "widex1440", "5120x1440x2"] {
            assert!(wide_of(bad).is_err(), "{bad}");
        }
    }

    /// With the retail executable (as above): a 5120x1440 output has the
    /// mode at that size, the scene at the whole of it, and a scanout
    /// reservation that holds two such buffers.
    #[test]
    fn patches_the_retail_image_for_a_wide_output() {
        let Some(path) = std::env::var_os("VICTORY_DISPLAY_PROFILE_TEST_IMAGE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let mut image = victory_loader::prepare(&bytes).unwrap();
        assert_eq!(prepare_sized(&mut image, (5120, 1440), false).unwrap(), (5120, 1440));
        let mode = *field::<16>(&mut image, SDR_MODE).unwrap();
        assert_eq!(mode[..4], 5120u32.to_le_bytes());
        assert_eq!(mode[4..8], 1440u32.to_le_bytes());
        assert_eq!(f32::from_le_bytes(*field::<4>(&mut image, TEMPORAL_SCALE).unwrap()), 1.0);
        let reservation = *field::<5>(&mut image, SCANOUT_RESERVATION).unwrap();
        let pool = u32::from_le_bytes(reservation[1..].try_into().unwrap());
        assert!(pool >= 2 * 5120 * 1440 * 4 && pool % (2 << 20) == 0, "{pool}");
        // A 1080p scene belongs to the 16:9 output.
        let mut image = victory_loader::prepare(&bytes).unwrap();
        assert!(prepare_sized(&mut image, (5120, 1440), true).is_err());
    }

    /// With the retail executable (as above): the upscaler path at each
    /// scale, alone (4K output) and after the 1440p patch; the scaled
    /// branch then loads the mode's scale and the noted mode is its
    /// FidelityFX mode.
    #[test]
    fn patches_the_upscaler_path() {
        let Some(path) = std::env::var_os("VICTORY_DISPLAY_PROFILE_TEST_IMAGE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        for native in [false, true] {
            for mode in [Mode::Dlaa, Mode::Quality, Mode::Balanced, Mode::Performance, Mode::UltraPerformance] {
                let mut image = victory_loader::prepare(&bytes).unwrap();
                if native {
                    prepare(&mut image, false).unwrap();
                }
                prepare_upscaler(&mut image, mode).unwrap();
                assert_eq!(*field::<8>(&mut image, RENDERER_SELECTION).unwrap(), [0xb8, 7, 0, 0, 0, 0x90, 0x90, 0x90]);
                let sizing = *field::<14>(&mut image, UPSCALER_SIZING).unwrap();
                assert_eq!(sizing[1], mode.ffx_mode());
                assert_eq!(sizing[12..], [0xeb, 0x0a]);
                // The jump lands on the scaled load.
                assert_eq!(UPSCALER_SIZING + 14 + 0x0a, SCALED_LOAD);
                let disp = i32::from_le_bytes(*field::<4>(&mut image, SCALED_LOAD + 4).unwrap());
                let loaded = (SCALED_LOAD + 8).wrapping_add_signed(disp);
                assert_eq!(f32::from_le_bytes(*field::<4>(&mut image, loaded).unwrap()), mode.scale());
                assert_eq!(f32::from_le_bytes(*field::<4>(&mut image, TEMPORAL_SCALE).unwrap()), mode.scale());
                // The shared constant stays as it was.
                assert_eq!(*field::<4>(&mut image, UPSCALER_SCALE).unwrap(), (2.0f32 / 3.0).to_le_bytes());
                // Patched once, refused.
                assert!(prepare_upscaler(&mut image, mode).is_err());
            }
        }
    }
}
