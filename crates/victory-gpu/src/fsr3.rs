//! AMD FSR 3 Frame Generation through the FidelityFX API (FSR 3.1), for
//! GPUs that cannot use DLSS Frame Generation — an RTX 3080 Ti (Ampere)
//! in particular. Both off by default.
//!
//! - `VICTORY_FSR3_FG=1`: FSR 3 Frame Generation (2x) on our present, with
//!   the title's own upscaling still doing the upscaling. This is the
//!   milestone shipped here: upscaling stays the title's, frame generation
//!   is FSR 3's.
//! - `VICTORY_FSR3=1`: FSR 3 Super Resolution in place of the title's
//!   upscaling. Accepted, but **not implemented in this milestone**:
//!   `init` logs that and leaves it off, rather than pretending.
//! - `VICTORY_FSR3_DIR`: the folder of `fsr3_bridge.dll` (default: the
//!   `fsr3-sdk` folder beside this program, then
//!   `%LOCALAPPDATA%\victory\fsr3-sdk`, then `fsr3-sdk` under the current
//!   folder). `Get-FSR3.cmd` / `scripts/get-fsr3.ps1` build that bridge
//!   from the FidelityFX SDK and fetch AMD's signed DLL there.
//! - `VICTORY_FSR3_QUALITY`: quality, balanced, performance or dlaa.
//!   Only recorded for now; frame generation uses the render size the
//!   title already passes, not an upscaler mode.
//!
//! Why a bridge DLL, and not a direct `LoadLibrary` of an AMD DLL from
//! Rust: there is no `ffx_fsr3.dll`. FSR 3.1 is the FidelityFX API, five
//! functions (`ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`,
//! `ffxQuery`, `ffxConfigure`) in one prebuilt signed DLL,
//! `amd_fidelityfx_dx12.dll` (SDK 2.0 splits it into
//! `amd_fidelityfx_loader_dx12.dll` + effect DLLs), and every argument
//! is a descriptor struct whose layout lives in the SDK headers
//! (`ffx-api/include`). Hand-mirroring those structs in Rust from memory
//! would be guessing at the ABI. So all FidelityFX structs are built in
//! the small C++ bridge in `tools/fsr3-bridge/`, compiled against the
//! real SDK headers on Windows, and Rust talks to the bridge over a
//! minimal C ABI defined here (`BRIDGE_ABI_VERSION = 1`): opaque
//! pointers and scalars only, no AMD structs cross the boundary.
//!
//! The bridge, in turn, does the documented FSR 3.1 steps (GPUOpen,
//! "AMD FSR Frame Generation API", "FSR 3.1 Integration Overview"):
//! create a frame-generation context (`ffxCreateContextDescFrameGeneration`
//! + backend desc; SDK 1.1.4 has no version struct to chain — that arrived
//! in SDK 2.1), wrap the swap chain
//! (`ffxCreateContextDescFrameGenerationSwapChainWrapDX12`, type `0x30001`),
//! configure it once per frame (`ffxConfigureDescFrameGeneration`, frame ID
//! increments by exactly 1), and dispatch the prepare pass
//! (`ffxDispatchDescFrameGenerationPrepare`, with the camera info as a linked
//! struct — PrepareV2 replaced both only in SDK 2.1 / FSR 3.1.6) with the
//! depth and motion vectors the title's upscale dispatch already gave us.
//!
//! Anything missing (the bridge, AMD's DLL, the device hand-over, the
//! swap-chain wrap) leaves the title's own upscaling and our plain
//! present as they were, with a warning in the log — the same soft-fail
//! pattern as Streamline. FSR 3 frame generation and DLSS frame
//! generation are mutually exclusive: never both proxy swap chains.
//! `init` refuses FSR 3 if Streamline frame generation is already
//! available, and the launchers refuse to ask for both.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use crate::streamline::{self, Taken, Upscale};

/// The bridge DLL, built from `tools/fsr3-bridge/` against the
/// FidelityFX SDK. It loads AMD's signed DLL itself.
pub const BRIDGE_DLL: &str = "fsr3_bridge.dll";
/// The bridge C ABI this file speaks. The bridge returns this from
/// `fsr3_bridge_version()`; anything else is a build mismatch.
pub const BRIDGE_ABI_VERSION: i32 = 1;
/// AMD's signed FSR 3.1 DLL, which the bridge loads (SDK 2.0 loader
/// as a fallback). Neither is committed to the repository.
pub const AMD_DLL: &str = "amd_fidelityfx_dx12.dll";
pub const AMD_LOADER_DLL: &str = "amd_fidelityfx_loader_dx12.dll";

/// What the user asked for, read from the environment once.
#[derive(Clone, Debug)]
pub struct Asked {
    pub framegen: bool,
    /// Accepted, not implemented in this milestone (see the module docs).
    pub fsr3_upscale: bool,
    pub dir: PathBuf,
    pub quality: String,
}

impl Asked {
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok();
        let on = |name: &str| var(name).is_some_and(|v| v.trim() == "1");
        let quality = var("VICTORY_FSR3_QUALITY").unwrap_or_else(|| "quality".into());
        if !["quality", "balanced", "performance", "dlaa"].contains(&quality.trim().to_ascii_lowercase().as_str()) {
            return Err(format!(
                "VICTORY_FSR3_QUALITY={quality}: not one of quality, balanced, performance, dlaa"
            ));
        }
        Ok(Asked {
            framegen: on("VICTORY_FSR3_FG"),
            fsr3_upscale: on("VICTORY_FSR3"),
            dir: var("VICTORY_FSR3_DIR").map(PathBuf::from).unwrap_or_else(default_dir),
            quality: quality.trim().to_ascii_lowercase(),
        })
    }

    pub fn any(&self) -> bool {
        self.framegen
    }
}

/// Where the bridge is looked for without `VICTORY_FSR3_DIR`: the
/// `fsr3-sdk` folder beside this program (what `Get-FSR3.cmd` and
/// `victory get-fsr3` populate), then `%LOCALAPPDATA%\victory\fsr3-sdk`,
/// then `fsr3-sdk` under the current folder. The first that exists wins;
/// otherwise the first candidate, so the error names a real place.
fn default_dir() -> PathBuf {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("fsr3-sdk"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("victory").join("fsr3-sdk"));
    }
    candidates.push(PathBuf::from("fsr3-sdk"));
    for c in &candidates {
        if c.join(BRIDGE_DLL).is_file() {
            return c.clone();
        }
    }
    candidates.into_iter().next().unwrap_or_else(|| PathBuf::from("fsr3-sdk"))
}

/// What [`init`] made available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Available {
    pub framegen: bool,
}

/// The bridge's exported functions. All return 0 on success, otherwise
/// the FidelityFX return code (or -1 for a bridge-side failure).
struct Bridge {
    _library: Library,
    version: unsafe extern "C" fn() -> i32,
    create: unsafe extern "C" fn(
        *mut c_void, // device (ID3D12Device*)
        *mut c_void, // game queue (ID3D12CommandQueue*)
        u32,         // display width
        u32,         // display height
        u32,         // render width
        u32,         // render height
        u32,         // back-buffer format (DXGI_FORMAT)
        u32,         // flags (see fsr3_bridge.h)
        *mut *mut c_void, // out: Fsr3Fg*
    ) -> i32,
    wrap_swapchain: unsafe extern "C" fn(
        *mut c_void, // Fsr3Fg*
        *mut *mut c_void, // in/out: IDXGISwapChain4*
        *mut *mut c_void, // out: wrapped IDXGISwapChain4*
    ) -> i32,
    prepare: unsafe extern "C" fn(
        *mut c_void, // Fsr3Fg*
        *mut c_void, // command list (ID3D12GraphicsCommandList*)
        *mut c_void, // depth (ID3D12Resource*)
        u32,         // depth state (D3D12_RESOURCE_STATES)
        *mut c_void, // motion vectors (ID3D12Resource*)
        u32,         // motion-vector state
        f32,         // jitter x
        f32,         // jitter y
        f32,         // motion-vector scale x
        f32,         // motion-vector scale y
        f32,         // frame time, ms
        i32,         // reset (1 on a scene cut / first frame; accepted but
                    // ignored in SDK 1.1.4 — its prepare has only
                    // unused_reset, so a cut resets by a disjoint frameID)
        f32,         // camera near
        f32,         // camera far
        f32,         // camera vertical FOV, radians
        u64,         // frame id (increments by exactly 1 per frame)
    ) -> i32,
    configure: unsafe extern "C" fn(
        *mut c_void, // Fsr3Fg*
        *mut c_void, // swap chain (IDXGISwapChain*)
        *mut c_void, // hudless colour (ID3D12Resource*, may be null)
        u32,         // hudless colour state
        u64,         // frame id (same as the prepare's)
        i32,         // frame generation enabled
    ) -> i32,
    destroy: unsafe extern "C" fn(*mut c_void),
    /// Remake the frame-generation context in place at a new size,
    /// keeping the wrapped swap chain alive (see `resizing`).
    recreate: unsafe extern "C" fn(
        *mut c_void, // Fsr3Fg*
        u32,         // display width
        u32,         // display height
        u32,         // render width
        u32,         // render height
        u32,         // back-buffer format (DXGI_FORMAT)
        u32,         // flags (see fsr3_bridge.h)
    ) -> i32,
}

struct Library(windows_sys::Win32::Foundation::HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this owns the reference LoadLibraryExW took.
        unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0) };
    }
}

// SAFETY: a module handle.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();
static ASKED: OnceLock<Asked> = OnceLock::new();
static FG_READY: AtomicBool = AtomicBool::new(false);
static SWAPCHAIN_WRAPPED: AtomicBool = AtomicBool::new(false);
/// A bridge frame-generation context pointer. The bridge owns it; it is
/// only used on the presenting thread and destroyed with it.
#[derive(Clone, Copy)]
struct FgContext(*mut c_void);
// SAFETY: the bridge owns the context; it is only used on the presenting
// thread and destroyed with it.
unsafe impl Send for FgContext {}

/// The frame-generation context the bridge made, if any. Created lazily
/// on the first present, once the display size is known.
static FG: Mutex<Option<FgContext>> = Mutex::new(None);
unsafe impl Send for Bridge {}
unsafe impl Sync for Bridge {}

/// What is asked of frame generation now: 0 off, 2 frames shown for each
/// one rendered; `u32::MAX`: not asked yet (then 2, on from the start).
static FG_WANTED: AtomicU32 = AtomicU32::new(u32::MAX);
/// Frames shown for each one rendered (1 while it is off). FSR 3 FG is
/// 2x only in this milestone; the bridge does not yet report a measured
/// count, so this is the asked count, not a measured one.
static FG_PER: AtomicU32 = AtomicU32::new(1);

/// The host device and direct queue, for the bridge's context creation.
struct DeviceCtx {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for DeviceCtx {}
unsafe impl Sync for DeviceCtx {}

static DEVICE: OnceLock<DeviceCtx> = OnceLock::new();

/// The latest upscaling submitted, for the next present.
#[derive(Clone)]
struct Noted {
    id: u64,
    depth: std::sync::Arc<crate::resource::HostTexture>,
    motion: std::sync::Arc<crate::resource::HostTexture>,
    camera: streamline::plan::Camera,
    frame_ms: f32,
}

// SAFETY: host textures are free-threaded.
unsafe impl Send for Noted {}

static PENDING: Mutex<Vec<Noted>> = Mutex::new(Vec::new());
static SUBMITTED: Mutex<Option<Noted>> = Mutex::new(None);
static PRESENTED_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Flags passed to the bridge's create call. Bit 0: HDR colour.
/// Bit 1: inverted depth. Bit 2: display-resolution motion vectors.
/// Bit 3: motion vectors carry the jitter. Bit 4 (not set here):
/// async workloads — off in this milestone, so the optical-flow and
/// interpolation work runs on the graphics queue and cannot overlap
/// the next frame's work in a way this layer does not order.
const FLAG_HDR: u32 = 1 << 0;
const FLAG_DEPTH_INVERTED: u32 = 1 << 1;
const FLAG_DISPLAY_MV: u32 = 1 << 2;
const FLAG_MV_JITTER: u32 = 1 << 3;

fn flags_for(camera: &streamline::plan::Camera) -> u32 {
    let mut flags = 0;
    if camera.flags & streamline::plan::FFX_HIGH_DYNAMIC_RANGE != 0 {
        flags |= FLAG_HDR;
    }
    if camera.depth_inverted() {
        flags |= FLAG_DEPTH_INVERTED;
    }
    if camera.display_motion_vectors() {
        flags |= FLAG_DISPLAY_MV;
    }
    if camera.flags & streamline::plan::FFX_MOTION_VECTORS_JITTER_CANCELLATION != 0 {
        flags |= FLAG_MV_JITTER;
    }
    flags
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn load_bridge(dir: &Path) -> Result<Bridge, String> {
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    let path = std::path::absolute(dir.join(BRIDGE_DLL)).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(format!(
            "no {BRIDGE_DLL} in that folder (VICTORY_FSR3_DIR); run Get-FSR3.cmd to build it from the FidelityFX SDK"
        ));
    }
    let file = wide(&path);
    // Its own folder and System32 only, never the current folder.
    // SAFETY: a NUL-terminated full path.
    let module = unsafe {
        LoadLibraryExW(
            file.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(format!("{BRIDGE_DLL} did not load ({})", std::io::Error::last_os_error()));
    }
    let library = Library(module);
    let find = |name: &CStr| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const c_void)
            .ok_or_else(|| format!("{BRIDGE_DLL} has no {}", name.to_string_lossy()))
    };
    macro_rules! get {
        ($name:literal, $t:ty) => {
            // SAFETY: the export of that name has the bridge header's signature.
            unsafe { std::mem::transmute::<*const c_void, $t>(find($name)?) }
        };
    }
    let bridge = Bridge {
        version: get!(c"fsr3_bridge_version", unsafe extern "C" fn() -> i32),
        create: get!(
            c"fsr3_fg_create",
            unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, u32, u32, u32, u32, *mut *mut c_void) -> i32
        ),
        wrap_swapchain: get!(
            c"fsr3_fg_wrap_swapchain",
            unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut *mut c_void) -> i32
        ),
        prepare: get!(
            c"fsr3_fg_prepare",
            unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32, *mut c_void, u32, f32, f32, f32, f32, f32, i32, f32, f32, f32, u64) -> i32
        ),
        configure: get!(
            c"fsr3_fg_configure",
            unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32, u64, i32) -> i32
        ),
        destroy: get!(c"fsr3_fg_destroy", unsafe extern "C" fn(*mut c_void)),
        recreate: get!(
            c"fsr3_fg_recreate",
            unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, u32) -> i32
        ),
        _library: library,
    };
    // SAFETY: the bridge's own version query.
    let version = unsafe { (bridge.version)() };
    if version != BRIDGE_ABI_VERSION {
        return Err(format!(
            "{BRIDGE_DLL} speaks ABI version {version}, this build speaks {BRIDGE_ABI_VERSION}: rebuild the bridge (Get-FSR3.cmd)"
        ));
    }
    Ok(bridge)
}

/// Loads the bridge and asks for frame generation; call once, before
/// the host device is made and before the title's imports are bound.
/// Fails soft: what is not there is logged and left off.
pub fn init(asked: &Asked) -> Available {
    let _ = ASKED.set(asked.clone());
    if asked.fsr3_upscale {
        tracing::warn!(
            "FSR 3 upscaling (VICTORY_FSR3=1) is not implemented in this milestone: the upscaling already running (DLSS or the title's own) runs; frame generation is the FSR 3 feature here"
        );
    }
    if !asked.any() {
        return Available::default();
    }
    // Mutual exclusion with DLSS frame generation: two proxy swap
    // chains cannot both own the present. DLSS was initialised first
    // (run.rs); if its frame generation is live, FSR 3 stays off.
    // DLSS upscaling alone does not block FSR 3: on this machine
    // (RTX 3080 Ti) DLSS frame generation is not available at all,
    // so DLSS upscaling + FSR 3 frame generation is the combination
    // that runs at 1440p. (streamline::serving() is the wrong check
    // here: it is true whenever DLSS upscaling serves, even with no
    // frame generation — the control run proved it.)
    if streamline::framegen_live() {
        tracing::warn!(
            "FSR 3 frame generation and DLSS frame generation were both asked for: DLSS frame generation wins, FSR 3 stays off (set only one of VICTORY_FRAMEGEN / VICTORY_FSR3_FG)"
        );
        return Available::default();
    }
    match load_bridge(&asked.dir) {
        Ok(bridge) => {
            let _ = BRIDGE.set(bridge);
            tracing::info!(
                folder = %asked.dir.display(),
                quality = asked.quality,
                "FSR 3 frame generation: bridge loaded (AMD FidelityFX API 3.1); frame generation is made ready when the host device is"
            );
            Available { framegen: true }
        }
        Err(why) => {
            tracing::warn!(
                folder = %asked.dir.display(),
                "FSR 3 frame generation: {why}; the upscaling already running and our present run"
            );
            Available::default()
        }
    }
}

/// Whether the title's FidelityFX calls are to be served for FSR 3
/// frame generation: `victory_hle` then puts its own in front of them.
pub fn serving() -> bool {
    BRIDGE.get().is_some() && ASKED.get().is_some_and(|a| a.framegen)
}

/// Whether FSR 3 frame generation is to note the title's upscale
/// dispatches (the bridge is loaded and frame generation was asked).
pub fn framegen_wanted() -> bool {
    serving()
}

/// The host device is made: remember it and the direct queue for the
/// bridge's frame-generation context, which is created lazily on the
/// first present, once the display size is known. Returns whether the
/// bridge is there to make it with.
pub(crate) fn device_made(device: &ID3D12Device, queue: &ID3D12CommandQueue) -> bool {
    if BRIDGE.get().is_none() {
        return false;
    }
    let _ = DEVICE.set(DeviceCtx { device: device.clone(), queue: queue.clone() });
    true
}

/// Create the bridge's frame-generation context for `display` /
/// `render` / `format`, if it does not exist yet. `None` on failure
/// (logged, frame generation stays off). The flags come from the
/// latest noted upscaling's camera, if there is one yet; otherwise
/// 0 (no HDR, no inverted depth), and the context is remade on
/// resize if a later camera disagrees — in practice the first
/// upscaling is noted before the first present.
fn ensure_context(display: [u32; 2], render: [u32; 2], format: DXGI_FORMAT) -> Option<*mut c_void> {
    {
        let fg = FG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fg) = *fg {
            return Some(fg.0);
        }
    }
    let (bridge, device) = (BRIDGE.get()?, DEVICE.get()?);
    let mut fg: *mut c_void = std::ptr::null_mut();
    let flags = SUBMITTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |n| flags_for(&n.camera));
    // SAFETY: live device and queue, sizes and format of the swap chain.
    let code = unsafe {
        (bridge.create)(
            device.device.as_raw(),
            device.queue.as_raw(),
            display[0],
            display[1],
            render[0],
            render[1],
            format.0 as u32,
            flags,
            &mut fg,
        )
    };
    if code != 0 || fg.is_null() {
        tracing::warn!(code, "FSR 3 frame generation: the bridge did not make a frame-generation context ({code}); frame generation stays off");
        return None;
    }
    *FG.lock().unwrap_or_else(|e| e.into_inner()) = Some(FgContext(fg));
    FG_READY.store(true, Release);
    // NOTE: the identifier `display` must not appear inside a tracing
    // macro at all — it resolves to tracing::field::display, not this
    // local, whether as `?display`, `{display:?}`, or a plain argument
    // (all three forms failed to compile). Bind the sizes to other names
    // in plain code first; only those names go into the macro.
    let (dw, dh) = (display[0], display[1]);
    let (rw, rh) = (render[0], render[1]);
    tracing::info!("FSR 3 frame generation: context made (display [{dw}, {dh}], render [{rw}, {rh}])");
    Some(fg)
}

/// Wrap `swap_chain` in the bridge's frame-interpolation swap chain
/// (FSR 3.1's proxy, `IDXGISwapChain4`). Returns the wrapped chain to
/// present through, or the original on any failure — frame generation
/// then stays off and the plain present runs.
pub(crate) fn wrap_swapchain(
    swap_chain: IDXGISwapChain3,
    display: [u32; 2],
    render: [u32; 2],
    format: DXGI_FORMAT,
) -> IDXGISwapChain3 {
    let Some(fg) = ensure_context(display, render, format) else {
        return swap_chain;
    };
    let Some(bridge) = BRIDGE.get() else {
        return swap_chain;
    };
    let Ok(swap4) = swap_chain.cast::<IDXGISwapChain4>() else {
        tracing::warn!("FSR 3 frame generation: the swap chain does not offer IDXGISwapChain4; frame generation stays off");
        return swap_chain;
    };
    let mut raw: *mut c_void = swap4.as_raw();
    let mut wrapped: *mut c_void = std::ptr::null_mut();
    // SAFETY: a live swap chain; the bridge replaces it with its proxy
    // and hands back a new reference to that proxy.
    let code = unsafe { (bridge.wrap_swapchain)(fg, &mut raw, &mut wrapped) };
    if code != 0 || wrapped.is_null() {
        tracing::warn!(code, "FSR 3 frame generation: the swap chain was not wrapped ({code}); presenting as before, frame generation off");
        return swap_chain;
    }
    SWAPCHAIN_WRAPPED.store(true, Release);
    tracing::info!("FSR 3 frame generation: our swap chain is the frame-interpolation proxy; frame generation can run on its presents");
    // SAFETY: the proxy's one reference, now ours; it implements the
    // swap-chain interfaces up to IDXGISwapChain4 and holds the chain.
    unsafe { IDXGISwapChain3::from_raw(wrapped) }
}

/// The upscaling `up` asked for on the title's thread: with FSR 3
/// frame generation on, its depth and motion vectors are noted
/// (recorded into the title's list, for the present) and the upscaling
/// already running (DLSS or the title's own) runs. The upscaling runs
/// otherwise, without frame generation.
pub fn upscale(up: &Upscale) -> Taken {
    if !serving() || (!FG_READY.load(Acquire) && DEVICE.get().is_none()) {
        return Taken::No("FSR 3 frame generation is not loaded");
    }
    // The context is made lazily at the first present; until then the
    // upscaling is still noted, so the first present has depth and
    // motion vectors to tag.
    if [streamline::COLOUR, streamline::DEPTH, streamline::MOTION, streamline::OUTPUT]
        .iter()
        .any(|&i| up.resources[i].0.is_null())
    {
        return Taken::No("a required texture (colour, depth, motion vectors, output) is missing");
    }
    if up.camera.render.contains(&0) || up.camera.output.contains(&0) {
        return Taken::No("an empty render or output size");
    }
    let id = NEXT_ID.fetch_add(1, Relaxed) + 1;
    let up = *up;
    let asked = crate::list::external(
        up.list,
        Box::new(move |host| {
            note_prepare(host, &up, id);
            true
        }),
    );
    if !asked {
        return Taken::No("the title's list records nowhere (no host list)");
    }
    Taken::Noted
}

/// Record the frame-generation prepare pass into `host` (where the
/// title's list is now), and remember this upscaling for the present.
/// The prepare needs depth and motion vectors as host textures, in
/// the states the title gave; without them there is nothing to
/// interpolate from, and the title's own upscaling runs alone.
fn note_prepare(host: &crate::host::HostList, up: &Upscale, id: u64) {
    let (Some(depth), Some(motion)) = (
        crate::resource::host_copy(up.resources[streamline::DEPTH].0),
        crate::resource::host_copy(up.resources[streamline::MOTION].0),
    ) else {
        return;
    };
    // The inputs as a compute shader's would be (aliased views, uploads
    // of title-written textures), then in the states FSR 3 is told.
    for i in [streamline::DEPTH, streamline::MOTION] {
        if crate::external::prepare_input(host, up.resources[i].0).is_none() {
            return;
        }
    }
    let wanted = |state: u32| streamline::plan::input_state(state);
    let (Some((depth_tex, depth_state)), Some((motion_tex, motion_state))) = (
        crate::external::host_texture(host, up.resources[streamline::DEPTH].0, wanted(up.resources[streamline::DEPTH].1)),
        crate::external::host_texture(host, up.resources[streamline::MOTION].0, wanted(up.resources[streamline::MOTION].1)),
    ) else {
        return;
    };
    // The bridge records the prepare dispatch into this open list.
    // Until the context exists (first present), the dispatch is
    // skipped and only the noting below happens.
    if let (Some(bridge), Some(fg)) = (BRIDGE.get(), *FG.lock().unwrap_or_else(|e| e.into_inner())) {
        let camera = up.camera;
        let mv_scale = [
            camera.motion_scale[0] / camera.render[0].max(1) as f32,
            camera.motion_scale[1] / camera.render[1].max(1) as f32,
        ];
        // SAFETY: an open command list; live textures in the states given.
        let code = unsafe {
            (bridge.prepare)(
                fg.0,
                host.list.as_raw(),
                depth_tex.as_raw(),
                depth_state,
                motion_tex.as_raw(),
                motion_state,
                camera.jitter[0],
                camera.jitter[1],
                mv_scale[0],
                mv_scale[1],
                up.frame_ms,
                camera.reset as i32,
                camera.near,
                camera.far,
                camera.fov_y,
                id,
            )
        };
        if code != 0 {
            tracing::warn!(id, code, "FSR 3 frame generation: the prepare dispatch was not recorded ({code})");
            return;
        }
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|n| id.saturating_sub(n.id) < 16);
    pending.push(Noted { id, depth, motion, camera: up.camera, frame_ms: up.frame_ms });
    drop(pending);
    host.mark_upscale(id);
}

/// A host list that recorded upscaling `id` is submitted (in
/// submission order, before the present that follows it).
pub(crate) fn submitted(id: u64) {
    let noted = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let at = pending.iter().position(|n| n.id == id);
        at.map(|i| pending.swap_remove(i))
    };
    if let Some(noted) = noted {
        let mut latest = SUBMITTED.lock().unwrap_or_else(|e| e.into_inner());
        if latest.as_ref().is_none_or(|l| l.id < noted.id) {
            *latest = Some(noted);
        }
    }
}

/// What [`present_begins`] began, for [`present_ends`].
pub(crate) struct Presenting {
    frame: u64,
    on: bool,
}

/// Before our swap chain's Present of title frame `frame`: configure
/// FSR 3 frame generation for this frame (frame ID increments by
/// exactly 1 per frame; any other step resets its logic), with the
/// hudless colour if there is one. Returns `None` without frame
/// generation (nothing is timed then, and the plain present runs).
pub(crate) fn present_begins(
    frame: u64,
    hudless: Option<(&ID3D12Resource, u32)>,
    display: [u32; 2],
    swap_chain: &IDXGISwapChain3,
) -> Option<Presenting> {
    if !FG_READY.load(Acquire) || !SWAPCHAIN_WRAPPED.load(Acquire) {
        // First present: make the context now that the size is known.
        // `wrap_swapchain` already did this when the presenter was
        // made; if it did not, there is nothing to begin here.
        return None;
    }
    if !crate::queue::line_on() {
        tracing::warn!("FSR 3 frame generation: the line of submissions did not start; frame generation stays off");
        return None;
    }
    let fresh = {
        let latest = SUBMITTED.lock().unwrap_or_else(|e| e.into_inner());
        latest
            .as_ref()
            .filter(|n| n.id > PRESENTED_ID.load(Relaxed))
            .map(|n| n.id)
    };
    let Some(id) = fresh else {
        // No upscaling this frame (a menu, a load): frame generation
        // stays off for it; the next present with an upscaling turns
        // it back on.
        return Some(Presenting { frame, on: false });
    };
    PRESENTED_ID.store(id, Relaxed);
    let (bridge, fg) = (BRIDGE.get()?, *FG.lock().unwrap_or_else(|e| e.into_inner()));
    let fg = fg?;
    let wanted = framegen_wanted_times();
    let on = wanted != 0;
    let (hudless_tex, hudless_state) = hudless
        .map(|(t, s)| (t.as_raw(), s))
        .unwrap_or((std::ptr::null_mut(), 0));
    // SAFETY: the swap chain and (if any) the hudless texture are live.
    let code = unsafe { (bridge.configure)(fg.0, swap_chain.as_raw(), hudless_tex, hudless_state, id, on as i32) };
    if code != 0 {
        tracing::warn!(frame, code, "FSR 3 frame generation: configure failed ({code}); presenting as before");
        return Some(Presenting { frame, on: false });
    }
    if on {
        FG_PER.store(2, Relaxed);
    } else {
        FG_PER.store(1, Relaxed);
    }
    let _ = display;
    Some(Presenting { frame, on })
}

/// After our swap chain's Present: nothing to time for FSR 3 (the
/// bridge's swap chain reports its own state); kept for symmetry with
/// Streamline's `present_ends`, so the host's present path reads the
/// same either way.
pub(crate) fn present_ends(_presenting: Option<Presenting>) {}

/// Whether frame generation can be switched while the title runs: it
/// was planned at the start, so our swap chain is the frame-
/// interpolation proxy (the F1 menu asks).
pub(crate) fn framegen_live() -> bool {
    FG_READY.load(Acquire) && SWAPCHAIN_WRAPPED.load(Acquire)
}

/// Frames shown for each one rendered, as last asked: 2 with frame
/// generation on, 0 ready but off; 1 without it.
fn framegen_wanted_times() -> u32 {
    match FG_WANTED.load(Relaxed) {
        u32::MAX => 2,
        n => n,
    }
}

/// Asks frame generation off (0) or on (2 frames shown for each one
/// rendered), from the next present on.
pub(crate) fn set_framegen(times: u32) {
    FG_WANTED.store(if times == 0 { 0 } else { 2 }, Relaxed);
    tracing::info!(times, "FSR 3 frame generation: asked from the settings menu");
}

/// For the frame-rate texts: frames shown for each one rendered
/// (1 without frame generation).
pub(crate) fn shown_per_frame() -> u32 {
    FG_PER.load(Relaxed).max(1)
}

/// Our swap chain's buffers are about to be made again at `display` /
/// `format` (the host `ResizeBuffers` the chain right after this
/// call): the frame-generation context is remade in place at the new
/// size, and frame generation resumes on the next present.
///
/// The remake is in place on purpose. Destroying the whole bridge
/// context here would destroy the swapchain context too, and the host
/// would then be presenting (and `ResizeBuffers`ing) through a dead
/// proxy. The wrapped chain stays alive — its swapchain context's
/// callbacks point at the same bridge state — while only its
/// frame-generation context is replaced, at the new size, with the
/// render size of the latest noted upscaling (or the display size).
/// On failure frame generation stays off and the proxy presents
/// through as before.
pub(crate) fn resizing(display: [u32; 2], format: DXGI_FORMAT) {
    if !FG_READY.load(Acquire) {
        return;
    }
    let Some(fg) = *FG.lock().unwrap_or_else(|e| e.into_inner()) else {
        return;
    };
    let Some(bridge) = BRIDGE.get() else {
        return;
    };
    let (render, flags) = SUBMITTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|n| (n.camera.render, flags_for(&n.camera)))
        .unwrap_or((display, 0));
    // SAFETY: a context the bridge made; the host has settled every
    // submission, so nothing is in flight through it.
    let code = unsafe {
        (bridge.recreate)(fg.0, display[0], display[1], render[0], render[1], format.0 as u32, flags)
    };
    if code != 0 {
        FG_READY.store(false, Release);
        SWAPCHAIN_WRAPPED.store(false, Release);
        FG_PER.store(1, Relaxed);
        tracing::warn!(code, "FSR 3 frame generation: its context was not remade at the new size ({code}); frame generation stays off, presenting as before");
        return;
    }
    // NOTE: see the same note at the "context made" log above — the
    // identifier `display` must not appear inside a tracing macro.
    let (dw, dh) = (display[0], display[1]);
    let (rw, rh) = (render[0], render[1]);
    tracing::info!("FSR 3 frame generation: its context was remade at the new size (display [{dw}, {dh}], render [{rw}, {rh}])");
}

/// A plain check, without the title: what is there for FSR 3 frame
/// generation on this PC. Used by the log at start; a fuller check
/// needs the game running (depth / motion vectors only exist there).
pub fn check() -> Vec<String> {
    let asked = Asked::from_env().unwrap_or(Asked {
        framegen: false,
        fsr3_upscale: false,
        dir: default_dir(),
        quality: "quality".into(),
    });
    let mut lines = vec![format!("FSR 3 frame generation: asked {}", asked.framegen)];
    lines.push(format!("  folder: {}", asked.dir.display()));
    lines.push(format!(
        "  bridge: {}",
        if asked.dir.join(BRIDGE_DLL).is_file() { "found" } else { "missing — run Get-FSR3.cmd" }
    ));
    lines.push(format!(
        "  AMD DLL: {}",
        if asked.dir.join(AMD_DLL).is_file() {
            "amd_fidelityfx_dx12.dll found"
        } else if asked.dir.join(AMD_LOADER_DLL).is_file() {
            "amd_fidelityfx_loader_dx12.dll found (SDK 2.0 split)"
        } else {
            "missing — Get-FSR3.cmd fetches it from the FidelityFX SDK release"
        }
    ));
    lines.push("  Note: frame generation itself can only be checked in the game (it needs the title's depth and motion vectors).".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_and_amd_dll_names_are_the_real_ones() {
        // Not `ffx_fsr3.dll`: there is no such DLL. FSR 3.1 is the
        // FidelityFX API in amd_fidelityfx_dx12.dll, reached here
        // through the bridge built from the SDK.
        assert_eq!(BRIDGE_ABI_VERSION, 1);
        assert_eq!(BRIDGE_DLL, "fsr3_bridge.dll");
        assert_eq!(AMD_DLL, "amd_fidelityfx_dx12.dll");
        assert_eq!(AMD_LOADER_DLL, "amd_fidelityfx_loader_dx12.dll");
    }

    #[test]
    fn flags_follow_the_cameras_ffx_flags() {
        let camera = |flags: u32| streamline::plan::Camera {
            jitter: [0.0, 0.0],
            motion_scale: [1.0, 1.0],
            render: [1280, 720],
            output: [2560, 1440],
            near: 0.1,
            far: 5000.0,
            fov_y: 0.9,
            reset: false,
            flags,
        };
        assert_eq!(flags_for(&camera(0)), 0);
        assert_eq!(
            flags_for(&camera(
                streamline::plan::FFX_HIGH_DYNAMIC_RANGE | streamline::plan::FFX_DEPTH_INVERTED
            )),
            FLAG_HDR | FLAG_DEPTH_INVERTED
        );
    }

    #[test]
    fn framegen_is_two_times_only() {
        FG_WANTED.store(6, Relaxed);
        assert_eq!(framegen_wanted_times(), 2);
        FG_WANTED.store(0, Relaxed);
        assert_eq!(framegen_wanted_times(), 0);
        FG_WANTED.store(u32::MAX, Relaxed);
    }
}
