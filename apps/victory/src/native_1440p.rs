//! A 2560x1440 SDR display for the supported Series X retail image (the game,
//! 1.0.2.1): the title presents at 3840x2160 on Series X, its scene drawn at
//! 2560x1440 and scaled up by its own temporal pass. With 1440p asked for
//! (`VICTORY_NATIVE_1440P=1`, or `VICTORY_RENDER_1080P=1` for a 1920x1080
//! scene scaled to 1440p), the title's own settings get what Series S's
//! preset gives them, with a 2560x1440 output, and the UI memory pool and the
//! temporal scene scale are patched before the image is mapped. The SDR
//! display mode stays at stock 3840x2160, as Series S keeps it. The title then
//! makes a 1440p output, lays its UI and pause map out on the 3840x2160 size
//! they are authored for, scales them onto the 2560x1440 targets, and runs no
//! 4K pass.
//!
//! Series S's preset sets three video settings: the legacy UI flag (1), the
//! legacy UI size (3840x2160) and the output size (1920x1080). Every reader
//! (the UI layout, the title's screen size `0x92D7924`, the pause map's
//! target size) takes what it needs from them. Series X's preset has the flag
//! at 0 and no legacy size, so with a 1440p output its menus and HUD were laid
//! out on a 2560x1440 stage with their 4K-sized content, 1.5 times too large.
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
const LEGACY_UI_WIDTH: u32 = 3840;
const LEGACY_UI_HEIGHT: u32 = 2160;
/// The UI memory pool's size in the UI setup at `0x247AD10`: `mov ecx,
/// 0x4000000`, the Series X value (Series S: `0x5200000`, picked by
/// `cmovne` on the title's Series S check).
const UI_POOL: u32 = 0x0247_ad4a;
const TEMPORAL_SCALE: u32 = 0x028f_1e7e;
/// The settings collection's copy (`0x1D70750`), which every settings store
/// goes through (the init's apply, the runtime updates, the backups): its
/// last store of the video block and the `vzeroupper` after it (11 bytes).
/// The video settings start at collection `+0x450`, so the legacy UI flag is
/// at `+0x638` (settings `+0x1E8`, byte), the legacy UI size at
/// `+0x63C/+0x640` (`+0x1EC/+0x1F0`) and the output size at `+0x654/+0x658`
/// (`+0x204/+0x208`).
const SETTINGS_COPY: u32 = 0x01d7_0b98;
const STOCK_SETTINGS_COPY: &str = "9d8829e6720f7624fd7004b8905ccfd6e1d9c6610ec186100f8350e091344038";
const SETTINGS_COPY_LEN: usize = 11;
/// Padding (`int3`, 60 bytes from `0x55FF284`) between two functions, inside
/// the mapped code.
const SETTINGS_CAVE: u32 = 0x055f_f285;
const SETTINGS_CAVE_LEN: usize = 59;

/// Our code for the cave: the copy's store (as `vmovdqu`, the same store),
/// then the three settings as Series S has them, with a 2560x1440 output, and
/// the jump back after the patched site.
fn settings_cave_bytes() -> Result<Vec<u8>> {
    let pair = |w: u32, h: u32| (u64::from(h) << 32 | u64::from(w)).to_le_bytes();
    let mut cave = Vec::with_capacity(SETTINGS_CAVE_LEN);
    cave.extend_from_slice(&[0xc5, 0xfe, 0x7f, 0x86, 0x38, 0x06, 0, 0]); // vmovdqu [rsi+0x638], ymm0
    cave.extend_from_slice(&[0xc6, 0x86, 0x38, 0x06, 0, 0, 0x01]); // mov byte [rsi+0x638], 1
    cave.extend_from_slice(&[0x48, 0xb8]); // mov rax, legacy size
    cave.extend_from_slice(&pair(LEGACY_UI_WIDTH, LEGACY_UI_HEIGHT));
    cave.extend_from_slice(&[0x48, 0x89, 0x86, 0x3c, 0x06, 0, 0]); // mov [rsi+0x63c], rax
    cave.extend_from_slice(&[0x48, 0xb8]); // mov rax, output size
    cave.extend_from_slice(&pair(WIDTH, HEIGHT));
    cave.extend_from_slice(&[0x48, 0x89, 0x86, 0x54, 0x06, 0, 0]); // mov [rsi+0x654], rax
    cave.extend_from_slice(&[0xc5, 0xf8, 0x77]); // vzeroupper
    let back = i64::from(SETTINGS_COPY) + SETTINGS_COPY_LEN as i64 - (i64::from(SETTINGS_CAVE) + cave.len() as i64 + 5);
    cave.push(0xe9); // jmp back
    cave.extend_from_slice(&i32::try_from(back)?.to_le_bytes());
    ensure!(cave.len() <= SETTINGS_CAVE_LEN, "settings cave code too long");
    Ok(cave)
}

/// `jmp cave` and NOPs over the copy's last store.
fn settings_copy_bytes() -> Result<[u8; SETTINGS_COPY_LEN]> {
    let mut site = [0x90; SETTINGS_COPY_LEN];
    site[0] = 0xe9;
    let to = i64::from(SETTINGS_CAVE) - (i64::from(SETTINGS_COPY) + 5);
    site[1..5].copy_from_slice(&i32::try_from(to)?.to_le_bytes());
    Ok(site)
}

/// Bytes of one 64 KiB-aligned, 64 KiB-tiled RGBA8 UI surface.
fn ui_surface_bytes(width: u32, height: u32) -> Result<u64> {
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
    .map_err(|error| anyhow::anyhow!("UI surface layout ({width}x{height}): {error:?}"))?;
    Ok(texture.size.next_multiple_of(64 * 1024))
}

/// Patches `image` for a 2560x1440 output; the scene is drawn at 2560x1440,
/// or at 1920x1080 with `render_1080p`. Returns the scene's size.
pub(super) fn prepare(image: &mut PreparedImage, render_1080p: bool) -> Result<(u32, u32)> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "1440p output is only known for the retail build {RETAIL_BUILD}"
    );

    // The SDR mode stays at stock 3840x2160: the pause map's targets and the
    // title's screen size follow the layout size, which stays 4K as on
    // Series S. Only checked here.
    ensure!(same(&field::<16>(image, SDR_MODE)?[..], STOCK_SDR_MODE), "unexpected Series X SDR mode");

    // With the legacy UI flag set, the UI setup makes two 3840x2160 legacy
    // UI surfaces and two 2560x1440 UI targets in one pool, and asserts that
    // no more than 0x1FFFFF of it is left over (`0x247AF25`). Rounded to
    // 2 MiB: 0x5E00000 (94 MiB). The stock Series X 64 MiB had no room for
    // the legacy surfaces (the title's allocator asserted, `int3` at
    // `0x56A2AFD`).
    let pool_size = u32::try_from(
        (ui_surface_bytes(LEGACY_UI_WIDTH, LEGACY_UI_HEIGHT)? * 2 + ui_surface_bytes(WIDTH, HEIGHT)? * 2)
            .next_multiple_of(2 << 20),
    )?;
    let pool = field::<5>(image, UI_POOL)?;
    ensure!(*pool == [0xb9, 0, 0, 0, 4], "unexpected Series X UI pool size");
    pool[1..].copy_from_slice(&pool_size.to_le_bytes());

    // The output settings as Series S's preset has them (legacy UI flag 1,
    // legacy UI size 3840x2160), with a 2560x1440 output, written into the
    // title's settings each time they are stored. The rest of the preset
    // stays Series X.
    let site = field::<SETTINGS_COPY_LEN>(image, SETTINGS_COPY)?;
    ensure!(same(&site[..], STOCK_SETTINGS_COPY), "unexpected settings copy");
    let cave = field::<SETTINGS_CAVE_LEN>(image, SETTINGS_CAVE)?;
    ensure!(cave.iter().all(|b| *b == 0xcc), "settings cave is not free padding");
    let code = settings_cave_bytes()?;
    cave[..code.len()].copy_from_slice(&code);
    *field::<SETTINGS_COPY_LEN>(image, SETTINGS_COPY)? = settings_copy_bytes()?;

    let scale = field::<4>(image, TEMPORAL_SCALE)?;
    ensure!(*scale == (2.0f32 / 3.0).to_le_bytes(), "unexpected Series X temporal scene scale");
    let (render_width, render_height) = if render_1080p { (1920, 1080) } else { (WIDTH, HEIGHT) };
    *scale = (render_width as f32 / WIDTH as f32).to_le_bytes();
    // The Settings preview panels' viewport (`0x58048D4`) loads the legacy
    // UI size, which is right with the legacy flag set: left stock.
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
            assert!(same(&field::<16>(&mut image, SDR_MODE).unwrap()[..], STOCK_SDR_MODE));
            let scale = f32::from_le_bytes(*field::<4>(&mut image, TEMPORAL_SCALE).unwrap());
            assert_eq!((WIDTH as f32 * scale) as u32, scene.0);
            // The UI pool holds two legacy surfaces and two 1440p targets.
            assert_eq!(*field::<5>(&mut image, UI_POOL).unwrap(), [0xb9, 0x00, 0x00, 0xe0, 0x05]);
            // The settings copy jumps to the cave, whose jump comes back
            // right after the patched site.
            assert_eq!(*field::<SETTINGS_COPY_LEN>(&mut image, SETTINGS_COPY).unwrap(), settings_copy_bytes().unwrap());
            let code = settings_cave_bytes().unwrap();
            let cave = *field::<SETTINGS_CAVE_LEN>(&mut image, SETTINGS_CAVE).unwrap();
            assert_eq!(cave[..code.len()], code[..]);
            let back = i32::from_le_bytes(code[code.len() - 4..].try_into().unwrap());
            assert_eq!((SETTINGS_CAVE + code.len() as u32).wrapping_add_signed(back), SETTINGS_COPY + SETTINGS_COPY_LEN as u32);
            // Patched once, the image is no longer the stock one: refused.
            assert!(prepare(&mut image, render_1080p).is_err());
        }
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
