//! FidelityFX SDK v1.1.4 public C ABI (FSR 3.1.4).
//! Layouts from GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/ffx-api/include.
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Header {
    pub kind: u64,
    pub next: *mut Header,
}
impl Header {
    pub fn new(kind: u64) -> Self {
        Self {
            kind,
            next: std::ptr::null_mut(),
        }
    }
}
pub type Context = *mut c_void;
pub type Create = unsafe extern "C" fn(*mut Context, *mut Header, *const c_void) -> u32;
pub type Destroy = unsafe extern "C" fn(*mut Context, *const c_void) -> u32;
pub type Call = unsafe extern "C" fn(*mut Context, *const Header) -> u32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Resource {
    pub resource: *mut c_void,
    // type, format, width, height, depth, mipCount, flags, usage
    pub description: [u32; 8],
    pub state: u32,
}
#[repr(C)]
pub struct Backend {
    pub header: Header,
    pub device: *mut c_void,
}
#[repr(C)]
pub struct CreateFg {
    pub header: Header,
    pub flags: u32,
    pub display: [u32; 2],
    pub render: [u32; 2],
    pub format: u32,
}
#[repr(C)]
pub struct CreateSwap {
    pub header: Header,
    pub swapchain: *mut *mut c_void,
    pub hwnd: *mut c_void,
    pub desc: *const c_void,
    pub fullscreen: *const c_void,
    pub factory: *mut c_void,
    pub queue: *mut c_void,
}
#[repr(C)]
pub struct SwapKeyValue {
    pub header: Header,
    pub key: u64,
    pub value: u64,
    pub ptr: *mut c_void,
}
#[repr(C)]
pub struct PacingTuning {
    pub safety_margin_ms: f32,
    pub variance_factor: f32,
    pub hybrid_spin: bool,
    pub spin_time: u32,
    pub wait_on_fence: bool,
}
#[repr(C)]
pub struct Present {
    pub header: Header,
    pub device: *mut c_void,
    pub list: *mut c_void,
    pub color: Resource,
    pub ui: Resource,
    pub output: Resource,
    pub generated: bool,
    pub frame: u64,
}
#[repr(C)]
#[derive(Default)]
pub struct Configure {
    pub header: Header,
    pub swapchain: *mut c_void,
    pub present_callback: Option<unsafe extern "C" fn(*mut Present, *mut c_void) -> u32>,
    pub present_user: *mut c_void,
    pub generate: Option<unsafe extern "C" fn(*mut Generate, *mut c_void) -> u32>,
    pub generate_user: *mut c_void,
    pub enabled: bool,
    pub asynchronous: bool,
    pub hudless: Resource,
    pub flags: u32,
    pub only_generated: bool,
    pub rect: [i32; 4],
    pub frame: u64,
}
#[repr(C)]
pub struct Prepare {
    pub header: Header,
    pub frame: u64,
    pub flags: u32,
    pub list: *mut c_void,
    pub render: [u32; 2],
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub frame_ms: f32,
    pub reset: bool,
    pub near: f32,
    pub far: f32,
    pub fov: f32,
    pub meters: f32,
    pub depth: Resource,
    pub motion: Resource,
}
#[repr(C)]
pub struct Generate {
    pub header: Header,
    pub list: *mut c_void,
    pub color: Resource,
    pub outputs: [Resource; 4],
    pub count: u32,
    pub reset: bool,
    pub transfer: u32,
    pub luminance: [f32; 2],
    pub rect: [i32; 4],
    pub frame: u64,
}

pub fn surface(format: i32) -> Option<u32> {
    Some(match format {
        1 => 1,
        2 => 3,
        3 => 2,
        9 => 30,
        10 => 4,
        15 => 31,
        16 => 6,
        19..=21 => 28,
        22 => 7,
        23 => 32,
        24 => 17,
        26 => 16,
        27 => 9,
        28 => 10,
        29 => 12,
        31 => 11,
        33 => 33,
        34 => 18,
        36 => 19,
        39 => 37,
        40 | 41 => 28,
        42 => 8,
        44..=46 => 8,
        47 => 7,
        48 => 36,
        49 => 26,
        50 => 27,
        53 => 34,
        54 => 21,
        55 | 56 => 23,
        57 => 22,
        58 => 24,
        60 => 35,
        61 | 65 => 25,
        62 => 7,
        67 => 29,
        87 => 14,
        90 => 13,
        91 => 15,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_x64_layout() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(size_of::<Resource>(), 48);
        assert_eq!(size_of::<CreateFg>(), 40);
        assert_eq!(size_of::<Configure>(), 144);
        assert_eq!(size_of::<Prepare>(), 184);
        assert_eq!(size_of::<Generate>(), 312);
        assert_eq!(size_of::<Present>(), 192);
        assert_eq!(size_of::<SwapKeyValue>(), 40);
        assert_eq!(size_of::<PacingTuning>(), 20);
        assert_eq!(std::mem::offset_of!(Present, frame), 184);
        assert_eq!(std::mem::offset_of!(PacingTuning, spin_time), 12);
        assert_eq!(std::mem::offset_of!(Configure, hudless), 64);
        assert_eq!(std::mem::offset_of!(Prepare, depth), 88);
    }
}
