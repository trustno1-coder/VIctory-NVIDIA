//! HDR10 output: the one home of the state the other files ask about.
//!
//! Off unless `VICTORY_HDR=1`, and then only when the primary monitor's
//! output really reports the HDR10 colour space (Windows HDR switched on for
//! that display). With the switch unset nothing in this file does anything
//! but answer "no", so the layer behaves exactly as it did before.
//!
//! The pure parts (the switch, paper white, the PQ curve) build everywhere
//! because the HLE crate builds on non-windows hosts too; every DXGI item sits
//! under `cfg(windows)`.

use std::sync::OnceLock;

/// What the display says about itself, in nits. Read from the output's DXGI
/// description once, before the title has a window (see `display`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    /// Black level the panel reports.
    pub min_nits: f32,
    /// Peak luminance of a small highlight.
    pub max_nits: f32,
    /// Luminance the panel holds with the whole frame lit.
    pub max_full_frame_nits: f32,
}

/// Whether HDR output was asked for: `VICTORY_HDR=1`. Off unless exactly 1
/// (unset, 0, true and the rest all mean off), read once.
pub fn asked() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    *ASKED.get_or_init(|| std::env::var_os("VICTORY_HDR").is_some_and(|v| v == "1"))
}

/// The display's luminances when HDR output is on: `Some` only if `asked()`
/// and the primary monitor reports G2084/P2020. Read once; logs why when not.
pub fn display() -> Option<Display> {
    static DISPLAY: OnceLock<Option<Display>> = OnceLock::new();
    *DISPLAY.get_or_init(|| {
        if !asked() {
            return None;
        }
        probe()
    })
}

/// Whether the title is told HDR and the swap chain is HDR10.
pub fn active() -> bool {
    display().is_some()
}

/// The nits SDR content (the interface, intro videos, the F1/F3 text) is put
/// at on the HDR10 swap chain: `VICTORY_HDR_WHITE`, 80..=1000, default 203
/// (the BT.2408 reference white). Windows' own SDR white slider is not read.
pub fn paper_white_nits() -> f32 {
    static NITS: OnceLock<f32> = OnceLock::new();
    *NITS.get_or_init(|| {
        std::env::var("VICTORY_HDR_WHITE")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .map_or(203.0, |v| v.clamp(80.0, 1000.0))
    })
}

/// SMPTE ST 2084 inverse EOTF: absolute nits (clamped to 0..=10000) to the
/// 0..=1 PQ signal.
fn pq_encode(nits: f32) -> f32 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    // a non-finite input would poison the code; treat it as black
    let y = if nits.is_finite() { (nits.clamp(0.0, 10000.0) / 10000.0) as f64 } else { 0.0 };
    let p = y.powf(M1);
    (((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)) as f32
}

/// One R10G10B10A2 pixel (little-endian word) holding paper white in red,
/// green and blue with alpha 3: what the panel and menu draw their text with,
/// since a full-code white on a PQ swap chain would be 10,000 nits.
pub fn paper_white_pixel() -> &'static [u8; 4] {
    static PIXEL: OnceLock<[u8; 4]> = OnceLock::new();
    PIXEL.get_or_init(|| {
        let code = (pq_encode(paper_white_nits()) * 1023.0).round() as u32;
        (code | code << 10 | code << 20 | 3 << 30).to_le_bytes()
    })
}

/// Non-windows hosts have no DXGI, so there is never an HDR display.
#[cfg(not(windows))]
fn probe() -> Option<Display> {
    tracing::info!("HDR: VICTORY_HDR=1 was asked, but this host has no DXGI; it stays SDR");
    None
}

#[cfg(windows)]
pub(crate) use win::*;

/// The probe on windows is the same entry point `display` calls.
#[cfg(windows)]
fn probe() -> Option<Display> {
    win::probe()
}

#[cfg(windows)]
mod win {
    use super::Display;
    use std::sync::Mutex;
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;

    /// A plane colour space the title's present call did not give us (or one
    /// we could not read).
    pub(crate) const UNKNOWN_SPACE: u32 = u32::MAX;

    /// How the compose shader reads a plane; the numbers are its codes.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[repr(u32)]
    pub(crate) enum Space {
        /// Gamma 2.2 on Rec.709 primaries (SDR).
        Gamma22 = 0,
        /// Linear light on Rec.709 primaries, 1.0 = 80 nits (scRGB).
        Linear709 = 1,
        /// PQ on Rec.2020 primaries (HDR10).
        Pq2020 = 2,
        /// Linear light on Rec.709 primaries, 1.0 = paper white. What a
        /// sampler hands back for an `_SRGB` view: the hardware has already
        /// removed the curve, so the shader must not apply a gamma again.
        LinearPaper = 3,
    }

    /// What `Composer::record` needs for an HDR10 output. `over_on` false
    /// means there is no interface plane (the second view repeats the first).
    #[derive(Clone, Copy)]
    pub(crate) struct Compose {
        /// Colour space of the scene plane.
        pub base: Space,
        /// Colour space of the interface plane.
        pub over: Space,
        /// Whether an interface plane is blended at all.
        pub over_on: bool,
        /// Nits the SDR planes are put at.
        pub paper_nits: f32,
    }

    /// The probe `display` runs once: pick the primary monitor's output and
    /// read its colour space and luminances.
    pub(super) fn probe() -> Option<Display> {
        // SAFETY: plain DXGI enumeration; every interface is owned and dropped here.
        let found = unsafe { find_output() };
        let Some(desc) = found else {
            tracing::info!("HDR: VICTORY_HDR=1 was asked, but no DXGI output was found; it stays SDR");
            return None;
        };
        let space = desc.ColorSpace;
        let hdr = space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
        // a panel that reports nonsense must not hand the title a zero or NaN peak
        let max = if desc.MaxLuminance.is_finite() && desc.MaxLuminance > 0.0 { desc.MaxLuminance } else { 1000.0 };
        let min = if desc.MinLuminance.is_finite() { desc.MinLuminance.clamp(0.0, max) } else { 0.0 };
        let full = if desc.MaxFullFrameLuminance.is_finite() { desc.MaxFullFrameLuminance.clamp(0.0, max) } else { 0.0 };
        tracing::info!(
            colour_space = space.0,
            bits_per_colour = desc.BitsPerColor,
            min_nits = min,
            max_nits = max,
            full_frame_nits = full,
            "HDR: primary output colour space {} ({} bits), {min} to {max} nits, {full} full frame: {}",
            space.0,
            desc.BitsPerColor,
            if hdr { "HDR10 on" } else { "not HDR10, it stays SDR (VICTORY_HDR=1 was asked; switch Windows HDR on for this display)" }
        );
        hdr.then_some(Display { min_nits: min, max_nits: max, max_full_frame_nits: full })
    }

    /// The description of the output whose desktop rectangle holds (0,0).
    /// GUESS: the title window opens on that monitor; it does not exist yet
    /// at the first HDR query (that comes before the device). Falls back to
    /// the first output found.
    ///
    /// # Safety
    /// Calls DXGI; no preconditions beyond a working runtime.
    unsafe fn find_output() -> Option<DXGI_OUTPUT_DESC1> {
        // SAFETY: plain factory creation; the caller promised a working runtime.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
        let mut first = None;
        let mut adapter_index = 0;
        // SAFETY: the factory is live for the whole walk.
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
            adapter_index += 1;
            let mut output_index = 0;
            // SAFETY: the adapter is live for the whole walk.
            while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
                output_index += 1;
                // an output without the sixth interface is an old runtime: skip it
                let Ok(output6) = output.cast::<IDXGIOutput6>() else { continue };
                // SAFETY: the output is live and the call fills a value we own.
                let Ok(desc) = (unsafe { output6.GetDesc1() }) else { continue };
                let r = desc.DesktopCoordinates;
                if r.left <= 0 && 0 < r.right && r.top <= 0 && 0 < r.bottom {
                    return Some(desc);
                }
                first.get_or_insert(desc);
            }
        }
        first
    }

    /// The colour space a plane is in, from the title's PresentX field.
    /// Whether that field is read at the right offset and holds DXGI values
    /// is a GUESS until a run logs it (see `note_planes`); anything else
    /// decides by format: a float plane is linear scRGB, a 10-bit plane is
    /// PQ (the title was told HDR, and an SDR one would not be 10-bit), the
    /// rest is SDR. The values are DXGI_COLOR_SPACE_TYPE: 0 full G22 709,
    /// 1 scRGB, 2 studio G22 709, 12 and 14 G2084 2020.
    pub(crate) fn plane_space(colour_space: u32, format: DXGI_FORMAT) -> Space {
        // an _SRGB view is sampled already linear, whatever the field says
        if matches!(format.0, 29 | 91) {
            return Space::LinearPaper;
        }
        match colour_space {
            12 | 14 => Space::Pq2020,
            1 => Space::Linear709,
            0 | 2 => Space::Gamma22,
            _ if matches!(format.0, 9..=14) => Space::Linear709,
            _ if matches!(format.0, 23..=25) => Space::Pq2020,
            _ => Space::Gamma22,
        }
    }

    /// A single plane that can be copied untouched into the R10G10B10A2
    /// HDR10 back buffer.
    pub(crate) fn passthrough(space: Space, format: DXGI_FORMAT) -> bool {
        space == Space::Pq2020 && matches!(format.0, 23..=25)
    }

    /// Put the swap chain in HDR10: G2084/P2020 and the display's metadata.
    /// A no-op returning false when HDR is not active. The title was told HDR
    /// already, so a swap chain that cannot do it leaves the picture wrong,
    /// and that is logged as an error.
    pub(crate) fn set_swap_chain(swap_chain: &IDXGISwapChain3) -> bool {
        let Some(shown) = super::display() else { return false };
        let space = DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
        // SAFETY: a live swap chain and a local out value.
        let support = unsafe { swap_chain.CheckColorSpaceSupport(space) }.unwrap_or(0);
        if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 == 0 {
            tracing::error!("HDR10 not supported by this swap chain; the picture will be wrong (the title was told HDR already)");
            return false;
        }
        // SAFETY: a live swap chain and a colour space it just reported as presentable.
        if let Err(e) = unsafe { swap_chain.SetColorSpace1(space) } {
            tracing::error!("HDR10 not supported by this swap chain; the picture will be wrong (SetColorSpace1: {e}; the title was told HDR already)");
            return false;
        }
        // Rec.2020 primaries and D65 in units of 0.00002
        let meta = DXGI_HDR_METADATA_HDR10 {
            RedPrimary: [35400, 14600],
            GreenPrimary: [8500, 39850],
            BluePrimary: [6550, 2300],
            WhitePoint: [15635, 16450],
            MaxMasteringLuminance: shown.max_nits as u32,
            MinMasteringLuminance: (shown.min_nits * 10000.0) as u32,
            MaxContentLightLevel: shown.max_nits as u16,
            MaxFrameAverageLightLevel: shown.max_full_frame_nits as u16,
        };
        // the proxy swap chain may not forward the fourth interface (a GUESS), so this only warns
        let sent = swap_chain.cast::<IDXGISwapChain4>().and_then(|sc4| {
            // SAFETY: `meta` is a live DXGI_HDR_METADATA_HDR10 and the size passed is its own.
            unsafe {
                sc4.SetHDRMetaData(
                    DXGI_HDR_METADATA_TYPE_HDR10,
                    Some(std::slice::from_raw_parts(
                        &meta as *const DXGI_HDR_METADATA_HDR10 as *const u8,
                        size_of::<DXGI_HDR_METADATA_HDR10>(),
                    )),
                )
            }
        });
        if let Err(e) = sent {
            tracing::warn!("HDR: the swap chain is G2084/P2020 but the HDR10 metadata was refused ({e})");
        }
        tracing::info!(
            "HDR: swap chain set to HDR10 (G2084, P2020), mastering {} to {} nits, content peak {}, frame average {}",
            shown.min_nits,
            shown.max_nits,
            meta.MaxContentLightLevel,
            meta.MaxFrameAverageLightLevel
        );
        true
    }

    /// Distinct plane layouts already logged.
    static SEEN: Mutex<Vec<[i32; 5]>> = Mutex::new(Vec::new());

    /// Log the first eight distinct (count, spaces, formats) the title
    /// presents. This line is how a run answers the unknowns about the title's
    /// colour space field, so keep the wording stable.
    pub(crate) fn note_planes(count: u32, spaces: [u32; 2], formats: [DXGI_FORMAT; 2]) {
        if !super::asked() {
            return;
        }
        let key = [count as i32, spaces[0] as i32, spaces[1] as i32, formats[0].0, formats[1].0];
        {
            let Ok(mut seen) = SEEN.lock() else { return };
            if seen.len() >= 8 || seen.contains(&key) {
                return;
            }
            seen.push(key);
        }
        tracing::info!(
            "HDR: the title presents {count} planes, colour spaces {} and {} (4294967295: unreadable), formats {} and {}",
            spaces[0],
            spaces[1],
            formats[0].0,
            formats[1].0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_ends_and_order() {
        assert!(pq_encode(0.0) < 1e-5);
        assert!((pq_encode(10000.0) - 1.0).abs() < 1e-6);
        let mut last = -1.0;
        for n in [0.0, 0.1, 1.0, 80.0, 203.0, 1000.0, 4000.0, 10000.0] {
            let v = pq_encode(n);
            assert!(v > last);
            last = v;
        }
    }

    #[test]
    fn paper_white_word() {
        // computed from the curve, not the cached pixel, so the env cannot make it brittle
        let code = (pq_encode(203.0) * 1023.0).round() as u32;
        assert!((592..=595).contains(&code));
        let word = u32::from_le_bytes((code | code << 10 | code << 20 | 3 << 30).to_le_bytes());
        assert_eq!(word >> 30, 3);
        assert_eq!(word & 1023, code);
        assert_eq!((word >> 20) & 1023, code);
    }

    #[cfg(windows)]
    #[test]
    fn spaces() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        assert_eq!(plane_space(12, DXGI_FORMAT_R8G8B8A8_UNORM), Space::Pq2020);
        assert_eq!(plane_space(UNKNOWN_SPACE, DXGI_FORMAT_R16G16B16A16_FLOAT), Space::Linear709);
        assert_eq!(plane_space(UNKNOWN_SPACE, DXGI_FORMAT_R8G8B8A8_UNORM), Space::Gamma22);
        assert_eq!(plane_space(1, DXGI_FORMAT_R8G8B8A8_UNORM), Space::Linear709);
        assert_eq!(plane_space(2, DXGI_FORMAT_R8G8B8A8_UNORM), Space::Gamma22);
        assert_eq!(plane_space(0, DXGI_FORMAT_R16G16B16A16_FLOAT), Space::Gamma22);
        assert_eq!(plane_space(UNKNOWN_SPACE, DXGI_FORMAT_R10G10B10A2_UNORM), Space::Pq2020);
        assert_eq!(plane_space(0, DXGI_FORMAT_R10G10B10A2_UNORM), Space::Gamma22);
        assert_eq!(plane_space(0, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB), Space::LinearPaper);
        assert_eq!(plane_space(UNKNOWN_SPACE, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB), Space::LinearPaper);
        for f in 0..40 {
            let want = matches!(f, 23..=25);
            assert_eq!(passthrough(Space::Pq2020, DXGI_FORMAT(f)), want);
            assert!(!passthrough(Space::Gamma22, DXGI_FORMAT(f)));
        }
    }
}
