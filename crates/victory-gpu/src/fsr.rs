//! Optional FSR 3.1 frame generation, independent of the upscaler.
//! Interpolation uses the game queue; presentation is paced on the SDK thread.
mod abi;
mod pacing;

use crate::streamline::plan::Camera;
use std::ffi::{CStr, c_void};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*},
};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::*};
use windows::core::Interface;

struct Api {
    _library: usize,
    create: abi::Create,
    destroy: abi::Destroy,
    configure: abi::Call,
    dispatch: abi::Call,
}
static API: OnceLock<Api> = OnceLock::new();
static READY: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicBool = AtomicBool::new(false);
static GENERATED: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static PER_FRAME: AtomicU32 = AtomicU32::new(1);
static PRESENT_REAL: AtomicU64 = AtomicU64::new(0);
static PRESENT_GENERATED: AtomicU64 = AtomicU64::new(0);
static STATE: Mutex<Option<Box<State>>> = Mutex::new(None);

struct State {
    swap_context: abi::Context,
    callback: Box<Callback>,
    swap: IDXGISwapChain4,
    key: Option<([u32; 2], [u32; 2], u32, u32)>,
    frame: u64,
    reset: bool,
    enabled: bool,
    generated_before: u64,
    // Retained until WAIT_FOR_PRESENTS completes.
    held: Vec<ID3D12Resource>,
}
struct Callback {
    context: std::cell::UnsafeCell<abi::Context>,
    reset: AtomicBool,
    pacer: Mutex<pacing::Pacer>,
}
// SAFETY: only the ordered presentation thread uses this state, under STATE.
// The generation callback executes synchronously inside SDK Present. The
// presentation callback uses only its own pacer lock, never STATE.
unsafe impl Send for State {}

pub(crate) fn selected() -> bool {
    std::env::var("VICTORY_FRAMEGEN_BACKEND").is_ok_and(|v| v.trim().eq_ignore_ascii_case("fsr"))
}
pub(crate) fn ready() -> bool {
    READY.load(Acquire)
}
pub(crate) fn live() -> bool {
    LIVE.load(Acquire)
}
pub(crate) fn counts() -> (u64, u64) {
    (GENERATED.load(Relaxed), ERRORS.load(Relaxed))
}
pub(crate) fn per_frame() -> u32 {
    PER_FRAME.load(Relaxed)
}
pub(crate) fn presentation_counts() -> (u64, u64) {
    (PRESENT_REAL.load(Relaxed), PRESENT_GENERATED.load(Relaxed))
}

pub(crate) fn init() -> Result<(), String> {
    if API.get().is_none() {
        let dir = std::env::var_os("VICTORY_FSR_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("amd-sdk"));
        let path =
            std::path::absolute(dir.join("amd_fidelityfx_dx12.dll")).map_err(|e| e.to_string())?;
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // Keep the module loaded for the lifetime of SDK callbacks.
        unsafe {
            use windows_sys::Win32::System::LibraryLoader::*;
            let library = LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            );
            if library.is_null() {
                return Err(format!(
                    "cannot load {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let symbol = |name: &CStr| {
                GetProcAddress(library, name.as_ptr().cast())
                    .ok_or_else(|| format!("missing {}", name.to_string_lossy()))
            };
            let loaded = (|| {
                Ok::<_, String>(Api {
                    _library: library as usize,
                    create: std::mem::transmute::<unsafe extern "system" fn() -> isize, abi::Create>(
                        symbol(c"ffxCreateContext")?,
                    ),
                    destroy: std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        abi::Destroy,
                    >(symbol(c"ffxDestroyContext")?),
                    configure: std::mem::transmute::<unsafe extern "system" fn() -> isize, abi::Call>(
                        symbol(c"ffxConfigure")?,
                    ),
                    dispatch: std::mem::transmute::<unsafe extern "system" fn() -> isize, abi::Call>(
                        symbol(c"ffxDispatch")?,
                    ),
                })
            })();
            match loaded {
                Ok(api) => {
                    let _ = API.set(api);
                }
                Err(e) => {
                    windows_sys::Win32::Foundation::FreeLibrary(library);
                    return Err(e);
                }
            }
        }
        tracing::info!(path = %path.display(), "FSR 3.1 frame-generation API loaded (2x, paced presentation)");
    }
    READY.store(true, Release);
    Ok(())
}

fn ok(code: u32, operation: &str) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: FidelityFX error {code}"))
    }
}

pub(crate) fn create_swap(
    factory: &IDXGIFactory2,
    queue: &ID3D12CommandQueue,
    hwnd: windows::Win32::Foundation::HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Result<IDXGISwapChain3, String> {
    let api = API.get().ok_or("FSR not loaded")?;
    let mut raw = std::ptr::null_mut();
    let mut desc = abi::CreateSwap {
        header: abi::Header::new(0x30006),
        swapchain: &mut raw,
        hwnd: hwnd.0,
        desc: std::ptr::from_ref(desc).cast(),
        fullscreen: std::ptr::null(),
        factory: factory.as_raw(),
        queue: queue.as_raw(),
    };
    let mut context = std::ptr::null_mut();
    unsafe {
        let code = (api.create)(&mut context, &mut desc.header, std::ptr::null());
        if code != 0 {
            if !raw.is_null() {
                drop(IDXGISwapChain4::from_raw(raw));
            }
            return Err(format!("FSR swap-chain creation: {code}"));
        }
        let swap = IDXGISwapChain4::from_raw(raw);
        let result = swap.cast().map_err(|e| e.to_string())?;
        // v1.1.4's built-in average resets on >100ms frames and stays zero
        // until ten samples accumulate. Use our callback to space each pair.
        // The SDK subtracts this margin from its half-period, clamping at zero;
        // 1000ms exceeds every sample its pacer accepts, preventing double pacing.
        let mut tuning = abi::PacingTuning {
            safety_margin_ms: 1000.0,
            variance_factor: 0.0,
            hybrid_spin: true,
            spin_time: 2,
            wait_on_fence: true,
        };
        let pacing = abi::SwapKeyValue {
            header: abi::Header::new(0x30008),
            key: 2,
            value: 0,
            ptr: std::ptr::from_mut(&mut tuning).cast(),
        };
        if let Err(error) = ok(
            (api.configure)(&mut context, &pacing.header),
            "configure FSR pacing",
        ) {
            let _ = (api.destroy)(&mut context, std::ptr::null());
            return Err(error);
        }
        *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(State {
            swap_context: context,
            callback: Box::new(Callback {
                context: std::cell::UnsafeCell::new(std::ptr::null_mut()),
                reset: AtomicBool::new(true),
                pacer: Mutex::new(pacing::Pacer::default()),
            }),
            swap,
            key: None,
            frame: 0,
            reset: true,
            enabled: false,
            generated_before: 0,
            held: Vec::new(),
        }));
        LIVE.store(true, Release);
        tracing::info!("FSR: interpolation swap chain created");
        Ok(result)
    }
}

impl State {
    fn wait(&mut self) -> Result<(), String> {
        let header = abi::Header::new(0x30007);
        unsafe {
            ok(
                (API.get().unwrap().dispatch)(&mut self.swap_context, &header),
                "wait for FSR presents",
            )?;
        }
        self.held.clear();
        Ok(())
    }
    fn configure(
        &mut self,
        enabled: bool,
        hudless: abi::Resource,
        size: [u32; 2],
    ) -> Result<(), String> {
        if unsafe { (*self.callback.context.get()).is_null() } {
            return Ok(());
        }
        let desc = abi::Configure {
            header: abi::Header::new(0x20002),
            swapchain: self.swap.as_raw(),
            present_callback: Some(present),
            present_user: std::ptr::from_ref(&*self.callback).cast_mut().cast(),
            generate: Some(generate),
            generate_user: std::ptr::from_ref(&*self.callback).cast_mut().cast(),
            enabled,
            hudless,
            rect: [0, 0, size[0] as i32, size[1] as i32],
            frame: self.frame,
            ..Default::default()
        };
        self.callback.reset.store(self.reset, Release);
        if !enabled {
            self.callback
                .pacer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
        }
        unsafe {
            ok(
                (API.get().unwrap().configure)(self.callback.context.get(), &desc.header),
                "configure FSR",
            )?;
        }
        self.enabled = enabled;
        Ok(())
    }
    fn destroy_effect(&mut self) -> Result<(), String> {
        self.wait()?;
        self.configure(false, abi::Resource::default(), [0, 0])?;
        self.wait()?;
        if unsafe { !(*self.callback.context.get()).is_null() } {
            unsafe {
                ok(
                    (API.get().unwrap().destroy)(self.callback.context.get(), std::ptr::null()),
                    "destroy FSR",
                )?;
                *self.callback.context.get() = std::ptr::null_mut();
            }
        }
        self.key = None;
        self.reset = true;
        Ok(())
    }
}

unsafe extern "C" fn generate(desc: *mut abi::Generate, user: *mut c_void) -> u32 {
    // SAFETY: SDK invokes this with its dispatch descriptor and our stable Box.
    // No STATE lock here: Present/wait may be waiting on this callback.
    unsafe {
        let callback = &*user.cast::<Callback>();
        (*desc).reset |= callback.reset.load(Acquire);
        let code = (API.get().unwrap().dispatch)(callback.context.get(), &(*desc).header);
        if code == 0 {
            let before = GENERATED.fetch_add(u64::from((*desc).count), Relaxed);
            if before == 0 {
                tracing::info!(
                    frames = (*desc).count,
                    "FSR: first interpolation dispatch succeeded"
                );
            }
        } else {
            let before = ERRORS.fetch_add(1, Relaxed);
            if before == 0 {
                tracing::error!(code, "FSR: interpolation dispatch failed");
            }
        }
        code
    }
}

/// The SDK already reconstructs/extracts UI into the generated colour. Our
/// presentation callback copies that composited image, as the default UI path
/// does with no separately registered UI surface, and spaces the real partner.
unsafe extern "C" fn present(desc: *mut abi::Present, user: *mut c_void) -> u32 {
    use std::mem::ManuallyDrop;
    use std::time::Instant;
    unsafe {
        let desc = &*desc;
        let callback = &*user.cast::<Callback>();
        // The swapchain supplies UAV interpolation output or PRESENT real colour.
        // Reject unexpected inputs rather than record incorrect resource barriers.
        let source_state = match desc.color.state {
            2 => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            128 => D3D12_RESOURCE_STATE_PRESENT,
            _ => {
                ERRORS.fetch_add(1, Relaxed);
                tracing::error!(
                    state = desc.color.state,
                    "FSR: unsupported presentation source state"
                );
                return 1;
            }
        };
        if desc.list.is_null()
            || desc.color.resource.is_null()
            || desc.output.resource.is_null()
            || !desc.ui.resource.is_null()
            || desc.output.state != 128
        {
            ERRORS.fetch_add(1, Relaxed);
            tracing::error!("FSR: invalid presentation resources or separately registered UI");
            return 1;
        }
        // Borrow SDK-owned interfaces; do not Release them on return.
        let list = ManuallyDrop::new(ID3D12GraphicsCommandList::from_raw(desc.list));
        let color = ManuallyDrop::new(ID3D12Resource::from_raw(desc.color.resource));
        let output = ManuallyDrop::new(ID3D12Resource::from_raw(desc.output.resource));
        list.ResourceBarrier(&[
            crate::host::transition(&color, source_state, D3D12_RESOURCE_STATE_COPY_SOURCE),
            crate::host::transition(
                &output,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ),
        ]);
        list.CopyResource(&*output, &*color);
        list.ResourceBarrier(&[
            crate::host::transition(&color, D3D12_RESOURCE_STATE_COPY_SOURCE, source_state),
            crate::host::transition(
                &output,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PRESENT,
            ),
        ]);
        let deadline = callback
            .pacer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deadline(desc.generated, desc.frame, Instant::now());
        if let Some(deadline) = deadline {
            if let Some(delay) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(delay);
            }
        }
        if desc.generated {
            &PRESENT_GENERATED
        } else {
            &PRESENT_REAL
        }
        .fetch_add(1, Relaxed);
        0
    }
}

fn resource(texture: &ID3D12Resource, state: u32) -> Result<abi::Resource, String> {
    let d = unsafe { texture.GetDesc() };
    if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || d.DepthOrArraySize != 1
        || d.SampleDesc.Count != 1
    {
        return Err("FSR inputs must be single-sample, non-array 2D textures".into());
    }
    let format = abi::surface(d.Format.0)
        .ok_or_else(|| format!("unsupported FSR texture format {}", d.Format.0))?;
    let mut usage = match d.Format.0 {
        40 | 55 => 4,
        20 | 45 => 4 | 32,
        _ => 0,
    };
    if d.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS) {
        usage |= 2;
    }
    Ok(abi::Resource {
        resource: texture.as_raw(),
        description: [
            2,
            format,
            d.Width as u32,
            d.Height,
            u32::from(d.DepthOrArraySize),
            u32::from(d.MipLevels),
            0,
            usage,
        ],
        state,
    })
}

/// Inputs from the latest submitted upscale; owned until interpolation finishes.
pub(crate) struct Inputs {
    pub depth: (ID3D12Resource, u32),
    pub motion: (ID3D12Resource, u32),
    pub camera: Camera,
    pub frame_ms: f32,
}

/// Called on the open presentation list, after composition and before submission.
pub(crate) fn prepare(
    device: &ID3D12Device,
    list: &crate::host::HostList,
    size: [u32; 2],
    format: i32,
    inputs: Option<Inputs>,
    hudless: Option<(ID3D12Resource, u32)>,
    enabled: bool,
) {
    if !live() {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = guard.as_mut() else { return };
    let result = (|| -> Result<(), String> {
        s.wait()?;
        s.generated_before = GENERATED.load(Relaxed);
        s.frame += 1;
        let Some(input) = inputs.filter(|_| enabled) else {
            s.reset = true;
            return s.configure(false, abi::Resource::default(), size);
        };
        let camera = input.camera;
        if camera.output != size {
            return Err("upscale and presentation sizes differ".into());
        }
        // This prototype's presentation path is SDR.
        if !matches!(format, 28 | 87) {
            return Err("FSR prototype needs an SDR RGBA8/BGRA8 swap chain".into());
        }
        let flags = camera.flags & 0x1e;
        let format = abi::surface(format).ok_or("unsupported backbuffer format")?;
        let key = (size, camera.render, flags, format);
        if s.key != Some(key) {
            s.destroy_effect()?;
            let mut backend = abi::Backend {
                header: abi::Header::new(2),
                device: device.as_raw(),
            };
            let mut desc = abi::CreateFg {
                header: abi::Header {
                    kind: 0x20001,
                    next: &mut backend.header,
                },
                flags,
                display: size,
                render: camera.render,
                format,
            };
            unsafe {
                ok(
                    (API.get().unwrap().create)(
                        s.callback.context.get(),
                        &mut desc.header,
                        std::ptr::null(),
                    ),
                    "create FSR context",
                )?;
            }
            s.key = Some(key);
            tracing::info!(?size, render = ?camera.render, "FSR: frame-generation context created");
        }
        s.reset |= camera.reset;
        let mut hud = abi::Resource::default();
        if let Some((t, before)) = &hudless {
            let ffx_state = match *before {
                0 => 1,
                0x8 => 2,
                0x40 => 4,
                0x80 => 8,
                0xc0 => 12,
                0x800 => 16,
                0x400 => 32,
                4 => 256,
                _ => return Err("unsupported HUDless state".into()),
            };
            hud = resource(t, ffx_state)?;
            list.keep(t);
        }
        // SDK restores the declared resource states. Normalize to shader read
        // explicitly so unknown D3D12 state combinations never cross the ABI.
        let mut resources = vec![input.depth, input.motion];
        let mut barriers = Vec::new();
        for (t, before) in &resources {
            list.keep(t);
            if *before != 0xc0 {
                barriers.push(crate::host::transition(
                    t,
                    D3D12_RESOURCE_STATES(*before as i32),
                    D3D12_RESOURCE_STATES(0xc0),
                ));
            }
        }
        let depth = resource(&resources[0].0, 12)?;
        let motion = resource(&resources[1].0, 12)?;
        s.configure(true, hud, size)?;
        let desc = abi::Prepare {
            header: abi::Header::new(0x20004),
            frame: s.frame,
            flags: 0,
            list: list.list.as_raw(),
            render: camera.render,
            jitter: camera.jitter,
            motion_scale: camera.motion_scale,
            frame_ms: if input.frame_ms.is_finite() && input.frame_ms > 0.0 {
                input.frame_ms
            } else {
                16.667
            },
            reset: camera.reset,
            near: camera.near,
            far: camera.far,
            fov: camera.fov_y,
            meters: 1.0,
            depth,
            motion,
        };
        unsafe {
            if !barriers.is_empty() {
                list.list.ResourceBarrier(&barriers);
            }
            let result = ok(
                (API.get().unwrap().dispatch)(s.callback.context.get(), &desc.header),
                "prepare FSR",
            );
            // Depth/motion are consumed by prepare. HUDless is consumed later by
            // interpolation, so it must be tagged with its original state.
            let post: Vec<_> = resources
                .iter()
                .filter(|(_, b)| *b != 0xc0)
                .map(|(t, b)| {
                    crate::host::transition(
                        t,
                        D3D12_RESOURCE_STATES(0xc0),
                        D3D12_RESOURCE_STATES(*b as i32),
                    )
                })
                .collect();
            if !post.is_empty() {
                list.list.ResourceBarrier(&post);
            }
            result?;
        }
        if let Some(h) = hudless {
            resources.push(h);
        }
        s.held = resources.into_iter().map(|(t, _)| t).collect();
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "FSR: frame generation disabled");
        ERRORS.fetch_add(1, Relaxed);
        let _ = s.configure(false, abi::Resource::default(), size);
        s.reset = true;
    }
}

/// SDK Present has synchronously recorded/enqueued interpolation on the game
/// queue (async workloads are disabled). Subsequent title submissions therefore
/// cannot overwrite inputs before interpolation reads them. Keep resource refs
/// until prepare's drain, but don't block the next rendered frame on scanout.
pub(crate) fn presented() {
    if !live() {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.as_mut() {
        // SDK has no public displayed-frame counter. This counts successful
        // dispatch submissions, not GPU completions or monitor scanouts.
        PER_FRAME.store(
            1 + (GENERATED.load(Relaxed) - s.generated_before).min(1) as u32,
            Relaxed,
        );
        s.reset = !s.enabled;
    }
}
pub(crate) fn resizing() {
    PER_FRAME.store(1, Relaxed);
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.as_mut() {
        if let Err(error) = s.destroy_effect() {
            ERRORS.fetch_add(1, Relaxed);
            tracing::error!(%error, "FSR resize teardown failed");
        }
    }
}
pub(crate) fn shutdown() {
    PER_FRAME.store(1, Relaxed);
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.as_mut() {
        let result = s.destroy_effect().and_then(|()| unsafe {
            ok(
                (API.get().unwrap().destroy)(&mut s.swap_context, std::ptr::null()),
                "destroy FSR swap chain",
            )
        });
        if let Err(error) = result {
            ERRORS.fetch_add(1, Relaxed);
            tracing::error!(%error, "FSR teardown failed; retaining callback storage");
            return;
        }
    }
    *guard = None;
    LIVE.store(false, Release);
}

pub(crate) fn line() {
    if live() {
        let (generated, errors) = counts();
        tracing::info!(
            generated,
            errors,
            per_frame = per_frame(),
            present_real_callbacks = PRESENT_REAL.load(Relaxed),
            present_generated_callbacks = PRESENT_GENERATED.load(Relaxed),
            "FSR frame generation totals (successful dispatches, not scanouts)"
        );
    }
}
