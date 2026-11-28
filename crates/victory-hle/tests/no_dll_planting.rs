//! Host pass-through and `LoadLibrary*` must only load allow-listed DLLs
//! from System32, never from the current folder, arbitrary paths, or via
//! path traversal in a wildcard-matched name.
#![cfg(windows)]

use victory_hle::modules::kernel::{FreeLibrary, LoadLibraryA, LoadLibraryExW, LoadLibraryW};
use victory_hle::{HleResolver, HostPolicy};
use victory_loader::{ImportResolver, Resolution};
use victory_pe::ImportSymbol;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_WITH_ALTERED_SEARCH_PATH,
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[test]
fn ignores_dlls_in_current_folder() {
    let name = "msvcp140_victorytest.dll";
    assert!(
        HostPolicy::default().allows(name),
        "test needs an allow-listed name"
    );
    assert!(HostPolicy::default().allows("vcruntime140_1.dll"));

    let dir = tempfile::tempdir().unwrap();
    // Any loadable DLL will do.
    let planted = dir.path().join(name);
    let planted_dbghelp = dir.path().join("dbghelp.dll");
    std::fs::copy(r"C:\Windows\System32\version.dll", &planted).unwrap();
    std::fs::copy(r"C:\Windows\System32\version.dll", &planted_dbghelp).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let symbol = ImportSymbol::Name {
        hint: 0,
        name: "GetFileVersionInfoSizeW".into(),
    };
    let found = HleResolver::default().classify(name, &symbol);
    let loaded_w = LoadLibraryW(wide(name).as_ptr());
    let loaded_a = LoadLibraryA(c"msvcp140_victorytest.dll".as_ptr().cast());
    let loaded_app_dir = LoadLibraryExW(
        wide(name).as_ptr(),
        std::ptr::null_mut(),
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
    );
    let loaded_altered = LoadLibraryExW(
        wide(name).as_ptr(),
        std::ptr::null_mut(),
        LOAD_WITH_ALTERED_SEARCH_PATH,
    );
    let loaded_full = LoadLibraryW(wide(&planted.to_string_lossy()).as_ptr());
    let loaded_dbghelp = LoadLibraryW(wide("dbghelp.dll").as_ptr());
    std::env::set_current_dir(env!("CARGO_MANIFEST_DIR")).unwrap();

    assert_eq!(found, None, "DLL was loaded from the current folder");
    assert!(
        loaded_w.is_null(),
        "LoadLibraryW loaded from current folder"
    );
    assert!(
        loaded_a.is_null(),
        "LoadLibraryA loaded from current folder"
    );
    assert!(
        loaded_app_dir.is_null(),
        "LoadLibraryExW(APPLICATION_DIR) loaded from current folder"
    );
    assert!(
        loaded_altered.is_null(),
        "LoadLibraryExW(ALTERED_SEARCH_PATH) loaded from current folder"
    );
    assert!(
        loaded_full.is_null(),
        "LoadLibraryW loaded an allow-listed wildcard name from outside System32"
    );
    if !loaded_dbghelp.is_null() {
        let mut buf = [0u16; 512];
        // SAFETY: valid module handle returned by LoadLibraryW.
        let len = unsafe {
            GetModuleFileNameW(loaded_dbghelp, buf.as_mut_ptr(), buf.len() as u32) as usize
        };
        let loaded_path = String::from_utf16_lossy(&buf[..len.min(buf.len())]).to_ascii_lowercase();
        assert_eq!(FreeLibrary(loaded_dbghelp), 1);
        assert!(
            loaded_path.contains(r"\system32\"),
            "dbghelp.dll loaded outside System32: {loaded_path}"
        );
    }
}

#[test]
fn rejects_path_traversal_in_host_policy_and_imports() {
    let policy = HostPolicy::default();
    let resolver = HleResolver::default();
    let cases = [
        (r"vcruntime140\..\wininet.dll", "InternetOpenW"),
        (r"vcruntime140/../wininet.dll", "InternetOpenW"),
        (r"vcruntime140\..\ws2_32.dll", "connect"),
        (r"msvcp140\..\ws2_32.dll", "connect"),
        (
            r"api-ms-win-core-synch-l1-1-0\..\wininet.dll",
            "InternetOpenW",
        ),
        (r"vcruntime140:stream", "memcpy"),
        (r"C:\Windows\System32\kernel32.dll", "GetTickCount"),
    ];
    for (dll, func) in cases {
        assert!(!policy.allows(dll), "HostPolicy::allows accepted {dll}");
        let sym = ImportSymbol::Name {
            hint: 0,
            name: func.into(),
        };
        assert_eq!(
            resolver.resolve(dll, &sym),
            Resolution::Missing,
            "HleResolver resolved {dll}!{func}"
        );
    }
}
