//! Our own compute shaders, compiled at run time from HLSL with their root
//! signature inside: by the system's compiler (`D3DCompile`, `cs_5_1`) and,
//! when it refuses one, by DXC (`cs_6_0`). Wine's builtin `d3dcompiler_47`
//! (vkd3d-shader) refuses `Load`, `Load4` and `InterlockedOr` on
//! `RWByteAddressBuffer`, which most of these use; DXC compiles the same
//! source. On Windows the system's compiler takes them all, so nothing
//! changes there.
//!
//! After `hlsl.rs` of JMAN730/victory `dffc5924` (by Codex), which did this
//! for upstream's clamp and rectangle helpers.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::{PCSTR, s};

/// Which compiler made a helper's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compiler {
    /// The system's `D3DCompile`, `cs_5_1`.
    System,
    /// DXC, `cs_6_0`.
    Dxc,
}

/// Compiles compute shader `source` (entry `main`), named `name` in
/// messages: the system's compiler first, then DXC. The code holds the
/// root signature the source declares.
pub(crate) fn compile_compute(name: &str, source: &str) -> windows::core::Result<Vec<u8>> {
    compile_compute_by(name, source).map(|(code, _)| code)
}

/// [`compile_compute`], saying which compiler made the code.
pub(crate) fn compile_compute_by(
    name: &str,
    source: &str,
) -> windows::core::Result<(Vec<u8>, Compiler)> {
    match system_compile(name, source) {
        Ok(code) => Ok((code, Compiler::System)),
        Err((e, messages)) => {
            tracing::info!(
                error = %e,
                "{name}: the system's HLSL compiler refused it ({}); trying DXC",
                messages.trim()
            );
            match dxc_compile(name, source) {
                Ok(code) => {
                    tracing::info!("{name}: compiled by DXC");
                    Ok((code, Compiler::Dxc))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "{name}: DXC failed too (VICTORY_DXC names dxcompiler.dll or its folder)");
                    Err(e)
                }
            }
        }
    }
}

/// Compiles function `entry` of `source` for `target` (a `_5_1` target:
/// `cs_5_1`, `vs_5_1`, `ps_5_1`): the system's compiler, then DXC at the 6.0
/// model of the same stage. For the helpers that `crate::host::compile`
/// and others compile at run time; Wine's builtin compiler refuses many of
/// them (the ambient light's stand-in, the card compare, the depth planes'
/// passes, the scatter, the upload check, keeping the card awake).
pub(crate) fn compile_for(name: &str, source: &str, entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    match system_compile_for(name, source, entry, target) {
        Ok(code) => Ok(code),
        Err((e, messages)) => {
            tracing::info!(error = %e, "{name} ({entry}, {target}): the system's HLSL compiler refused it ({}); trying DXC", messages.trim());
            let six = target.replacen("_5_1", "_6_0", 1).replacen("_5_0", "_6_0", 1);
            match dxc_compile_for(name, source, entry, &six) {
                Ok(code) => {
                    tracing::info!("{name} ({entry}): compiled by DXC as {six}");
                    Ok(code)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "{name} ({entry}): DXC failed too (VICTORY_DXC names dxcompiler.dll or its folder)");
                    Err(e)
                }
            }
        }
    }
}

/// A blob holding `bytes`, for code that hands compiled shaders around as
/// blobs.
pub(crate) fn blob(bytes: &[u8]) -> windows::core::Result<ID3DBlob> {
    // SAFETY: a blob of `bytes.len()` bytes, filled before it is handed out.
    unsafe {
        let blob = windows::Win32::Graphics::Direct3D::Fxc::D3DCreateBlob(bytes.len())?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), blob.GetBufferPointer().cast::<u8>(), bytes.len());
        Ok(blob)
    }
}

/// Compiles `source` with the system's `D3DCompile` as `cs_5_1`; on failure
/// the error and the compiler's messages.
pub(crate) fn system_compile(
    name: &str,
    source: &str,
) -> Result<Vec<u8>, (windows::core::Error, String)> {
    system_compile_for(name, source, "main", "cs_5_1")
}

/// [`system_compile`] of function `entry` for `target` (a `_5_1` target).
fn system_compile_for(
    name: &str,
    source: &str,
    entry: &str,
    target: &str,
) -> Result<Vec<u8>, (windows::core::Error, String)> {
    let cname = std::ffi::CString::new(name).unwrap_or_default();
    let centry = std::ffi::CString::new(entry).unwrap_or_default();
    let ctarget = std::ffi::CString::new(target).unwrap_or_default();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(cname.as_ptr().cast()),
            None,
            None,
            PCSTR(centry.as_ptr().cast()),
            PCSTR(ctarget.as_ptr().cast()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let messages = || {
        errors
            .as_ref()
            .map(|b| {
                String::from_utf8_lossy(blob_bytes(b))
                    .trim_end_matches('\0')
                    .to_owned()
            })
            .unwrap_or_default()
    };
    if let Err(e) = compiled {
        return Err((e, messages()));
    }
    let code = code
        .as_ref()
        .map(|c| blob_bytes(c).to_vec())
        .ok_or_else(|| (windows::core::Error::empty(), messages()))?;
    // Wine's builtin compiler accepts `[RootSignature(...)]` and leaves it
    // out of the code; the runtime then finds no root signature in it and
    // fails with E_FAIL (vkd3d-proton: no RTS0 part is VKD3D_ERROR). Every
    // helper that reads its root signature out of its code was off under
    // Wine for that (the depth planes' pass, keeping the card awake, the
    // scatter, the upload check, the tiles' look). Such code is refused, so
    // that DXC compiles it.
    if declares_root_signature(source, entry) && !parts(&code).any(|p| &p == b"RTS0") {
        return Err((
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL),
            "its code holds no root signature: the compiler left out [RootSignature]".to_owned(),
        ));
    }
    Ok(code)
}

/// The four-character codes of the parts of the shader container `code`
/// (DXBC and DXIL code alike are in one); none if it is not one.
fn parts(code: &[u8]) -> impl Iterator<Item = [u8; 4]> + '_ {
    let word = |at: usize| -> Option<usize> { Some(u32::from_le_bytes(code.get(at..at + 4)?.try_into().ok()?) as usize) };
    let count = if code.starts_with(b"DXBC") { word(28).unwrap_or(0) } else { 0 };
    (0..count.min(64)).filter_map(move |i| {
        let at = word(32 + 4 * i)?;
        code.get(at..at + 4)?.try_into().ok()
    })
}

/// Whether `code` is DXIL (made by DXC), not DXBC. The stages of one
/// pipeline are best of one kind.
pub(crate) fn is_dxil(code: &[u8]) -> bool {
    parts(code).any(|p| &p == b"DXIL")
}

/// Whether `source` puts a root signature on function `entry`: an
/// `[RootSignature` with `entry` named before the next `{`.
fn declares_root_signature(source: &str, entry: &str) -> bool {
    source.match_indices("[RootSignature").any(|(at, _)| {
        let head = &source[at..];
        let head = &head[..head.find('{').unwrap_or(head.len())];
        head.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|word| word == entry)
    })
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, borrowed for the blob's lifetime.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// `dxcompiler.dll`: at `VICTORY_DXC` (the DLL or its folder), beside the
/// `dxil.dll` shader validation uses (the SDK ships them together), then in
/// the executable's folder and System32.
fn dxc_library() -> windows::core::Result<windows::Win32::Foundation::HMODULE> {
    use windows::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    use windows::core::{HSTRING, w};
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("VICTORY_DXC").map(std::path::PathBuf::from) {
        paths.push(if p.is_dir() {
            p.join("dxcompiler.dll")
        } else {
            p
        });
    }
    if let Some(dir) = victory_shader::emit::validator::Validator::default_path()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    {
        paths.push(dir.join("dxcompiler.dll"));
    }
    for path in paths.iter().filter(|p| p.is_file()) {
        // SAFETY: a NUL-terminated path; its folder (dxil.dll) is searched first.
        if let Ok(m) = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_path()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        } {
            return Ok(m);
        }
    }
    // SAFETY: a bare name, searched only in the executable's folder and System32.
    unsafe {
        LoadLibraryExW(
            w!("dxcompiler.dll"),
            None,
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
}

/// Compiles compute shader `source` with DXC as `cs_6_0` (see
/// [`dxc_library`]); `name` labels its messages.
pub(crate) fn dxc_compile(name: &str, source: &str) -> windows::core::Result<Vec<u8>> {
    dxc_compile_for(name, source, "main", "cs_6_0")
}

/// [`dxc_compile`] of function `entry` for `target` (a 6.x target).
pub(crate) fn dxc_compile_for(name: &str, source: &str, entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Dxc::{
        CLSID_DxcCompiler, DXC_CP_UTF8, DxcBuffer, IDxcCompiler3, IDxcResult,
    };
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    use windows::core::{Interface, PCWSTR, w};
    type Create = unsafe extern "system" fn(
        *const windows::core::GUID,
        *const windows::core::GUID,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    // SAFETY: loading the compiler and calling its documented entry point with
    // out pointers of ours; the buffer and arguments outlive the calls.
    unsafe {
        let library = dxc_library()?;
        let create: Create = std::mem::transmute(
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
        let (target, entry) = (windows::core::HSTRING::from(target), windows::core::HSTRING::from(entry));
        let arguments = [w!("-T"), PCWSTR(target.as_ptr()), w!("-E"), PCWSTR(entry.as_ptr()), w!("-O3")];
        let result: IDxcResult = compiler.Compile(&buffer, Some(&arguments), None)?;
        if let Err(e) = result.GetStatus()?.ok() {
            if let Ok(errors) = result.GetErrorBuffer() {
                let text = std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                );
                tracing::warn!("{name}: DXC: {}", String::from_utf8_lossy(text).trim());
            }
            return Err(e);
        }
        let blob = result.GetResult()?;
        Ok(
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: &str = r#"
        RWByteAddressBuffer b : register(u0);
        [RootSignature("UAV(u0)")]
        [numthreads(1, 1, 1)]
        void main() { b.Store(0, b.Load(0)); }
    "#;

    /// A root signature counts as declared for the function it is put on
    /// only.
    #[test]
    fn a_root_signature_is_declared_for_its_function() {
        assert!(declares_root_signature(PROBE, "main"));
        assert!(!declares_root_signature(PROBE, "other"));
        let two = "float4 corner() : SV_Position { return 0; }\n[RootSignature(RS)]\nfloat plane() : SV_Depth { return 0; }";
        assert!(declares_root_signature(two, "plane"));
        assert!(!declares_root_signature(two, "corner"));
    }

    /// The code a helper is given holds the root signature its source
    /// declares, whichever compiler made it (Wine's own leaves it out).
    #[test]
    fn compiled_code_holds_its_root_signature() {
        if let Ok(code) = compile_compute("probe", PROBE) {
            assert!(parts(&code).any(|p| &p == b"RTS0"));
        }
        assert_eq!(parts(b"not a container").count(), 0);
    }

    /// A raw-buffer compute shader compiles with one of the two compilers
    /// whenever either is present, and both give shader containers.
    #[test]
    fn raw_buffer_shader_compiles_with_a_fallback() {
        let system = system_compile("probe", PROBE);
        let dxc = dxc_compile("probe", PROBE);
        for code in [system.as_ref().ok(), dxc.as_ref().ok()]
            .into_iter()
            .flatten()
        {
            assert_eq!(&code[..4], b"DXBC");
        }
        if system.is_ok() || dxc.is_ok() {
            assert_eq!(&compile_compute("probe", PROBE).unwrap()[..4], b"DXBC");
        } else {
            assert!(compile_compute("probe", PROBE).is_err());
        }
    }
}
