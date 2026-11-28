//! The layer's own HLSL shaders compiled with Microsoft's DXC, for Wine.
//!
//! Under Wine (Proton, vkd3d-proton), `d3dcompiler_47` either refuses some of
//! these sources outright (`RWByteAddressBuffer` methods such as `Load4` and
//! `InterlockedMax`), or compiles them and leaves out the root signature
//! their `[RootSignature]` attribute declares, so `CreateRootSignature` fails
//! later and the world stays black. With `VICTORY_DXC` set (`dxcompiler.dll`
//! or its folder, beside `dxil.dll`), [`d3d_compile`] tries DXC first and
//! falls back to `D3DCompile` when DXC cannot be loaded or refuses a source.
//! Without `VICTORY_DXC` nothing changes: every call is `D3DCompile`.
//!
//! Ported from the Linux branch of fxceless/victory-fork (its helpers follow
//! Marvisak/VIctory pull request #178).

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCreateBlob};
use windows::Win32::Graphics::Direct3D::{D3D_SHADER_MACRO, ID3DBlob, ID3DInclude};
use windows::core::{PCSTR, Param, s};

/// `VICTORY_DXC`, when it is set and not empty: `dxcompiler.dll` or its folder.
fn dxc_asked() -> Option<std::path::PathBuf> {
    std::env::var_os("VICTORY_DXC")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// `dxcompiler.dll` from `VICTORY_DXC` (the DLL or its folder); its folder is
/// searched first for the `dxil.dll` it signs with.
fn dxc_library() -> windows::core::Result<windows::Win32::Foundation::HMODULE> {
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
    };
    use windows::core::HSTRING;
    let Some(p) = dxc_asked() else {
        return Err(windows::core::Error::new(E_FAIL, "VICTORY_DXC is not set"));
    };
    let path = if p.is_dir() {
        p.join("dxcompiler.dll")
    } else {
        p
    };
    // SAFETY: a NUL-terminated path; its folder (dxil.dll) is searched first.
    unsafe {
        LoadLibraryExW(
            &HSTRING::from(path.as_path()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    }
}

/// `source` compiled by DXC for `entry` and `target` (a shader-model-6
/// profile such as `cs_6_0`), signed.
pub(crate) fn dxc_compile_for(
    source: &str,
    entry: &str,
    target: &str,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    use windows::core::{Interface, w};
    type Create = unsafe extern "system" fn(
        *const windows::core::GUID,
        *const windows::core::GUID,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    // SAFETY: loading the compiler and calling its documented entry point with
    // out pointers of ours; the buffer and arguments outlive the calls.
    unsafe {
        let library = dxc_library()?;
        let create = std::mem::transmute::<unsafe extern "system" fn() -> isize, Create>(
            GetProcAddress(library, s!("DxcCreateInstance"))
                .ok_or_else(windows::core::Error::from_thread)?,
        );
        let mut raw = std::ptr::null_mut();
        create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut raw).ok()?;
        let compiler = IDxcCompiler3::from_raw(raw);
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let (target, entry) = (
            windows::core::HSTRING::from(target),
            windows::core::HSTRING::from(entry),
        );
        let arguments = [
            w!("-T"),
            windows::core::PCWSTR(target.as_ptr()),
            w!("-E"),
            windows::core::PCWSTR(entry.as_ptr()),
            w!("-O3"),
        ];
        let result: IDxcResult = compiler.Compile(&buffer, Some(&arguments), None)?;
        if let Err(e) = result.GetStatus()?.ok() {
            if let Ok(errors) = result.GetErrorBuffer() {
                let text = std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                );
                tracing::warn!(
                    "a shader of ours: DXC: {}",
                    String::from_utf8_lossy(text).trim()
                );
            }
            return Err(e);
        }
        let blob = result.GetResult()?;
        let mut bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec();
        sign_dxc_bytecode(library, &mut bytes)?;
        Ok(bytes)
    }
}

/// Older DXC DLLs can compile without finding the signing DLL; the container
/// then has a zero digest and D3D12 rejects it.
fn sign_dxc_bytecode(
    compiler: windows::Win32::Foundation::HMODULE,
    bytes: &mut [u8],
) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStringExt;
    use victory_shader::emit::validator::Validator;
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

    if bytes
        .get(4..20)
        .is_some_and(|digest| digest.iter().any(|&b| b != 0))
    {
        return Ok(());
    }
    let explicit = std::env::var_os("VICTORY_DXIL").map(std::path::PathBuf::from);
    let beside_compiler = || {
        let mut path = vec![0; 32_768];
        // SAFETY: the compiler module is loaded and the buffer is ours.
        let len = unsafe { GetModuleFileNameW(Some(compiler), &mut path) } as usize;
        if len == 0 || len >= path.len() {
            return None;
        }
        let compiler = std::path::PathBuf::from(std::ffi::OsString::from_wide(&path[..len]));
        let validator = compiler.with_file_name("dxil.dll");
        validator.is_file().then_some(validator)
    };
    let path = explicit
        .or_else(beside_compiler)
        .or_else(Validator::default_path);
    Validator::load(path.as_deref())
        .and_then(|mut validator| validator.validate(bytes))
        .map_err(|error| {
            windows::core::Error::new(E_FAIL, format!("cannot sign DXC shader: {error}"))
        })
}

/// `D3DCompile`'s signature, DXC first when `VICTORY_DXC` asks for it: for a
/// source of ours with no defines, DXC compiles it for the same stage at
/// shader model 6.0 and the result is handed back as a blob, as `D3DCompile`
/// would. Without `VICTORY_DXC`, or when DXC cannot be loaded or refuses the
/// source, `D3DCompile` itself runs.
///
/// # Safety
/// As `D3DCompile`: `src` points to `len` readable bytes and the out pointers are valid.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn d3d_compile<P4: Param<ID3DInclude>>(
    src: *const std::ffi::c_void,
    len: usize,
    name: PCSTR,
    defines: Option<*const D3D_SHADER_MACRO>,
    include: P4,
    entry: PCSTR,
    target: PCSTR,
    flags1: u32,
    flags2: u32,
    code: *mut Option<ID3DBlob>,
    errors: Option<*mut Option<ID3DBlob>>,
) -> windows::core::Result<()> {
    if defines.is_none() && dxc_asked().is_some() {
        // SAFETY: the caller's contract.
        let bytes = unsafe { std::slice::from_raw_parts(src.cast::<u8>(), len) };
        // SAFETY: PCSTRs of the caller's, NUL-terminated.
        let (entry_s, target_s) = unsafe { (entry.to_string(), target.to_string()) };
        if let (Ok(source), Ok(entry_s), Ok(target_s)) =
            (std::str::from_utf8(bytes), entry_s, target_s)
        {
            let target6 = match target_s.split('_').next() {
                Some(stage) => format!("{stage}_6_0"),
                None => target_s.clone(),
            };
            match dxc_compile_for(source, &entry_s, &target6) {
                Ok(out) => {
                    // SAFETY: a fresh blob of the size asked, written in full.
                    unsafe {
                        let blob = D3DCreateBlob(out.len())?;
                        std::ptr::copy_nonoverlapping(
                            out.as_ptr(),
                            blob.GetBufferPointer().cast::<u8>(),
                            out.len(),
                        );
                        *code = Some(blob);
                    }
                    // SAFETY: the caller's PCSTR.
                    let shown = unsafe { name.to_string() }.unwrap_or_default();
                    tracing::debug!(name = %shown, target = %target6, "a shader of ours: compiled with DXC");
                    return Ok(());
                }
                Err(e) => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| tracing::info!(error = %e, "a shader of ours: DXC not used, D3DCompile instead"));
                }
            }
        }
    }
    // SAFETY: the caller's contract, passed on.
    unsafe {
        D3DCompile(
            src, len, name, defines, include, entry, target, flags1, flags2, code, errors,
        )
    }
}
