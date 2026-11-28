//! Host FidelityFX upscaling, adapted from Marvisak/VIctory PR #2751
//! (GPL-3.0-or-later). Enabled explicitly and independent of DLSS.

use super::*;
use std::ffi::CStr;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, OnceLock};
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

const UPSCALER_DLL: &str = "amd_fidelityfx_upscaler_dx12.dll";
const GET_VERSIONS: u64 = 4;
const QUERY_MEMORY_USAGE: u64 = 0x1_0009;
const NO_PROVIDER: u32 = 4;

#[repr(C)]
struct Versions {
    header: Header,
    effect: u64,
    device: *mut c_void,
    count: *mut u64,
    ids: *mut u64,
    names: *mut *const std::ffi::c_char,
}

#[repr(C)]
struct Backend {
    header: Header,
    device: *mut c_void,
}

struct Module(usize);
impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: one owned reference from LoadLibraryExW.
        unsafe { FreeLibrary(self.0 as _) };
    }
}

struct Api {
    _library: Module,
    create: Create,
    destroy: Destroy,
    configure: Call,
    query: Call,
    dispatch: Call,
    device: usize,
}

static API: OnceLock<Api> = OnceLock::new();

struct Context {
    handle: Mutex<usize>,
    created: Created,
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Some(api) = API.get() {
            let handle = *self.handle.get_mut().unwrap_or_else(|e| e.into_inner());
            let mut handle = handle as *mut c_void;
            // SAFETY: the title and any queued recording have released their reference.
            unsafe { (api.destroy)(&mut handle, null()) };
        }
    }
}

static CONTEXTS: Mutex<Option<HashMap<usize, Arc<Context>>>> = Mutex::new(None);

pub(super) fn init(dir: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let path = std::path::absolute(dir.join(UPSCALER_DLL)).map_err(|e| e.to_string())?;
    streamline::verify_signature(&path)?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a signed DLL, named by absolute path and resolved from its directory and System32.
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let library = Module(module as usize);
    let export = |name: &CStr| {
        // SAFETY: valid module and a nul-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const ())
            .ok_or_else(|| format!("missing {}", name.to_string_lossy()))
    };
    let device = streamline::host_upscale_device().ok_or("no host D3D12 device")?;
    let device_ptr = windows::core::Interface::as_raw(&device);
    // SAFETY: these are the signed SDK's published entry points.
    let api = unsafe {
        Api {
            _library: library,
            create: std::mem::transmute::<*const (), Create>(export(c"ffxCreateContext")?),
            destroy: std::mem::transmute::<*const (), Destroy>(export(c"ffxDestroyContext")?),
            configure: std::mem::transmute::<*const (), Call>(export(c"ffxConfigure")?),
            query: std::mem::transmute::<*const (), Call>(export(c"ffxQuery")?),
            dispatch: std::mem::transmute::<*const (), Call>(export(c"ffxDispatch")?),
            device: device_ptr as usize,
        }
    };
    let mut count = 0;
    let mut versions = Versions {
        header: Header {
            kind: GET_VERSIONS,
            next: null_mut(),
        },
        effect: CREATE_UPSCALE,
        device: device_ptr,
        count: &mut count,
        ids: null_mut(),
        names: null_mut(),
    };
    // SAFETY: a null context enumerates providers for the live device.
    if unsafe { (api.query)(null_mut(), &mut versions.header) } != OK || count == 0 {
        return Err("the selected device exposes no PC FidelityFX upscaler".to_owned());
    }
    let _ = API.set(api);
    tracing::info!(count, dll = %path.display(), "PC FidelityFX upscaling available");
    Ok(())
}

pub(super) fn active() -> bool {
    API.get().is_some()
}

pub(super) fn create(
    context: *mut *mut c_void,
    mut asked: CreateUpscale,
    memory: *const c_void,
) -> u32 {
    let Some(api) = API.get() else {
        return NO_PROVIDER;
    };
    if context.is_null() {
        return PARAMETER;
    }
    let mut backend = Backend {
        header: Header {
            kind: 2,
            next: asked.header.next,
        },
        device: api.device as *mut c_void,
    };
    // SAFETY: the immediate extension, when present, is the title's borrowed descriptor.
    if let Some(extension) = unsafe { asked.header.next.as_ref() }
        && extension.kind == 0x100_0002
    {
        backend.header.next = extension.next;
    }
    asked.header.next = &raw mut backend.header;
    // SAFETY: the SDK sees a DX12 backend and the context output for this call.
    let code = unsafe { (api.create)(context, &mut asked.header, memory) };
    if code != OK || context.is_null() {
        tracing::warn!(code, render = ?asked.max_render, output = ?asked.max_output, "PC FidelityFX context creation failed");
        return code;
    }
    // SAFETY: successful creation initialized the output slot.
    let handle = unsafe { *context } as usize;
    if handle == 0 {
        return NO_PROVIDER;
    }
    let created = Created {
        flags: asked.flags,
        max_render: asked.max_render,
        max_output: asked.max_output,
        clamped: false,
    };
    CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(
            handle,
            Arc::new(Context {
                handle: Mutex::new(handle),
                created,
            }),
        );
    tracing::info!(render = ?asked.max_render, output = ?asked.max_output, "PC FidelityFX upscaler created");
    OK
}

fn get(slot: *mut *mut c_void) -> Option<(usize, Arc<Context>)> {
    // SAFETY: the title supplies the shared context slot. A null slot means no context.
    let key = unsafe { *slot.as_ref()? } as usize;
    let contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
    Some((key, contexts.as_ref()?.get(&key)?.clone()))
}

pub(super) fn destroy(slot: *mut *mut c_void) -> Option<u32> {
    let (key, context) = get(slot)?;
    streamline::finish_host_upscale();
    CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()?
        .remove(&key);
    drop(context);
    // SAFETY: the title owns a writable context slot, as required by the destroy ABI.
    unsafe { *slot = null_mut() };
    Some(OK)
}

pub(super) fn call(slot: *mut *mut c_void, desc: *mut Header, query: bool) -> Option<u32> {
    let api = API.get()?;
    // Device queries use Xbox device pointers and are handled by the original loader.
    if query
        && unsafe { desc.as_ref() }
            .is_some_and(|header| matches!(header.kind, GET_VERSIONS | QUERY_MEMORY_USAGE))
    {
        return None;
    }
    let (_, context) = get(slot)?;
    let handle = context.handle.lock().unwrap_or_else(|e| e.into_inner());
    let mut handle = *handle as *mut c_void;
    let call = if query { api.query } else { api.configure };
    // SAFETY: the description is borrowed for this call and the PC context is live.
    Some(unsafe { call(&mut handle, desc) })
}

struct Dispatch(DispatchUpscale);

// The title command list and textures travel separately as Upscale IDs.
// Before queuing, the header chain was checked empty and these fields
// are cleared. Recording installs the corresponding host references.
unsafe impl Send for Dispatch {}

impl Dispatch {
    fn record(
        mut self,
        api: &Api,
        context: &Context,
        list: &windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList,
        textures: [Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>; 7],
    ) -> bool {
        use windows::core::Interface;
        self.0.list = list.as_raw();
        for (desc, texture) in self.0.resources.iter_mut().zip(&textures) {
            desc.resource = texture.as_ref().map_or(null_mut(), Interface::as_raw);
        }
        let handle = context.handle.lock().unwrap_or_else(|e| e.into_inner());
        let mut handle = *handle as *mut c_void;
        // SAFETY: the descriptors contain live host resources, and the PC context is retained.
        let code = unsafe { (api.dispatch)(&mut handle, &mut self.0.header) };
        if code == OK {
            once!(info, render = ?self.0.render_size, output = ?self.0.upscale_size,
                "PC FidelityFX recorded its first host upscale");
        } else {
            once!(warn, code, "PC FidelityFX refused a host upscale dispatch");
        }
        code == OK
    }
}

pub(super) fn dispatch(slot: *mut *mut c_void, desc: *mut Header) -> Option<u32> {
    let api = API.get()?;
    let (key, context) = get(slot)?;
    // SAFETY: an existing title context and a description supplied by its caller.
    let Some(header) = (unsafe { desc.as_ref() }) else {
        return Some(PARAMETER);
    };
    if header.kind != DISPATCH_UPSCALE || !header.next.is_null() {
        return Some(PARAMETER);
    }
    // SAFETY: the checked descriptor type begins with DispatchUpscale.
    let mut desc = unsafe { desc.cast::<DispatchUpscale>().read() };
    let up = upscale(&desc, &context.created, key);
    desc.list = null_mut();
    for resource in &mut desc.resources {
        resource.resource = null_mut();
    }
    let dispatch = Dispatch(desc);
    let queued = streamline::record_host_upscale(up, move |list, textures| {
        dispatch.record(api, &context, list, textures)
    });
    Some(if queued { OK } else { PARAMETER })
}
