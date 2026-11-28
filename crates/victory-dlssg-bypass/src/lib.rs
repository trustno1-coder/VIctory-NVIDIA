//! Built-in RTX 30 (Ampere) DLSS Frame Generation and Multi-Frame Generation
//! bypass derived from `rodjunger/opendlssgenabler` (GPL-3.0-or-later).

use std::path::Path;

#[cfg(windows)]
unsafe extern "C" {
    fn victory_dlssg_bypass_init(multi_frame: bool, log_dir: *const u16) -> bool;
}

/// Initializes the in-process NVAPI spoof, Streamline plugin patcher,
/// multi-frame gate unlocker, and CUDA/D3D12 PTX-to-SM86 kernel substituter.
/// Safe to call multiple times; initialization runs at most once per process.
pub fn init(multi_frame: bool, log_dir: Option<&Path>) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Option<Vec<u16>> = log_dir.map(|p| p.as_os_str().encode_wide().chain([0]).collect());
        let ptr = wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
        // SAFETY: `ptr` is either null or points to a NUL-terminated UTF-16 string
        // alive across the FFI call.
        unsafe { victory_dlssg_bypass_init(multi_frame, ptr) }
    }
    #[cfg(not(windows))]
    {
        let _ = (multi_frame, log_dir);
        false
    }
}
