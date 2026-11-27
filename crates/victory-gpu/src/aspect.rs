//! 32:9 patch: widen the title's own perspective projections before the
//! copies of them go up.
//!
//! The title (Xbox code) writes a fresh view projection into its memory every
//! frame and the runtime copies it up (`crate::shadow::copy_all`). RAGE bakes
//! the display aspect into the matrix (`_11` over `_22`), so the honest wide
//! fix is not stretching pixels: it is rewriting `_11` while the bytes still
//! lie in title memory. Everything downstream — depth, screen-space work, the
//! motion vectors DLSS is fed — is drawn by shaders that read this very
//! matrix, so a single rewrite here carries the whole frame chain to the new
//! aspect and the vectors keep pointing at the right pixels.
//!
//! `VICTORY_ASPECT=5120x1440` (or a bare `3.5556`) turns it on; unset, every
//! function here costs one cached `None` check. Rewriting happens in the
//! title's own memory, not in a copy, so the write watch and `stale_pages`
//! find title and held copies agreeing (they compare) and never undo the fix.
//!
//! Orthographic (user interface) matrices carry `_43 == 0` and fall through
//! untouched, which is half of why the 16:9 interface plane can be letterboxed
//! beside a wide world instead of smeared across it.

use std::sync::OnceLock;

/// The aspect the title thinks it renders at (Series S/X both present 16:9).
const CONSOLE_ASPECT: f32 = 16.0 / 9.0;
/// How far a matrix may sit from the console aspect and still count as one of
/// the title's projections. The title recomputes every frame, so a tolerance
/// of two percent catches antialiasing-sized drift and nothing else.
const ASPECT_TOLERANCE: f32 = 0.02;

/// The width over height to render at, when the patch is on.
pub(crate) fn target() -> Option<f32> {
    static TARGET: OnceLock<Option<f32>> = OnceLock::new();
    *TARGET.get_or_init(|| {
        let raw = std::env::var("VICTORY_ASPECT").ok()?;
        let raw = raw.trim();
        // Accept "5120x1440" as written, or the ratio itself.
        let ratio = match raw.split_once(['x', 'X']) {
            Some((w, h)) => w.trim().parse::<f32>().ok()? / h.trim().parse::<f32>().ok()?,
            None => raw.parse::<f32>().ok()?,
        };
        // Wider than the console mode or the patch is pointless (narrower
        // would need pillarboxing the world, not a projection).
        (ratio > CONSOLE_ASPECT + ASPECT_TOLERANCE && ratio < 8.0).then_some(ratio)
    })
}

/// One matrix's worth of floats, `_ij` at its row-major float offset.
#[repr(C)]
struct Mat4([f32; 16]);

/// Whether these sixteen floats look like the title's perspective projection:
/// the empty rows an axis-aligned camera leaves, a `_43` of +-1 (LH is the
/// console's hand), a `_44` of 1 or 0 (plain or infinite z), a positive pair
/// of diagonal scales sitting exactly at the console aspect, and — the tell
/// orthography lacks — no other finite entry on the bottom row.
fn projection(m: &Mat4) -> bool {
    let f = &m.0;
    let zero = |i: usize| f[i] == 0.0;
    // Row 0 keeps only _11; row 1 keeps only _22; row 2 keeps _33 and _34.
    let rows_empty =
        zero(1) && zero(2) && zero(3) && zero(4) && zero(6) && zero(7) && zero(8) && zero(9);
    let handed = f[14] == 1.0 || f[14] == -1.0; // _43
    let finite_z = f[15] == 1.0 || f[15] == 0.0; // _44
    let bottom_row_clear = zero(12) && zero(13);
    if !(rows_empty && handed && finite_z && bottom_row_clear) {
        return false;
    }
    // The vertical scale must be a sane positive fov, the horizontal one the
    // same scaled by exactly the console aspect.
    let (m11, m22) = (f[0], f[5]); // _11, _22
    m11 > 1e-4 && m22 > 1e-4 && ((m22 / m11) - CONSOLE_ASPECT).abs() < ASPECT_TOLERANCE
}

/// Scan one block the runtime is about to copy and rewrite every projection
/// found inside it, in the title's memory, before it leaves the CPU.
///
/// Called on every upload copy: the scan is aligned float reads of plain
/// comparisons, far cheaper than the copy it guards when nothing matches
/// (a texture page cannot match: `_43 == +-1` with that zero pattern in
/// image data is a one-in-10^19 accident, and even then it rewrites a float
/// of a pixel that the next page copy puts right).
pub(crate) fn tune(block: &mut [u8]) {
    let Some(aspect) = target() else { return };
    // Whole matrices only; a matrix that straddles the block's edge was read
    // in full elsewhere or is not a matrix.
    let floats = block.len() / 4;
    if floats < 16 {
        return;
    }
    // SAFETY: the block is the caller's plain memory of copied bytes; f32
    // needs no alignment on x86 and reads little-endian the same way the
    // copystat's word() helper already does.
    let f = unsafe { std::slice::from_raw_parts_mut(block.as_mut_ptr().cast::<f32>(), floats) };
    // The 16-byte grid the console's constant buffers place matrices on.
    for base in (0..=(floats - 16)).step_by(4) {
        // Borrow each window as the matrix shape, then rewrite.
        // SAFETY: 16 floats inside the slice, same alignment argument.
        let m = unsafe { &*(f[base..base + 16].as_ptr().cast::<Mat4>()) };
        if projection(m) {
            // Keep the vertical field the game authored, buy the horizontal
            // one back: _11 = _22 / aspect.
            f[base] = m.0[5] / aspect;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console's own 16:9 left-handed infinite-z projection, the shape
    /// the patch must catch and rewrite.
    fn title_projection() -> [f32; 16] {
        let f = 1.0f32 / (60f32.to_radians() / 2.0).tan();
        let a = 16.0 / 9.0;
        [
            f / a, 0.0, 0.0, 0.0, //
            0.0, f, 0.0, 0.0,  //
            0.0, 0.0, 1.0, 0.1, //
            0.0, 0.0, 1.0, 0.0, //
        ]
    }

    #[test]
    fn a_title_projection_is_a_projection() {
        let m = Mat4(title_projection());
        assert!(projection(&m));
    }

    #[test]
    fn an_orthographic_ui_matrix_is_not() {
        let m = Mat4([
            0.5, 0.0, 0.0, 0.0, //
            0.0, -0.5, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0, //
        ]);
        assert!(!projection(&m));
    }

    #[test]
    fn a_wide_matrix_already_is_not_touched_again() {
        // 32:9 bakes _22 over _11 = 3.555: outside the console band, skip.
        let mut m = Mat4(title_projection());
        m.0[0] = m.0[5] / 3.5556;
        assert!(!projection(&m));
    }
}
