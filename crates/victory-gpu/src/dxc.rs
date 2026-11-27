//! Compiling our own HLSL at run time.
//!
//! A few of the host's passes are written in HLSL and compiled when the host
//! starts: the indirect-argument clamp ([`crate::indirect`]), the compare of
//! copies in the card's memory and the depth-plane twins ([`crate::host`]),
//! and the in-place probe ([`crate::inplace`]). The system's compiler
//! (`D3DCompile`, `d3dcompiler_47`) is tried first, as it always was. Under
//! Wine and Proton that is vkd3d-shader, which refuses some of this source
//! outright (`E5005: Method 'Load' is not defined on type
//! 'RWByteAddressBuffer'`), and every caller then gives up: no argument
//! clamp, so indirect executions are dropped; no card compare, so card
//! buffers silently switch off; no twins. It also compiles the rest of the
//! source but drops the root signature the shader asks for, so
//! `CreateRootSignature` over its bytes fails and the twins still have no
//! pipeline. So the same source is compiled with DXC (`dxcompiler.dll`) as
//! shader model 6.0 when the system compiler refuses it, or leaves the root
//! signature out of a shader that asked for one. The root signature stays
//! inside the shader, so the callers see only bytes, and on Windows, where
//! `D3DCompile` succeeds and keeps the root signature, nothing changes.
//!
//! DXC is looked for in `VICTORY_DXC` (the DLL or its folder), beside the
//! `dxil.dll` that shader validation uses (the Windows SDK ships the two
//! together), then the executable's folder and System32. A container DXC
//! compiled without finding `dxil.dll` carries a zero digest, which D3D12
//! refuses: such containers are signed here (`sign`), `VICTORY_DXIL` naming
//! the validator if it is somewhere else.

use std::path::PathBuf;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Dxc::{
    CLSID_DxcCompiler, DXC_CP_UTF8, DxcBuffer, IDxcCompiler3, IDxcIncludeHandler, IDxcResult,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetProcAddress, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{GUID, HSTRING, Interface, PCWSTR, s, w};

/// Compiles function `entry` of `source` for `target` (e.g. `cs_5_1`), with
/// the system's compiler or, if it refuses or leaves the root signature out,
/// with DXC (see the module docs).
pub(crate) fn compile(source: &str, entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    let bytes = match system(source, entry, target) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::info!(
                error = %e,
                entry,
                target,
                "the system compiler refused one of our shaders: trying DXC"
            );
            return dxc(source, entry, target)
                .inspect_err(|e| tracing::warn!(error = %e, entry, target, "DXC failed too"));
        }
    };
    // Wine's compiler takes the source but drops the root signature it asks
    // for, and D3D12 refuses those bytes. A shader with no root signature of
    // its own (a vertex shader whose pixel shader carries it) keeps the
    // system's bytes: neither compiler has one to give.
    if !asks_for_root_signature(source, entry) || carries_root_signature(&bytes) {
        return Ok(bytes);
    }
    tracing::info!(
        entry,
        target,
        "the system compiler left the root signature out of one of our shaders: trying DXC"
    );
    match dxc(source, entry, target) {
        Ok(with_signature) if carries_root_signature(&with_signature) => Ok(with_signature),
        Ok(_) => Ok(bytes),
        Err(e) => {
            tracing::warn!(error = %e, entry, target, "DXC failed too");
            Ok(bytes)
        }
    }
}

/// Whether the container holds the root signature the shader asked for: both
/// compilers store it as the chunk `RTS0`.
fn carries_root_signature(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|chunk| chunk == b"RTS0")
}

/// Whether the declaration of `entry` asks for a root signature. A source can
/// hold several entries (a test compiles `corner`, which carries the
/// signature, and `lit`, which does not), and only the entry's own attribute
/// should send it to DXC. An attribute this cannot tie to a declaration counts
/// as the entry's: the answer only decides whether a second compile is
/// attempted, so it errs towards compiling again.
fn asks_for_root_signature(source: &str, entry: &str) -> bool {
    if !source.contains("RootSignature") {
        return false;
    }
    let mut from = 0;
    let mut found = false;
    while let Some(at) = source[from..].find("[RootSignature") {
        let at = from + at;
        from = at + 1;
        found = true;
        // Skip this attribute and any others between it and the declaration.
        let mut decl = &source[at..];
        while let Some(attribute) = decl.trim_start().strip_prefix('[') {
            let Some(close) = attribute.find(']') else {
                return true;
            };
            decl = &attribute[close + 1..];
        }
        // The declared function's name is the word before its `(`.
        let name = decl
            .trim_start()
            .split_once('(')
            .and_then(|(head, _)| head.split_whitespace().last());
        match name {
            Some(name) if name == entry => return true,
            Some(_) => {}
            None => return true,
        }
    }
    // A mention outside an attribute (`#define`) keeps the conservative
    // answer.
    !found
}

/// `D3DCompile`: the system's compiler, which on Windows compiles all of
/// this source.
fn system(source: &str, entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::PCSTR;
    // A `PCSTR` ends at its first zero: an interior zero truncates the name,
    // which is what the compiler would read anyway.
    let entry = format!("{entry}\0");
    let target = format!("{target}\0");
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("victory"),
            None,
            None,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::info!(
                "one of our shaders: {}",
                String::from_utf8_lossy(text).trim()
            );
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

/// Compiles with DXC: the same source as shader model 6.0 (5.1 is the
/// oldest profile DXC knows, and 6.0 takes everything 5.1 does), root
/// signature inside.
fn dxc(source: &str, entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    type Create = unsafe extern "system" fn(
        *const GUID,
        *const GUID,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    let library = library()?;
    let profile = match target.strip_suffix("_5_1") {
        Some(kind) => format!("{kind}_6_0"),
        None => target.to_string(),
    };
    let (target_flag, profile, entry) = (wide("-T"), wide(&profile), wide(entry));
    let arguments = [
        PCWSTR(target_flag.as_ptr()),
        PCWSTR(profile.as_ptr()),
        w!("-E"),
        PCWSTR(entry.as_ptr()),
        w!("-O3"),
    ];
    // SAFETY: loading the compiler and calling its documented entry point
    // with out pointers of ours; the source, the arguments and the strings
    // they point at outlive the calls.
    unsafe {
        let create: Create = std::mem::transmute(
            GetProcAddress(library, s!("DxcCreateInstance"))
                .ok_or_else(windows::core::Error::from_thread)?,
        );
        let mut compiler = std::ptr::null_mut();
        create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut compiler).ok()?;
        let compiler = IDxcCompiler3::from_raw(compiler);
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let result: IDxcResult =
            compiler.Compile(&buffer, Some(&arguments), None::<&IDxcIncludeHandler>)?;
        if let Err(e) = result.GetStatus()?.ok() {
            if let Ok(errors) = result.GetErrorBuffer() {
                // SAFETY: the blob's own bytes.
                let text = std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                );
                tracing::warn!(
                    "one of our shaders: DXC: {}",
                    String::from_utf8_lossy(text).trim()
                );
            }
            return Err(e);
        }
        let blob = result.GetResult()?;
        // SAFETY: the blob's own bytes.
        let mut bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec();
        sign(library, &mut bytes)?;
        Ok(bytes)
    }
}

/// Where `dxcompiler.dll` is looked for: `VICTORY_DXC` (the DLL or its
/// folder), beside the `dxil.dll` shader validation uses (the SDK ships
/// them together), then the executable's folder and System32.
fn library() -> windows::core::Result<HMODULE> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("VICTORY_DXC").map(PathBuf::from) {
        paths.push(if p.is_dir() {
            p.join("dxcompiler.dll")
        } else {
            p
        });
    }
    if let Some(dir) = victory_shader::emit::validator::Validator::default_path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        paths.push(dir.join("dxcompiler.dll"));
    }
    for path in paths.iter().filter(|p| p.is_file()) {
        // SAFETY: a NUL-terminated path; its folder (`dxil.dll`) is searched
        // first.
        if let Ok(module) = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_path()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        } {
            return Ok(module);
        }
    }
    // SAFETY: a bare name, searched only in the executable's folder and
    // System32.
    unsafe {
        LoadLibraryExW(
            w!("dxcompiler.dll"),
            None,
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
}

/// Older DXC DLLs compile without finding `dxil.dll`, and the container then
/// carries a zero digest, which D3D12 refuses: such a container is validated
/// (and signed) here.
fn sign(compiler: HMODULE, bytes: &mut [u8]) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStringExt;

    use victory_shader::emit::validator::Validator;
    use windows::Win32::Foundation::E_FAIL;

    if bytes
        .get(4..20)
        .is_some_and(|digest| digest.iter().any(|&b| b != 0))
    {
        return Ok(());
    }
    let beside_compiler = || {
        let mut path = vec![0; 32_768];
        // SAFETY: the compiler's module is loaded and the buffer is ours.
        let len = unsafe { GetModuleFileNameW(Some(compiler), &mut path) } as usize;
        if len == 0 || len >= path.len() {
            return None;
        }
        let compiler = PathBuf::from(std::ffi::OsString::from_wide(&path[..len]));
        let validator = compiler.with_file_name("dxil.dll");
        validator.is_file().then_some(validator)
    };
    let explicit = std::env::var_os("VICTORY_DXIL").map(PathBuf::from);
    let path = explicit
        .or_else(beside_compiler)
        .or_else(Validator::default_path);
    Validator::load(path.as_deref())
        .and_then(|mut validator| validator.validate(bytes))
        .map_err(|error| {
            windows::core::Error::new(E_FAIL, format!("cannot sign a DXC shader: {error}"))
        })
}

/// A NUL-terminated UTF-16 copy of `text`, for DXC's arguments.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    //! The fallback, and the bytes it makes: under Wine the system compiler
    //! refuses this source and DXC compiles it; on Windows `D3DCompile`
    //! compiles it, and either way D3D12 must take what comes out.

    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_COMPUTE_PIPELINE_STATE_DESC, D3D12_SHADER_BYTECODE, D3D12CreateDevice, ID3D12Device,
        ID3D12PipelineState, ID3D12RootSignature,
    };

    use super::*;

    /// A shader that loads through an `RWByteAddressBuffer` (vkd3d-shader
    /// refuses the `Load`) and asks for a root signature (which vkd3d-shader
    /// would drop): the shape the in-place probe has.
    const SOURCE: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer b : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() { b.Store(8, b.Load(0) + b.Load(4)); }
"#;

    #[test]
    fn a_shader_the_system_compiler_refuses_is_compiled_by_dxc() {
        let refused = match system(SOURCE, "main", "cs_5_1") {
            Ok(_) => {
                println!("the system compiler compiled it: DXC is not exercised here");
                false
            }
            Err(e) => {
                println!("the system compiler refused it: {e}");
                true
            }
        };
        // With neither a compiler for this source nor DXC, the fixture is
        // missing, not the fallback broken: Wine's builtin `d3dcompiler_47`
        // is the usual case (VICTORY_DXC names the DLL or its folder).
        if refused && library().is_err() {
            println!("no dxcompiler.dll here: the fallback is not checked (set VICTORY_DXC)");
            return;
        }
        let bytes = compile(SOURCE, "main", "cs_5_1").expect("a compiler must take the shader");
        let magic = std::str::from_utf8(&bytes[..4]).unwrap_or("?");
        println!("container {magic}, {} bytes", bytes.len());
        assert!(
            matches!(magic, "DXIL" | "DXBC"),
            "not a shader container: {magic}"
        );
        assert!(
            bytes[4..20].iter().any(|&b| b != 0),
            "the container carries no digest: D3D12 would refuse it"
        );
        assert!(
            carries_root_signature(&bytes),
            "the container carries no root signature, which the shader asks for"
        );
        let Some(device) = device() else {
            println!("no D3D12 device here: the root signature and pipeline are not checked");
            return;
        };
        // SAFETY: the root signature is inside the shader; the description
        // points at live bytes and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device
                .CreateRootSignature(0, &bytes)
                .expect("D3D12 must take the root signature inside the shader");
            device
                .CreateComputePipelineState::<ID3D12PipelineState>(
                    &D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    },
                )
                .expect("the shader must make a compute pipeline");
        }
        println!("D3D12 took the root signature and made the pipeline");
    }

    /// A source can hold several entries: only the entry carrying the
    /// attribute asks for a root signature, so only it is retried with DXC.
    #[test]
    fn a_root_signature_belongs_to_the_entry_it_sits_on() {
        const TWO: &str = r#"
#define RS "UAV(u0)"
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() { }
float4 lit() : SV_Target { return 1.0; }
"#;
        assert!(asks_for_root_signature(TWO, "main"));
        assert!(!asks_for_root_signature(TWO, "lit"));
        assert!(!asks_for_root_signature(
            "float4 lit() : SV_Target { return 1.0; }",
            "lit"
        ));
    }

    /// A shader with no root signature of its own keeps the system's bytes:
    /// its root signature comes from another stage.
    #[test]
    fn a_shader_that_asks_for_no_root_signature_compiles() {
        const CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
        let bytes = compile(CORNERS, "corner", "vs_5_1").expect("a compiler must take the shader");
        assert!(!carries_root_signature(&bytes));
    }

    /// A D3D12 device (the default adapter; WARP counts), if there is one.
    fn device() -> Option<ID3D12Device> {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: the out pointer is ours.
        unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device).ok()? };
        device
    }
}
